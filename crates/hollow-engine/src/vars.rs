//! CSS custom properties (`--name: value`) and `var()`.
//!
//! Custom properties are inherited text. `var()` is replaced by that text at
//! computed-value time and the result is parsed like any other value, which
//! is why a property using it can't be parsed with the stylesheet.
//!
//! Substitution is a classic amplification target: each level of
//! `--b: var(--a) var(--a)` doubles the text, and `--a: var(--b)` with
//! `--b: var(--a)` never ends. So values are capped at 64 KiB, nesting is
//! capped, an element sees a bounded number of properties, and a document
//! has a fixed budget for property storage and for substitution work. Going
//! over any of them makes the value invalid at computed-value time; it never
//! panics or takes unbounded memory or time.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::css::{self, Decl, RawDecl};
use crate::dom::NodeId;

/// Longest custom property value, and longest value after substitution.
pub(crate) const MAX_VALUE_BYTES: usize = 64 * 1024;
/// Custom properties one element can see.
const MAX_PROPS: usize = 4096;
/// Nesting of `var()` fallbacks and of references between one element's own
/// properties; deeper is invalid.
const MAX_DEPTH: usize = 32;
/// Maps chained by inheritance before one is flattened, which bounds the
/// cost of a lookup.
const MAX_LAYERS: u32 = 8;
/// Property entries all the maps of one style computation may hold.
const MAX_TOTAL_ENTRIES: usize = 1 << 20;
/// Bytes substitution may scan and produce in one style computation.
const MAX_TOTAL_WORK: usize = 128 << 20;

/// Whether a value calls `var()`. Strings are not looked into: a false
/// positive only costs a substitution that changes nothing.
pub(crate) fn has_var(v: &str) -> bool {
    v.as_bytes().windows(4).any(|w| w.eq_ignore_ascii_case(b"var("))
}

/// The custom properties an element sees: its own over its parent's.
/// Elements that declare none share their parent's map.
pub(crate) struct Vars {
    parent: Option<Rc<Vars>>,
    /// Sorted by name. `None` hides an inherited property (`initial`, or
    /// invalid at computed-value time).
    own: Vec<(Arc<str>, Option<Arc<str>>)>,
    /// Maps in the chain, this one included.
    layers: u32,
    /// Upper bound on the number of properties visible.
    len: usize,
}

impl Vars {
    fn get(&self, name: &str) -> Option<&Arc<str>> {
        let mut cur = Some(self);
        while let Some(v) = cur {
            if let Ok(i) = v.own.binary_search_by(|(n, _)| (**n).cmp(name)) {
                return v.own[i].1.as_ref();
            }
            cur = v.parent.as_deref();
        }
        None
    }
}

/// Custom-property state for one style computation.
pub(crate) struct Custom {
    /// What each node sees; text nodes and the root see nothing.
    nodes: Vec<Option<Rc<Vars>>>,
    /// What is left of [`MAX_TOTAL_ENTRIES`] and [`MAX_TOTAL_WORK`].
    entries: usize,
    work: usize,
}

impl Custom {
    pub fn new(nodes: usize) -> Custom {
        Custom { nodes: vec![None; nodes], entries: MAX_TOTAL_ENTRIES, work: MAX_TOTAL_WORK }
    }

