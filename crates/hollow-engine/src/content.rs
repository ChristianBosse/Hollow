//! Generated content: what `::before` and `::after` say, with CSS counters
//! and quotes resolved.
//!
//! Counters and quote depth depend on everything earlier in the document,
//! so they are worked out in one walk over the element tree in document
//! order, before boxes are built. Elements with `display: none` (and their
//! subtrees) take no part, as CSS says.

use std::collections::BTreeMap;
use std::sync::Arc;

use url::Url;

use crate::css::{ContentItem, CounterOp};
use crate::dom::{Dom, NodeId};
use crate::style::{Display, ListStyle, Style, Styles};

/// Counter instances alive at once; resets past this are ignored.
const MAX_COUNTERS: usize = 512;
/// Text all generated content of a document may add up to.
const MAX_GENERATED_BYTES: usize = 1 << 20;
/// Quote nesting is counted up to here.
const MAX_QUOTE_DEPTH: usize = 1024;

/// One piece of generated content.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GenItem {
    Text(String),
    Image(Arc<Url>),
}

/// The content of each generated pseudo-element, by its style index.
pub(crate) type Generated = BTreeMap<usize, Vec<GenItem>>;

/// One counter instance: CSS 2.1 §12.4.1 scoping, where a reset is seen by
/// the element's following siblings and their descendants, until the
/// parent ends or a later sibling resets the counter again.
struct Counter {
    name: Arc<str>,
    value: i64,
    /// The parent of the element that created it; it dies with that scope.
    scope: NodeId,
}

struct Walk<'a> {
    dom: &'a Dom,
    styles: &'a Styles,
    counters: Vec<Counter>,
    quote_depth: usize,
    budget: usize,
    out: Generated,
}

pub(crate) fn generate(dom: &Dom, styles: &Styles) -> Generated {
    if styles.pseudo.is_empty() {
        return Generated::new();
    }
    let mut w =
        Walk { dom, styles, counters: Vec::new(), quote_depth: 0, budget: MAX_GENERATED_BYTES, out: Generated::new() };
    // An explicit stack: documents are deep and ours must not overflow.
    enum Step {
        Enter(NodeId),
        Leave(NodeId),
    }
    let mut stack: Vec<Step> = dom.nodes[0].children.iter().rev().map(|&c| Step::Enter(c)).collect();
    while let Some(step) = stack.pop() {
        match step {
            Step::Enter(n) => {
                let Some(st) = styles.all.get(n).filter(|_| dom.element(n).is_some()) else { continue };
                if st.display == Display::None {
                    continue;
                }
                let parent = dom.nodes[n].parent.unwrap_or(0);
                w.apply_counters(st, parent);
                let pseudo = styles.pseudo.get(&n).copied().unwrap_or_default();
                if let Some(p) = pseudo[0] {
                    w.pseudo_element(p, n);
                }
                stack.push(Step::Leave(n));
                stack.extend(dom.nodes[n].children.iter().rev().map(|&c| Step::Enter(c)));
            }
            Step::Leave(n) => {
                if let Some(p) = styles.pseudo.get(&n).and_then(|p| p[1]) {
                    w.pseudo_element(p, n);
                }
                // The counters its children created go out of scope.
                while w.counters.last().is_some_and(|c| c.scope == n) {
                    w.counters.pop();
                }
            }
        }
    }
    w.out
}

