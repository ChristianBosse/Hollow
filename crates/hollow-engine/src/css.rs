//! A deliberately small CSS parser: selectors, declarations and the value
//! types the engine understands.
//!
//! Anything outside the supported subset is dropped silently, never treated
//! as an error: real-world CSS is mostly things we don't implement yet. Rules
//! whose selector list contains an unsupported selector are dropped whole,
//! as CSS requires, rather than being matched approximately.

use std::collections::BTreeMap;

use crate::dom::Element;
use crate::style::{Display, ListStyle, TextAlign, TextTransform, VAlign, WhiteSpace};
use crate::text::Family;

/// Hostile-input budgets for one stylesheet.
pub(crate) const MAX_CSS_BYTES: usize = 2 << 20;
const MAX_RULES: usize = 16_384;
/// Nested `@media` blocks deeper than this are skipped.
const MAX_MEDIA_NESTING: usize = 8;
const MAX_SELECTORS_PER_RULE: usize = 64;
const MAX_COMPOUNDS: usize = 32;
const MAX_DECLS_PER_BLOCK: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }

    const fn from_u32(v: u32) -> Color {
        Color::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum ColorValue {
    Rgba(Color),
    Current,
}

/// A specified length. Which variants a property accepts is up to its parser.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Length {
    Px(f32),
    Em(f32),
    Rem(f32),
    Pct(f32),
    Auto,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum FontSize {
    Length(Length),
    Px(f32),
    Larger,
    Smaller,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum FontWeight {
    Absolute(u16),
    Bolder,
    Lighter,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum LineHeightValue {
    Normal,
    Number(f32),
    Length(Length),
}

/// Properties that can take `inherit` / `initial` / `unset`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Prop {
    Display,
    Color,
    Background,
    FontSize,
    FontWeight,
    FontStyle,
    FontFamily,
    TextAlign,
    TextDecoration,
    LineHeight,
    WhiteSpace,
    ListStyle,
    Visibility,
    TextTransform,
    Margin(usize),
    Padding(usize),
    Width,
    MaxWidth,
    VerticalAlign,
    BoxSizing,
    BorderSpacing,
}

impl Prop {
    pub fn inherited(self) -> bool {
        matches!(
            self,
            Prop::Color
                | Prop::FontSize
                | Prop::FontWeight
                | Prop::FontStyle
                | Prop::FontFamily
                | Prop::TextAlign
                | Prop::LineHeight
                | Prop::WhiteSpace
                | Prop::ListStyle
                | Prop::Visibility
                | Prop::TextTransform
                | Prop::BorderSpacing
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wide {
    Inherit,
    Initial,
    Unset,
}

/// One parsed longhand declaration. Shorthands expand into several.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Decl {
    Display(Display),
    Color(ColorValue),
    Background(ColorValue),
    FontSize(FontSize),
    FontWeight(FontWeight),
    Italic(bool),
    FontFamily(Family),
    TextAlign(TextAlign),
    Decoration {
        underline: bool,
        line_through: bool,
    },
    LineHeight(LineHeightValue),
    WhiteSpace(WhiteSpace),
    ListStyle(ListStyle),
    Visible(bool),
    TextTransform(TextTransform),
    /// Side order: top, right, bottom, left.
    Margin(usize, Length),
    Padding(usize, Length),
    /// `Length::Auto` means `auto`.
    Width(Length),
    /// `None` means `none`.
    MaxWidth(Option<Length>),
    VerticalAlign(VAlign),
    BorderBox(bool),
    BorderSpacing(Length),
    Wide(Prop, Wide),
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Declaration {
    pub decl: Decl,
    pub important: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Combinator {
    Descendant,
    Child,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pseudo {
    /// `:link` / `:any-link`. `:visited` never matches: history must not
    /// influence rendering.
    Link,
    Root,
    /// Dynamic states (`:hover`, `:focus`, ...) that a static render never has.
    Never,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    pseudo: Vec<Pseudo>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Selector {
    /// Left to right; the last compound matches the subject element.
    compounds: Vec<Compound>,
    /// `combinators[i]` sits between `compounds[i]` and `compounds[i + 1]`.
    combinators: Vec<Combinator>,
    pub specificity: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub selectors: Vec<Selector>,
    pub decls: Vec<Declaration>,
    /// Enclosing `@media` conditions; all must hold.
    pub media: Vec<MediaList>,
}

/// A parsed stylesheet with rules indexed by their subject compound, so
/// matching an element only looks at rules that could possibly apply.
#[derive(Clone, Debug, Default)]
pub(crate) struct Stylesheet {
    pub rules: Vec<Rule>,
    by_id: BTreeMap<String, Vec<(u32, u32)>>,
    by_class: BTreeMap<String, Vec<(u32, u32)>>,
    by_tag: BTreeMap<String, Vec<(u32, u32)>>,
    universal: Vec<(u32, u32)>,
}

/// A rule that matched an element.
pub(crate) struct Matched<'a> {
    pub specificity: u32,
    /// Source order across all sheets of the cascade.
    pub order: u32,
    pub decls: &'a [Declaration],
}

impl Stylesheet {
    pub fn parse(css: &str) -> Stylesheet {
        let mut sheet = Stylesheet::default();
        sheet.append(css);
        sheet
    }

    /// Adds rules at the end of the sheet.
    pub fn append(&mut self, css: &str) {
        let css = strip_comments(truncate(css, MAX_CSS_BYTES).trim_start_matches('\u{FEFF}'));
        self.parse_rules(&css, &mut Vec::new());
    }

    /// Parses a rule list: the top level of a sheet or the body of `@media`.
    fn parse_rules(&mut self, css: &str, media: &mut Vec<MediaList>) {
        let b = css.as_bytes();
        let mut pos = 0;
        while pos < b.len() && self.rules.len() < MAX_RULES {
            while pos < b.len() && b[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if css[pos..].starts_with("<!--") {
                pos += 4;
                continue;
            }
            if css[pos..].starts_with("-->") {
                pos += 3;
                continue;
            }
            if pos >= b.len() {
                break;
            }
            let at_rule = b[pos] == b'@';
            let (i, stop) = find_top_level(b, pos, b"{;}");
            match stop {
                Some(b'{') => {
                    let (end, close) = skip_block(b, i);
                    let prelude = &css[pos..i];
                    let body = &css[i + 1..close];
                    if !at_rule {
                        self.add_rule(prelude, body, media);
                    } else if let Some(query) = strip_prefix_ci(prelude, "@media")
                        && media.len() < MAX_MEDIA_NESTING
                    {
                        // Evaluated at layout time against the viewport width.
                        // Which sheets are *fetched* never depends on it.
                        media.push(MediaList::parse(query));
                        self.parse_rules(body, media);
                        media.pop();
                    }
                    // Other block at-rules (@supports, @font-face, @keyframes,
                    // @layer, ...) are ignored with their contents.
                    pos = end;
                }
                // Statement at-rules such as `@import` (never fetched: no
                // extra requests) and stray `;` / `}`.
                Some(_) => pos = i + 1,
                None => break,
            }
        }
    }

    fn add_rule(&mut self, prelude: &str, body: &str, media: &[MediaList]) {
        let Some(selectors) = parse_selector_list(prelude) else { return };
        let mut decls = Vec::new();
        parse_declarations_into(body, &mut decls);
        if decls.is_empty() {
            return;
        }
        let rule = self.rules.len() as u32;
        for (si, sel) in selectors.iter().enumerate() {
            let subject = &sel.compounds[sel.compounds.len() - 1];
            if subject.pseudo.contains(&Pseudo::Never) {
                continue;
            }
            let entry = (rule, si as u32);
            if let Some(id) = &subject.id {
                self.by_id.entry(id.clone()).or_default().push(entry);
            } else if let Some(class) = subject.classes.first() {
                self.by_class.entry(class.clone()).or_default().push(entry);
            } else if let Some(tag) = &subject.tag {
                self.by_tag.entry(tag.clone()).or_default().push(entry);
            } else {
                self.universal.push(entry);
            }
        }
        self.rules.push(Rule { selectors, decls, media: media.to_vec() });
    }

    /// Every rule matching `chain[0]`, whose ancestors are `chain[1..]`
    /// (nearest first), for a viewport `width` CSS px wide. `order_base` is
    /// added to rule indices so several sheets can share one cascade order.
    pub fn matching<'a>(&'a self, chain: &[&Element], width: f32, order_base: u32, out: &mut Vec<Matched<'a>>) {
        let Some(el) = chain.first() else { return };
        let mut candidates: Vec<(u32, u32)> = self.universal.clone();
        if let Some(v) = el.id.as_ref().and_then(|id| self.by_id.get(id)) {
            candidates.extend_from_slice(v);
        }
        for class in &el.classes {
            if let Some(v) = self.by_class.get(class) {
                candidates.extend_from_slice(v);
            }
        }
        if let Some(v) = self.by_tag.get(&el.name) {
            candidates.extend_from_slice(v);
        }
        // An element with a repeated class would otherwise match twice; the
        // result would be the same but sorting keeps it deterministic anyway.
        candidates.sort_unstable();
        candidates.dedup();
        for (rule, sel) in candidates {
            let r = &self.rules[rule as usize];
            let s = &r.selectors[sel as usize];
            if r.media.iter().all(|m| m.matches(width)) && s.matches(chain) {
                out.push(Matched {
                    specificity: s.specificity,
                    order: order_base.saturating_add(rule),
                    decls: &r.decls,
                });
            }
        }
    }
}

impl Selector {
    fn matches(&self, chain: &[&Element]) -> bool {
        let n = self.compounds.len();
        if chain.is_empty() || !compound_matches(&self.compounds[n - 1], chain, 0) {
            return false;
        }
        // Dynamic programming over (compound, ancestor) instead of recursive
        // backtracking: O(compounds * depth) even for adversarial selectors.
        let mut reachable = vec![false; chain.len()];
        reachable[0] = true;
        for i in (0..n - 1).rev() {
            let mut next = vec![false; chain.len()];
            let mut any = false;
            match self.combinators[i] {
                Combinator::Child => {
                    for p in 0..chain.len() - 1 {
                        if reachable[p] && compound_matches(&self.compounds[i], chain, p + 1) {
                            next[p + 1] = true;
                            any = true;
                        }
                    }
                }
                Combinator::Descendant => {
                    let mut seen = false;
                    for p in 0..chain.len() {
                        if seen && compound_matches(&self.compounds[i], chain, p) {
                            next[p] = true;
                            any = true;
                        }
                        seen |= reachable[p];
                    }
                }
            }
            if !any {
                return false;
            }
            reachable = next;
        }
        true
    }
}

fn compound_matches(c: &Compound, chain: &[&Element], pos: usize) -> bool {
    let el = chain[pos];
    if c.tag.as_ref().is_some_and(|t| *t != el.name) {
        return false;
    }
    if c.id.is_some() && c.id != el.id {
        return false;
    }
    if !c.classes.iter().all(|cl| el.classes.contains(cl)) {
        return false;
    }
    c.pseudo.iter().all(|p| match p {
        Pseudo::Link => el.has_href && (el.name == "a" || el.name == "area"),
        Pseudo::Root => pos == chain.len() - 1 && el.name == "html",
        Pseudo::Never => false,
    })
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    let rest = &s[prefix.len()..];
    (head.eq_ignore_ascii_case(prefix) && rest.starts_with(|c: char| c.is_whitespace() || c == '(')).then_some(rest)
}

// ---------------------------------------------------------------------------
// Media queries

/// A comma-separated media query list. Matches if any query matches; an
/// empty list matches everything. Queries using features we can't evaluate
/// locally never match.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MediaList(Vec<Option<MediaQuery>>);

#[derive(Clone, Debug, PartialEq)]
struct MediaQuery {
    not: bool,
    /// `all`/`screen` match; `print` and other types don't.
    type_matches: bool,
    /// Inclusive width bounds and exclusive ones, all of which must hold.
    conds: Vec<WidthCond>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum WidthCond {
    Min(f32),
    Max(f32),
    Above(f32),
    Below(f32),
}

impl MediaList {
    pub fn parse(s: &str) -> MediaList {
        let s = s.trim();
        if s.is_empty() {
            return MediaList(Vec::new());
        }
        MediaList(
            split_top_level(&s.to_ascii_lowercase(), b',').into_iter().map(|q| parse_media_query(q.trim())).collect(),
        )
    }

    pub fn matches(&self, width: f32) -> bool {
        self.0.is_empty()
            || self.0.iter().flatten().any(|q| {
                let hit = q.type_matches
                    && q.conds.iter().all(|c| match *c {
                        WidthCond::Min(v) => width >= v,
                        WidthCond::Max(v) => width <= v,
                        WidthCond::Above(v) => width > v,
                        WidthCond::Below(v) => width < v,
                    });
                hit != q.not
            })
    }
}

fn parse_media_query(q: &str) -> Option<MediaQuery> {
    let mut not = false;
    let mut type_matches = true;
    let mut conds = Vec::new();
    let mut rest = q;
    let mut first = true;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(r) = rest.strip_prefix('(') {
            let close = r.find(')')?;
            conds.extend(parse_media_condition(&r[..close])?);
            rest = &r[close + 1..];
        } else {
            let end = rest.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(rest.len());
            let word = &rest[..end];
            rest = &rest[end..];
            match word {
                "and" if !first => {}
                "not" if first => not = true,
                "only" if first => {}
                "all" | "screen" => {}
                "print" | "speech" | "tv" | "handheld" | "projection" | "braille" | "embossed" | "aural" | "tty" => {
                    type_matches = false
                }
                _ => return None,
            }
        }
        first = false;
    }
    // `not` needs a media type before any condition, per the grammar.
    Some(MediaQuery { not, type_matches, conds })
}

/// `min-width: 600px`, `max-width: 40em`, `width >= 600px`,
/// `400px <= width < 800px`, ... Lengths in em/rem use the initial 16px.
fn parse_media_condition(s: &str) -> Option<Vec<WidthCond>> {
    let px = |v: &str| -> Option<f32> {
        match parse_length(v.trim())? {
            Length::Px(p) => Some(p),
            Length::Em(e) | Length::Rem(e) => Some(e * 16.0),
            _ => None,
        }
    };
    if let Some((name, value)) = s.split_once(':') {
        let v = px(value)?;
        return match name.trim() {
            "min-width" => Some(vec![WidthCond::Min(v)]),
            "max-width" => Some(vec![WidthCond::Max(v)]),
            "width" => Some(vec![WidthCond::Min(v), WidthCond::Max(v)]),
            _ => None,
        };
    }
    // Range syntax: operands separated by comparison operators.
    let mut operands = Vec::new();
    let mut ops = Vec::new();
    let b = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        if matches!(b[i], b'<' | b'>' | b'=') {
            operands.push(s[start..i].trim());
            let len = if b.get(i + 1) == Some(&b'=') && b[i] != b'=' { 2 } else { 1 };
            ops.push(&s[i..i + len]);
            i += len;
            start = i;
        } else {
            i += 1;
        }
    }
    operands.push(s[start..].trim());
    if ops.is_empty() || ops.len() > 2 {
        return None;
    }
    let mut conds = Vec::new();
    for (k, op) in ops.iter().enumerate() {
        let (a, b) = (operands[k], operands[k + 1]);
        let c = if a == "width" {
            let v = px(b)?;
            match *op {
                ">=" => vec![WidthCond::Min(v)],
                ">" => vec![WidthCond::Above(v)],
                "<=" => vec![WidthCond::Max(v)],
                "<" => vec![WidthCond::Below(v)],
                _ => vec![WidthCond::Min(v), WidthCond::Max(v)],
            }
        } else if b == "width" {
            let v = px(a)?;
            match *op {
                "<=" => vec![WidthCond::Min(v)],
                "<" => vec![WidthCond::Above(v)],
                ">=" => vec![WidthCond::Max(v)],
                ">" => vec![WidthCond::Below(v)],
                _ => vec![WidthCond::Min(v), WidthCond::Max(v)],
            }
        } else {
            return None;
        };
        conds.extend(c);
    }
    Some(conds)
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        out.push(' ');
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Index of the first byte in `stops` at nesting depth zero, skipping
/// strings and parenthesized/bracketed groups. Structural characters are
/// ASCII, so byte offsets found here are always char boundaries.
fn find_top_level(b: &[u8], from: usize, stops: &[u8]) -> (usize, Option<u8>) {
    let mut depth = 0usize;
    let mut i = from;
    while i < b.len() {
        let c = b[i];
        match c {
            b'"' | b'\'' => {
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'\\' => i += 1,
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            _ if depth == 0 && stops.contains(&c) => return (i, Some(c)),
            _ => {}
        }
        i += 1;
    }
    (b.len(), None)
}

/// Given the index of a `{`, returns (index after the matching `}`, index of
/// that `}`), or the end of input for an unterminated block.
fn skip_block(b: &[u8], open: usize) -> (usize, usize) {
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        match c {
            b'"' | b'\'' => {
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return (i + 1, i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    (b.len(), b.len())
}

/// Splits on `sep` at nesting depth zero (parens, brackets, braces, strings).
fn split_top_level(s: &str, sep: u8) -> Vec<&str> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'"' | b'\'' => {
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'\\' => i += 1,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ if c == sep && depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start <= b.len() {
        parts.push(&s[start.min(b.len())..]);
    }
    parts
}

// ---------------------------------------------------------------------------
// Selectors

pub(crate) fn parse_selector_list(s: &str) -> Option<Vec<Selector>> {
    let parts = split_top_level(s, b',');
    if parts.len() > MAX_SELECTORS_PER_RULE {
        return None;
    }
    parts.into_iter().map(parse_selector).collect()
}

fn parse_selector(s: &str) -> Option<Selector> {
    let chars: Vec<char> = s.trim().chars().collect();
    let mut compounds: Vec<Compound> = Vec::new();
    let mut combinators = Vec::new();
    let mut cur = Compound::default();
    let mut cur_has = false;
    let mut pending: Option<Combinator> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if cur_has {
                compounds.push(std::mem::take(&mut cur));
                cur_has = false;
                pending = Some(Combinator::Descendant);
            }
            i += 1;
            continue;
        }
        if c == '>' {
            if cur_has {
                compounds.push(std::mem::take(&mut cur));
                cur_has = false;
            }
            if compounds.is_empty() {
                return None;
            }
            pending = Some(Combinator::Child);
            i += 1;
            continue;
        }
        if !cur_has && !compounds.is_empty() {
            combinators.push(pending.take()?);
        }
        match c {
            '*' => {
                if cur_has {
                    return None;
                }
                i += 1;
            }
            '.' => {
                i += 1;
                cur.classes.push(ident(&chars, &mut i)?);
            }
            '#' => {
                i += 1;
                let id = ident(&chars, &mut i)?;
                if cur.id.is_some() {
                    // `#a#b` can never match a single element.
                    cur.pseudo.push(Pseudo::Never);
                }
                cur.id = Some(id);
            }
            ':' => {
                i += 1;
                if chars.get(i) == Some(&':') {
                    return None;
                }
                let name = ident(&chars, &mut i)?.to_ascii_lowercase();
                if chars.get(i) == Some(&'(') {
                    return None;
                }
                cur.pseudo.push(match name.as_str() {
                    "link" | "any-link" => Pseudo::Link,
                    "root" => Pseudo::Root,
                    "visited" | "hover" | "active" | "focus" | "focus-visible" | "focus-within" | "target"
                    | "checked" | "disabled" | "enabled" | "indeterminate" | "default" | "autofill"
                    | "placeholder-shown" | "popover-open" | "modal" | "fullscreen" | "open" => Pseudo::Never,
                    _ => return None,
                });
            }
            c if c == '\\' || is_ident_start(c) => {
                if cur_has {
                    return None;
                }
                cur.tag = Some(ident(&chars, &mut i)?.to_ascii_lowercase());
            }
            _ => return None,
        }
        cur_has = true;
    }
    if cur_has {
        compounds.push(cur);
    } else if pending == Some(Combinator::Child) {
        return None;
    }
    if compounds.is_empty() || compounds.len() > MAX_COMPOUNDS {
        return None;
    }
    let (mut ids, mut classes, mut tags) = (0u32, 0u32, 0u32);
    for c in &compounds {
        ids += c.id.is_some() as u32;
        classes += (c.classes.len() + c.pseudo.len()) as u32;
        tags += c.tag.is_some() as u32;
    }
    let specificity = (ids.min(255) << 16) | (classes.min(255) << 8) | tags.min(255);
    Some(Selector { compounds, combinators, specificity })
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '-' || !c.is_ascii()
}

fn ident(chars: &[char], i: &mut usize) -> Option<String> {
    let mut out = String::new();
    while let Some(&c) = chars.get(*i) {
        if c == '\\' {
            *i += 1;
            let mut hex = String::new();
            while hex.len() < 6 && chars.get(*i).is_some_and(|c| c.is_ascii_hexdigit()) {
                hex.push(chars[*i]);
                *i += 1;
            }
            if hex.is_empty() {
                out.push(*chars.get(*i)?);
                *i += 1;
            } else {
                out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}'));
                if chars.get(*i).is_some_and(|c| c.is_whitespace()) {
                    *i += 1;
                }
            }
        } else if is_ident_start(c) || c.is_ascii_digit() {
            out.push(c);
            *i += 1;
        } else {
            break;
        }
    }
    (!out.is_empty()).then_some(out)
}

// ---------------------------------------------------------------------------
// Declarations

/// Parses the contents of a `style=""` attribute.
pub(crate) fn parse_declarations(s: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    parse_declarations_into(&strip_comments(truncate(s, 64 * 1024)), &mut out);
    out
}

fn parse_declarations_into(body: &str, out: &mut Vec<Declaration>) {
    for part in split_top_level(body, b';').into_iter().take(MAX_DECLS_PER_BLOCK) {
        let Some(colon) = part.find(':') else { continue };
        let name = part[..colon].trim().to_ascii_lowercase();
        let mut value = part[colon + 1..].trim();
        let mut important = false;
        if let Some(bang) = value.rfind('!')
            && value[bang + 1..].trim().eq_ignore_ascii_case("important")
        {
            important = true;
            value = value[..bang].trim_end();
        }
        if value.is_empty() || value.contains("var(") || value.contains('{') {
            continue;
        }
        let mut decls = Vec::new();
        parse_property(&name, value, &mut decls);
        out.extend(decls.into_iter().map(|decl| Declaration { decl, important }));
    }
}

fn wide_keyword(v: &str) -> Option<Wide> {
    match v.to_ascii_lowercase().as_str() {
        "inherit" => Some(Wide::Inherit),
        "initial" => Some(Wide::Initial),
        "unset" | "revert" | "revert-layer" => Some(Wide::Unset),
        _ => None,
    }
}

fn parse_property(name: &str, value: &str, out: &mut Vec<Decl>) {
    let props: &[Prop] = match name {
        "display" => &[Prop::Display],
        "color" => &[Prop::Color],
        "background-color" | "background" => &[Prop::Background],
        "font-size" => &[Prop::FontSize],
        "font-weight" => &[Prop::FontWeight],
        "font-style" => &[Prop::FontStyle],
        "font-family" => &[Prop::FontFamily],
        "font" => &[Prop::FontStyle, Prop::FontWeight, Prop::FontSize, Prop::LineHeight, Prop::FontFamily],
        "text-align" => &[Prop::TextAlign],
        "text-decoration" | "text-decoration-line" => &[Prop::TextDecoration],
        "line-height" => &[Prop::LineHeight],
        "white-space" => &[Prop::WhiteSpace],
        "list-style-type" | "list-style" => &[Prop::ListStyle],
        "visibility" => &[Prop::Visibility],
        "text-transform" => &[Prop::TextTransform],
        "margin" => &[Prop::Margin(0), Prop::Margin(1), Prop::Margin(2), Prop::Margin(3)],
        "padding" => &[Prop::Padding(0), Prop::Padding(1), Prop::Padding(2), Prop::Padding(3)],
        "margin-top" => &[Prop::Margin(0)],
        "margin-right" => &[Prop::Margin(1)],
        "margin-bottom" => &[Prop::Margin(2)],
        "margin-left" => &[Prop::Margin(3)],
        "padding-top" => &[Prop::Padding(0)],
        "padding-right" => &[Prop::Padding(1)],
        "padding-bottom" => &[Prop::Padding(2)],
        "padding-left" => &[Prop::Padding(3)],
        "width" => &[Prop::Width],
        "max-width" => &[Prop::MaxWidth],
        "vertical-align" => &[Prop::VerticalAlign],
        "box-sizing" => &[Prop::BoxSizing],
        "border-spacing" => &[Prop::BorderSpacing],
        _ => return,
    };
    if let Some(w) = wide_keyword(value) {
        out.extend(props.iter().map(|&p| Decl::Wide(p, w)));
        return;
    }
    let lower = value.to_ascii_lowercase();
    let decl = match name {
        "display" => parse_display(&lower).map(Decl::Display),
        "color" => parse_color(&lower).map(Decl::Color),
        "background-color" => parse_color(&lower).map(Decl::Background),
        "background" => Some(Decl::Background(parse_background(&lower))),
        "font-size" => parse_font_size(&lower).map(Decl::FontSize),
        "font-weight" => parse_font_weight(&lower).map(Decl::FontWeight),
        "font-style" => match lower.split_whitespace().next() {
            Some("normal") => Some(Decl::Italic(false)),
            Some("italic" | "oblique") => Some(Decl::Italic(true)),
            _ => None,
        },
        "font-family" => Some(Decl::FontFamily(parse_family(&lower))),
        "font" => return parse_font_shorthand(&lower, out),
        "text-align" => match lower.as_str() {
            "left" | "start" | "justify" | "-moz-left" | "-webkit-left" => Some(Decl::TextAlign(TextAlign::Left)),
            "right" | "end" | "-moz-right" | "-webkit-right" => Some(Decl::TextAlign(TextAlign::Right)),
            "center" | "-moz-center" | "-webkit-center" => Some(Decl::TextAlign(TextAlign::Center)),
            _ => None,
        },
        "text-decoration" | "text-decoration-line" => {
            let mut underline = false;
            let mut line_through = false;
            for t in lower.split_whitespace() {
                match t {
                    "underline" => underline = true,
                    "line-through" => line_through = true,
                    _ => {}
                }
            }
            Some(Decl::Decoration { underline, line_through })
        }
        "line-height" => parse_line_height(&lower).map(Decl::LineHeight),
        "white-space" => match lower.as_str() {
            "normal" => Some(WhiteSpace::Normal),
            "nowrap" => Some(WhiteSpace::NoWrap),
            "pre" => Some(WhiteSpace::Pre),
            "pre-wrap" | "break-spaces" => Some(WhiteSpace::PreWrap),
            "pre-line" => Some(WhiteSpace::PreLine),
            _ => None,
        }
        .map(Decl::WhiteSpace),
        "list-style-type" | "list-style" => lower.split_whitespace().find_map(parse_list_style).map(Decl::ListStyle),
        "visibility" => match lower.as_str() {
            "visible" => Some(Decl::Visible(true)),
            "hidden" | "collapse" => Some(Decl::Visible(false)),
            _ => None,
        },
        "text-transform" => match lower.as_str() {
            "none" => Some(TextTransform::None),
            "uppercase" => Some(TextTransform::Upper),
            "lowercase" => Some(TextTransform::Lower),
            "capitalize" => Some(TextTransform::Capitalize),
            _ => None,
        }
        .map(Decl::TextTransform),
        "margin" | "padding" => {
            let vals: Option<Vec<Length>> = lower.split_whitespace().map(parse_length).collect();
            let Some(vals) = vals else { return };
            let sides = match vals.as_slice() {
                [a] => [*a, *a, *a, *a],
                [a, b] => [*a, *b, *a, *b],
                [a, b, c] => [*a, *b, *c, *b],
                [a, b, c, d] => [*a, *b, *c, *d],
                _ => return,
            };
            for (side, len) in sides.into_iter().enumerate() {
                if name == "margin" {
                    out.push(Decl::Margin(side, len));
                } else if is_valid_padding(len) {
                    out.push(Decl::Padding(side, len));
                }
            }
            return;
        }
        "margin-top" | "margin-right" | "margin-bottom" | "margin-left" => {
            let side = match name {
                "margin-top" => 0,
                "margin-right" => 1,
                "margin-bottom" => 2,
                _ => 3,
            };
            parse_length(&lower).map(|l| Decl::Margin(side, l))
        }
        "padding-top" | "padding-right" | "padding-bottom" | "padding-left" => {
            let side = match name {
                "padding-top" => 0,
                "padding-right" => 1,
                "padding-bottom" => 2,
                _ => 3,
            };
            parse_length(&lower).filter(|l| is_valid_padding(*l)).map(|l| Decl::Padding(side, l))
        }
        "width" => parse_length(&lower).filter(|l| non_negative(*l)).map(Decl::Width),
        "max-width" => {
            if lower == "none" {
                Some(Decl::MaxWidth(None))
            } else {
                parse_length(&lower).filter(|l| non_negative(*l) && *l != Length::Auto).map(|l| Decl::MaxWidth(Some(l)))
            }
        }
        "vertical-align" => match lower.as_str() {
            "baseline" => Some(VAlign::Baseline),
            "sub" => Some(VAlign::Sub),
            "super" => Some(VAlign::Super),
            "top" | "text-top" => Some(VAlign::Top),
            "middle" => Some(VAlign::Middle),
            "bottom" | "text-bottom" => Some(VAlign::Bottom),
            _ => None,
        }
        .map(Decl::VerticalAlign),
        "box-sizing" => match lower.as_str() {
            "border-box" => Some(Decl::BorderBox(true)),
            "content-box" => Some(Decl::BorderBox(false)),
            _ => None,
        },
        "border-spacing" => lower
            .split_whitespace()
            .next()
            .and_then(parse_length)
            .filter(|l| non_negative(*l) && !matches!(l, Length::Pct(_) | Length::Auto))
            .map(Decl::BorderSpacing),
        _ => None,
    };
    out.extend(decl);
}

fn non_negative(l: Length) -> bool {
    match l {
        Length::Px(v) | Length::Em(v) | Length::Rem(v) | Length::Pct(v) => v >= 0.0,
        Length::Auto => true,
    }
}

fn is_valid_padding(l: Length) -> bool {
    l != Length::Auto && non_negative(l)
}

pub(crate) fn parse_display(v: &str) -> Option<Display> {
    // Multi-keyword syntax: `block flow`, `inline flex`, ...
    let mut outer = None;
    let mut list_item = false;
    for t in v.split_whitespace() {
        match t {
            "list-item" => list_item = true,
            "flow" | "flow-root" => {}
            _ => outer = Some(t),
        }
    }
    if list_item {
        return Some(Display::ListItem);
    }
    Some(match outer? {
        "none" => Display::None,
        "inline" | "inline-block" | "inline-flex" | "inline-grid" | "inline-table" | "contents" | "ruby" => {
            Display::Inline
        }
        "block" | "flex" | "grid" | "-webkit-box" | "-webkit-flex" | "-ms-flexbox" | "table-caption" => Display::Block,
        "table" => Display::Table,
        "table-row-group" | "table-header-group" | "table-footer-group" => Display::TableRowGroup,
        "table-row" => Display::TableRow,
        "table-cell" => Display::TableCell,
        "table-column" | "table-column-group" => Display::None,
        _ => return None,
    })
}

fn parse_list_style(t: &str) -> Option<ListStyle> {
    Some(match t {
        "disc" => ListStyle::Disc,
        "circle" => ListStyle::Circle,
        "square" => ListStyle::Square,
        "decimal" | "decimal-leading-zero" => ListStyle::Decimal,
        "lower-alpha" | "lower-latin" => ListStyle::LowerAlpha,
        "upper-alpha" | "upper-latin" => ListStyle::UpperAlpha,
        "lower-roman" => ListStyle::LowerRoman,
        "upper-roman" => ListStyle::UpperRoman,
        "none" => ListStyle::None,
        _ => return None,
    })
}

/// Parses a number with an optional unit; `None` for anything unsupported
/// (including `calc()` and viewport units, whose results would depend on
/// state we don't want styles to depend on yet).
pub(crate) fn parse_length(t: &str) -> Option<Length> {
    let t = t.trim();
    if t.eq_ignore_ascii_case("auto") {
        return Some(Length::Auto);
    }
    let (n, unit) = split_number(t)?;
    let n = n.clamp(-1e6, 1e6);
    Some(match unit.to_ascii_lowercase().as_str() {
        "px" => Length::Px(n),
        "em" => Length::Em(n),
        "rem" => Length::Rem(n),
        "%" => Length::Pct(n),
        "pt" => Length::Px(n * 4.0 / 3.0),
        "pc" => Length::Px(n * 16.0),
        "in" => Length::Px(n * 96.0),
        "cm" => Length::Px(n * 96.0 / 2.54),
        "mm" => Length::Px(n * 96.0 / 25.4),
        "q" => Length::Px(n * 96.0 / 101.6),
        "ex" | "ch" => Length::Em(n * 0.5),
        "" if n == 0.0 => Length::Px(0.0),
        _ => return None,
    })
}

/// Splits `12.5px` into (12.5, "px"). Rust's float parsing is correctly
/// rounded, so this is deterministic across platforms.
fn split_number(t: &str) -> Option<(f32, &str)> {
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == digits_start || (i == digits_start + 1 && b[digits_start] == b'.') {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    let n: f32 = t[..i].parse().ok()?;
    n.is_finite().then_some((n, &t[i..]))
}

fn parse_number(t: &str) -> Option<f32> {
    match split_number(t.trim()) {
        Some((n, "")) => Some(n),
        _ => None,
    }
}

fn parse_font_size(v: &str) -> Option<FontSize> {
    Some(match v {
        "xx-small" => FontSize::Px(9.0),
        "x-small" => FontSize::Px(10.0),
        "small" => FontSize::Px(13.0),
        "medium" => FontSize::Px(16.0),
        "large" => FontSize::Px(18.0),
        "x-large" => FontSize::Px(24.0),
        "xx-large" => FontSize::Px(32.0),
        "xxx-large" => FontSize::Px(48.0),
        "larger" => FontSize::Larger,
        "smaller" => FontSize::Smaller,
        _ => match parse_length(v)? {
            Length::Auto => return None,
            l if !non_negative(l) => return None,
            l => FontSize::Length(l),
        },
    })
}

fn parse_font_weight(v: &str) -> Option<FontWeight> {
    Some(match v {
        "normal" => FontWeight::Absolute(400),
        "bold" => FontWeight::Absolute(700),
        "bolder" => FontWeight::Bolder,
        "lighter" => FontWeight::Lighter,
        _ => {
            let n = parse_number(v)?;
            if !(1.0..=1000.0).contains(&n) {
                return None;
            }
            FontWeight::Absolute(n as u16)
        }
    })
}

fn parse_line_height(v: &str) -> Option<LineHeightValue> {
    if v == "normal" {
        return Some(LineHeightValue::Normal);
    }
    if let Some(n) = parse_number(v) {
        return (n >= 0.0).then_some(LineHeightValue::Number(n.min(100.0)));
    }
    match parse_length(v)? {
        Length::Auto => None,
        l if !non_negative(l) => None,
        l => Some(LineHeightValue::Length(l)),
    }
}

/// Maps a `font-family` list onto a bundled family. Unknown names are
/// skipped; a list naming only unknown (web) fonts gets sans, which is what
/// most web fonts are.
pub(crate) fn parse_family(v: &str) -> Family {
    for name in v.split(',') {
        let name = name.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        let family = match name {
            "monospace" | "ui-monospace" | "courier" | "courier new" | "consolas" | "menlo" | "monaco"
            | "lucida console" | "andale mono" | "sfmono-regular" | "fira code" | "fira mono" | "source code pro"
            | "liberation mono" | "jetbrains mono" => Some(Family::Mono),
            "serif" | "ui-serif" | "times" | "times new roman" | "georgia" | "cambria" | "garamond" | "palatino"
            | "palatino linotype" | "book antiqua" | "liberation serif" | "noto serif" | "baskerville" | "charter"
            | "constantia" => Some(Family::Serif),
            "sans-serif" | "ui-sans-serif" | "system-ui" | "-apple-system" | "blinkmacsystemfont" | "arial"
            | "helvetica" | "helvetica neue" | "verdana" | "tahoma" | "segoe ui" | "roboto" | "trebuchet ms"
            | "liberation sans" | "noto sans" | "ubuntu" | "cantarell" | "open sans" | "lucida grande" => {
                Some(Family::Sans)
            }
            n if n.contains("mono") || n.contains("code") => Some(Family::Mono),
            n if n.contains("sans") => Some(Family::Sans),
            n if n.contains("serif") => Some(Family::Serif),
            _ => None,
        };
        if let Some(f) = family {
            return f;
        }
    }
    Family::Sans
}

fn parse_font_shorthand(v: &str, out: &mut Vec<Decl>) {
    let parts = split_top_level(v, b' ');
    let parts: Vec<&str> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    let mut italic = false;
    let mut weight = FontWeight::Absolute(400);
    let mut i = 0;
    let mut size = None;
    let mut line_height = LineHeightValue::Normal;
    while i < parts.len() {
        let p = parts[i];
        let (size_part, lh_part) = match p.split_once('/') {
            Some((a, b)) => (a, Some(b)),
            None => (p, None),
        };
        if let Some(s) = parse_font_size(size_part) {
            size = Some(s);
            let mut lh = lh_part.map(str::to_owned);
            // `12px / 1.5` with spaces around the slash.
            if lh_part.is_none() && parts.get(i + 1).is_some_and(|n| n.starts_with('/')) {
                i += 1;
                let rest = parts[i].trim_start_matches('/');
                if rest.is_empty() {
                    i += 1;
                    lh = parts.get(i).map(|s| s.to_string());
                } else {
                    lh = Some(rest.to_owned());
                }
            }
            if let Some(lh) = lh {
                let Some(l) = parse_line_height(lh.trim()) else { return };
                line_height = l;
            }
            i += 1;
            break;
        }
        match p {
            "italic" | "oblique" => italic = true,
            "normal" | "small-caps" => {}
            _ => match parse_font_weight(p) {
                Some(w) => weight = w,
                // Stretch keywords and the like; anything else is invalid.
                None if p.ends_with("condensed") || p.ends_with("expanded") => {}
                None => return,
            },
        }
        i += 1;
    }
    let Some(size) = size else { return };
    if i >= parts.len() {
        return;
    }
    let family = parse_family(&parts[i..].join(" "));
    out.extend([
        Decl::Italic(italic),
        Decl::FontWeight(weight),
        Decl::FontSize(size),
        Decl::LineHeight(line_height),
        Decl::FontFamily(family),
    ]);
}

/// `background` shorthand: only its color matters to us. Images and
/// gradients can't be drawn yet, so a gradient contributes its first color
/// stop, which keeps light-on-gradient text readable.
fn parse_background(v: &str) -> ColorValue {
    let transparent = ColorValue::Rgba(Color { r: 0, g: 0, b: 0, a: 0 });
    let mut first_stop = None;
    for t in split_top_level(v, b' ') {
        let t = t.trim().trim_end_matches(',');
        if t.is_empty() {
            continue;
        }
        if let Some(c) = parse_color(t) {
            return c;
        }
        if first_stop.is_none()
            && t.contains("gradient(")
            && let Some(open) = t.find('(')
        {
            let inner = t[open + 1..].trim_end_matches(')');
            first_stop = split_top_level(inner, b',')
                .into_iter()
                .find_map(|stop| split_top_level(stop.trim(), b' ').into_iter().find_map(|s| parse_color(s.trim())));
        }
    }
    first_stop.unwrap_or(transparent)
}

pub(crate) fn parse_color(v: &str) -> Option<ColorValue> {
    let v = v.trim();
    if v == "currentcolor" {
        return Some(ColorValue::Current);
    }
    if v == "transparent" {
        return Some(ColorValue::Rgba(Color { r: 0, g: 0, b: 0, a: 0 }));
    }
    if let Some(hex) = v.strip_prefix('#') {
        return parse_hex(hex).map(ColorValue::Rgba);
    }
    if let Some(open) = v.find('(') {
        let func = &v[..open];
        let args = v[open + 1..].strip_suffix(')')?;
        let args: Vec<&str> =
            args.split(|c: char| c == ',' || c == '/' || c.is_whitespace()).filter(|s| !s.is_empty()).collect();
        if args.len() != 3 && args.len() != 4 {
            return None;
        }
        let alpha = match args.get(3) {
            Some(a) => channel(a, 1.0)?,
            None => 1.0,
        };
        let (r, g, b) = match func {
            "rgb" | "rgba" => (channel(args[0], 255.0)?, channel(args[1], 255.0)?, channel(args[2], 255.0)?),
            "hsl" | "hsla" => {
                let h = hue(args[0])?;
                let s = channel(args[1], 100.0)?;
                let l = channel(args[2], 100.0)?;
                hsl_to_rgb(h, s, l)
            }
            _ => return None,
        };
        return Some(ColorValue::Rgba(Color { r: to_u8(r), g: to_u8(g), b: to_u8(b), a: to_u8(alpha) }));
    }
    NAMED_COLORS
        .binary_search_by(|(n, _)| (*n).cmp(v))
        .ok()
        .map(|i| ColorValue::Rgba(Color::from_u32(NAMED_COLORS[i].1)))
}

/// A color channel as a fraction of 1. Numbers are relative to `scale`,
/// percentages to 100%.
fn channel(t: &str, scale: f32) -> Option<f32> {
    let (n, unit) = split_number(t)?;
    let f = match unit {
        "%" => n / 100.0,
        "" => n / scale,
        _ => return None,
    };
    Some(f.clamp(0.0, 1.0))
}

fn hue(t: &str) -> Option<f32> {
    let (n, unit) = split_number(t)?;
    let deg = match unit {
        "" | "deg" => n,
        "turn" => n * 360.0,
        "grad" => n * 0.9,
        _ => return None,
    };
    Some(deg.rem_euclid(360.0) / 360.0)
}

/// Basic arithmetic only (no trigonometry), so results are bit-exact on
/// every platform.
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

fn to_u8(f: f32) -> u8 {
    (f.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn parse_hex(hex: &str) -> Option<Color> {
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
    let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(match hex.len() {
        3 | 4 => Color {
            r: digit(0)? * 17,
            g: digit(1)? * 17,
            b: digit(2)? * 17,
            a: if hex.len() == 4 { digit(3)? * 17 } else { 255 },
        },
        6 | 8 => Color { r: pair(0)?, g: pair(2)?, b: pair(4)?, a: if hex.len() == 8 { pair(6)? } else { 255 } },
        _ => return None,
    })
}

/// CSS named colors, sorted for binary search.
const NAMED_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn el(name: &str, id: Option<&str>, classes: &[&str]) -> Element {
        Element {
            name: name.into(),
            id: id.map(Into::into),
            classes: classes.iter().map(|c| c.to_string()).collect(),
            attrs: vec![],
            has_href: false,
        }
    }

    #[test]
    fn named_colors_are_sorted() {
        assert!(NAMED_COLORS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn parses_colors() {
        let c = |s| match parse_color(s) {
            Some(ColorValue::Rgba(c)) => Some(c),
            _ => None,
        };
        assert_eq!(c("#fff"), Some(Color::WHITE));
        assert_eq!(c("#ff000080"), Some(Color { r: 255, g: 0, b: 0, a: 128 }));
        assert_eq!(c("rgb(0, 128, 255)"), Some(Color::rgb(0, 128, 255)));
        assert_eq!(c("rgb(0 128 255 / 50%)"), Some(Color { r: 0, g: 128, b: 255, a: 128 }));
        assert_eq!(c("hsl(120, 100%, 25%)"), Some(Color::rgb(0, 128, 0)));
        assert_eq!(c("rebeccapurple"), Some(Color::rgb(0x66, 0x33, 0x99)));
        assert_eq!(c("#ggg"), None);
        assert_eq!(c("nonsense"), None);
        assert_eq!(parse_color("currentcolor"), Some(ColorValue::Current));
    }

    #[test]
    fn parses_lengths() {
        assert_eq!(parse_length("12px"), Some(Length::Px(12.0)));
        assert_eq!(parse_length("1.5em"), Some(Length::Em(1.5)));
        assert_eq!(parse_length(".5rem"), Some(Length::Rem(0.5)));
        assert_eq!(parse_length("50%"), Some(Length::Pct(50.0)));
        assert_eq!(parse_length("12pt"), Some(Length::Px(16.0)));
        assert_eq!(parse_length("0"), Some(Length::Px(0.0)));
        assert_eq!(parse_length("auto"), Some(Length::Auto));
        assert_eq!(parse_length("12"), None);
        assert_eq!(parse_length("10vw"), None);
        assert_eq!(parse_length("calc(1px + 2px)"), None);
        assert_eq!(parse_length("1e400px"), None);
    }

    #[test]
    fn specificity_orders_ids_classes_types() {
        let s = |t| parse_selector(t).unwrap().specificity;
        assert!(s("#a") > s(".a.b.c"));
        assert!(s(".a") > s("div p span"));
        assert!(s("a.btn") > s(".btn"));
        assert_eq!(s("*"), 0);
        assert_eq!(s("ul li a:link"), (1 << 8) | 3);
    }

    #[test]
    fn rejects_unsupported_selectors() {
        assert!(parse_selector("a + b").is_none());
        assert!(parse_selector("a ~ b").is_none());
        assert!(parse_selector("p::before").is_none());
        assert!(parse_selector("[type=text]").is_none());
        assert!(parse_selector("li:nth-child(2)").is_none());
        assert!(parse_selector("> a").is_none());
        // One bad selector drops the whole list, as CSS requires.
        assert!(parse_selector_list("h1, p::after").is_none());
        assert_eq!(parse_selector_list("h1, .x > p").unwrap().len(), 2);
    }

    #[test]
    fn matches_descendant_and_child() {
        let html = el("html", None, &[]);
        let div = el("div", Some("main"), &["box", "wide"]);
        let p = el("p", None, &["note"]);
        let chain = [&p, &div, &html];
        let m = |sel: &str| parse_selector(sel).unwrap().matches(&chain);
        assert!(m("p"));
        assert!(m("div p"));
        assert!(m("html p"));
        assert!(m("div > p"));
        assert!(m("html > div > p.note"));
        assert!(m("#main .note"));
        assert!(m(".box.wide p"));
        assert!(!m("html > p"));
        assert!(!m(".missing p"));
        assert!(!m("p div"));
        assert!(!m("p:hover"));
        assert!(m("* p"));
        assert!(parse_selector(":root").unwrap().matches(&[&html]));
    }

    #[test]
    fn stylesheet_skips_at_rules_and_garbage() {
        let sheet = Stylesheet::parse(
            "@import url(x.css); @media (max-width: 600px) { p { color: red } }
             /* comment { } */ p { color: blue; margin: 1px 2px } ;;; }
             garbage without block; h1, h2 { font-weight: bold !important }
             a:hover { color: red } div { unknown-prop: 1; color: var(--x) }
             @font-face { font-family: x }",
        );
        assert_eq!(sheet.rules.len(), 4);
        assert_eq!(sheet.rules[0].media.len(), 1, "@media rules are kept, with their condition");
        assert_eq!(sheet.rules[1].decls.len(), 5);
        assert!(sheet.rules[1].media.is_empty());
        assert!(sheet.rules[2].decls[0].important);
    }

    #[test]
    fn import_is_ignored_but_the_rest_parses() {
        let sheet =
            Stylesheet::parse("@import url(\"a.css\") screen; @import 'b.css'; p { color: red } @charset \"x\";");
        assert_eq!(sheet.rules.len(), 1);
    }

    #[test]
    fn media_queries() {
        let m = |q: &str, w: f32| MediaList::parse(q).matches(w);
        assert!(m("", 100.0));
        assert!(m("all", 100.0));
        assert!(m("screen", 100.0));
        assert!(!m("print", 100.0));
        assert!(m("print, screen", 100.0));
        assert!(m("(min-width: 600px)", 600.0));
        assert!(!m("(min-width: 600px)", 599.0));
        assert!(m("screen and (max-width: 40em)", 640.0));
        assert!(!m("screen and (max-width: 40em)", 641.0));
        assert!(m("only screen and (min-width: 400px) and (max-width: 800px)", 500.0));
        assert!(!m("only screen and (min-width: 400px) and (max-width: 800px)", 900.0));
        assert!(m("(width >= 600px)", 600.0));
        assert!(!m("(width > 600px)", 600.0));
        assert!(m("(400px <= width < 800px)", 400.0));
        assert!(!m("(400px <= width < 800px)", 800.0));
        assert!(m("not print", 100.0));
        assert!(!m("not screen and (min-width: 100px)", 200.0));
        // Unevaluable features never match, and never error.
        assert!(!m("(prefers-color-scheme: dark)", 100.0));
        assert!(!m("screen and (orientation: landscape)", 100.0));
        assert!(!m("(-webkit-min-device-pixel-ratio: 2)", 100.0));
        assert!(!m("(min-width: calc(1px + 2px))", 100.0));
        assert!(!m("garbage (((", 100.0));
    }

    #[test]
    fn media_rules_match_by_width() {
        let sheet = Stylesheet::parse(
            "p { color: red } @media (min-width: 600px) { p { color: blue } @media print { p { color: green } } }",
        );
        let p = el("p", None, &[]);
        let count = |w: f32| {
            let mut out = Vec::new();
            sheet.matching(&[&p], w, 0, &mut out);
            out.len()
        };
        assert_eq!(count(500.0), 1);
        assert_eq!(count(700.0), 2);
    }

    #[test]
    fn matching_uses_index_and_order() {
        let sheet = Stylesheet::parse("p { color: red } .note { color: blue } #x { color: green } * { color: gray }");
        let p = el("p", None, &["note"]);
        let mut out = Vec::new();
        sheet.matching(&[&p], 800.0, 100, &mut out);
        let orders: Vec<u32> = out.iter().map(|m| m.order).collect();
        assert_eq!(orders, vec![100, 101, 103]);
    }

    #[test]
    fn shorthands_expand() {
        let d = parse_declarations("margin: 1px 2px 3px; padding: 4px; font: italic bold 12px/1.5 Georgia, serif");
        assert!(d.contains(&Declaration { decl: Decl::Margin(3, Length::Px(2.0)), important: false }));
        assert!(d.contains(&Declaration { decl: Decl::Padding(2, Length::Px(4.0)), important: false }));
        assert!(d.contains(&Declaration { decl: Decl::FontFamily(Family::Serif), important: false }));
        assert!(d.contains(&Declaration { decl: Decl::LineHeight(LineHeightValue::Number(1.5)), important: false }));
        assert!(d.contains(&Declaration { decl: Decl::Italic(true), important: false }));
        let bg = parse_declarations("background: url(a.png) no-repeat #123456");
        assert_eq!(bg[0].decl, Decl::Background(ColorValue::Rgba(Color::rgb(0x12, 0x34, 0x56))));
        let grad = parse_declarations("background: linear-gradient(to right, #ff0000 0%, blue 100%)");
        assert_eq!(grad[0].decl, Decl::Background(ColorValue::Rgba(Color::rgb(255, 0, 0))));
    }

    #[test]
    fn hostile_css_does_not_panic() {
        let inputs = [
            "{{{{{{{{",
            "}}}}}}}",
            "a { color: \"unterminated",
            "@media {",
            "/* unterminated",
            "p{margin:-1e99px;font-size:99999999px;width:-5px}",
            "\\",
            "a\\",
            ".\\31 23{color:red}",
            "#{}",
            "p{color:rgb(1,2)}",
            "p{color:#}",
            "ä ö ü { color: red }",
        ];
        for i in inputs {
            let _ = Stylesheet::parse(i);
            let _ = parse_declarations(i);
        }
        let deep = "(".repeat(100_000) + "{" + &"[".repeat(100_000);
        let _ = Stylesheet::parse(&deep);
    }
}