    /// Cascades element `node`'s custom properties from its declarations,
    /// given in cascade order (later wins), and returns those declarations
    /// with every `var()` substituted and parsed.
    pub fn apply<'a>(&mut self, node: NodeId, parent: Option<NodeId>, decls: &[&'a Decl]) -> Vec<Cow<'a, Decl>> {
        let inherited = parent.and_then(|p| self.nodes.get(p)).cloned().flatten();
        let mut own: BTreeMap<&'a str, &'a RawDecl> = BTreeMap::new();
        let mut uses_var = false;
        for &d in decls {
            match d {
                Decl::Custom(raw) => {
                    let raw: &'a RawDecl = raw;
                    own.insert(&raw.name, raw);
                }
                Decl::Var(_) => uses_var = true,
                _ => {}
            }
        }
        let vars = if own.is_empty() { inherited } else { self.declare(inherited, own) };
        if let Some(slot) = self.nodes.get_mut(node) {
            slot.clone_from(&vars);
        }
        if !uses_var {
            return decls.iter().map(|&d| Cow::Borrowed(d)).collect();
        }
        let mut r = Resolver { own: Vec::new(), inherited: vars.as_deref(), stack: Vec::new(), work: &mut self.work };
        let mut out = Vec::with_capacity(decls.len());
        for &d in decls {
            match d {
                Decl::Var(raw) => {
                    let mut text = String::new();
                    let ok = r.substitute(&raw.value, &mut text, 0);
                    let parsed = css::parse_substituted(&raw.name, ok.then_some(text.as_str()), raw.base.as_ref());
                    out.extend(parsed.into_iter().map(Cow::Owned));
                }
                _ => out.push(Cow::Borrowed(d)),
            }
        }
        out
    }

    /// The map of an element that declares `own`, with `var()` in them
    /// resolved. Over budget, the declarations are ignored.
    fn declare(&mut self, inherited: Option<Rc<Vars>>, own: BTreeMap<&str, &RawDecl>) -> Option<Rc<Vars>> {
        if self.entries == 0 {
            return inherited;
        }
        let base = inherited.as_ref().map_or(0, |v| v.len);
        let mut r = Resolver {
            own: own
                .into_values()
                .take(MAX_PROPS.saturating_sub(base))
                .map(|raw| Own { raw, state: State::Todo, value: None, invalid: false })
                .collect(),
            inherited: inherited.as_deref(),
            stack: Vec::new(),
            work: &mut self.work,
        };
        for i in 0..r.own.len() {
            r.resolve(i, 0);
        }
        let below = inherited.as_deref();
        let mut entries: Vec<(Arc<str>, Option<Arc<str>>)> = r
            .own
            .into_iter()
            // An invalid property only needs an entry to hide an inherited one.
            .filter(|o| o.value.is_some() || below.and_then(|p| p.get(&o.raw.name)).is_some())
            .map(|o| (o.raw.name.clone(), o.value))
            .collect();
        let (parent, layers) = match &inherited {
            Some(p) if p.layers >= MAX_LAYERS => {
                // Flatten: copy everything visible into this one map.
                let mut all: BTreeMap<Arc<str>, Option<Arc<str>>> = entries.into_iter().collect();
                let mut cur = Some(p.as_ref());
                while let Some(v) = cur {
                    for (name, value) in &v.own {
                        all.entry(name.clone()).or_insert_with(|| value.clone());
                    }
                    cur = v.parent.as_deref();
                }
                entries = all.into_iter().filter(|(_, v)| v.is_some()).collect();
                (None, 1)
            }
            Some(p) => (Some(p.clone()), p.layers + 1),
            None => (None, 1),
        };
        if entries.len() > self.entries {
            self.entries = 0;
            return inherited;
        }
        self.entries -= entries.len();
        let len = parent.as_ref().map_or(0, |p| p.len) + entries.len();
        Some(Rc::new(Vars { parent, own: entries, layers, len }))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Todo,
    Active,
    Done,
}

/// One of an element's own custom properties during resolution.
struct Own<'a> {
    raw: &'a RawDecl,
    state: State,
    /// The computed value once done; `None` is the guaranteed-invalid value.
    value: Option<Arc<str>>,
    /// On a reference cycle, or on a chain too deep to follow: invalid
    /// whatever its fallbacks say.
    invalid: bool,
}

/// Resolves `var()` against an element's own custom properties, which may
/// refer to each other, and the inherited ones, which are already resolved.
struct Resolver<'a, 'v> {
    /// Sorted by name.
    own: Vec<Own<'a>>,
    inherited: Option<&'v Vars>,
    /// Own properties being resolved, outermost first.
    stack: Vec<usize>,
    work: &'v mut usize,
}