impl Walk<'_> {
    /// Resets, then increments, then sets (CSS Lists 3), for an element (or
    /// pseudo-element) whose parent is `parent`.
    fn apply_counters(&mut self, st: &Style, parent: NodeId) {
        if let Some(list) = &st.counters[CounterOp::Reset as usize] {
            for (name, v) in list.iter() {
                match self.innermost(name) {
                    // A sibling's reset ends the earlier instance's scope.
                    Some(i) if self.counters[i].scope == parent => self.counters[i].value = i64::from(*v),
                    _ => self.push(name, i64::from(*v), parent),
                }
            }
        }
        if let Some(list) = &st.counters[CounterOp::Increment as usize] {
            for (name, v) in list.iter() {
                let i = self.instance(name, parent);
                if let Some(c) = i.and_then(|i| self.counters.get_mut(i)) {
                    c.value = c.value.saturating_add(i64::from(*v));
                }
            }
        }
        if let Some(list) = &st.counters[CounterOp::Set as usize] {
            for (name, v) in list.iter() {
                let i = self.instance(name, parent);
                if let Some(c) = i.and_then(|i| self.counters.get_mut(i)) {
                    c.value = i64::from(*v);
                }
            }
        }
    }

    fn innermost(&self, name: &str) -> Option<usize> {
        self.counters.iter().rposition(|c| &*c.name == name)
    }

    /// The counter in scope, or a new one reset to 0 on the element, as
    /// CSS says for counters used without a reset.
    fn instance(&mut self, name: &Arc<str>, parent: NodeId) -> Option<usize> {
        if let Some(i) = self.innermost(name) {
            return Some(i);
        }
        self.push(name, 0, parent);
        self.innermost(name)
    }

    fn push(&mut self, name: &Arc<str>, value: i64, scope: NodeId) {
        if self.counters.len() < MAX_COUNTERS {
            self.counters.push(Counter { name: name.clone(), value, scope });
        }
    }

    /// Resolves the content of pseudo-element `p` of element `el`.
    fn pseudo_element(&mut self, p: usize, el: NodeId) {
        let styles = self.styles;
        let Some(st) = styles.all.get(p) else { return };
        // Its own counter properties apply first; it is a child of `el`.
        self.apply_counters(st, el);
        let Some(items) = st.content.clone() else { return };
        let mut out: Vec<GenItem> = Vec::new();
        let mut text = String::new();
        for item in items.iter() {
            match item {
                ContentItem::Text(s) => text.push_str(s),
                ContentItem::Attr(name) => {
                    text.push_str(self.dom.element(el).and_then(|e| e.attr(name)).unwrap_or(""));
                }
                ContentItem::Counter(name, style) => {
                    let i = self.instance(name, el);
                    let v = i.map_or(0, |i| self.counters[i].value);
                    text.push_str(&counter_text(*style, v));
                }
                ContentItem::Counters(name, sep, style) => {
                    if self.innermost(name).is_none() {
                        self.instance(name, el);
                    }
                    let mut first = true;
                    for c in self.counters.iter().filter(|c| c.name == *name) {
                        if !first {
                            text.push_str(sep);
                        }
                        first = false;
                        text.push_str(&counter_text(*style, c.value));
                        if text.len() > self.budget {
                            break;
                        }
                    }
                }
                ContentItem::OpenQuote | ContentItem::NoOpenQuote => {
                    if *item == ContentItem::OpenQuote {
                        text.push_str(quote(st, self.quote_depth, true));
                    }
                    self.quote_depth = (self.quote_depth + 1).min(MAX_QUOTE_DEPTH);
                }
                ContentItem::CloseQuote | ContentItem::NoCloseQuote => {
                    if self.quote_depth > 0 {
                        self.quote_depth -= 1;
                        if *item == ContentItem::CloseQuote {
                            text.push_str(quote(st, self.quote_depth, false));
                        }
                    }
                }
                ContentItem::Image(url) => {
                    if !text.is_empty() {
                        out.push(GenItem::Text(std::mem::take(&mut text)));
                    }
                    out.push(GenItem::Image(url.clone()));
                }
            }
            if text.len() > self.budget {
                text.clear();
                self.budget = 0;
            }
        }
        if !text.is_empty() {
            self.budget -= text.len().min(self.budget);
            out.push(GenItem::Text(text));
        }
        self.out.insert(p, out);
    }
}

/// The open or close quote for nesting `depth` (0 is outermost).
fn quote(st: &Style, depth: usize, open: bool) -> &str {
    const AUTO: [(&str, &str); 2] = [("\u{201C}", "\u{201D}"), ("\u{2018}", "\u{2019}")];
    match &st.quotes {
        Some(q) if q.is_empty() => "",
        Some(q) => {
            let (o, c) = &q[depth.min(q.len() - 1)];
            if open { o } else { c }
        }
        None => {
            let (o, c) = AUTO[depth.min(1)];
            if open { o } else { c }
        }
    }
}

