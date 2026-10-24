//! A deliberately small CSS parser: selectors, declarations and the value
//! types the engine understands.
//!
//! Anything outside the supported subset is dropped silently, never treated
//! as an error: real-world CSS is mostly things we don't implement yet. Rules
//! whose selector list contains an unsupported selector are dropped whole,
//! as CSS requires, rather than being matched approximately.

mod values;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use url::Url;

use crate::dom::{Dom, Element, NodeData, NodeId};
use crate::style::{Display, ListStyle, TextAlign, TextTransform, VAlign, WhiteSpace};
use crate::text::Family;
use crate::vars;

pub(crate) use values::{
    Base, BgSize, BoxArea, ContentItem, CounterList, CounterOp, Gradient, Image, QuoteList, RadialSize, Repeat, Shape,
    decl_urls, resolve_url,
};

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
    /// `calc()` and viewport units.
    Calc(Calc),
    /// `clamp(min, value, max)`; `min(a, b)` and `max(a, b)` are clamps with
    /// one infinite bound. Never with percentages, so it always computes to px.
    Clamp(Calc, Calc, Calc),
    MinContent,
    MaxContent,
    FitContent,
}

/// A linear `calc()` sum. All but the percentage resolve when styles are
/// computed; the percentage waits for the containing block.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub(crate) struct Calc {
    pub px: f32,
    pub em: f32,
    pub rem: f32,
    /// Percent of the viewport width.
    pub vw: f32,
    pub pct: f32,
}