impl Resolver<'_, '_> {
    /// The computed value of own property `i`.
    fn resolve(&mut self, i: usize, depth: usize) -> Option<Arc<str>> {
        match self.own[i].state {
            State::Done => return self.own[i].value.clone(),
            State::Active => {
                // A cycle: every property on it is invalid.
                let start = self.stack.iter().position(|&s| s == i).unwrap_or(0);
                self.invalidate(start);
                return None;
            }
            State::Todo => {}
        }
        let raw = self.own[i].raw;
        let keyword = |k: &str| raw.value.eq_ignore_ascii_case(k);
        let value = if keyword("initial") {
            None
        } else if ["inherit", "unset", "revert", "revert-layer"].into_iter().any(keyword) {
            // Custom properties are inherited, so `unset` inherits too.
            self.inherited.and_then(|v| v.get(&raw.name)).cloned()
        } else if !has_var(&raw.value) {
            Some(raw.value.clone())
        } else if depth >= MAX_DEPTH {
            // Treated like a cycle, so long cycles are caught too.
            self.invalidate(0);
            None
        } else {
            self.own[i].state = State::Active;
            self.stack.push(i);
            let mut out = String::new();
            let ok = self.substitute(&raw.value, &mut out, depth + 1);
            self.stack.pop();
            (ok && !self.own[i].invalid).then(|| Arc::from(out))
        };
        self.own[i].state = State::Done;
        self.own[i].value.clone_from(&value);
        value
    }

    /// Marks the properties on the stack from `start` invalid.
    fn invalidate(&mut self, start: usize) {
        for k in start..self.stack.len() {
            let s = self.stack[k];
            self.own[s].invalid = true;
        }
    }

    fn lookup(&mut self, name: &str, depth: usize) -> Option<Arc<str>> {
        match self.own.binary_search_by(|o| (*o.raw.name).cmp(name)) {
            Ok(i) => self.resolve(i, depth),
            Err(_) => self.inherited.and_then(|v| v.get(name)).cloned(),
        }
    }

    /// Appends `value` to `out` with every `var()` replaced. False if a
    /// reference is invalid and has no fallback, a `var()` is malformed, or
    /// a budget runs out.
    fn substitute(&mut self, value: &str, out: &mut String, depth: usize) -> bool {
        if !self.charge(value.len()) {
            return false;
        }
        let b = value.as_bytes();
        let mut copied = 0;
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                q @ (b'"' | b'\'') => i = skip_string(b, i, q),
                b'\\' => i += 1,
                b'(' if is_var_call(b, i) => {
                    let Some(close) = close_paren(b, i) else { return false };
                    if !push(out, &value[copied..i - 3]) || !self.var(&value[i + 1..close], out, depth) {
                        return false;
                    }
                    i = close + 1;
                    copied = i;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        push(out, &value[copied..])
    }

    /// One `var(--name, fallback)`, given what is between its parentheses.
    fn var(&mut self, args: &str, out: &mut String, depth: usize) -> bool {
        if depth >= MAX_DEPTH {
            self.invalidate(0);
            return false;
        }
        let (name, fallback) = match top_level_comma(args) {
            Some(c) => (&args[..c], Some(&args[c + 1..])),
            None => (args, None),
        };
        let name = name.trim();
        if name.len() < 3 || !name.starts_with("--") {
            return false;
        }
        match self.lookup(name, depth) {
            Some(v) => self.charge(v.len()) && push(out, &v),
            None => fallback.is_some_and(|f| self.substitute(f, out, depth + 1)),
        }
    }

    fn charge(&mut self, bytes: usize) -> bool {
        if *self.work < bytes {
            *self.work = 0;
            return false;
        }
        *self.work -= bytes;
        true
    }
}

/// Appends unless that would make `out` longer than [`MAX_VALUE_BYTES`].
fn push(out: &mut String, s: &str) -> bool {
    if out.len() + s.len() > MAX_VALUE_BYTES {
        return false;
    }
    out.push_str(s);
    true
}

/// Whether the `(` at `i` opens `var(`, and not e.g. `myvar(`.
fn is_var_call(b: &[u8], i: usize) -> bool {
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c >= 0x80;
    i >= 3 && b[i - 3..i].eq_ignore_ascii_case(b"var") && (i == 3 || !ident(b[i - 4]))
}

/// Index of the closing quote of the string starting at `i` (or the end).
fn skip_string(b: &[u8], mut i: usize, quote: u8) -> usize {
    i += 1;
    while i < b.len() && b[i] != quote {
        if b[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    i
}

/// Index of the `)` closing the `(` at `open`.
fn close_paren(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'') => i = skip_string(b, i, q),
            b'\\' => i += 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Index of the first `,` outside strings and brackets.
fn top_level_comma(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'') => i = skip_string(b, i, q),
            b'\\' => i += 1,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}