/// A counter value in a counter style, without any suffix.
pub(crate) fn counter_text(style: ListStyle, n: i64) -> String {
    let alpha = |n: i64, base: u8| -> String {
        if n <= 0 {
            return n.to_string();
        }
        let mut n = n;
        let mut s = Vec::new();
        while n > 0 {
            n -= 1;
            s.push(base + (n % 26) as u8);
            n /= 26;
        }
        s.reverse();
        String::from_utf8(s).unwrap_or_default()
    };
    let roman = |n: i64| -> String {
        if !(1..4000).contains(&n) {
            return n.to_string();
        }
        let table = [
            (1000, "m"),
            (900, "cm"),
            (500, "d"),
            (400, "cd"),
            (100, "c"),
            (90, "xc"),
            (50, "l"),
            (40, "xl"),
            (10, "x"),
            (9, "ix"),
            (5, "v"),
            (4, "iv"),
            (1, "i"),
        ];
        let mut n = n;
        let mut s = String::new();
        for (v, r) in table {
            while n >= v {
                s.push_str(r);
                n -= v;
            }
        }
        s
    };
    match style {
        ListStyle::None => String::new(),
        ListStyle::Disc => "\u{2022}".into(),
        ListStyle::Circle => "\u{25E6}".into(),
        ListStyle::Square => "\u{25AA}".into(),
        ListStyle::Decimal => n.to_string(),
        ListStyle::LowerAlpha => alpha(n, b'a'),
        ListStyle::UpperAlpha => alpha(n, b'A'),
        ListStyle::LowerRoman => roman(n),
        ListStyle::UpperRoman => roman(n).to_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated(html: &str) -> Vec<String> {
        let base = url::Url::parse("https://example.org/").unwrap();
        let parsed = crate::dom::parse(html, &base);
        let sheets: Vec<crate::css::Stylesheet> = parsed
            .sheets
            .iter()
            .filter_map(|s| match s {
                crate::dom::SheetSource::Inline { css, .. } => {
                    Some(crate::css::Stylesheet::parse_with_base(css, Some(&parsed.dom.base)))
                }
                _ => None,
            })
            .collect();
        let author: Vec<crate::style::AuthorSheet<'_>> =
            sheets.iter().map(|sheet| crate::style::AuthorSheet { sheet, media: None }).collect();
        let styles = crate::style::compute(&parsed.dom, &crate::style::ua_sheet(), &author, 800.0);
        let generated = generate(&parsed.dom, &styles);
        generated
            .values()
            .map(|items| {
                items
                    .iter()
                    .map(|i| match i {
                        GenItem::Text(t) => t.clone(),
                        GenItem::Image(u) => format!("[{u}]"),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn counters_nest_and_scope() {
        let html = "<style>ol { counter-reset: item } li { counter-increment: item }
                    li::before { content: counters(item, '.') ' ' }</style>
                    <ol><li>a<li>b<ol><li>c<li>d</ol><li>e</ol>";
        assert_eq!(generated(html), ["1 ", "2 ", "2.1 ", "2.2 ", "3 "]);
        // A sibling's reset replaces the earlier counter; `counter-set`
        // and negative increments; a counter nobody reset starts at 0.
        let html = "<style>h2 { counter-increment: h } h2::before { content: counter(h, upper-roman) '.' }
                    .r { counter-reset: h 9 } .s { counter-set: h 40 } .d { counter-increment: h -2 }
                    p::after { content: counter(other) }</style>
                    <h2>a</h2><h2 class=r>b</h2><h2>c</h2><h2 class=s>d</h2><h2 class=d>e</h2><p>p</p>";
        assert_eq!(generated(html), ["I.", "X.", "XI.", "XL.", "XXXVIII.", "0"]);
    }

    #[test]
    fn quotes_attr_and_styles() {
        let html = "<style>.x::before { content: attr(data-n) ' ' counter(c, lower-alpha) url(i.png) }
                    .x { counter-reset: c 27 } .p { quotes: '<' '>' '[' ']' }</style>
                    <q>a<q>b</q></q><div class=x data-n=N></div><p class=p><q>c<q>d</q></q></p>
                    <div style='display:none'><q>gone</q></div>";
        // In element order: each element's ::before, then its ::after.
        assert_eq!(
            generated(html),
            ["\u{201C}", "\u{201D}", "\u{2018}", "\u{2019}", "N aa[https://example.org/i.png]", "<", ">", "[", "]"]
        );
    }

    #[test]
    fn counter_styles() {
        assert_eq!(counter_text(ListStyle::LowerAlpha, 28), "ab");
        assert_eq!(counter_text(ListStyle::UpperRoman, 1994), "MCMXCIV");
        assert_eq!(counter_text(ListStyle::LowerRoman, 0), "0");
        assert_eq!(counter_text(ListStyle::Decimal, -3), "-3");
        assert_eq!(counter_text(ListStyle::None, 3), "");
        assert_eq!(counter_text(ListStyle::UpperAlpha, i64::MIN), i64::MIN.to_string());
    }
}