impl Calc {
    fn scaled(self, k: f32) -> Calc {
        Calc { px: self.px * k, em: self.em * k, rem: self.rem * k, vw: self.vw * k, pct: self.pct * k }
    }
    fn plus(self, o: Calc) -> Calc {
        Calc {
            px: self.px + o.px,
            em: self.em + o.em,
            rem: self.rem + o.rem,
            vw: self.vw + o.vw,
            pct: self.pct + o.pct,
        }
    }
    fn of(l: Length) -> Option<Calc> {
        Some(match l {
            Length::Px(v) => Calc { px: v, ..Calc::default() },
            Length::Em(v) => Calc { em: v, ..Calc::default() },
            Length::Rem(v) => Calc { rem: v, ..Calc::default() },
            Length::Pct(v) => Calc { pct: v, ..Calc::default() },
            Length::Calc(c) => c,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
    Sticky,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Float {
    None,
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Clear {
    None,
    Left,
    Right,
    Both,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

/// `justify-content` and `align-content`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Justify {
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
    Stretch,
}

/// `align-items` and `align-self`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Align {
    /// Only for `align-self`: use the container's `align-items`.
    Auto,
    Stretch,
    Start,
    End,
    Center,
    Baseline,
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
    Opacity,
    TextTransform,
    Margin(usize),
    Padding(usize),
    Width,
    MaxWidth,
    VerticalAlign,
    BoxSizing,
    BorderSpacing,
    Position,
    Inset(usize),
    ZIndex,
    Float,
    Clear,
    Overflow(usize),
    Height,
    MinHeight,
    MaxHeight,
    MinWidth,
    BorderWidth(usize),
    BorderStyle(usize),
    BorderColor(usize),
    BorderRadius,
    FlexDirection,
    FlexWrap,
    JustifyContent,
    AlignItems,
    AlignSelf,
    AlignContent,
    FlexGrow,
    FlexShrink,
    FlexBasis,
    Order,
    Gap(usize),
    Mask,
    BgImage,
    BgRepeat,
    BgPosition,
    BgSize,
    BgOrigin,
    BgClip,
    BgAttachment,
    Content,
    Counters(CounterOp),
    Quotes,
    BorderCollapse,
    TableLayout,
    CaptionSide,
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
                | Prop::Quotes
                | Prop::BorderCollapse
                | Prop::CaptionSide
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
    /// `--name: value`, cascaded and inherited by `vars`.
    Custom(Box<RawDecl>),
    /// A property whose value uses `var()`; parsed once `vars` substitutes it.
    Var(Box<RawDecl>),
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
    /// `opacity` at (or within a hair of) zero: drawn as nothing.
    Transparent(bool),
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
    /// Horizontal, vertical.
    BorderSpacing(Length, Length),
    Position(Position),
    /// Side order as for margins: top, right, bottom, left.
    Inset(usize, Length),
    /// `None` means `auto`.
    ZIndex(Option<i32>),
    Float(Float),
    Clear(Clear),
    /// Axis 0 is x, 1 is y; `true` for anything but `visible`.
    Overflow(usize, bool),
    Height(Length),
    MinHeight(Length),
    MaxHeight(Option<Length>),
    MinWidth(Length),
    BorderWidth(usize, Length),
    /// `false` for `none` and `hidden`.
    BorderStyle(usize, bool),
    BorderColor(usize, ColorValue),
    BorderRadius(Length),
    FlexDirection(FlexDirection),
    FlexWrap(bool),
    JustifyContent(Justify),
    AlignItems(Align),
    AlignSelf(Align),
    AlignContent(Justify),
    FlexGrow(f32),
    FlexShrink(f32),
    /// `Length::Auto` means `auto` (and `content`).
    FlexBasis(Length),
    Order(i32),
    /// Axis 0 is the row gap, 1 the column gap.
    Gap(usize, Length),
    /// Any `mask`/`mask-image` other than `none`.
    Masked(bool),
    /// Background layers, top first; `None` is `none`.
    BgImage(Arc<[Option<Image>]>),
    /// x and y.
    BgRepeat(Arc<[[Repeat; 2]]>),
    BgPosition(Arc<[[Length; 2]]>),
    BgSize(Arc<[BgSize]>),
    BgOrigin(Arc<[BoxArea]>),
    BgClip(Arc<[BoxArea]>),
    /// `background-attachment: fixed`.
    BgFixed(Arc<[bool]>),
    /// `None` is `normal` and `none`, which are the same for `::before` and
    /// `::after`.
    Content(Option<Arc<[ContentItem]>>),
    Counters(CounterOp, CounterList),
    /// `None` is `auto`.
    Quotes(Option<QuoteList>),
    BorderCollapse(bool),
    /// `table-layout: fixed`.
    TableFixed(bool),
    /// `caption-side: bottom`.
    CaptionBottom(bool),
    Wide(Prop, Wide),
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Declaration {
    pub decl: Decl,
    pub important: bool,
}

/// A declaration kept as text until computed-value time: a custom property,
/// or a property whose value uses `var()`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct RawDecl {
    /// Custom property names keep their case; other names are lowercase.
    pub name: Arc<str>,
    pub value: Arc<str>,
    /// What `url()`s in the substituted value resolve against.
    pub base: Option<Arc<Url>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Combinator {
    Descendant,
    Child,
    /// `a + b`.
    Next,
    /// `a ~ b`.
    Later,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AttrOp {
    Exists,
    Equals,
    /// `~=`: one of the whitespace-separated words.
    Includes,
    /// `|=`: equal, or followed by `-`.
    Dash,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct AttrSel {
    /// Lowercase: HTML attribute names are case-insensitive.
    name: String,
    op: AttrOp,
    /// Already lowercase when `ci`.
    value: String,
    /// ASCII case-insensitive value comparison.
    ci: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Pseudo {
    /// `:link` / `:any-link`. `:visited` never matches: history must not
    /// influence rendering.
    Link,
    Root,
    /// Dynamic states (`:hover`, `:focus`, ...) that a static render never has.
    Never,
    /// `:checked`, from the `checked` / `selected` attributes: nothing can
    /// toggle them in a static render.
    Checked,
    /// `:required` when true, `:optional` when false: form controls by
    /// their `required` attribute.
    Required(bool),
    /// `:nth-child(an+b)` and the other structural pseudo-classes
    /// (`:first-child` is `a = 0, b = 1`).
    Nth {
        a: i32,
        b: i32,
        of_type: bool,
        from_end: bool,
    },
    Empty,
    Not(Vec<Selector>),
    /// `:is()` and `:where()`, which only differ in specificity.
    Is(Vec<Selector>),
    /// Lowercase language ranges.
    Lang(Vec<String>),
    /// `:dir(rtl)` when true, `:dir(ltr)` when false.
    Dir(bool),
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrSel>,
    pseudo: Vec<Pseudo>,
}

/// The generated-content pseudo-elements.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum PseudoElement {
    Before,
    After,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Selector {
    /// Left to right; the last compound matches the subject element.
    compounds: Vec<Compound>,
    /// `combinators[i]` sits between `compounds[i]` and `compounds[i + 1]`.
    combinators: Vec<Combinator>,
    /// `::before` / `::after`: the rule styles that pseudo-element of the
    /// subject rather than the subject itself.
    pub pseudo: Option<PseudoElement>,
    pub specificity: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub selectors: Vec<Selector>,
    pub decls: Vec<Declaration>,
    /// Enclosing `@media` conditions; all must hold.
    pub media: Vec<MediaList>,
    /// http(s) images the declarations refer to.
    pub images: Vec<Arc<Url>>,
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
#[derive(Clone, Copy)]
pub(crate) struct Matched<'a> {
    pub specificity: u32,
    /// Source order across all sheets of the cascade.
    pub order: u32,
    pub decls: &'a [Declaration],
    /// Set when the rule applies to that pseudo-element of the element.
    pub pseudo: Option<PseudoElement>,
}

impl Stylesheet {
    pub fn parse(css: &str) -> Stylesheet {
        Stylesheet::parse_with_base(css, None)
    }

    /// Parses a sheet whose relative `url()`s resolve against `base`.
    pub fn parse_with_base(css: &str, base: Base<'_>) -> Stylesheet {
        let mut sheet = Stylesheet::default();
        let css = strip_comments(truncate(css, MAX_CSS_BYTES).trim_start_matches('\u{FEFF}'));
        sheet.parse_rules(&css, &mut Vec::new(), base);
        sheet
    }

    /// Parses a rule list: the top level of a sheet or the body of `@media`.
    fn parse_rules(&mut self, css: &str, media: &mut Vec<MediaList>, base: Base<'_>) {
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
            // A `;` ends an at-rule statement; for a style rule it is part of
            // the selector, which makes the whole rule invalid (so
            // `p {} ; q {}` drops the `q` rule, as browsers do). Stray `}`
            // and other closers are just tokens.
            let stops: &[u8] = if at_rule { b"{;" } else { b"{" };
            let (i, stop) = find_top_level(b, pos, stops);
            match stop {
                Some(b'{') => {
                    let (end, close) = skip_block(b, i);
                    let prelude = &css[pos..i];
                    let body = &css[i + 1..close];
                    if !at_rule {
                        self.add_rule(prelude, body, media, base);
                    } else if let Some(query) = strip_prefix_ci(prelude, "@media")
                        && media.len() < MAX_MEDIA_NESTING
                    {
                        // Evaluated at layout time against the viewport width.
                        // Which sheets are *fetched* never depends on it.
                        media.push(MediaList::parse(query));
                        self.parse_rules(body, media, base);
                        media.pop();
                    }
                    // Other block at-rules (@supports, @font-face, @keyframes,
                    // @layer, ...) are ignored with their contents.
                    pos = end;
                }
                // Statement at-rules such as `@import` (never fetched: no
                // extra requests).
                Some(_) => pos = i + 1,
                None => break,
            }
        }
    }

    fn add_rule(&mut self, prelude: &str, body: &str, media: &[MediaList], base: Base<'_>) {
        let Some(selectors) = parse_selector_list(prelude) else { return };
        let mut decls = Vec::new();
        parse_declarations_into(body, &mut decls, base);
        if decls.is_empty() {
            return;
        }
        let mut images = Vec::new();
        for d in &decls {
            decl_urls(&d.decl, &mut images);
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
        self.rules.push(Rule { selectors, decls, media: media.to_vec(), images });
    }

    /// Rules that could match `el`, from the index, sorted and deduplicated.
    fn candidates(&self, el: &Element) -> Vec<(u32, u32)> {
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
        candidates
    }

    /// The http(s) images of rules that apply to some element of the
    /// document (or to its `::before`/`::after`), in document order of the
    /// first element each applies to, appended to `out` unless already in
    /// `seen`, until `out` holds `max`. Media queries are ignored, so what
    /// is fetched never depends on the viewport; rules for states a static
    /// page never has (`:hover`, ...) never match.
    pub fn image_urls(&self, tree: &Tree<'_>, seen: &mut BTreeSet<String>, out: &mut Vec<Url>, max: usize) {
        if self.rules.iter().all(|r| r.images.is_empty()) {
            return;
        }
        let mut done = vec![false; self.rules.len()];
        for node in 0..tree.dom.nodes.len() {
            let Some(el) = tree.dom.element(node) else { continue };
            for (rule, sel) in self.candidates(el) {
                let r = &self.rules[rule as usize];
                if done[rule as usize] || r.images.is_empty() || !r.selectors[sel as usize].matches(tree, node) {
                    continue;
                }
                done[rule as usize] = true;
                for u in &r.images {
                    if out.len() >= max {
                        return;
                    }
                    if seen.insert(u.as_str().to_owned()) {
                        out.push((**u).clone());
                    }
                }
            }
        }
    }

    /// Every rule matching element `node` or one of its pseudo-elements,
    /// for a viewport `width` CSS px wide. `order_base` is added to rule
    /// indices so several sheets can share one cascade order.
    pub fn matching<'a>(
        &'a self,
        tree: &Tree<'_>,
        node: NodeId,
        width: f32,
        order_base: u32,
        out: &mut Vec<Matched<'a>>,
    ) {
        let Some(el) = tree.dom.element(node) else { return };
        for (rule, sel) in self.candidates(el) {
            let r = &self.rules[rule as usize];
            let s = &r.selectors[sel as usize];
            if r.media.iter().all(|m| m.matches(width)) && s.matches(tree, node) {
                out.push(Matched {
                    specificity: s.specificity,
                    order: order_base.saturating_add(rule),
                    decls: &r.decls,
                    pseudo: s.pseudo,
                });
            }
        }
    }
}

/// Matching one selector looks at no more elements than this, `:is()` and
/// `:not()` arguments included; a selector that would need more does not
/// match. Real selectors stay far below it.
const MAX_MATCH_STEPS: u32 = 10_000;
/// `a ~ b` looks back at most this many siblings from each candidate.
const MAX_SIBLING_SCAN: usize = 512;

/// The DOM as selector matching sees it: the elements plus what structural,
/// `:lang()` and `:dir()` selectors need, computed once per document rather
/// than on every match.
pub(crate) struct Tree<'a> {
    pub dom: &'a Dom,
    info: Vec<NodeInfo>,
}

#[derive(Clone, Copy, Default)]
struct NodeInfo {
    /// Previous element sibling.
    prev: Option<NodeId>,
    /// 1-based position among the parent's element children, and their count.
    index: u32,
    count: u32,
    /// The same among siblings with the same tag name.
    type_index: u32,
    type_count: u32,
    /// Nearest inclusive ancestor with a `lang` attribute.
    lang: Option<NodeId>,
    rtl: bool,
}

impl<'a> Tree<'a> {
    pub fn new(dom: &'a Dom) -> Tree<'a> {
        let mut info = vec![NodeInfo::default(); dom.nodes.len()];
        let mut types: BTreeMap<&str, u32> = BTreeMap::new();
        // The arena is in document order, so parents are done before children.
        for (id, node) in dom.nodes.iter().enumerate() {
            if let Some(el) = dom.element(id) {
                let parent = node.parent.and_then(|p| info.get(p).copied()).unwrap_or_default();
                info[id].lang = if el.attr("lang").is_some() { Some(id) } else { parent.lang };
                info[id].rtl = match el.attr("dir").map(str::trim) {
                    Some(d) if d.eq_ignore_ascii_case("rtl") => true,
                    Some(d) if d.eq_ignore_ascii_case("ltr") => false,
                    _ => parent.rtl,
                };
            }
            types.clear();
            let mut prev = None;
            let mut count = 0;
            for &c in &node.children {
                let Some(el) = dom.element(c) else { continue };
                count += 1;
                let of_type = types.entry(el.name.as_str()).or_default();
                *of_type += 1;
                info[c].prev = prev;
                info[c].index = count;
                info[c].type_index = *of_type;
                prev = Some(c);
            }
            for &c in &node.children {
                if let Some(el) = dom.element(c) {
                    info[c].count = count;
                    info[c].type_count = types.get(el.name.as_str()).copied().unwrap_or(0);
                }
            }
        }
        Tree { dom, info }
    }

    fn parent(&self, n: NodeId) -> Option<NodeId> {
        self.dom.nodes.get(n)?.parent.filter(|&p| self.dom.element(p).is_some())
    }

    fn prev(&self, n: NodeId) -> Option<NodeId> {
        self.info.get(n)?.prev
    }
}

impl Selector {
    fn matches(&self, tree: &Tree<'_>, node: NodeId) -> bool {
        let mut budget = MAX_MATCH_STEPS;
        // Running out part-way could make a `:not()` succeed, so an exhausted
        // budget is a miss whatever the partial result.
        self.matches_within(tree, node, &mut budget) && budget > 0
    }

    fn matches_within(&self, tree: &Tree<'_>, node: NodeId, budget: &mut u32) -> bool {
        let n = self.compounds.len();
        if !compound_matches(&self.compounds[n - 1], tree, node, budget) {
            return false;
        }
        // Right to left, keeping the set of elements the compounds so far can
        // stand for instead of backtracking: each step visits an element at
        // most once, so adversarial selectors stay linear.
        let mut reachable = vec![node];
        let mut seen = BTreeSet::new();
        for i in (0..n - 1).rev() {
            let compound = &self.compounds[i];
            let comb = self.combinators[i];
            let up = matches!(comb, Combinator::Child | Combinator::Descendant);
            let walk = matches!(comb, Combinator::Descendant | Combinator::Later);
            let step = |e: NodeId| if up { tree.parent(e) } else { tree.prev(e) };
            // Walks from a single start never revisit an element.
            let dedup = reachable.len() > 1;
            seen.clear();
            let mut next = Vec::new();
            for &start in &reachable {
                let mut cur = step(start);
                let mut scanned = 0;
                while let Some(e) = cur {
                    // Everything past an element seen before was walked from it.
                    if dedup && !seen.insert(e) {
                        break;
                    }
                    if compound_matches(compound, tree, e, budget) {
                        if i == 0 {
                            return true;
                        }
                        next.push(e);
                    }
                    if *budget == 0 {
                        return false;
                    }
                    scanned += 1;
                    if !walk || (!up && scanned >= MAX_SIBLING_SCAN) {
                        break;
                    }
                    cur = step(e);
                }
            }
            if next.is_empty() {
                return false;
            }
            reachable = next;
        }
        true
    }
}

fn compound_matches(c: &Compound, tree: &Tree<'_>, node: NodeId, budget: &mut u32) -> bool {
    let Some(el) = tree.dom.element(node) else { return false };
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    if c.tag.as_ref().is_some_and(|t| *t != el.name) {
        return false;
    }
    if c.id.is_some() && c.id != el.id {
        return false;
    }
    if !c.classes.iter().all(|cl| el.classes.contains(cl)) {
        return false;
    }
    if !c.attrs.iter().all(|a| a.matches(el)) {
        return false;
    }
    let info = tree.info.get(node).copied().unwrap_or_default();
    c.pseudo.iter().all(|p| match p {
        Pseudo::Link => el.has_href && (el.name == "a" || el.name == "area"),
        Pseudo::Root => el.name == "html" && tree.parent(node).is_none(),
        Pseudo::Never => false,
        Pseudo::Checked => match el.name.as_str() {
            "input" => {
                el.attr("checked").is_some()
                    && el
                        .attr("type")
                        .map(str::trim)
                        .is_some_and(|t| t.eq_ignore_ascii_case("checkbox") || t.eq_ignore_ascii_case("radio"))
            }
            "option" => el.attr("selected").is_some(),
            _ => false,
        },
        &Pseudo::Required(required) => {
            matches!(el.name.as_str(), "input" | "select" | "textarea") && el.attr("required").is_some() == required
        }
        &Pseudo::Nth { a, b, of_type, from_end } => {
            let (index, count) = if of_type { (info.type_index, info.type_count) } else { (info.index, info.count) };
            let k = if from_end { i64::from(count) - i64::from(index) + 1 } else { i64::from(index) };
            nth_matches(a, b, k)
        }
        Pseudo::Empty => tree.dom.nodes[node]
            .children
            .iter()
            .all(|&ch| matches!(&tree.dom.nodes[ch].data, NodeData::Text(t) if t.is_empty())),
        Pseudo::Not(list) => !list.iter().any(|s| s.matches_within(tree, node, budget)),
        Pseudo::Is(list) => list.iter().any(|s| s.matches_within(tree, node, budget)),
        Pseudo::Lang(ranges) => {
            let lang = info.lang.and_then(|l| tree.dom.element(l)).and_then(|e| e.attr("lang")).unwrap_or("").trim();
            !lang.is_empty() && ranges.iter().any(|r| lang_matches(lang, r))
        }
        &Pseudo::Dir(rtl) => info.rtl == rtl,
    })
}

/// Whether the 1-based position `k` is `a*n + b` for some `n >= 0`.
fn nth_matches(a: i32, b: i32, k: i64) -> bool {
    let (a, b) = (i64::from(a), i64::from(b));
    if a == 0 {
        return k == b;
    }
    let d = k - b;
    d % a == 0 && d / a >= 0
}

/// `:lang()` matching: `en` matches `en` and `en-GB`, case-insensitively;
/// `*` matches any language.
fn lang_matches(lang: &str, range: &str) -> bool {
    if range == "*" {
        return true;
    }
    let (l, r) = (lang.as_bytes(), range.as_bytes());
    l.get(..r.len()).is_some_and(|p| p.eq_ignore_ascii_case(r)) && (l.len() == r.len() || l[r.len()] == b'-')
}

/// Attributes whose values HTML compares ASCII case-insensitively in
/// selectors (unless the `s` flag says otherwise). Sorted.
const CI_ATTRS: &[&str] = &[
    "accept",
    "accept-charset",
    "align",
    "alink",
    "axis",
    "bgcolor",
    "charset",
    "checked",
    "clear",
    "codetype",
    "color",
    "compact",
    "declare",
    "defer",
    "dir",
    "direction",
    "disabled",
    "enctype",
    "face",
    "frame",
    "hreflang",
    "http-equiv",
    "lang",
    "language",
    "link",
    "media",
    "method",
    "multiple",
    "nohref",
    "noresize",
    "noshade",
    "nowrap",
    "readonly",
    "rel",
    "rev",
    "rules",
    "scope",
    "scrolling",
    "selected",
    "shape",
    "target",
    "text",
    "type",
    "valign",
    "valuetype",
    "vlink",
];

impl AttrSel {
    fn matches(&self, el: &Element) -> bool {
        let Some(value) = el.attr(&self.name) else { return false };
        let (v, want) = (value.as_bytes(), self.value.as_bytes());
        let eq = |a: &[u8]| if self.ci { a.eq_ignore_ascii_case(want) } else { a == want };
        match self.op {
            AttrOp::Exists => true,
            AttrOp::Equals => eq(v),
            AttrOp::Includes => {
                !want.is_empty()
                    && !want.iter().any(u8::is_ascii_whitespace)
                    && v.split(u8::is_ascii_whitespace).any(eq)
            }
            AttrOp::Dash => v.get(..want.len()).is_some_and(eq) && (v.len() == want.len() || v[want.len()] == b'-'),
            AttrOp::Prefix => !want.is_empty() && v.get(..want.len()).is_some_and(eq),
            AttrOp::Suffix => !want.is_empty() && v.len() >= want.len() && eq(&v[v.len() - want.len()..]),
            AttrOp::Substring => {
                !want.is_empty()
                    && if self.ci {
                        value.to_ascii_lowercase().contains(self.value.as_str())
                    } else {
                        value.contains(self.value.as_str())
                    }
            }
        }
    }
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
    // Closers to expect, innermost last; a stray one is just a token.
    let mut open: Vec<u8> = Vec::new();
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
            b'(' => open.push(b')'),
            b'[' => open.push(b']'),
            b')' | b']' if open.last() == Some(&c) => {
                open.pop();
            }
            _ if open.is_empty() && stops.contains(&c) => return (i, Some(c)),
            _ => {}
        }
        i += 1;
    }
    (b.len(), None)
}

/// Given the index of a `{`, returns (index after the matching `}`, index of
/// that `}`), or the end of input for an unterminated block.
/// Brackets nest as CSS Syntax says: inside `(...)` or `[...]` a `}` is
/// just a token and doesn't end the block, and stray closers are ignored.
fn skip_block(b: &[u8], open: usize) -> (usize, usize) {
    let mut open_brackets: Vec<u8> = Vec::new();
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
            b'{' => open_brackets.push(b'}'),
            b'(' => open_brackets.push(b')'),
            b'[' => open_brackets.push(b']'),
            b'}' | b')' | b']' if open_brackets.last() == Some(&c) => {
                open_brackets.pop();
                if open_brackets.is_empty() {
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
/// Whitespace-separated words, keeping `calc(1px + 2px)` and the like whole.
fn words(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if c.is_ascii_whitespace() && depth == 0 {
            if let Some(st) = start.take() {
                out.push(&s[st..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

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

/// `:is()` / `:not()` / `:where()` nested deeper than this make a selector
/// invalid.
const MAX_SELECTOR_NESTING: usize = 8;

pub(crate) fn parse_selector_list(s: &str) -> Option<Vec<Selector>> {
    parse_list(s, 0)
}

/// `depth` is the `:is()`/`:not()` nesting the list sits in.
fn parse_list(s: &str, depth: usize) -> Option<Vec<Selector>> {
    let parts = split_top_level(s, b',');
    if parts.len() > MAX_SELECTORS_PER_RULE {
        return None;
    }
    parts.into_iter().map(|p| parse_selector(p, depth)).collect()
}

/// The forgiving list of `:is()` and `:where()`: entries we can't parse are
/// dropped instead of invalidating the whole selector.
fn parse_forgiving_list(s: &str, depth: usize) -> Option<Vec<Selector>> {
    let parts = split_top_level(s, b',');
    if parts.len() > MAX_SELECTORS_PER_RULE {
        return None;
    }
    Some(parts.into_iter().filter_map(|p| parse_selector(p, depth)).collect())
}

fn parse_selector(s: &str, depth: usize) -> Option<Selector> {
    let chars: Vec<char> = s.trim().chars().collect();
    let mut compounds: Vec<Compound> = Vec::new();
    let mut combinators = Vec::new();
    let mut cur = Compound::default();
    let mut cur_has = false;
    let mut pending: Option<Combinator> = None;
    let mut pseudo_element = None;
    // Ids; classes, attributes and pseudo-classes; types.
    let mut spec = [0u32; 3];
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if pseudo_element.is_some() {
            // A pseudo-element ends the selector.
            return None;
        }
        if c.is_whitespace() {
            if cur_has {
                compounds.push(std::mem::take(&mut cur));
                cur_has = false;
                pending = Some(Combinator::Descendant);
            }
            i += 1;
            continue;
        }
        let explicit = match c {
            '>' => Some(Combinator::Child),
            '+' => Some(Combinator::Next),
            '~' => Some(Combinator::Later),
            _ => None,
        };
        if let Some(comb) = explicit {
            if cur_has {
                compounds.push(std::mem::take(&mut cur));
                cur_has = false;
            } else if pending.is_some_and(|p| p != Combinator::Descendant) {
                // `a > > b`.
                return None;
            }
            if compounds.is_empty() {
                return None;
            }
            pending = Some(comb);
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
                spec[1] += 1;
            }
            '#' => {
                i += 1;
                let id = ident(&chars, &mut i)?;
                if cur.id.is_some() {
                    // `#a#b` can never match a single element.
                    cur.pseudo.push(Pseudo::Never);
                }
                cur.id = Some(id);
                spec[0] += 1;
            }
            '[' => {
                i += 1;
                cur.attrs.push(attribute(&chars, &mut i)?);
                spec[1] += 1;
            }
            ':' => {
                i += 1;
                let double = chars.get(i) == Some(&':');
                if double {
                    i += 1;
                }
                let name = ident(&chars, &mut i)?.to_ascii_lowercase();
                // `::before` and `::after` (also with CSS 2's single colon).
                // Other pseudo-elements (`::first-line`, `::placeholder`, ...)
                // can't be drawn, so their rules are dropped rather than
                // applied to the element itself.
                if double || matches!(name.as_str(), "before" | "after" | "first-line" | "first-letter") {
                    let pe = match name.as_str() {
                        "before" => PseudoElement::Before,
                        "after" => PseudoElement::After,
                        _ => return None,
                    };
                    // Not inside `:is()` / `:not()`.
                    if depth > 0 {
                        return None;
                    }
                    pseudo_element = Some(pe);
                    spec[2] += 1;
                    cur_has = true;
                    while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                        i += 1;
                    }
                    continue;
                }
                if chars.get(i) == Some(&'(') {
                    let close = close_paren(&chars, i)?;
                    let arg: String = chars[i + 1..close].iter().collect();
                    i = close + 1;
                    let (pseudo, s) = functional_pseudo(&name, &arg, depth)?;
                    cur.pseudo.push(pseudo);
                    for (total, s) in spec.iter_mut().zip(s) {
                        *total = total.saturating_add(s);
                    }
                    cur_has = true;
                    continue;
                }
                let nth = |a, b, of_type, from_end| Pseudo::Nth { a, b, of_type, from_end };
                match name.as_str() {
                    "link" | "any-link" => cur.pseudo.push(Pseudo::Link),
                    // In a stylesheet, `:scope` is the root.
                    "root" | "scope" => cur.pseudo.push(Pseudo::Root),
                    "checked" => cur.pseudo.push(Pseudo::Checked),
                    "required" => cur.pseudo.push(Pseudo::Required(true)),
                    "optional" => cur.pseudo.push(Pseudo::Required(false)),
                    "empty" => cur.pseudo.push(Pseudo::Empty),
                    "first-child" => cur.pseudo.push(nth(0, 1, false, false)),
                    "last-child" => cur.pseudo.push(nth(0, 1, false, true)),
                    "only-child" => cur.pseudo.extend([nth(0, 1, false, false), nth(0, 1, false, true)]),
                    "first-of-type" => cur.pseudo.push(nth(0, 1, true, false)),
                    "last-of-type" => cur.pseudo.push(nth(0, 1, true, true)),
                    "only-of-type" => cur.pseudo.extend([nth(0, 1, true, false), nth(0, 1, true, true)]),
                    "visited" | "hover" | "active" | "focus" | "focus-visible" | "focus-within" | "target"
                    | "disabled" | "enabled" | "indeterminate" | "default" | "autofill" | "placeholder-shown"
                    | "popover-open" | "modal" | "fullscreen" | "open" => cur.pseudo.push(Pseudo::Never),
                    _ => return None,
                }
                spec[1] += 1;
            }
            c if c == '\\' || is_ident_start(c) => {
                if cur_has {
                    return None;
                }
                cur.tag = Some(ident(&chars, &mut i)?.to_ascii_lowercase());
                spec[2] += 1;
            }
            _ => return None,
        }
        cur_has = true;
    }
    if cur_has {
        compounds.push(cur);
    } else if pending.is_some_and(|p| p != Combinator::Descendant) {
        return None;
    }
    if compounds.is_empty() || compounds.len() > MAX_COMPOUNDS {
        return None;
    }
    let specificity = (spec[0].min(255) << 16) | (spec[1].min(255) << 8) | spec[2].min(255);
    Some(Selector { compounds, combinators, pseudo: pseudo_element, specificity })
}

/// `:not()`, `:is()`, `:nth-child()`, ... with their specificity (ids,
/// classes, types).
fn functional_pseudo(name: &str, arg: &str, depth: usize) -> Option<(Pseudo, [u32; 3])> {
    let class = [0, 1, 0];
    Some(match name {
        "not" | "is" | "where" => {
            if depth >= MAX_SELECTOR_NESTING {
                return None;
            }
            let list = if name == "not" { parse_list(arg, depth + 1)? } else { parse_forgiving_list(arg, depth + 1)? };
            // Selectors 4: the most specific argument counts, `:where()` none.
            let max = list.iter().map(|s| s.specificity).max().unwrap_or(0);
            let spec = if name == "where" { [0; 3] } else { [max >> 16, (max >> 8) & 255, max & 255] };
            (if name == "not" { Pseudo::Not(list) } else { Pseudo::Is(list) }, spec)
        }
        "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
            let (a, b) = parse_nth(arg)?;
            (Pseudo::Nth { a, b, of_type: name.ends_with("of-type"), from_end: name.starts_with("nth-last") }, class)
        }
        "lang" => {
            let ranges: Option<Vec<String>> = split_top_level(arg, b',')
                .into_iter()
                .map(|r| {
                    let r = r.trim().trim_matches(|c| c == '"' || c == '\'');
                    (!r.is_empty() && !r.contains(char::is_whitespace)).then(|| r.to_ascii_lowercase())
                })
                .collect();
            (Pseudo::Lang(ranges?), class)
        }
        "dir" => match arg.trim().to_ascii_lowercase().as_str() {
            "ltr" => (Pseudo::Dir(false), class),
            "rtl" => (Pseudo::Dir(true), class),
            _ => return None,
        },
        _ => return None,
    })
}

/// The `an+b` microsyntax: `odd`, `even`, `3`, `-n+2`, `2n + 1`, ... The
/// `of S` form is not supported, so selectors using it are dropped.
fn parse_nth(arg: &str) -> Option<(i32, i32)> {
    let s = arg.trim().to_ascii_lowercase();
    match s.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    let unsigned = |t: &str| -> Option<i32> {
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    };
    let signed = |t: &str| match t.strip_prefix('-') {
        Some(d) => unsigned(d).map(|n| -n),
        None => unsigned(t.strip_prefix('+').unwrap_or(t)),
    };
    let Some(n) = s.find('n') else { return Some((0, signed(&s)?)) };
    let a = match &s[..n] {
        "" | "+" => 1,
        "-" => -1,
        t => signed(t)?,
    };
    let rest = s[n + 1..].trim_start();
    if rest.is_empty() {
        return Some((a, 0));
    }
    let b = match rest.strip_prefix('+') {
        Some(d) => unsigned(d.trim_start())?,
        None => -unsigned(rest.strip_prefix('-')?.trim_start())?,
    };
    Some((a, b))
}

/// `[name]` or `[name op value flag]`; `i` is just past the `[`.
fn attribute(chars: &[char], i: &mut usize) -> Option<AttrSel> {
    let ws = |i: &mut usize| {
        while chars.get(*i).is_some_and(|c| c.is_whitespace()) {
            *i += 1;
        }
    };
    ws(i);
    let name = ident(chars, i)?.to_ascii_lowercase();
    ws(i);
    let op = match *chars.get(*i)? {
        ']' => {
            *i += 1;
            return Some(AttrSel { name, op: AttrOp::Exists, value: String::new(), ci: false });
        }
        '=' => {
            *i += 1;
            AttrOp::Equals
        }
        c if chars.get(*i + 1) == Some(&'=') => {
            let op = match c {
                '~' => AttrOp::Includes,
                '|' => AttrOp::Dash,
                '^' => AttrOp::Prefix,
                '$' => AttrOp::Suffix,
                '*' => AttrOp::Substring,
                _ => return None,
            };
            *i += 2;
            op
        }
        // Namespaces (`[ns|attr]`) and garbage.
        _ => return None,
    };
    ws(i);
    let mut value = match *chars.get(*i)? {
        q @ ('"' | '\'') => {
            *i += 1;
            string(chars, i, q)?
        }
        _ => ident(chars, i)?,
    };
    ws(i);
    let mut ci = CI_ATTRS.binary_search(&name.as_str()).is_ok();
    if chars.get(*i) != Some(&']') {
        match ident(chars, i)?.to_ascii_lowercase().as_str() {
            "i" => ci = true,
            "s" => ci = false,
            _ => return None,
        }
        ws(i);
    }
    if chars.get(*i) != Some(&']') {
        return None;
    }
    *i += 1;
    if ci {
        value.make_ascii_lowercase();
    }
    Some(AttrSel { name, op, value, ci })
}

/// Index of the `)` closing the `(` at `open`, skipping strings, escapes
/// and nested parentheses.
fn close_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            q @ ('"' | '\'') => {
                i += 1;
                while i < chars.len() && chars[i] != q {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '\\' => i += 1,
            '(' => depth += 1,
            ')' => {
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

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '-' || !c.is_ascii()
}

fn ident(chars: &[char], i: &mut usize) -> Option<String> {
    let mut out = String::new();
    while let Some(&c) = chars.get(*i) {
        if c == '\\' {
            *i += 1;
            out.push(escape(chars, i)?);
        } else if is_ident_start(c) || c.is_ascii_digit() {
            out.push(c);
            *i += 1;
        } else {
            break;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Decodes an escape; `i` is just past the backslash.
fn escape(chars: &[char], i: &mut usize) -> Option<char> {
    let mut hex = String::new();
    while hex.len() < 6 && chars.get(*i).is_some_and(|c| c.is_ascii_hexdigit()) {
        hex.push(chars[*i]);
        *i += 1;
    }
    if hex.is_empty() {
        let c = *chars.get(*i)?;
        *i += 1;
        return Some(c);
    }
    if chars.get(*i).is_some_and(|c| c.is_whitespace()) {
        *i += 1;
    }
    Some(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}'))
}

/// A quoted string's contents; `i` is just past the opening quote.
fn string(chars: &[char], i: &mut usize, quote: char) -> Option<String> {
    let mut out = String::new();
    loop {
        let c = *chars.get(*i)?;
        *i += 1;
        match c {
            '\\' => match chars.get(*i) {
                // An escaped newline continues the string.
                Some('\n') => *i += 1,
                Some(_) => out.push(escape(chars, i)?),
                None => {}
            },
            '\n' => return None,
            c if c == quote => return Some(out),
            c => out.push(c),
        }
    }
}

// ---------------------------------------------------------------------------
// Declarations

/// Parses the contents of a `style=""` attribute; relative `url()`s
/// resolve against `base`.
pub(crate) fn parse_declarations(s: &str, base: Base<'_>) -> Vec<Declaration> {
    let mut out = Vec::new();
    parse_declarations_into(&strip_comments(truncate(s, 64 * 1024)), &mut out, base);
    out
}

fn parse_declarations_into(body: &str, out: &mut Vec<Declaration>, base: Base<'_>) {
    for part in split_top_level(body, b';').into_iter().take(MAX_DECLS_PER_BLOCK) {
        let Some(colon) = part.find(':') else { continue };
        let raw_name = part[..colon].trim();
        let name = raw_name.to_ascii_lowercase();
        let mut value = part[colon + 1..].trim();
        let mut important = false;
        if let Some(bang) = value.rfind('!')
            && value[bang + 1..].trim().eq_ignore_ascii_case("important")
        {
            important = true;
            value = value[..bang].trim_end();
        }
        if let Some(custom) = raw_name.strip_prefix("--") {
            // Custom properties keep their case and their text, even empty;
            // `vars` gives them meaning at computed-value time.
            let valid_name = !custom.is_empty()
                && custom.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii());
            if valid_name && value.len() <= vars::MAX_VALUE_BYTES && !value.contains('{') {
                let raw = RawDecl { name: raw_name.into(), value: value.into(), base: None };
                out.push(Declaration { decl: Decl::Custom(Box::new(raw)), important });
            }
            continue;
        }
        if value.is_empty() || value.contains('{') {
            continue;
        }
        if vars::has_var(value) {
            // Substituted and parsed per element, if it is a property we know.
            if !parse_substituted(&name, None, None).is_empty() {
                let raw = RawDecl { name: name.into(), value: value.into(), base: base.cloned() };
                out.push(Declaration { decl: Decl::Var(Box::new(raw)), important });
            }
            continue;
        }
        let mut decls = Vec::new();
        parse_property(&name, value, &mut decls, base);
        out.extend(decls.into_iter().map(|decl| Declaration { decl, important }));
    }
}

/// Parses a property's value after `var()` substitution. `None` (a
/// reference failed) or a value we can't parse leaves every longhand of the
/// property `unset`: the declaration is invalid at computed-value time.
pub(crate) fn parse_substituted(name: &str, value: Option<&str>, base: Base<'_>) -> Vec<Decl> {
    let mut out = Vec::new();
    if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty() && !v.contains('{')) {
        parse_property(name, v, &mut out, base);
    }
    if out.is_empty() {
        parse_property(name, "unset", &mut out, None);
    }
    out
}

fn wide_keyword(v: &str) -> Option<Wide> {
    match v.to_ascii_lowercase().as_str() {
        "inherit" => Some(Wide::Inherit),
        "initial" => Some(Wide::Initial),
        "unset" | "revert" | "revert-layer" => Some(Wide::Unset),
        _ => None,
    }
}

const BACKGROUND_ALL: [Prop; 8] = [
    Prop::Background,
    Prop::BgImage,
    Prop::BgRepeat,
    Prop::BgPosition,
    Prop::BgSize,
    Prop::BgOrigin,
    Prop::BgClip,
    Prop::BgAttachment,
];

fn parse_property(name: &str, value: &str, out: &mut Vec<Decl>, base: Base<'_>) {
    let props: &[Prop] = match name {
        "display" => &[Prop::Display],
        "color" => &[Prop::Color],
        "background-color" => &[Prop::Background],
        "background" => &BACKGROUND_ALL,
        "background-image" => &[Prop::BgImage],
        "background-repeat" => &[Prop::BgRepeat],
        "background-position" => &[Prop::BgPosition],
        "background-size" | "-webkit-background-size" => &[Prop::BgSize],
        "background-origin" | "-webkit-background-origin" => &[Prop::BgOrigin],
        "background-clip" | "-webkit-background-clip" => &[Prop::BgClip],
        "background-attachment" => &[Prop::BgAttachment],
        "content" => &[Prop::Content],
        "counter-reset" => &[Prop::Counters(CounterOp::Reset)],
        "counter-increment" => &[Prop::Counters(CounterOp::Increment)],
        "counter-set" => &[Prop::Counters(CounterOp::Set)],
        "quotes" => &[Prop::Quotes],
        "border-collapse" => &[Prop::BorderCollapse],
        "table-layout" => &[Prop::TableLayout],
        "caption-side" => &[Prop::CaptionSide],
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
        "opacity" => &[Prop::Opacity],
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
        "position" => &[Prop::Position],
        "top" => &[Prop::Inset(0)],
        "right" => &[Prop::Inset(1)],
        "bottom" => &[Prop::Inset(2)],
        "left" => &[Prop::Inset(3)],
        "inset" => &[Prop::Inset(0), Prop::Inset(1), Prop::Inset(2), Prop::Inset(3)],
        "z-index" => &[Prop::ZIndex],
        "float" => &[Prop::Float],
        "clear" => &[Prop::Clear],
        "overflow" => &[Prop::Overflow(0), Prop::Overflow(1)],
        "overflow-x" => &[Prop::Overflow(0)],
        "overflow-y" => &[Prop::Overflow(1)],
        "height" => &[Prop::Height],
        "min-height" => &[Prop::MinHeight],
        "max-height" => &[Prop::MaxHeight],
        "min-width" => &[Prop::MinWidth],
        "border" => &BORDER_ALL,
        "border-top" => &[Prop::BorderWidth(0), Prop::BorderStyle(0), Prop::BorderColor(0)],
        "border-right" => &[Prop::BorderWidth(1), Prop::BorderStyle(1), Prop::BorderColor(1)],
        "border-bottom" => &[Prop::BorderWidth(2), Prop::BorderStyle(2), Prop::BorderColor(2)],
        "border-left" => &[Prop::BorderWidth(3), Prop::BorderStyle(3), Prop::BorderColor(3)],
        "border-width" => &[Prop::BorderWidth(0), Prop::BorderWidth(1), Prop::BorderWidth(2), Prop::BorderWidth(3)],
        "border-style" => &[Prop::BorderStyle(0), Prop::BorderStyle(1), Prop::BorderStyle(2), Prop::BorderStyle(3)],
        "border-color" => &[Prop::BorderColor(0), Prop::BorderColor(1), Prop::BorderColor(2), Prop::BorderColor(3)],
        "border-top-width" => &[Prop::BorderWidth(0)],
        "border-right-width" => &[Prop::BorderWidth(1)],
        "border-bottom-width" => &[Prop::BorderWidth(2)],
        "border-left-width" => &[Prop::BorderWidth(3)],
        "border-top-style" => &[Prop::BorderStyle(0)],
        "border-right-style" => &[Prop::BorderStyle(1)],
        "border-bottom-style" => &[Prop::BorderStyle(2)],
        "border-left-style" => &[Prop::BorderStyle(3)],
        "border-top-color" => &[Prop::BorderColor(0)],
        "border-right-color" => &[Prop::BorderColor(1)],
        "border-bottom-color" => &[Prop::BorderColor(2)],
        "border-left-color" => &[Prop::BorderColor(3)],
        "border-radius" => &[Prop::BorderRadius],
        "flex-direction" => &[Prop::FlexDirection],
        "flex-wrap" => &[Prop::FlexWrap],
        "flex-flow" => &[Prop::FlexDirection, Prop::FlexWrap],
        "justify-content" => &[Prop::JustifyContent],
        "align-items" => &[Prop::AlignItems],
        "align-self" => &[Prop::AlignSelf],
        "align-content" => &[Prop::AlignContent],
        "place-items" => &[Prop::AlignItems],
        "place-content" => &[Prop::AlignContent, Prop::JustifyContent],
        "flex-grow" => &[Prop::FlexGrow],
        "flex-shrink" => &[Prop::FlexShrink],
        "flex-basis" => &[Prop::FlexBasis],
        "flex" => &[Prop::FlexGrow, Prop::FlexShrink, Prop::FlexBasis],
        "order" => &[Prop::Order],
        "gap" | "grid-gap" => &[Prop::Gap(0), Prop::Gap(1)],
        "mask" | "mask-image" | "-webkit-mask" | "-webkit-mask-image" => &[Prop::Mask],
        "row-gap" | "grid-row-gap" => &[Prop::Gap(0)],
        "column-gap" | "grid-column-gap" => &[Prop::Gap(1)],
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
        "background" => return values::background_shorthand(value, base, out),
        "background-image"
        | "background-repeat"
        | "background-position"
        | "background-size"
        | "-webkit-background-size"
        | "background-origin"
        | "-webkit-background-origin"
        | "background-clip"
        | "-webkit-background-clip"
        | "background-attachment" => values::background_longhand(name, value, base),
        "content" => values::content(value, base),
        "counter-reset" => values::counters(CounterOp::Reset, value),
        "counter-increment" => values::counters(CounterOp::Increment, value),
        "counter-set" => values::counters(CounterOp::Set, value),
        "quotes" => values::quotes(value),
        "border-collapse" => match lower.as_str() {
            "collapse" => Some(Decl::BorderCollapse(true)),
            "separate" => Some(Decl::BorderCollapse(false)),
            _ => None,
        },
        "table-layout" => match lower.as_str() {
            "fixed" => Some(Decl::TableFixed(true)),
            "auto" => Some(Decl::TableFixed(false)),
            _ => None,
        },
        "caption-side" => match lower.as_str() {
            "top" | "block-start" => Some(Decl::CaptionBottom(false)),
            "bottom" | "block-end" => Some(Decl::CaptionBottom(true)),
            _ => None,
        },
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
        // Partial opacity is drawn opaque; only "invisible" matters, which
        // is how pages hide things (focus-only controls, menu toggles).
        "opacity" => {
            let n = match lower.strip_suffix('%') {
                Some(p) => p.trim().parse::<f32>().ok().map(|p| p / 100.0),
                None => lower.parse::<f32>().ok(),
            };
            n.filter(|n| n.is_finite()).map(|n| Decl::Transparent(n <= 0.01))
        }
        "text-transform" => match lower.as_str() {
            "none" => Some(TextTransform::None),
            "uppercase" => Some(TextTransform::Upper),
            "lowercase" => Some(TextTransform::Lower),
            "capitalize" => Some(TextTransform::Capitalize),
            _ => None,
        }
        .map(Decl::TextTransform),
        "margin" | "padding" => {
            let vals: Option<Vec<Length>> = words(&lower).into_iter().map(parse_length).collect();
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
        "border-spacing" => {
            let one =
                |t: &str| parse_length(t).filter(|l| non_negative(*l) && !matches!(l, Length::Pct(_) | Length::Auto));
            let vals: Option<Vec<Length>> = words(&lower).into_iter().map(one).collect();
            match vals.as_deref() {
                Some([s]) => Some(Decl::BorderSpacing(*s, *s)),
                Some([h, v]) => Some(Decl::BorderSpacing(*h, *v)),
                _ => None,
            }
        }
        "position" => match lower.as_str() {
            "static" => Some(Position::Static),
            "relative" => Some(Position::Relative),
            "absolute" => Some(Position::Absolute),
            "fixed" => Some(Position::Fixed),
            "sticky" | "-webkit-sticky" => Some(Position::Sticky),
            _ => None,
        }
        .map(Decl::Position),
        "top" | "right" | "bottom" | "left" => {
            let side = match name {
                "top" => 0,
                "right" => 1,
                "bottom" => 2,
                _ => 3,
            };
            parse_length(&lower).map(|l| Decl::Inset(side, l))
        }
        "inset" => {
            if let Some(sides) = four_sides(&lower, parse_length) {
                out.extend(sides.into_iter().enumerate().map(|(side, l)| Decl::Inset(side, l)));
            }
            return;
        }
        "z-index" => match lower.as_str() {
            "auto" => Some(Decl::ZIndex(None)),
            v => v.parse::<i64>().ok().map(|z| Decl::ZIndex(Some(z.clamp(-(1 << 30), 1 << 30) as i32))),
        },
        "float" => match lower.as_str() {
            "none" => Some(Float::None),
            "left" | "inline-start" => Some(Float::Left),
            "right" | "inline-end" => Some(Float::Right),
            _ => None,
        }
        .map(Decl::Float),
        "clear" => match lower.as_str() {
            "none" => Some(Clear::None),
            "left" | "inline-start" => Some(Clear::Left),
            "right" | "inline-end" => Some(Clear::Right),
            "both" | "all" => Some(Clear::Both),
            _ => None,
        }
        .map(Decl::Clear),
        "overflow" | "overflow-x" | "overflow-y" => {
            let vals: Option<Vec<bool>> = lower.split_whitespace().map(parse_overflow).collect();
            let axes: &[usize] = match name {
                "overflow-x" => &[0],
                "overflow-y" => &[1],
                _ => &[0, 1],
            };
            match vals.as_deref() {
                Some([v]) => out.extend(axes.iter().map(|&a| Decl::Overflow(a, *v))),
                Some([x, y]) if axes.len() == 2 => out.extend([Decl::Overflow(0, *x), Decl::Overflow(1, *y)]),
                _ => {}
            }
            return;
        }
        "height" => parse_size(&lower).map(Decl::Height),
        "min-height" => parse_size(&lower).map(Decl::MinHeight),
        "min-width" => parse_size(&lower).map(Decl::MinWidth),
        "max-height" => {
            if lower == "none" {
                Some(Decl::MaxHeight(None))
            } else {
                parse_size(&lower).filter(|l| *l != Length::Auto).map(|l| Decl::MaxHeight(Some(l)))
            }
        }
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let sides: &[usize] = match name {
                "border-top" => &[0],
                "border-right" => &[1],
                "border-bottom" => &[2],
                "border-left" => &[3],
                _ => &[0, 1, 2, 3],
            };
            parse_border_shorthand(&lower, sides, out);
            return;
        }
        "border-width" | "border-style" | "border-color" => {
            match name {
                "border-width" => {
                    if let Some(s) = four_sides(&lower, parse_border_width) {
                        out.extend(s.into_iter().enumerate().map(|(i, l)| Decl::BorderWidth(i, l)));
                    }
                }
                "border-style" => {
                    if let Some(s) = four_sides(&lower, parse_border_style) {
                        out.extend(s.into_iter().enumerate().map(|(i, v)| Decl::BorderStyle(i, v)));
                    }
                }
                _ => {
                    let colors: Option<Vec<ColorValue>> = words(&lower).into_iter().map(parse_color).collect();
                    if let Some(s) = colors.as_deref().and_then(expand_sides) {
                        out.extend(s.into_iter().enumerate().map(|(i, c)| Decl::BorderColor(i, c)));
                    }
                }
            }
            return;
        }
        n if n.starts_with("border-") && n.ends_with("-width") => {
            side_of(n).zip(parse_border_width(&lower)).map(|(s, l)| Decl::BorderWidth(s, l))
        }
        n if n.starts_with("border-") && n.ends_with("-style") => {
            side_of(n).zip(parse_border_style(&lower)).map(|(s, v)| Decl::BorderStyle(s, v))
        }
        n if n.starts_with("border-") && n.ends_with("-color") => {
            side_of(n).zip(parse_color(&lower)).map(|(s, c)| Decl::BorderColor(s, c))
        }
        "border-radius" => words(&lower)
            .first()
            .and_then(|t| parse_length(t.split('/').next().unwrap_or("")))
            .filter(|l| non_negative(*l) && *l != Length::Auto)
            .map(Decl::BorderRadius),
        "flex-direction" => parse_flex_direction(&lower).map(Decl::FlexDirection),
        "flex-wrap" => parse_flex_wrap(&lower).map(Decl::FlexWrap),
        "flex-flow" => {
            for t in lower.split_whitespace() {
                if let Some(d) = parse_flex_direction(t) {
                    out.push(Decl::FlexDirection(d));
                } else if let Some(w) = parse_flex_wrap(t) {
                    out.push(Decl::FlexWrap(w));
                }
            }
            return;
        }
        "justify-content" => parse_justify(&lower).map(Decl::JustifyContent),
        "align-content" => {
            parse_justify(&lower).map(|j| Decl::AlignContent(if lower == "normal" { Justify::Stretch } else { j }))
        }
        "place-content" => {
            let mut it = lower.split_whitespace();
            let first = it.next().and_then(parse_justify);
            let second = it.next().map_or(first, parse_justify);
            if let (Some(a), Some(j)) = (first, second) {
                out.extend([Decl::AlignContent(a), Decl::JustifyContent(j)]);
            }
            return;
        }
        "align-items" => parse_align(&lower).filter(|a| *a != Align::Auto).map(Decl::AlignItems),
        "place-items" => {
            lower.split_whitespace().next().and_then(parse_align).filter(|a| *a != Align::Auto).map(Decl::AlignItems)
        }
        "align-self" => parse_align(&lower).map(Decl::AlignSelf),
        "flex-grow" => parse_number(&lower).filter(|n| *n >= 0.0).map(|n| Decl::FlexGrow(n.min(1e6))),
        "flex-shrink" => parse_number(&lower).filter(|n| *n >= 0.0).map(|n| Decl::FlexShrink(n.min(1e6))),
        "flex-basis" => parse_flex_basis(&lower).map(Decl::FlexBasis),
        "flex" => {
            parse_flex_shorthand(&lower, out);
            return;
        }
        "order" => lower.parse::<i64>().ok().map(|o| Decl::Order(o.clamp(-(1 << 30), 1 << 30) as i32)),
        "gap" | "grid-gap" => {
            let vals: Option<Vec<Length>> = words(&lower).into_iter().map(parse_gap).collect();
            match vals.as_deref() {
                Some([g]) => out.extend([Decl::Gap(0, *g), Decl::Gap(1, *g)]),
                Some([r, c]) => out.extend([Decl::Gap(0, *r), Decl::Gap(1, *c)]),
                _ => {}
            }
            return;
        }
        "row-gap" | "grid-row-gap" => parse_gap(&lower).map(|g| Decl::Gap(0, g)),
        "mask" | "mask-image" | "-webkit-mask" | "-webkit-mask-image" => Some(Decl::Masked(lower != "none")),
        "column-gap" | "grid-column-gap" => parse_gap(&lower).map(|g| Decl::Gap(1, g)),
        _ => None,
    };
    out.extend(decl);
}

const BORDER_ALL: [Prop; 12] = [
    Prop::BorderWidth(0),
    Prop::BorderWidth(1),
    Prop::BorderWidth(2),
    Prop::BorderWidth(3),
    Prop::BorderStyle(0),
    Prop::BorderStyle(1),
    Prop::BorderStyle(2),
    Prop::BorderStyle(3),
    Prop::BorderColor(0),
    Prop::BorderColor(1),
    Prop::BorderColor(2),
    Prop::BorderColor(3),
];

/// `border-top-width` -> 0, ..., `border-left-color` -> 3.
fn side_of(name: &str) -> Option<usize> {
    let side = name.strip_prefix("border-")?.split('-').next()?;
    Some(match side {
        "top" | "block-start" => 0,
        "right" | "inline-end" => 1,
        "bottom" | "block-end" => 2,
        "left" | "inline-start" => 3,
        _ => return None,
    })
}

/// One to four values expanded to top, right, bottom, left.
fn expand_sides<T: Copy>(vals: &[T]) -> Option<[T; 4]> {
    Some(match *vals {
        [a] => [a, a, a, a],
        [a, b] => [a, b, a, b],
        [a, b, c] => [a, b, c, b],
        [a, b, c, d] => [a, b, c, d],
        _ => return None,
    })
}

fn four_sides<T: Copy>(v: &str, parse: impl Fn(&str) -> Option<T>) -> Option<[T; 4]> {
    let vals: Option<Vec<T>> = words(v).into_iter().map(parse).collect();
    expand_sides(&vals?)
}

fn parse_overflow(v: &str) -> Option<bool> {
    match v {
        "visible" => Some(false),
        "hidden" | "clip" | "auto" | "scroll" | "overlay" => Some(true),
        _ => None,
    }
}

/// `width`-like values: non-negative lengths, `auto`, and the intrinsic
/// size keywords.
fn parse_size(v: &str) -> Option<Length> {
    match v {
        "min-content" | "-webkit-min-content" | "-moz-min-content" => Some(Length::MinContent),
        "max-content" | "-webkit-max-content" | "-moz-max-content" => Some(Length::MaxContent),
        "fit-content" | "-webkit-fit-content" | "-moz-fit-content" | "-webkit-fill-available" | "stretch" => {
            Some(if v.contains("fill") || v == "stretch" { Length::Auto } else { Length::FitContent })
        }
        _ => parse_length(v).filter(|l| non_negative(*l)),
    }
}

fn parse_border_width(v: &str) -> Option<Length> {
    match v {
        "thin" => Some(Length::Px(1.0)),
        "medium" => Some(Length::Px(3.0)),
        "thick" => Some(Length::Px(5.0)),
        _ => parse_length(v).filter(|l| non_negative(*l) && !matches!(l, Length::Pct(_) | Length::Auto)),
    }
}

fn parse_border_style(v: &str) -> Option<bool> {
    match v {
        "none" | "hidden" => Some(false),
        "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset" => Some(true),
        _ => None,
    }
}

/// `border: 1px solid red` and the per-side forms. Omitted parts reset to
/// their initial values, as for every shorthand.
fn parse_border_shorthand(v: &str, sides: &[usize], out: &mut Vec<Decl>) {
    let (mut width, mut style, mut color) = (None, None, None);
    for t in words(v) {
        if width.is_none()
            && let Some(w) = parse_border_width(t)
        {
            width = Some(w);
        } else if style.is_none()
            && let Some(s) = parse_border_style(t)
        {
            style = Some(s);
        } else if color.is_none()
            && let Some(c) = parse_color(t)
        {
            color = Some(c);
        } else {
            return;
        }
    }
    for &s in sides {
        out.push(Decl::BorderWidth(s, width.unwrap_or(Length::Px(3.0))));
        out.push(Decl::BorderStyle(s, style.unwrap_or(false)));
        out.push(Decl::BorderColor(s, color.unwrap_or(ColorValue::Current)));
    }
}

fn parse_flex_direction(v: &str) -> Option<FlexDirection> {
    match v {
        "row" => Some(FlexDirection::Row),
        "row-reverse" => Some(FlexDirection::RowReverse),
        "column" => Some(FlexDirection::Column),
        "column-reverse" => Some(FlexDirection::ColumnReverse),
        _ => None,
    }
}

fn parse_flex_wrap(v: &str) -> Option<bool> {
    match v {
        "nowrap" => Some(false),
        "wrap" | "wrap-reverse" => Some(true),
        _ => None,
    }
}

fn parse_justify(v: &str) -> Option<Justify> {
    let v = v.trim_start_matches("safe ").trim_start_matches("unsafe ").trim();
    match v {
        "flex-start" | "start" | "left" | "normal" | "baseline" | "first baseline" | "last baseline" => {
            Some(Justify::Start)
        }
        "flex-end" | "end" | "right" => Some(Justify::End),
        "center" => Some(Justify::Center),
        "space-between" => Some(Justify::SpaceBetween),
        "space-around" => Some(Justify::SpaceAround),
        "space-evenly" => Some(Justify::SpaceEvenly),
        "stretch" => Some(Justify::Stretch),
        _ => None,
    }
}

fn parse_align(v: &str) -> Option<Align> {
    let v = v.trim_start_matches("safe ").trim_start_matches("unsafe ").trim();
    match v {
        "auto" => Some(Align::Auto),
        "stretch" | "normal" => Some(Align::Stretch),
        "flex-start" | "start" | "self-start" | "left" => Some(Align::Start),
        "flex-end" | "end" | "self-end" | "right" => Some(Align::End),
        "center" => Some(Align::Center),
        "baseline" | "first baseline" | "last baseline" => Some(Align::Baseline),
        _ => None,
    }
}

fn parse_flex_basis(v: &str) -> Option<Length> {
    match v {
        "content" => Some(Length::Auto),
        _ => parse_size(v),
    }
}

/// `flex: none | auto | <grow> <shrink>? || <basis>`.
fn parse_flex_shorthand(v: &str, out: &mut Vec<Decl>) {
    let (grow, shrink, basis) = match v {
        "none" => (0.0, 0.0, Length::Auto),
        "auto" => (1.0, 1.0, Length::Auto),
        _ => {
            let mut nums = Vec::new();
            let mut basis = None;
            for t in words(v) {
                if let Some(n) = parse_number(t)
                    && nums.len() < 2
                    && n >= 0.0
                {
                    nums.push(n.min(1e6));
                } else if basis.is_none()
                    && let Some(b) = parse_flex_basis(t)
                {
                    basis = Some(b);
                } else {
                    return;
                }
            }
            if nums.is_empty() && basis.is_none() {
                return;
            }
            // A bare number means a zero basis; a bare basis means `1 1 <basis>`.
            let grow = nums.first().copied().unwrap_or(1.0);
            let shrink = nums.get(1).copied().unwrap_or(1.0);
            (grow, shrink, basis.unwrap_or(Length::Pct(0.0)))
        }
    };
    out.extend([Decl::FlexGrow(grow), Decl::FlexShrink(shrink), Decl::FlexBasis(basis)]);
}

fn parse_gap(v: &str) -> Option<Length> {
    match v {
        "normal" => Some(Length::Px(0.0)),
        _ => parse_length(v).filter(|l| non_negative(*l) && *l != Length::Auto),
    }
}

/// False only for lengths that are negative at parse time. A `calc()` that
/// turns out negative is clamped where it is used, as CSS says.
fn non_negative(l: Length) -> bool {
    match l {
        Length::Px(v) | Length::Em(v) | Length::Rem(v) | Length::Pct(v) => v >= 0.0,
        _ => true,
    }
}

fn is_valid_padding(l: Length) -> bool {
    l != Length::Auto && non_negative(l)
}

pub(crate) fn parse_display(v: &str) -> Option<Display> {
    // Multi-keyword syntax: `block flow`, `inline flex`, `block flow-root`.
    let words: Vec<&str> = v.split_whitespace().collect();
    if words.contains(&"list-item") {
        return Some(Display::ListItem);
    }
    let one = match words.as_slice() {
        [w] => *w,
        ["block", "flow"] | ["flow", "block"] => "block",
        ["inline", "flow"] | ["flow", "inline"] => "inline",
        ["block", "flow-root"] | ["flow-root", "block"] => "flow-root",
        ["inline", "flow-root"] | ["flow-root", "inline"] => "inline-block",
        ["block", inner] | [inner, "block"] => inner,
        ["inline", "flex"] | ["flex", "inline"] => "inline-flex",
        ["inline", "grid"] | ["grid", "inline"] => "inline-grid",
        ["inline", "table"] | ["table", "inline"] => "inline-table",
        _ => return None,
    };
    Some(match one {
        "none" => Display::None,
        "inline" | "ruby" => Display::Inline,
        "contents" => Display::Contents,
        "inline-block" | "-webkit-inline-box" => Display::InlineBlock,
        "inline-table" => Display::InlineTable,
        "inline-flex" | "-webkit-inline-flex" | "-ms-inline-flexbox" => Display::InlineFlex,
        "inline-grid" | "-ms-inline-grid" => Display::InlineGrid,
        "block" | "-webkit-box" => Display::Block,
        "flow-root" => Display::FlowRoot,
        "flex" | "-webkit-flex" | "-ms-flexbox" => Display::Flex,
        "grid" | "-ms-grid" => Display::Grid,
        "table" => Display::Table,
        "table-caption" => Display::TableCaption,
        "table-row-group" => Display::TableRowGroup,
        "table-header-group" => Display::TableHeaderGroup,
        "table-footer-group" => Display::TableFooterGroup,
        "table-row" => Display::TableRow,
        "table-cell" => Display::TableCell,
        "table-column" | "table-column-group" => Display::TableColumn,
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
    if let Some(open) = t.find('(') {
        return parse_math(&t[..open].to_ascii_lowercase(), t[open + 1..].strip_suffix(')')?);
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
        "vw" | "svw" | "lvw" | "dvw" | "vi" | "vmax" => Length::Calc(Calc { vw: n, ..Calc::default() }),
        "vh" | "svh" | "lvh" | "dvh" | "vb" | "vmin" => Length::Calc(Calc { vw: n * VH_PER_VW, ..Calc::default() }),
        "" if n == 0.0 => Length::Px(0.0),
        _ => return None,
    })
}

/// Layout never sees the viewport height (it must not influence the page,
/// and the window is letterboxed anyway), so `vh` assumes a 16:9 viewport.
pub(crate) const VH_PER_VW: f32 = 0.5625;
const MAX_MATH_DEPTH: usize = 16;
const MAX_MATH_TOKENS: usize = 256;

/// `calc()`, `min()`, `max()` and `clamp()`. `min`/`max`/`clamp` may not mix
/// in percentages (the result would depend on the containing block in a
/// non-linear way) and take at most three arguments.
fn parse_math(name: &str, inner: &str) -> Option<Length> {
    let simplify = |c: Calc| -> Length {
        if c.em == 0.0 && c.rem == 0.0 && c.vw == 0.0 {
            if c.pct == 0.0 {
                return Length::Px(c.px);
            }
            if c.px == 0.0 {
                return Length::Pct(c.pct);
            }
        }
        Length::Calc(c)
    };
    match name {
        "calc" | "-webkit-calc" | "-moz-calc" => match eval_math(inner)? {
            // A bare number is only a length when it is zero.
            (c, true) if c.px != 0.0 => None,
            (c, _) => Some(simplify(c)),
        },
        "min" | "max" | "clamp" => {
            let args: Option<Vec<Calc>> = split_top_level(inner, b',')
                .into_iter()
                .map(|a| match eval_math(a)? {
                    (c, false) if c.pct == 0.0 => Some(c),
                    (c, true) if c.px == 0.0 => Some(c),
                    _ => None,
                })
                .collect();
            let inf = |sign: f32| Calc { px: sign * f32::INFINITY, ..Calc::default() };
            match (name, args?.as_slice()) {
                ("min" | "max", [a]) => Some(simplify(*a)),
                ("min", [a, b]) => Some(Length::Clamp(inf(-1.0), *a, *b)),
                ("max", [a, b]) => Some(Length::Clamp(*a, *b, inf(1.0))),
                ("clamp", [a, b, c]) => Some(Length::Clamp(*a, *b, *c)),
                _ => None,
            }
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum MathToken {
    /// A value, and whether it is a plain number.
    Value(Calc, bool),
    Op(u8),
    Open,
    Close,
}

/// Evaluates a math expression to a linear sum; the flag is true for a
/// plain number. Products need a number on one side, quotients a nonzero
/// number on the right.
fn eval_math(s: &str) -> Option<(Calc, bool)> {
    let tokens = math_tokens(s)?;
    let mut pos = 0;
    let v = math_sum(&tokens, &mut pos, 0)?;
    (pos == tokens.len()).then_some(v)
}

fn math_tokens(s: &str) -> Option<Vec<MathToken>> {
    let mut tokens = Vec::new();
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        if tokens.len() >= MAX_MATH_TOKENS {
            return None;
        }
        let after_value = matches!(tokens.last(), Some(MathToken::Value(..) | MathToken::Close));
        let c = rest.as_bytes()[0];
        let len = match c {
            b'(' => {
                tokens.push(MathToken::Open);
                1
            }
            b')' => {
                tokens.push(MathToken::Close);
                1
            }
            b'*' | b'/' => {
                tokens.push(MathToken::Op(c));
                1
            }
            b'+' | b'-' if after_value => {
                tokens.push(MathToken::Op(c));
                1
            }
            c if c.is_ascii_alphabetic() => {
                // Nested functions: only `calc(`, which is just parentheses.
                let open = rest.find('(')?;
                if !rest[..open].eq_ignore_ascii_case("calc") {
                    return None;
                }
                tokens.push(MathToken::Open);
                open + 1
            }
            _ => {
                let end = rest.find(|ch: char| ch.is_ascii_whitespace() || "()*/".contains(ch)).unwrap_or(rest.len());
                let tok = &rest[..end];
                tokens.push(match parse_number(tok) {
                    Some(n) => MathToken::Value(Calc { px: n, ..Calc::default() }, true),
                    None => MathToken::Value(Calc::of(parse_length(tok)?)?, false),
                });
                end
            }
        };
        rest = rest[len..].trim_start();
    }
    Some(tokens)
}

fn math_sum(t: &[MathToken], pos: &mut usize, depth: usize) -> Option<(Calc, bool)> {
    let (mut acc, number) = math_product(t, pos, depth)?;
    while let Some(&MathToken::Op(op @ (b'+' | b'-'))) = t.get(*pos) {
        *pos += 1;
        let (rhs, rhs_number) = math_product(t, pos, depth)?;
        if rhs_number != number {
            return None;
        }
        acc = acc.plus(if op == b'-' { rhs.scaled(-1.0) } else { rhs });
    }
    Some((acc, number))
}

fn math_product(t: &[MathToken], pos: &mut usize, depth: usize) -> Option<(Calc, bool)> {
    let mut acc = math_value(t, pos, depth)?;
    while let Some(&MathToken::Op(op @ (b'*' | b'/'))) = t.get(*pos) {
        *pos += 1;
        let rhs = math_value(t, pos, depth)?;
        acc = match (op, acc.1, rhs.1) {
            (b'*', true, _) => (rhs.0.scaled(acc.0.px), rhs.1),
            (b'*', false, true) => (acc.0.scaled(rhs.0.px), false),
            (b'/', _, true) if rhs.0.px != 0.0 => (acc.0.scaled(1.0 / rhs.0.px), acc.1),
            _ => return None,
        };
    }
    Some(acc)
}

fn math_value(t: &[MathToken], pos: &mut usize, depth: usize) -> Option<(Calc, bool)> {
    match *t.get(*pos)? {
        MathToken::Value(c, n) => {
            *pos += 1;
            Some((c, n))
        }
        MathToken::Open if depth < MAX_MATH_DEPTH => {
            *pos += 1;
            let v = math_sum(t, pos, depth + 1)?;
            if t.get(*pos) != Some(&MathToken::Close) {
                return None;
            }
            *pos += 1;
            Some(v)
        }
        _ => None,
    }
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
        #[cfg(feature = "test-fonts")]
        if let Some(f) = crate::text::test_family(name) {
            return f;
        }
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

    fn doc(html: &str) -> Dom {
        crate::dom::parse(html, &url::Url::parse("https://example.org/").unwrap()).dom
    }

    /// The element with `id=...`.
    fn by_id(dom: &Dom, id: &str) -> NodeId {
        (0..dom.nodes.len()).find(|&n| dom.element(n).is_some_and(|e| e.id.as_deref() == Some(id))).expect(id)
    }

    fn sel(s: &str) -> Selector {
        parse_selector(s, 0).unwrap_or_else(|| panic!("{s} should parse"))
    }

    /// A matcher for one document: `m(selector, id)`.
    fn matcher(html: &str) -> impl Fn(&str, &str) -> bool + use<> {
        let dom = doc(html);
        move |s, id| sel(s).matches(&Tree::new(&dom), by_id(&dom, id))
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
        assert_eq!(parse_length("10vw"), Some(Length::Calc(Calc { vw: 10.0, ..Calc::default() })));
        assert_eq!(parse_length("1e400px"), None);
    }

    #[test]
    fn parses_math() {
        let calc = |px, em, pct| Length::Calc(Calc { px, em, pct, ..Calc::default() });
        assert_eq!(parse_length("calc(1px + 2px)"), Some(Length::Px(3.0)));
        assert_eq!(parse_length("calc(100% - 2 * 10px)"), Some(calc(-20.0, 0.0, 100.0)));
        assert_eq!(parse_length("calc((1em + 4px) / 2)"), Some(calc(2.0, 0.5, 0.0)));
        assert_eq!(parse_length("calc(50% - calc(1em - 2px))"), Some(calc(2.0, -1.0, 50.0)));
        assert_eq!(parse_length("CALC(0)"), Some(Length::Px(0.0)));
        // Invalid: unit times unit, division by zero or a length, bare numbers.
        assert_eq!(parse_length("calc(1px * 2px)"), None);
        assert_eq!(parse_length("calc(1px / 0)"), None);
        assert_eq!(parse_length("calc(1px / 1px)"), None);
        assert_eq!(parse_length("calc(1px + 2)"), None);
        assert_eq!(parse_length("calc(3)"), None);
        assert_eq!(parse_length("calc(1px + )"), None);
        assert_eq!(parse_length("calc(1px"), None);
        let px = |v| Calc { px: v, ..Calc::default() };
        let inf = |s: f32| Calc { px: s * f32::INFINITY, ..Calc::default() };
        assert_eq!(
            parse_length("min(10px, 2em)"),
            Some(Length::Clamp(inf(-1.0), px(10.0), Calc { em: 2.0, ..px(0.0) }))
        );
        assert_eq!(parse_length("max(10px,20px)"), Some(Length::Clamp(px(10.0), px(20.0), inf(1.0))));
        assert_eq!(parse_length("clamp(1px, 2px, 3px)"), Some(Length::Clamp(px(1.0), px(2.0), px(3.0))));
        assert_eq!(parse_length("min(100%, 10px)"), None, "percentages can't be clamped yet");
        // Hostile nesting and length stay bounded.
        let deep = format!("calc({}1px{})", "(".repeat(5000), ")".repeat(5000));
        assert_eq!(parse_length(&deep), None);
        let long = format!("calc({})", vec!["1px"; 10_000].join(" + "));
        assert_eq!(parse_length(&long), None);
    }

    #[test]
    fn parses_layout_properties() {
        let decls = |css: &str| parse_declarations(css, None).into_iter().map(|d| d.decl).collect::<Vec<_>>();
        assert_eq!(
            decls("border: 2px solid red"),
            [0, 1, 2, 3]
                .into_iter()
                .flat_map(|s| [
                    Decl::BorderWidth(s, Length::Px(2.0)),
                    Decl::BorderStyle(s, true),
                    Decl::BorderColor(s, ColorValue::Rgba(Color::rgb(255, 0, 0)))
                ])
                .collect::<Vec<_>>()
        );
        assert_eq!(decls("border-left-width: thick"), [Decl::BorderWidth(3, Length::Px(5.0))]);
        assert_eq!(
            decls("border-style: none solid"),
            [
                Decl::BorderStyle(0, false),
                Decl::BorderStyle(1, true),
                Decl::BorderStyle(2, false),
                Decl::BorderStyle(3, true)
            ]
        );
        assert_eq!(decls("flex: 1"), [Decl::FlexGrow(1.0), Decl::FlexShrink(1.0), Decl::FlexBasis(Length::Pct(0.0))]);
        assert_eq!(decls("flex: none"), [Decl::FlexGrow(0.0), Decl::FlexShrink(0.0), Decl::FlexBasis(Length::Auto)]);
        assert_eq!(
            decls("flex: 2 0 10px"),
            [Decl::FlexGrow(2.0), Decl::FlexShrink(0.0), Decl::FlexBasis(Length::Px(10.0))]
        );
        assert_eq!(
            decls("flex: 20%"),
            [Decl::FlexGrow(1.0), Decl::FlexShrink(1.0), Decl::FlexBasis(Length::Pct(20.0))]
        );
        assert_eq!(decls("flex-flow: column wrap"), [Decl::FlexDirection(FlexDirection::Column), Decl::FlexWrap(true)]);
        assert_eq!(decls("gap: 1px 2px"), [Decl::Gap(0, Length::Px(1.0)), Decl::Gap(1, Length::Px(2.0))]);
        assert_eq!(decls("justify-content: space-between"), [Decl::JustifyContent(Justify::SpaceBetween)]);
        assert_eq!(decls("align-items: flex-end"), [Decl::AlignItems(Align::End)]);
        assert_eq!(
            decls("position: absolute; top: 0; inset: 1px 2px"),
            [
                Decl::Position(Position::Absolute),
                Decl::Inset(0, Length::Px(0.0)),
                Decl::Inset(0, Length::Px(1.0)),
                Decl::Inset(1, Length::Px(2.0)),
                Decl::Inset(2, Length::Px(1.0)),
                Decl::Inset(3, Length::Px(2.0))
            ]
        );
        assert_eq!(decls("overflow: hidden visible"), [Decl::Overflow(0, true), Decl::Overflow(1, false)]);
        assert_eq!(
            decls("height: fit-content; max-height: none"),
            [Decl::Height(Length::FitContent), Decl::MaxHeight(None)]
        );
        assert_eq!(
            decls("margin: 0 calc(50% - 10px)")[1],
            Decl::Margin(1, Length::Calc(Calc { px: -10.0, pct: 50.0, ..Calc::default() }))
        );
        assert_eq!(decls("z-index: 99999999999"), [Decl::ZIndex(Some(1 << 30))]);
        assert_eq!(parse_display("inline flex"), Some(Display::InlineFlex));
        assert_eq!(parse_display("block flow-root"), Some(Display::FlowRoot));
        assert_eq!(parse_display("flow-root"), Some(Display::FlowRoot));
        // Invalid values add nothing.
        assert!(decls("border: 1px solid red blue; flex: 1 2 3; height: -5px; float: up").is_empty());
    }

    #[test]
    fn specificity_orders_ids_classes_types() {
        let s = |t| sel(t).specificity;
        assert!(s("#a") > s(".a.b.c"));
        assert!(s(".a") > s("div p span"));
        assert!(s("a.btn") > s(".btn"));
        assert_eq!(s("*"), 0);
        assert_eq!(s("ul li a:link"), (1 << 8) | 3);
        assert_eq!(s("[type=text]"), 1 << 8);
        assert_eq!(s("li:nth-child(2n+1)"), (1 << 8) | 1);
        assert_eq!(s("a:only-child"), (1 << 8) | 1);
        assert_eq!(s("h2 + p ~ p"), 3);
        // Selectors 4: `:is()` and `:not()` count their most specific
        // argument, `:where()` counts nothing.
        assert_eq!(s(":is(#a, .b, p)"), 1 << 16);
        assert_eq!(s("p:not(.a, .b.c)"), (2 << 8) | 1);
        assert_eq!(s(":where(#a .b) p"), 1);
        assert_eq!(s(":not(:where(#a))"), 0);
    }

    #[test]
    fn pseudo_element_selectors() {
        let pe = |s: &str| parse_selector(s, 0).map(|s| (s.pseudo, s.specificity));
        assert_eq!(pe("p::before"), Some((Some(PseudoElement::Before), 2)));
        assert_eq!(pe(".a:after"), Some((Some(PseudoElement::After), (1 << 8) | 1)));
        assert_eq!(pe("::BEFORE "), Some((Some(PseudoElement::Before), 1)));
        assert_eq!(pe("div > ::after"), Some((Some(PseudoElement::After), 2)));
        assert_eq!(pe("p"), Some((None, 1)));
        // They match the originating element, and say so.
        let sheet = Stylesheet::parse("p::before { color: red } p { color: blue } p:hover::after { color: red }");
        let dom = doc("<p id=p>x</p>");
        let mut out = Vec::new();
        sheet.matching(&Tree::new(&dom), by_id(&dom, "p"), 800.0, 0, &mut out);
        let got: Vec<Option<PseudoElement>> = out.iter().map(|m| m.pseudo).collect();
        assert_eq!(got, vec![Some(PseudoElement::Before), None]);
    }

    #[test]
    fn rejects_unsupported_selectors() {
        for bad in [
            "p::first-line",
            "a:first-letter",
            "p::before span",
            "p::before::after",
            "::placeholder",
            "> a",
            "a > > b",
            "a + + b",
            "a +",
            "a ~",
            "li:nth-child(2n of .x)",
            "li:nth-child(2 n)",
            "li:nth-child(n+)",
            "li:nth-child(2n+-1)",
            "li:nth-child(99999999999)",
            ":has(a)",
            "[ns|a]",
            "[a=b c]",
            "[a",
            "[a='b]",
            "[=b]",
            ":not()",
            ":not(p::before)",
            ":dir(up)",
            ":lang()",
        ] {
            assert!(parse_selector(bad, 0).is_none(), "{bad}");
        }
        // One bad selector drops the whole list, as CSS requires...
        assert!(parse_selector_list("h1, p::selection").is_none());
        assert_eq!(parse_selector_list("h1, .x > p").unwrap().len(), 2);
        // ...but `:is()` and `:where()` forgive bad arguments.
        assert!(parse_selector(":is(p::before, a)", 0).is_some());
        assert!(parse_selector(":where(:unknown)", 0).is_some());
    }

    #[test]
    fn matches_descendant_and_child() {
        let m = matcher("<html><body><div id=main class='box wide'><p id=p class=note>x</p></div></body></html>");
        for yes in
            ["p", "div p", "html p", "div > p", "html > body > div > p.note", "#main .note", ".box.wide p", "* p"]
        {
            assert!(m(yes, "p"), "{yes}");
        }
        for no in ["html > p", ".missing p", "p div", "p:hover", "div > div p", "#main#main p"] {
            assert!(!m(no, "p"), "{no}");
        }
        let root = matcher("<html id=h><body id=b>");
        assert!(root(":root", "h"));
        assert!(root(":scope > body", "b"));
        assert!(!root(":root", "b"));
    }

    #[test]
    fn sibling_combinators() {
        let m = matcher(
            "<h2 id=h>t</h2><p id=a>1</p> text <p id=b>2</p><div id=d></div>
             <input type=checkbox id=c checked><span id=s></span><ul><li id=l></li></ul>",
        );
        assert!(m("h2 + p", "a"));
        assert!(!m("h2 + p", "b"));
        assert!(m("p + p", "b"), "text between siblings doesn't count");
        assert!(m("h2 ~ p", "b"));
        assert!(m("h2 ~ p + div", "d"));
        assert!(!m("div ~ p", "a"));
        assert!(!m("p ~ h2", "h"));
        assert!(m("input:checked ~ span", "s"));
        assert!(m(":checked + span", "s"));
        assert!(m("h2 ~ ul > li", "l"));
        assert!(m("h2 ~ ul li", "l"));
        assert!(m("body > h2 ~ * li", "l"));
        assert!(!m("h2 + ul li", "l"));
        assert!(!m("li ~ li", "l"));
    }

    #[test]
    fn checked_is_static() {
        let m = matcher(
            "<input type=checkbox id=a checked><input type=checkbox id=b><input type=text id=c checked>
             <input type=RADIO id=r checked><select><option id=o selected>x</option><option id=p>y</option></select>",
        );
        let checked: Vec<&str> = ["a", "b", "c", "r", "o", "p"].into_iter().filter(|id| m(":checked", id)).collect();
        assert_eq!(checked, ["a", "r", "o"]);
    }

    #[test]
    fn attribute_selectors() {
        let m = matcher(
            r#"<a id=a href="https://example.org/file.PDF" lang=en-GB class="x  y" data-v="" title='a "b"' type=Text>l</a>"#,
        );
        for yes in [
            "[href]",
            "a[href]",
            "[HREF]",
            "[ href ]",
            "[href^='https://']",
            "[href$='.PDF']",
            "[href$='.pdf' i]",
            "[href$=\".pdf\" I]",
            "[href*=example]",
            "[href = 'https://example.org/file.PDF']",
            "[class~=y]",
            "[class~=x]",
            "[lang|=en]",
            "[lang=en-gb]",
            "[data-v]",
            "[data-v='']",
            r#"[title='a "b"']"#,
            r#"[title="a \"b\""]"#,
            r"[title='a \22 b\22 ']",
            // HTML compares `type` values case-insensitively.
            "[type=text]",
        ] {
            assert!(m(yes, "a"), "{yes}");
        }
        for no in [
            "[nope]",
            "[href$='.pdf']",
            "[href$='.pdf' s]",
            "[href*=EXAMPLE]",
            "[class~='x y']",
            "[class~='']",
            "[lang|=en-]",
            "[lang=en]",
            "[data-v^='']",
            "[data-v$='']",
            "[data-v*='']",
            "[type=text s]",
        ] {
            assert!(!m(no, "a"), "{no}");
        }
    }

    #[test]
    fn structural_pseudo_classes() {
        let m = matcher(
            "<ul><li id=l1>1</li><li id=l2>2</li><li id=l3>3</li><li id=l4>4</li><li id=l5>5</li></ul>
             <div><span id=s1></span><b id=b1></b><span id=s2></span></div>
             <p id=e></p><p id=t> </p><p id=only><i id=i></i></p>",
        );
        let which =
            |s: &str| -> Vec<&str> { ["l1", "l2", "l3", "l4", "l5"].into_iter().filter(|id| m(s, id)).collect() };
        assert_eq!(which(":first-child"), ["l1"]);
        assert_eq!(which(":last-child"), ["l5"]);
        assert_eq!(which(":nth-child(odd)"), ["l1", "l3", "l5"]);
        assert_eq!(which(":nth-child(EVEN)"), ["l2", "l4"]);
        assert_eq!(which(":nth-child(2)"), ["l2"]);
        assert_eq!(which(":nth-child(+2)"), ["l2"]);
        assert_eq!(which(":nth-child(-n+2)"), ["l1", "l2"]);
        assert_eq!(which(":nth-child(n+4)"), ["l4", "l5"]);
        assert_eq!(which(":nth-child(3n)"), ["l3"]);
        assert_eq!(which(":nth-child( 2n - 1 )"), ["l1", "l3", "l5"]);
        assert_eq!(which(":nth-child(-2n+5)"), ["l1", "l3", "l5"]);
        assert_eq!(which(":nth-child(0)"), Vec::<&str>::new());
        assert_eq!(which(":nth-child(0n+0)"), Vec::<&str>::new());
        assert_eq!(which(":nth-last-child(2)"), ["l4"]);
        assert_eq!(which(":nth-last-child(-n+2)"), ["l4", "l5"]);
        assert_eq!(which("li:not(:first-child):not(:last-child)"), ["l2", "l3", "l4"]);
        assert_eq!(which(":only-child"), Vec::<&str>::new());
        assert!(m("span:first-of-type", "s1"));
        assert!(!m("span:first-of-type", "s2"));
        assert!(m("span:last-of-type", "s2"));
        assert!(m("span:nth-of-type(2)", "s2"));
        assert!(m(":nth-last-of-type(1)", "s2"));
        assert!(m("b:only-of-type", "b1"));
        assert!(!m("span:only-of-type", "s1"));
        assert!(!m(":first-child", "b1"));
        assert!(m(":only-child", "i"));
        // Whitespace counts as content, as in browsers.
        assert!(m(":empty", "e"));
        assert!(m(":empty", "i"));
        assert!(!m(":empty", "t"));
        assert!(!m(":empty", "only"));
    }

    #[test]
    fn logical_pseudo_classes() {
        let m = matcher("<nav id=n class=menu><a id=a href=/x class=item>x</a><a id=b class='item active'>y</a></nav>");
        assert!(m("a:not(.active)", "a"));
        assert!(!m("a:not(.active)", "b"));
        assert!(m(":not(.x, .y)", "a"));
        assert!(!m(":not(.active, .zzz)", "b"));
        assert!(!m(":not(nav) > a", "a"));
        assert!(m(":not(:not(.item))", "a"));
        assert!(m(":is(nav, header) .item", "a"));
        assert!(m(":where(.menu) :is(.active, #nope)", "b"));
        assert!(m("a:is(:link)", "a"));
        assert!(!m("a:is(:link)", "b"));
        assert!(m(":is(nav > a)", "a"), "arguments are complex selectors");
        assert!(m(":is(.menu .item):not(nav .active)", "a"));
        assert!(m("a:not(:hover)", "a"), "a dynamic state never holds, so its negation always does");
        assert!(!m(":is(:hover, .nope)", "a"));
        assert!(!m(":where(:unknown)", "a"));
    }

    #[test]
    fn lang_and_dir() {
        let m = matcher(
            "<div id=d lang=en-US><p id=p>x</p><p id=f lang=fr dir=rtl><span id=s>y</span></p></div>
             <p id=n>z</p><p id=e lang=''>e</p>",
        );
        assert!(m(":lang(en)", "p"));
        assert!(m(":lang(EN-us)", "p"));
        assert!(m("div:lang(en)", "d"));
        assert!(!m(":lang(en-GB)", "p"));
        assert!(!m(":lang(e)", "p"));
        assert!(m(":lang(de, fr)", "s"));
        assert!(m(":lang(\"fr\")", "s"));
        assert!(!m(":lang(en)", "s"));
        assert!(!m(":lang(en)", "n"));
        assert!(m(":lang(*)", "p"));
        assert!(!m(":lang(*)", "n"));
        assert!(!m(":lang(*)", "e"));
        assert!(m(":dir(rtl)", "s"));
        assert!(m(":dir(ltr)", "p"));
        assert!(m(":dir(ltr)", "n"));
    }

    #[test]
    fn hostile_selectors_are_bounded() {
        // Deep nesting is refused at parse time, without recursing deeper.
        let deep = ":not(".repeat(100_000) + "a" + &")".repeat(100_000);
        assert!(parse_selector_list(&deep).is_none());
        assert!(parse_selector_list(&":is(".repeat(100_000)).is_none());
        let ok = ":not(".repeat(MAX_SELECTOR_NESTING) + "b" + &")".repeat(MAX_SELECTOR_NESTING);
        assert!(parse_selector_list(&ok).is_some());
        let too_deep = ":not(".repeat(MAX_SELECTOR_NESTING + 1) + "b" + &")".repeat(MAX_SELECTOR_NESTING + 1);
        assert!(parse_selector_list(&too_deep).is_none());
        // Exponential `:is()` fan-out over a deep and wide tree: the match
        // budget bounds the work whatever the answer.
        let mut s = "* * :not(div) ~ *".to_string();
        for _ in 0..4 {
            s = format!(":is({})", [s.as_str(); 8].join(", "));
        }
        let selector = sel(&format!("{s} {s} ~ {s} *"));
        let dom = doc(&("<div>".repeat(200) + &"<span></span>".repeat(2000) + "<b id=t></b>"));
        let tree = Tree::new(&dom);
        let _ = selector.matches(&tree, by_id(&dom, "t"));
        // Long sibling lists: nearby matches are found, far misses give up.
        assert!(sel("span ~ b").matches(&tree, by_id(&dom, "t")));
        assert!(!sel("i ~ b").matches(&tree, by_id(&dom, "t")));
        // Huge attribute values.
        let big = "a".repeat(16 * 1024);
        let m = matcher(&format!("<p id=p data-x='{big}'></p>"));
        assert!(m(&format!("[data-x*='{big}']"), "p"));
        assert!(!m(&format!("[data-x*='{big}b']"), "p"));
        assert!(m(&format!("[data-x^='{}' i]", "A".repeat(8000)), "p"));
        assert!(m(&format!("[data-x~={big}]"), "p"));
        assert!(!m(&format!("[data-x$='{}']", "a".repeat(100_000)), "p"));
    }

    #[test]
    fn ci_attrs_are_sorted() {
        assert!(CI_ATTRS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn stylesheet_skips_at_rules_and_garbage() {
        let sheet = Stylesheet::parse(
            "@import url(x.css); @media (max-width: 600px) { p { color: red } }
             /* comment { } */ p { color: blue; margin: 1px 2px } h1, h2 { font-weight: bold !important }
             ;;; } garbage without block; h3 { color: red }
             a:hover { color: red } div { unknown-prop: 1; color: var(--x) }
             @font-face { font-family: x }",
        );
        // The stray `;;; } garbage...` belongs to the `h3` selector, which
        // is then invalid.
        assert_eq!(sheet.rules.len(), 5);
        assert_eq!(sheet.rules[0].media.len(), 1, "@media rules are kept, with their condition");
        assert_eq!(sheet.rules[1].decls.len(), 5);
        assert!(sheet.rules[1].media.is_empty());
        assert!(sheet.rules[2].decls[0].important);
        assert!(matches!(&sheet.rules[4].decls[..], [Declaration { decl: Decl::Var(_), .. }]));
    }

    #[test]
    fn custom_properties_and_var_are_kept_as_text() {
        let d = parse_declarations(
            "--Main-Color: #ABC !important; --empty:; color: VAR(--x, red); margin: var(--a) 0;
             --bad name: 1; unknown: var(--x); -- : x; --x: {}",
            None,
        );
        let raw = |n: &str, v: &str| Box::new(RawDecl { name: n.into(), value: v.into(), base: None });
        assert_eq!(
            d,
            vec![
                Declaration { decl: Decl::Custom(raw("--Main-Color", "#ABC")), important: true },
                Declaration { decl: Decl::Custom(raw("--empty", "")), important: false },
                Declaration { decl: Decl::Var(raw("color", "VAR(--x, red)")), important: false },
                Declaration { decl: Decl::Var(raw("margin", "var(--a) 0")), important: false },
            ]
        );
        assert_eq!(parse_substituted("margin", Some("1px 2px"), None).len(), 4);
        // Invalid at computed-value time: every longhand is unset.
        assert_eq!(parse_substituted("margin", Some("bogus"), None), parse_substituted("margin", None, None));
        assert_eq!(parse_substituted("color", None, None), vec![Decl::Wide(Prop::Color, Wide::Unset)]);
        let huge = format!("--h: {}", "x".repeat(vars::MAX_VALUE_BYTES + 1));
        assert!(Stylesheet::parse(&format!("p {{ {huge} }}")).rules.is_empty());
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
        let dom = doc("<p id=p>x</p>");
        let tree = Tree::new(&dom);
        let count = |w: f32| {
            let mut out = Vec::new();
            sheet.matching(&tree, by_id(&dom, "p"), w, 0, &mut out);
            out.len()
        };
        assert_eq!(count(500.0), 1);
        assert_eq!(count(700.0), 2);
    }

    #[test]
    fn matching_uses_index_and_order() {
        let sheet = Stylesheet::parse("p { color: red } .note { color: blue } #x { color: green } * { color: gray }");
        let dom = doc("<p id=p class=note>x</p>");
        let mut out = Vec::new();
        sheet.matching(&Tree::new(&dom), by_id(&dom, "p"), 800.0, 100, &mut out);
        let orders: Vec<u32> = out.iter().map(|m| m.order).collect();
        assert_eq!(orders, vec![100, 101, 103]);
    }

    #[test]
    fn shorthands_expand() {
        let d =
            parse_declarations("margin: 1px 2px 3px; padding: 4px; font: italic bold 12px/1.5 Georgia, serif", None);
        assert!(d.contains(&Declaration { decl: Decl::Margin(3, Length::Px(2.0)), important: false }));
        assert!(d.contains(&Declaration { decl: Decl::Padding(2, Length::Px(4.0)), important: false }));
        assert!(d.contains(&Declaration { decl: Decl::FontFamily(Family::Serif), important: false }));
        assert!(d.contains(&Declaration { decl: Decl::LineHeight(LineHeightValue::Number(1.5)), important: false }));
        assert!(d.contains(&Declaration { decl: Decl::Italic(true), important: false }));
        let bg = parse_declarations("background: url(a.png) no-repeat #123456", None);
        assert_eq!(bg[0].decl, Decl::Background(ColorValue::Rgba(Color::rgb(0x12, 0x34, 0x56))));
        assert_eq!(bg.len(), 8, "every background longhand is set");
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
            let _ = parse_declarations(i, None);
        }
        let deep = "(".repeat(100_000) + "{" + &"[".repeat(100_000);
        let _ = Stylesheet::parse(&deep);
    }
}
