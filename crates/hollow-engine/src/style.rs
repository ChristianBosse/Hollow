//! Computed styles and the cascade.
//!
//! Styles are computed per layout, because `@media` rules depend on the
//! viewport width. Percentages stay unresolved until layout knows the
//! containing block.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::css::{
    self, Base, BgSize, BoxArea, Calc, Color, ColorValue, ContentItem, CounterList, Decl, Declaration, FontSize,
    FontWeight, Image, Length, LineHeightValue, Matched, MediaList, Prop, PseudoElement, QuoteList, Repeat, Stylesheet,
    Wide,
};
pub(crate) use crate::css::{Align, Clear, FlexDirection, Float, Justify, Position};
use crate::dom::{Dom, Element, NodeData, NodeId};
use crate::text::Family;
use crate::vars;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Display {
    None,
    Inline,
    /// The element's box disappears; its children take its place.
    Contents,
    Block,
    /// A block that establishes a new formatting context.
    FlowRoot,
    ListItem,
    InlineBlock,
    Flex,
    InlineFlex,
    Grid,
    InlineGrid,
    Table,
    InlineTable,
    TableCaption,
    TableRowGroup,
    TableHeaderGroup,
    TableFooterGroup,
    TableRow,
    TableCell,
    /// `table-column` and `table-column-group`, which generate no boxes.
    TableColumn,
}

impl Display {
    /// The block-level equivalent, for floats, absolutely positioned boxes
    /// and flex items (CSS "blockification").
    pub fn blockified(self) -> Display {
        match self {
            Display::Inline | Display::InlineBlock => Display::Block,
            Display::InlineFlex => Display::Flex,
            Display::InlineGrid => Display::Grid,
            Display::InlineTable => Display::Table,
            d if d.is_table_internal() => Display::Block,
            d => d,
        }
    }

    /// The parts of a table, which need a table around them (CSS 2.1 §17.2.1).
    pub fn is_table_internal(self) -> bool {
        matches!(
            self,
            Display::TableCaption
                | Display::TableRowGroup
                | Display::TableHeaderGroup
                | Display::TableFooterGroup
                | Display::TableRow
                | Display::TableCell
                | Display::TableColumn
        )
    }

    pub fn is_row_group(self) -> bool {
        matches!(self, Display::TableRowGroup | Display::TableHeaderGroup | Display::TableFooterGroup)
    }
}

/// One background image layer, with its lengths computed.
#[derive(Clone, Debug)]
pub(crate) struct BgLayer {
    pub image: Image,
    pub repeat: [Repeat; 2],
    /// Percentages refer to the free space, as `background-position` says.
    pub position: [Length; 2],
    pub size: BgSize,
    pub origin: BoxArea,
    pub clip: BoxArea,
    /// `background-attachment: fixed`: positioned against the page, since
    /// fixed things scroll with it in Hollow.
    pub fixed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TextAlign {
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WhiteSpace {
    Normal,
    NoWrap,
    Pre,
    PreWrap,
    PreLine,
}

impl WhiteSpace {
    pub fn collapses_spaces(self) -> bool {
        matches!(self, WhiteSpace::Normal | WhiteSpace::NoWrap | WhiteSpace::PreLine)
    }
    pub fn keeps_newlines(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine)
    }
    pub fn wraps(self) -> bool {
        matches!(self, WhiteSpace::Normal | WhiteSpace::PreWrap | WhiteSpace::PreLine)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ListStyle {
    Disc,
    Circle,
    Square,
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
    None,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TextTransform {
    None,
    Upper,
    Lower,
    Capitalize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VAlign {
    Baseline,
    Sub,
    Super,
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum LineHeight {
    Normal,
    /// Multiple of the element's own font size (inherited as a factor).
    Factor(f32),
    Px(f32),
}

/// A computed length that may still depend on the containing block.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Len {
    Px(f32),
    Pct(f32),
    /// `calc(px + pct%)`.
    Calc(f32, f32),
    Auto,
    MinContent,
    MaxContent,
    FitContent,
}

impl Len {
    /// The length in px; `auto` and the keywords resolve to 0, so callers
    /// check for them first where they mean something else.
    pub fn resolve(self, basis: f32) -> f32 {
        match self {
            Len::Px(v) => v,
            Len::Pct(p) => basis * p / 100.0,
            Len::Calc(px, p) => px + basis * p / 100.0,
            _ => 0.0,
        }
    }

    /// A definite length: not `auto` and not an intrinsic keyword.
    pub fn is_definite(self) -> bool {
        matches!(self, Len::Px(_) | Len::Pct(_) | Len::Calc(..))
    }

    /// Whether resolving needs the containing block's size.
    pub fn has_pct(self) -> bool {
        matches!(self, Len::Pct(_) | Len::Calc(..))
    }
}

/// The default family for unstyled text. Browsers default to serif; Hollow
/// follows that so plain documents look the way their authors saw them.
pub(crate) const DEFAULT_FAMILY: Family = Family::Serif;
pub(crate) const DEFAULT_FONT_SIZE: f32 = 16.0;
const MAX_FONT_SIZE: f32 = 512.0;
const MAX_EXTENT: f32 = 100_000.0;

#[derive(Clone, Debug)]
pub(crate) struct Style {
    // Inherited.
    pub color: Color,
    pub font_size: f32,
    pub weight: u16,
    pub italic: bool,
    pub family: Family,
    pub text_align: TextAlign,
    pub line_height: LineHeight,
    pub white_space: WhiteSpace,
    pub list_style: ListStyle,
    pub visible: bool,
    /// `opacity: 0` here (not inherited, but hides the whole subtree).
    pub transparent: bool,
    /// Inside an element with `opacity: 0`.
    pub in_transparent: bool,
    pub text_transform: TextTransform,
    /// Horizontal, vertical.
    pub border_spacing: [f32; 2],
    pub border_collapse: bool,
    pub caption_bottom: bool,
    /// `None` is `auto`.
    pub quotes: Option<QuoteList>,
    // Not inherited.
    pub display: Display,
    pub background: Option<Color>,
    /// Image layers, top first; `None` when there are none.
    pub bg_layers: Option<Arc<[BgLayer]>>,
    /// The box the background color fills: the bottom layer's clip.
    pub bg_color_clip: BoxArea,
    /// `None` for `normal` / `none`.
    pub content: Option<Arc<[ContentItem]>>,
    /// `counter-reset`, `counter-increment`, `counter-set`; `None` is `none`.
    pub counters: [Option<CounterList>; 3],
    pub table_fixed: bool,
    /// Text decorations propagate to all descendants, so these are the union
    /// of the element's own and its ancestors'.
    pub underline: bool,
    pub line_through: bool,
    /// Top, right, bottom, left.
    pub margin: [Len; 4],
    pub padding: [Len; 4],
    pub width: Len,
    pub max_width: Option<Len>,
    pub min_width: Len,
    pub height: Len,
    pub min_height: Len,
    pub max_height: Option<Len>,
    pub vertical_align: VAlign,
    pub border_box: bool,
    pub position: Position,
    /// Top, right, bottom, left.
    pub inset: [Len; 4],
    pub z_index: Option<i32>,
    pub float: Float,
    pub clear: Clear,
    /// x and y: anything but `overflow: visible`.
    pub overflow: [bool; 2],
    /// Used widths: zero where the style is `none`.
    pub border_width: [f32; 4],
    pub border_color: [Color; 4],
    pub radius: Len,
    pub flex_direction: FlexDirection,
    pub flex_wrap: bool,
    pub justify_content: Justify,
    pub align_items: Align,
    pub align_self: Align,
    pub align_content: Justify,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Len,
    pub order: i32,
    /// Row gap, column gap.
    pub gap: [Len; 2],
    /// Has a CSS mask, which the engine can't draw: usually an icon whose
    /// background color shows through an image, which would paint as a
    /// solid block without it.
    pub masked: bool,
}

impl Style {
    pub fn initial() -> Style {
        Style {
            color: Color::BLACK,
            font_size: DEFAULT_FONT_SIZE,
            weight: 400,
            italic: false,
            family: DEFAULT_FAMILY,
            text_align: TextAlign::Left,
            line_height: LineHeight::Normal,
            white_space: WhiteSpace::Normal,
            list_style: ListStyle::Disc,
            visible: true,
            transparent: false,
            in_transparent: false,
            text_transform: TextTransform::None,
            border_spacing: [0.0; 2],
            border_collapse: false,
            caption_bottom: false,
            quotes: None,
            display: Display::Inline,
            background: None,
            bg_layers: None,
            bg_color_clip: BoxArea::Border,
            content: None,
            counters: [None, None, None],
            table_fixed: false,
            underline: false,
            line_through: false,
            margin: [Len::Px(0.0); 4],
            padding: [Len::Px(0.0); 4],
            width: Len::Auto,
            max_width: None,
            min_width: Len::Auto,
            height: Len::Auto,
            min_height: Len::Auto,
            max_height: None,
            vertical_align: VAlign::Baseline,
            border_box: false,
            position: Position::Static,
            inset: [Len::Auto; 4],
            z_index: None,
            float: Float::None,
            clear: Clear::None,
            overflow: [false; 2],
            border_width: [0.0; 4],
            border_color: [Color::BLACK; 4],
            radius: Len::Px(0.0),
            flex_direction: FlexDirection::Row,
            flex_wrap: false,
            justify_content: Justify::Start,
            align_items: Align::Stretch,
            align_self: Align::Auto,
            // `normal`, which is `stretch` for flex containers.
            align_content: Justify::Stretch,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: Len::Auto,
            order: 0,
            gap: [Len::Px(0.0); 2],
            masked: false,
        }
    }

    /// A child's starting point: inherited properties copied, the rest initial.
    fn inherit_from(parent: &Style) -> Style {
        Style {
            color: parent.color,
            font_size: parent.font_size,
            weight: parent.weight,
            italic: parent.italic,
            family: parent.family,
            text_align: parent.text_align,
            line_height: parent.line_height,
            white_space: parent.white_space,
            list_style: parent.list_style,
            visible: parent.visible,
            in_transparent: parent.in_transparent || parent.transparent,
            text_transform: parent.text_transform,
            border_spacing: parent.border_spacing,
            border_collapse: parent.border_collapse,
            caption_bottom: parent.caption_bottom,
            quotes: parent.quotes.clone(),
            ..Style::initial()
        }
    }

    pub fn bold(&self) -> bool {
        self.weight >= 600
    }

    /// Taken out of flow: `absolute` or `fixed`.
    pub fn out_of_flow(&self) -> bool {
        matches!(self.position, Position::Absolute | Position::Fixed)
    }

    /// A containing block for absolutely positioned descendants.
    pub fn positioned(&self) -> bool {
        self.position != Position::Static
    }

    /// Clips its contents (`overflow` other than `visible`).
    pub fn clips(&self) -> bool {
        self.overflow[0] || self.overflow[1]
    }

    pub fn is_row(&self) -> bool {
        matches!(self.flex_direction, FlexDirection::Row | FlexDirection::RowReverse)
    }

    /// Line height in px for this element's font.
    pub fn line_height_px(&self, normal_factor: f32) -> f32 {
        match self.line_height {
            LineHeight::Normal => self.font_size * normal_factor,
            LineHeight::Factor(f) => self.font_size * f,
            LineHeight::Px(px) => px,
        }
    }
}

/// The built-in user-agent stylesheet. Written in the same CSS subset the
/// engine supports so there is only one cascade implementation.
const UA_CSS: &str = r#"
html, address, blockquote, body, center, dialog, div, figure, figcaption, footer, form, header, hr,
legend, listing, main, p, plaintext, pre, search, xmp, article, aside, h1, h2, h3, h4, h5, h6, hgroup,
nav, section, dir, dd, dl, dt, menu, ol, ul, fieldset, details, summary, optgroup, address, frameset, frame
{ display: block }
li { display: list-item }
table { display: table; border-spacing: 2px; border-collapse: separate; box-sizing: border-box }
caption { display: table-caption; text-align: center }
thead { display: table-header-group; vertical-align: middle }
tbody { display: table-row-group; vertical-align: middle }
tfoot { display: table-footer-group; vertical-align: middle }
tr { display: table-row; vertical-align: inherit }
td, th { display: table-cell; padding: 1px; vertical-align: inherit }
th { font-weight: bold; text-align: center }
col { display: table-column }
colgroup { display: table-column-group }
q::before { content: open-quote }
q::after { content: close-quote }
body { margin: 8px }
p, dl, multicol { margin: 1em 0 }
blockquote, figure { margin: 1em 40px }
dd { margin-left: 40px }
ul, ol, menu, dir { margin: 1em 0; padding-left: 40px }
ul { list-style-type: disc }
ol { list-style-type: decimal }
ul ul, ol ul, menu ul, ul menu { list-style-type: circle }
ul ul ul, ul ol ul, ol ul ul, ol ol ul { list-style-type: square }
ul ul, ul ol, ol ul, ol ol, ul menu, menu ul, ol menu, menu ol { margin-top: 0; margin-bottom: 0 }
h1 { font-size: 2em; margin: 0.67em 0; font-weight: bold }
h2 { font-size: 1.5em; margin: 0.83em 0; font-weight: bold }
h3 { font-size: 1.17em; margin: 1em 0; font-weight: bold }
h4 { font-size: 1em; margin: 1.33em 0; font-weight: bold }
h5 { font-size: 0.83em; margin: 1.67em 0; font-weight: bold }
h6 { font-size: 0.67em; margin: 2.33em 0; font-weight: bold }
b, strong { font-weight: bold }
i, em, cite, var, dfn, address { font-style: italic }
pre, listing, xmp, plaintext { font-family: monospace; white-space: pre; margin: 1em 0; font-size: 0.8125em }
code, kbd, samp, tt { font-family: monospace; font-size: 0.8125em }
pre code, pre kbd, pre samp, pre tt { font-size: 1em }
:link { color: #0000ee; text-decoration: underline }
u, ins { text-decoration: underline }
s, strike, del { text-decoration: line-through }
small { font-size: smaller }
big { font-size: larger }
sub { font-size: smaller; vertical-align: sub }
sup { font-size: smaller; vertical-align: super }
hr { margin: 0.5em auto; color: #a0a0a0 }
center { text-align: center }
mark { background-color: yellow; color: black }
fieldset { margin: 0 2px; padding: 0.35em 0.75em 0.625em }
legend { padding: 0 2px }
input, textarea, select, button { font-family: sans-serif; font-size: 13.333px }
button { display: inline-block; text-align: center }
input, textarea { border: 1px solid #767676; background-color: white; padding: 1px 2px; color: black }
input[type=checkbox], input[type=radio] { border: 0; padding: 0; background-color: transparent; margin: 3px 3px 3px 4px }
input[type=range], input[type=color], input[type=file], input[type=image] { border: 0; padding: 0; background-color: transparent }
select { border: 1px solid #767676; background-color: #e9e9ed; padding: 1px 2px; color: black; box-sizing: border-box }
button, input[type=submit], input[type=button], input[type=reset] { border: 1px solid #8f8f9d; border-radius: 4px; padding: 1px 6px; background-color: #e9e9ed; color: black; box-sizing: border-box }
summary { font-weight: bold }
"#;

/// Extra rules for quirks-mode documents (HTML Standard, rendering section):
/// tables don't inherit text styles from outside them.
const QUIRKS_CSS: &str = "table { font-weight: initial; font-style: initial; font-size: initial;
    line-height: initial; white-space: initial; text-align: initial }";

pub(crate) fn ua_sheet() -> Stylesheet {
    Stylesheet::parse(UA_CSS)
}

pub(crate) fn quirks_sheet() -> Stylesheet {
    Stylesheet::parse(QUIRKS_CSS)
}

/// Where a declaration came from, in increasing cascade precedence for
/// normal declarations. `!important` reverses author/UA order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    UaNormal,
    Hints,
    AuthorNormal,
    InlineNormal,
    AuthorImportant,
    InlineImportant,
    UaImportant,
}

/// One author stylesheet in cascade order, with its `media` attribute.
pub(crate) struct AuthorSheet<'a> {
    pub sheet: &'a Stylesheet,
    pub media: Option<&'a MediaList>,
}

/// Pseudo-elements one document may generate; later ones are dropped.
const MAX_PSEUDO_ELEMENTS: usize = 50_000;

/// Computed styles of one document at one viewport width.
pub(crate) struct Styles {
    /// One per DOM node (text nodes get their parent's, which is what
    /// layout uses for them), then one per generated pseudo-element. Layout
    /// refers to pseudo-elements by these indices, as if they were nodes.
    pub all: Vec<Style>,
    /// The `::before` and `::after` of elements that generate them.
    pub pseudo: BTreeMap<NodeId, [Option<usize>; 2]>,
}

/// Computes a style for every node and generated pseudo-element, for a
/// viewport `width` CSS px wide.
pub(crate) fn compute(dom: &Dom, ua: &Stylesheet, author: &[AuthorSheet<'_>], width: f32) -> Styles {
    let author: Vec<&Stylesheet> =
        author.iter().filter(|s| s.media.is_none_or(|m| m.matches(width))).map(|s| s.sheet).collect();
    static QUIRKS: std::sync::OnceLock<Stylesheet> = std::sync::OnceLock::new();
    let quirks = dom.quirks.then(|| QUIRKS.get_or_init(quirks_sheet));
    let mut styles: Vec<Style> = Vec::with_capacity(dom.nodes.len());
    let mut generated: Vec<(NodeId, PseudoElement, Style)> = Vec::new();
    let mut root_font_size = DEFAULT_FONT_SIZE;
    let mut matched: Vec<Matched<'_>> = Vec::new();
    let mut for_pseudo: Vec<Matched<'_>> = Vec::new();
    let tree = css::Tree::new(dom);
    let mut custom = vars::Custom::new(dom.nodes.len());
    let base = Some(&dom.base);
    // The arena is in document order, so a parent is always computed before
    // its children and ancestors can be gathered by walking parent links.
    for (id, node) in dom.nodes.iter().enumerate() {
        let parent_style = node.parent.and_then(|p| styles.get(p)).cloned();
        let style = match &node.data {
            NodeData::Element(el) => {
                let parent = parent_style.unwrap_or_else(Style::initial);
                let chain = ancestor_chain(dom, id);
                matched.clear();
                ua.matching(&tree, id, width, 0, &mut matched);
                if let Some(q) = quirks {
                    q.matching(&tree, id, width, 1 << 15, &mut matched);
                }
                let ua_count = matched.len();
                // Each sheet gets its own order range, so later sheets win ties.
                for (i, sheet) in author.iter().enumerate() {
                    sheet.matching(&tree, id, width, (i as u32 + 1).saturating_mul(1 << 16), &mut matched);
                }
                // Rules for the pseudo-elements go aside, in the same order.
                for_pseudo.clear();
                let mut own_ua = ua_count;
                if matched.iter().any(|m| m.pseudo.is_some()) {
                    own_ua = matched[..ua_count].iter().filter(|m| m.pseudo.is_none()).count();
                    for_pseudo.extend(matched.iter().filter(|m| m.pseudo.is_some()).copied());
                    matched.retain(|m| m.pseudo.is_none());
                }
                let hints = presentational_hints(el, &chain, base);
                let inline = el.attr("style").map(|s| css::parse_declarations(s, base)).unwrap_or_default();
                let s = cascade(&parent, &matched, own_ua, &hints, &inline, root_font_size, width, |decls| {
                    custom.apply(id, node.parent, decls)
                });
                if el.name == "html" && node.parent.is_some_and(|p| matches!(dom.nodes[p].data, NodeData::Root)) {
                    root_font_size = s.font_size;
                }
                for pe in [PseudoElement::Before, PseudoElement::After] {
                    if generated.len() >= MAX_PSEUDO_ELEMENTS || !for_pseudo.iter().any(|m| m.pseudo == Some(pe)) {
                        continue;
                    }
                    // UA rules come first in `for_pseudo`, as in `matched`.
                    let list: Vec<Matched<'_>> = for_pseudo.iter().filter(|m| m.pseudo == Some(pe)).copied().collect();
                    let ua_n = for_pseudo[..ua_count - own_ua].iter().filter(|m| m.pseudo == Some(pe)).count();
                    let ps = cascade(&s, &list, ua_n, &[], &[], root_font_size, width, |decls| {
                        // Never stored: nothing inherits from a pseudo-element's
                        // custom properties.
                        custom.apply(usize::MAX, Some(id), decls)
                    });
                    if ps.content.is_some() && ps.display != Display::None {
                        generated.push((id, pe, ps));
                    }
                }
                s
            }
            _ => parent_style.unwrap_or_else(Style::initial),
        };
        styles.push(style);
    }
    let mut pseudo: BTreeMap<NodeId, [Option<usize>; 2]> = BTreeMap::new();
    for (node, pe, st) in generated {
        pseudo.entry(node).or_default()[pe as usize] = Some(styles.len());
        styles.push(st);
    }
    Styles { all: styles, pseudo }
}

fn ancestor_chain(dom: &Dom, id: NodeId) -> Vec<&Element> {
    let mut chain = Vec::new();
    let mut cur = Some(id);
    while let Some(n) = cur {
        if let NodeData::Element(el) = &dom.nodes[n].data {
            chain.push(el);
        }
        cur = dom.nodes[n].parent;
    }
    chain
}

/// `substitute` gets the declarations in cascade order and returns them with
/// custom properties cascaded and `var()` replaced (see `vars`).
#[allow(clippy::too_many_arguments)]
fn cascade<'a>(
    parent: &Style,
    matched: &[Matched<'a>],
    ua_count: usize,
    hints: &'a [Decl],
    inline: &'a [Declaration],
    root_font_size: f32,
    viewport: f32,
    substitute: impl FnOnce(&[&'a Decl]) -> Vec<Cow<'a, Decl>>,
) -> Style {
    let mut list: Vec<(Level, u32, u32, u32, &Decl)> = Vec::new();
    for (i, m) in matched.iter().enumerate() {
        let ua = i < ua_count;
        for (j, d) in m.decls.iter().enumerate() {
            let level = match (ua, d.important) {
                (true, false) => Level::UaNormal,
                (true, true) => Level::UaImportant,
                (false, false) => Level::AuthorNormal,
                (false, true) => Level::AuthorImportant,
            };
            list.push((level, m.specificity, m.order, j as u32, &d.decl));
        }
    }
    for (j, d) in hints.iter().enumerate() {
        list.push((Level::Hints, 0, 0, j as u32, d));
    }
    for (j, d) in inline.iter().enumerate() {
        let level = if d.important { Level::InlineImportant } else { Level::InlineNormal };
        list.push((level, 0, 0, j as u32, &d.decl));
    }
    // Stable and total: later entries win, exactly as CSS source order says.
    list.sort_by_key(|&(level, spec, order, j, _)| (level, spec, order, j));
    let sorted: Vec<&Decl> = list.iter().map(|&(_, _, _, _, d)| d).collect();
    let decls = substitute(&sorted);

    let mut s = Style::inherit_from(parent);
    // Font size first: em units in every other property refer to it.
    for d in decls.iter().map(|d| &**d) {
        match d {
            Decl::FontSize(fs) => s.font_size = font_size(*fs, parent.font_size, root_font_size, viewport),
            Decl::Wide(Prop::FontSize, w) => {
                s.font_size = if *w == Wide::Initial { DEFAULT_FONT_SIZE } else { parent.font_size }
            }
            _ => {}
        }
    }
    let em = s.font_size;
    let len = |l: Length| -> Len {
        let clamp = |v: f32| if v.is_nan() { 0.0 } else { v.clamp(-MAX_EXTENT, MAX_EXTENT) };
        let calc = |c: Calc| calc_px(c, em, root_font_size, viewport);
        match l {
            Length::Px(v) => Len::Px(clamp(v)),
            Length::Em(v) => Len::Px(clamp(v * em)),
            Length::Rem(v) => Len::Px(clamp(v * root_font_size)),
            Length::Pct(p) => Len::Pct(p.clamp(-1000.0, 1000.0)),
            Length::Calc(c) if c.pct == 0.0 => Len::Px(clamp(calc(c))),
            Length::Calc(c) => Len::Calc(clamp(calc(c)), c.pct.clamp(-1000.0, 1000.0)),
            Length::Clamp(lo, v, hi) => Len::Px(clamp(calc(v).min(calc(hi)).max(calc(lo)))),
            Length::Auto => Len::Auto,
            Length::MinContent => Len::MinContent,
            Length::MaxContent => Len::MaxContent,
            Length::FitContent => Len::FitContent,
        }
    };
    // Computed lengths that stay `Length`s (backgrounds, gradients): only
    // px, percentages and their sums are left.
    let computed = |l: Length| -> Length {
        match len(l) {
            Len::Px(v) => Length::Px(v),
            Len::Pct(p) => Length::Pct(p),
            Len::Calc(px, pct) => Length::Calc(Calc { px, pct, ..Calc::default() }),
            _ => Length::Auto,
        }
    };
    let mut spec = Specified {
        underline: false,
        line_through: false,
        borders: BorderSpec { width: [3.0; 4], styled: [false; 4], color: [ColorValue::Current; 4] },
        bg: BgSpec::default(),
    };
    for d in decls.iter().map(|d| &**d) {
        match *d {
            // Already handled by `substitute`.
            Decl::Custom(_) | Decl::Var(_) => {}
            Decl::Display(v) => s.display = v,
            Decl::BgImage(ref l) => spec.bg.image = Some(l.clone()),
            Decl::BgRepeat(ref l) => spec.bg.repeat = Some(l.clone()),
            Decl::BgPosition(ref l) => spec.bg.position = Some(l.clone()),
            Decl::BgSize(ref l) => spec.bg.size = Some(l.clone()),
            Decl::BgOrigin(ref l) => spec.bg.origin = Some(l.clone()),
            Decl::BgClip(ref l) => spec.bg.clip = Some(l.clone()),
            Decl::BgFixed(ref l) => spec.bg.fixed = Some(l.clone()),
            Decl::Content(ref c) => s.content = c.clone(),
            Decl::Counters(op, ref l) => s.counters[op as usize] = (!l.is_empty()).then(|| l.clone()),
            Decl::Quotes(ref q) => s.quotes = q.clone(),
            Decl::BorderCollapse(v) => s.border_collapse = v,
            Decl::TableFixed(v) => s.table_fixed = v,
            Decl::CaptionBottom(v) => s.caption_bottom = v,
            Decl::Color(c) => s.color = resolve_color(c, parent.color),
            Decl::Background(c) => {
                let c = resolve_color(c, s.color);
                s.background = (c.a > 0).then_some(c);
            }
            Decl::FontSize(_) => {}
            Decl::FontWeight(w) => {
                s.weight = match w {
                    FontWeight::Absolute(v) => v,
                    FontWeight::Bolder => {
                        if parent.weight < 350 {
                            400
                        } else if parent.weight < 550 {
                            700
                        } else {
                            900
                        }
                    }
                    FontWeight::Lighter => {
                        if parent.weight < 550 {
                            100
                        } else if parent.weight < 750 {
                            400
                        } else {
                            700
                        }
                    }
                }
            }
            Decl::Italic(v) => s.italic = v,
            Decl::FontFamily(f) => s.family = f,
            Decl::TextAlign(a) => s.text_align = a,
            Decl::Decoration { underline: u, line_through: l } => {
                spec.underline = u;
                spec.line_through = l;
            }
            Decl::LineHeight(lh) => {
                s.line_height = match lh {
                    LineHeightValue::Normal => LineHeight::Normal,
                    LineHeightValue::Number(n) => LineHeight::Factor(n),
                    LineHeightValue::Length(Length::Pct(p)) => LineHeight::Px(em * p / 100.0),
                    LineHeightValue::Length(l) => LineHeight::Px(len(l).resolve(0.0).clamp(0.0, MAX_EXTENT)),
                }
            }
            Decl::WhiteSpace(w) => s.white_space = w,
            Decl::ListStyle(l) => s.list_style = l,
            Decl::Visible(v) => s.visible = v,
            Decl::Transparent(t) => s.transparent = t,
            Decl::TextTransform(t) => s.text_transform = t,
            Decl::Margin(side, l) => s.margin[side & 3] = len(l),
            Decl::Padding(side, l) => s.padding[side & 3] = len(l),
            Decl::Width(l) => s.width = len(l),
            Decl::MaxWidth(l) => s.max_width = l.map(len),
            Decl::VerticalAlign(v) => s.vertical_align = v,
            Decl::BorderBox(v) => s.border_box = v,
            Decl::BorderSpacing(h, v) => s.border_spacing = [h, v].map(|l| len(l).resolve(0.0).clamp(0.0, 1000.0)),
            Decl::Position(p) => s.position = p,
            Decl::Inset(i, l) => s.inset[i & 3] = len(l),
            Decl::ZIndex(z) => s.z_index = z,
            Decl::Float(f) => s.float = f,
            Decl::Clear(c) => s.clear = c,
            Decl::Overflow(axis, v) => s.overflow[axis & 1] = v,
            Decl::Height(l) => s.height = len(l),
            Decl::MinHeight(l) => s.min_height = len(l),
            Decl::MaxHeight(l) => s.max_height = l.map(len),
            Decl::MinWidth(l) => s.min_width = len(l),
            Decl::BorderWidth(i, l) => spec.borders.width[i & 3] = len(l).resolve(0.0).clamp(0.0, 1000.0),
            Decl::BorderStyle(i, v) => spec.borders.styled[i & 3] = v,
            Decl::BorderColor(i, c) => spec.borders.color[i & 3] = c,
            Decl::BorderRadius(l) => s.radius = len(l),
            Decl::FlexDirection(d) => s.flex_direction = d,
            Decl::FlexWrap(w) => s.flex_wrap = w,
            Decl::JustifyContent(j) => s.justify_content = j,
            Decl::AlignItems(a) => s.align_items = a,
            Decl::AlignSelf(a) => s.align_self = a,
            Decl::AlignContent(a) => s.align_content = a,
            Decl::FlexGrow(g) => s.flex_grow = g,
            Decl::FlexShrink(g) => s.flex_shrink = g,
            Decl::FlexBasis(l) => s.flex_basis = len(l),
            Decl::Order(o) => s.order = o,
            Decl::Gap(axis, l) => s.gap[axis & 1] = len(l),
            Decl::Masked(m) => s.masked = m,
            Decl::Wide(p, w) => apply_wide(&mut s, parent, p, w, &mut spec),
        }
    }
    s.underline = parent.underline || spec.underline;
    s.line_through = parent.line_through || spec.line_through;
    let borders = &spec.borders;
    for i in 0..4 {
        // Browsers draw borders in whole pixels, and never thinner than one.
        let w = borders.width[i];
        s.border_width[i] = if !borders.styled[i] || w <= 0.0 { 0.0 } else { w.floor().max(1.0) };
        s.border_color[i] = resolve_color(borders.color[i], s.color);
    }
    let (layers, color_clip) = spec.bg.layers(computed);
    s.bg_color_clip = color_clip;
    s.bg_layers = if spec.bg.inherit { parent.bg_layers.clone() } else { layers };
    // Floats and absolutely positioned boxes are always block-level.
    if s.out_of_flow() {
        s.float = Float::None;
    }
    if s.out_of_flow() || s.float != Float::None {
        s.display = s.display.blockified();
    }
    // Nothing inside a fully transparent element shows, whatever its own
    // `visibility` says.
    if s.transparent || s.in_transparent {
        s.visible = false;
    }
    s
}

/// Properties that are only computed once the whole cascade is known.
struct Specified {
    underline: bool,
    line_through: bool,
    borders: BorderSpec,
    bg: BgSpec,
}

/// Border properties as specified; the used widths depend on the style.
struct BorderSpec {
    width: [f32; 4],
    styled: [bool; 4],
    color: [ColorValue; 4],
}

/// The background longhands as specified; `None` is the initial value.
#[derive(Default)]
struct BgSpec {
    image: Option<Arc<[Option<Image>]>>,
    repeat: Option<Arc<[[Repeat; 2]]>>,
    position: Option<Arc<[[Length; 2]]>>,
    size: Option<Arc<[BgSize]>>,
    origin: Option<Arc<[BoxArea]>>,
    clip: Option<Arc<[BoxArea]>>,
    fixed: Option<Arc<[bool]>>,
    /// A longhand said `inherit`: take the parent's layers whole.
    inherit: bool,
}

impl BgSpec {
    /// The image layers (lists shorter than `background-image` repeat, as
    /// CSS says) and the box the color is clipped to.
    fn layers(&self, computed: impl Fn(Length) -> Length) -> (Option<Arc<[BgLayer]>>, BoxArea) {
        fn pick<T: Copy>(list: &Option<Arc<[T]>>, i: usize, initial: T) -> T {
            list.as_ref().filter(|l| !l.is_empty()).map_or(initial, |l| l[i % l.len()])
        }
        let n = self.image.as_ref().map_or(1, |l| l.len().max(1));
        let color_clip = pick(&self.clip, n - 1, BoxArea::Border);
        let Some(images) = &self.image else { return (None, color_clip) };
        let layers: Vec<BgLayer> = images
            .iter()
            .enumerate()
            .filter_map(|(i, image)| {
                let image = match image.as_ref()? {
                    Image::Url(u) => Image::Url(u.clone()),
                    Image::Gradient(g) => Image::Gradient(Arc::new(g.map_lengths(&computed))),
                };
                let size = match pick(&self.size, i, BgSize::Size(Length::Auto, Length::Auto)) {
                    BgSize::Size(w, h) => BgSize::Size(computed(w), computed(h)),
                    s => s,
                };
                Some(BgLayer {
                    image,
                    repeat: pick(&self.repeat, i, [Repeat::Repeat; 2]),
                    position: pick(&self.position, i, [Length::Pct(0.0); 2]).map(&computed),
                    size,
                    origin: pick(&self.origin, i, BoxArea::Padding),
                    clip: pick(&self.clip, i, BoxArea::Border),
                    fixed: pick(&self.fixed, i, false),
                })
            })
            .collect();
        ((!layers.is_empty()).then(|| layers.into()), color_clip)
    }
}

fn calc_px(c: Calc, em: f32, root_font_size: f32, viewport: f32) -> f32 {
    c.px + c.em * em + c.rem * root_font_size + c.vw * viewport / 100.0
}

fn apply_wide(s: &mut Style, parent: &Style, p: Prop, w: Wide, spec: &mut Specified) {
    let inherit = match w {
        Wide::Inherit => true,
        Wide::Initial => false,
        Wide::Unset => p.inherited(),
    };
    let src = if inherit { parent.clone() } else { Style::initial() };
    let borders = &mut spec.borders;
    let bg = &mut spec.bg;
    if matches!(
        p,
        Prop::BgImage
            | Prop::BgRepeat
            | Prop::BgPosition
            | Prop::BgSize
            | Prop::BgOrigin
            | Prop::BgClip
            | Prop::BgAttachment
    ) {
        bg.inherit = inherit;
    }
    match p {
        Prop::Display => s.display = src.display,
        Prop::Color => s.color = src.color,
        Prop::Background => s.background = src.background,
        Prop::BgImage => bg.image = None,
        Prop::BgRepeat => bg.repeat = None,
        Prop::BgPosition => bg.position = None,
        Prop::BgSize => bg.size = None,
        Prop::BgOrigin => bg.origin = None,
        Prop::BgClip => bg.clip = None,
        Prop::BgAttachment => bg.fixed = None,
        Prop::Content => s.content = src.content,
        Prop::Counters(op) => s.counters[op as usize] = src.counters[op as usize].clone(),
        Prop::Quotes => s.quotes = src.quotes,
        Prop::BorderCollapse => s.border_collapse = src.border_collapse,
        Prop::TableLayout => s.table_fixed = src.table_fixed,
        Prop::CaptionSide => s.caption_bottom = src.caption_bottom,
        Prop::FontSize => {}
        Prop::FontWeight => s.weight = src.weight,
        Prop::FontStyle => s.italic = src.italic,
        Prop::FontFamily => s.family = src.family,
        Prop::TextAlign => s.text_align = src.text_align,
        Prop::TextDecoration => {
            spec.underline = false;
            spec.line_through = false;
        }
        Prop::LineHeight => s.line_height = src.line_height,
        Prop::WhiteSpace => s.white_space = src.white_space,
        Prop::ListStyle => s.list_style = src.list_style,
        Prop::Visibility => s.visible = src.visible,
        Prop::Opacity => s.transparent = src.transparent,
        Prop::TextTransform => s.text_transform = src.text_transform,
        Prop::Margin(i) => s.margin[i & 3] = src.margin[i & 3],
        Prop::Padding(i) => s.padding[i & 3] = src.padding[i & 3],
        Prop::Width => s.width = src.width,
        Prop::MaxWidth => s.max_width = src.max_width,
        Prop::VerticalAlign => s.vertical_align = src.vertical_align,
        Prop::BoxSizing => s.border_box = src.border_box,
        Prop::BorderSpacing => s.border_spacing = src.border_spacing,
        Prop::Position => s.position = src.position,
        Prop::Inset(i) => s.inset[i & 3] = src.inset[i & 3],
        Prop::ZIndex => s.z_index = src.z_index,
        Prop::Float => s.float = src.float,
        Prop::Clear => s.clear = src.clear,
        Prop::Overflow(axis) => s.overflow[axis & 1] = src.overflow[axis & 1],
        Prop::Height => s.height = src.height,
        Prop::MinHeight => s.min_height = src.min_height,
        Prop::MaxHeight => s.max_height = src.max_height,
        Prop::MinWidth => s.min_width = src.min_width,
        Prop::BorderWidth(i) => borders.width[i & 3] = if inherit { src.border_width[i & 3] } else { 3.0 },
        Prop::BorderStyle(i) => borders.styled[i & 3] = inherit && src.border_width[i & 3] > 0.0,
        Prop::BorderColor(i) => {
            borders.color[i & 3] = if inherit { ColorValue::Rgba(src.border_color[i & 3]) } else { ColorValue::Current }
        }
        Prop::BorderRadius => s.radius = src.radius,
        Prop::FlexDirection => s.flex_direction = src.flex_direction,
        Prop::FlexWrap => s.flex_wrap = src.flex_wrap,
        Prop::JustifyContent => s.justify_content = src.justify_content,
        Prop::AlignItems => s.align_items = src.align_items,
        Prop::AlignSelf => s.align_self = src.align_self,
        Prop::AlignContent => s.align_content = src.align_content,
        Prop::FlexGrow => s.flex_grow = src.flex_grow,
        Prop::FlexShrink => s.flex_shrink = src.flex_shrink,
        Prop::FlexBasis => s.flex_basis = src.flex_basis,
        Prop::Order => s.order = src.order,
        Prop::Gap(axis) => s.gap[axis & 1] = src.gap[axis & 1],
        Prop::Mask => s.masked = src.masked,
    }
}

fn resolve_color(c: ColorValue, current: Color) -> Color {
    match c {
        ColorValue::Rgba(c) => c,
        ColorValue::Current => current,
    }
}

fn font_size(fs: FontSize, parent: f32, root: f32, viewport: f32) -> f32 {
    let calc = |c: Calc| calc_px(c, parent, root, viewport) + c.pct * parent / 100.0;
    let px = match fs {
        FontSize::Px(v) => v,
        FontSize::Larger => parent * 1.2,
        FontSize::Smaller => parent / 1.2,
        FontSize::Length(Length::Px(v)) => v,
        FontSize::Length(Length::Em(v)) => parent * v,
        FontSize::Length(Length::Rem(v)) => root * v,
        FontSize::Length(Length::Pct(p)) => parent * p / 100.0,
        FontSize::Length(Length::Calc(c)) => calc(c),
        FontSize::Length(Length::Clamp(lo, v, hi)) => calc(v).min(calc(hi)).max(calc(lo)),
        FontSize::Length(Length::Auto | Length::MinContent | Length::MaxContent | Length::FitContent) => parent,
    };
    let px = if px.is_nan() { parent } else { px };
    px.clamp(0.0, MAX_FONT_SIZE)
}

/// Legacy presentational attributes (`bgcolor`, `width`, `align`, `<font>`,
/// ...). Still common on old and minimalist sites; they sit just below
/// author CSS in the cascade.
fn presentational_hints(el: &Element, chain: &[&Element], base: Base<'_>) -> Vec<Decl> {
    let mut out = Vec::new();
    let name = el.name.as_str();
    let color = |v: &str| css::parse_color(&v.trim().to_ascii_lowercase()).or_else(|| legacy_color(v));
    if let Some(c) = el.attr("bgcolor").and_then(color) {
        out.push(Decl::Background(c));
    }
    if matches!(name, "body" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th")
        && let Some(url) = el.attr("background").and_then(|v| css::resolve_url(v, base))
    {
        out.push(Decl::BgImage(Arc::from([Some(Image::Url(url))])));
    }
    if name == "body"
        && let Some(c) = el.attr("text").and_then(color)
    {
        out.push(Decl::Color(c));
    }
    if name == "font" {
        if let Some(c) = el.attr("color").and_then(color) {
            out.push(Decl::Color(c));
        }
        if let Some(face) = el.attr("face") {
            out.push(Decl::FontFamily(css::parse_family(&face.to_ascii_lowercase())));
        }
        if let Some(size) = el.attr("size").and_then(legacy_font_size) {
            out.push(Decl::FontSize(FontSize::Px(size)));
        }
    }
    if matches!(name, "table" | "td" | "th" | "img" | "hr" | "col" | "iframe" | "embed" | "object" | "video")
        && let Some(w) = el.attr("width").and_then(dimension)
    {
        out.push(Decl::Width(w));
    }
    if let Some(align) = el.attr("align").map(|a| a.trim().to_ascii_lowercase()) {
        match (name, align.as_str()) {
            ("table", "center") => {
                out.push(Decl::Margin(1, Length::Auto));
                out.push(Decl::Margin(3, Length::Auto));
            }
            ("img" | "table" | "iframe" | "object" | "embed", _) => {}
            (_, "center" | "middle") => out.push(Decl::TextAlign(TextAlign::Center)),
            (_, "right") => out.push(Decl::TextAlign(TextAlign::Right)),
            (_, "left" | "justify") => out.push(Decl::TextAlign(TextAlign::Left)),
            _ => {}
        }
    }
    if matches!(name, "td" | "th" | "tr" | "tbody" | "thead" | "tfoot")
        && let Some(v) = el.attr("valign").map(|a| a.trim().to_ascii_lowercase())
    {
        let va = match v.as_str() {
            "top" => Some(VAlign::Top),
            "middle" | "center" => Some(VAlign::Middle),
            "bottom" => Some(VAlign::Bottom),
            "baseline" => Some(VAlign::Baseline),
            _ => None,
        };
        out.extend(va.map(Decl::VerticalAlign));
    }
    if name == "table"
        && let Some(sp) = el.attr("cellspacing").and_then(|v| v.trim().parse::<f32>().ok()).filter(|v| v.is_finite())
    {
        let sp = Length::Px(sp.clamp(0.0, 1000.0));
        out.push(Decl::BorderSpacing(sp, sp));
    }
    if matches!(name, "td" | "th")
        && let Some(table) = chain.iter().skip(1).find(|e| e.name == "table")
        && let Some(pad) =
            table.attr("cellpadding").and_then(|v| v.trim().parse::<f32>().ok()).filter(|v| v.is_finite())
    {
        for side in 0..4 {
            out.push(Decl::Padding(side, Length::Px(pad.clamp(0.0, 1000.0))));
        }
    }
    if matches!(name, "td" | "th") && el.attr("nowrap").is_some() {
        out.push(Decl::WhiteSpace(WhiteSpace::NoWrap));
    }
    if matches!(name, "ol" | "ul" | "li")
        && let Some(t) = el.attr("type")
    {
        let ls = match t.trim() {
            "1" => Some(ListStyle::Decimal),
            "a" => Some(ListStyle::LowerAlpha),
            "A" => Some(ListStyle::UpperAlpha),
            "i" => Some(ListStyle::LowerRoman),
            "I" => Some(ListStyle::UpperRoman),
            t if t.eq_ignore_ascii_case("disc") => Some(ListStyle::Disc),
            t if t.eq_ignore_ascii_case("circle") => Some(ListStyle::Circle),
            t if t.eq_ignore_ascii_case("square") => Some(ListStyle::Square),
            _ => None,
        };
        out.extend(ls.map(Decl::ListStyle));
    }
    out
}

/// HTML dimension attribute: `100`, `100px` or `50%`.
fn dimension(v: &str) -> Option<Length> {
    let v = v.trim();
    let num_end = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
    let n: f32 = v[..num_end].parse().ok().filter(|n: &f32| n.is_finite())?;
    if v[num_end..].trim_start().starts_with('%') {
        Some(Length::Pct(n.min(1000.0)))
    } else {
        Some(Length::Px(n.min(MAX_EXTENT)))
    }
}

pub(crate) fn dimension_px(v: &str) -> Option<f32> {
    match dimension(v)? {
        Length::Px(px) => Some(px),
        _ => None,
    }
}

/// `<font color=ff0000>`: hex without the `#`.
fn legacy_color(v: &str) -> Option<ColorValue> {
    let v = v.trim();
    (v.len() == 6 && v.bytes().all(|b| b.is_ascii_hexdigit())).then(|| css::parse_color(&format!("#{v}"))).flatten()
}

fn legacy_font_size(v: &str) -> Option<f32> {
    let v = v.trim();
    let (rel, digits) = match v.as_bytes().first() {
        Some(b'+') => (1, &v[1..]),
        Some(b'-') => (-1, &v[1..]),
        _ => (0, v),
    };
    let n: i32 = digits.parse().ok()?;
    let size = if rel == 0 { n } else { 3 + rel * n };
    Some(match size.clamp(1, 7) {
        1 => 10.0,
        2 => 13.0,
        3 => 16.0,
        4 => 18.0,
        5 => 24.0,
        6 => 32.0,
        _ => 48.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::Dom;

    fn styles_for(html: &str) -> (Dom, Vec<Style>) {
        let base = url::Url::parse("https://example.org/").unwrap();
        let parsed = crate::dom::parse(html, &base);
        let sheets: Vec<Stylesheet> = parsed
            .sheets
            .iter()
            .filter_map(|s| match s {
                crate::dom::SheetSource::Inline { css, .. } => Some(Stylesheet::parse(css)),
                _ => None,
            })
            .collect();
        let author: Vec<AuthorSheet<'_>> = sheets.iter().map(|sheet| AuthorSheet { sheet, media: None }).collect();
        let styles = compute(&parsed.dom, &ua_sheet(), &author, 1024.0);
        (parsed.dom, styles.all)
    }

    fn style_of<'a>(dom: &Dom, styles: &'a [Style], id: &str) -> &'a Style {
        let idx = dom
            .nodes
            .iter()
            .position(|n| matches!(&n.data, NodeData::Element(e) if e.id.as_deref() == Some(id)))
            .expect("element");
        &styles[idx]
    }

    #[test]
    fn specificity_and_source_order() {
        let (dom, st) = styles_for(
            "<style>p { color: red } .a { color: green } p { color: blue } #x { color: yellow }
             .b.c { font-size: 20px } .c { font-size: 30px } div p { font-weight: 300 } p { font-weight: 800 }</style>
             <div><p id=x class=a>1</p><p id=y class='a b c'>2</p></div>",
        );
        assert_eq!(style_of(&dom, &st, "x").color, Color::rgb(255, 255, 0));
        let y = style_of(&dom, &st, "y");
        assert_eq!(y.color, Color::rgb(0, 128, 0));
        assert_eq!(y.font_size, 20.0);
        assert_eq!(y.weight, 300);
    }

    #[test]
    fn inline_and_important() {
        let (dom, st) = styles_for(
            "<style>#a { color: red !important } #b { color: red }</style>
             <p id=a style='color: blue'>a</p><p id=b style='color: blue'>b</p>
             <p id=c style='color: blue !important; color: green'>c</p>",
        );
        assert_eq!(style_of(&dom, &st, "a").color, Color::rgb(255, 0, 0));
        assert_eq!(style_of(&dom, &st, "b").color, Color::rgb(0, 0, 255));
        assert_eq!(style_of(&dom, &st, "c").color, Color::rgb(0, 0, 255));
    }

    #[test]
    fn inheritance_and_em() {
        let (dom, st) = styles_for(
            "<style>body { font-size: 10px; color: #123456 } div { font-size: 2em; margin: 1em; padding: 50% }
             span { font-size: 150% } .r { font-size: 2rem }</style>
             <body><div id=d><span id=s>x</span><span id=r class=r>y</span></div></body>",
        );
        let d = style_of(&dom, &st, "d");
        assert_eq!(d.font_size, 20.0);
        assert_eq!(d.margin[0], Len::Px(20.0));
        assert_eq!(d.padding[3], Len::Pct(50.0));
        let s = style_of(&dom, &st, "s");
        assert_eq!(s.font_size, 30.0);
        assert_eq!(s.color, Color::rgb(0x12, 0x34, 0x56));
        assert_eq!(s.margin[0], Len::Px(0.0), "margin is not inherited");
        assert_eq!(style_of(&dom, &st, "r").font_size, 32.0, "rem is relative to the root (16px)");
    }

    #[test]
    fn ua_defaults() {
        let (dom, st) = styles_for("<h1 id=h>t</h1><a id=a href=/x>l</a><a id=n>n</a><pre id=p>x</pre>");
        let h = style_of(&dom, &st, "h");
        assert_eq!(h.font_size, 32.0);
        assert!(h.bold());
        assert_eq!(h.display, Display::Block);
        let a = style_of(&dom, &st, "a");
        assert!(a.underline);
        assert_eq!(a.color, Color::rgb(0, 0, 0xee));
        assert!(!style_of(&dom, &st, "n").underline, "an <a> without href is not a link");
        let p = style_of(&dom, &st, "p");
        assert_eq!(p.white_space, WhiteSpace::Pre);
        assert_eq!(p.family, Family::Mono);
    }

    #[test]
    fn decoration_propagates() {
        let (dom, st) = styles_for("<a href=/ ><span id=s style='text-decoration:none'>x</span></a>");
        assert!(style_of(&dom, &st, "s").underline);
    }

    #[test]
    fn presentational_attributes() {
        let (dom, st) = styles_for(
            "<table cellpadding=5 bgcolor=#ff0000 id=t><tr><td id=c align=right>x</td></tr></table>
             <font id=f color=00ff00 size=5>y</font>",
        );
        assert_eq!(style_of(&dom, &st, "t").background, Some(Color::rgb(255, 0, 0)));
        let c = style_of(&dom, &st, "c");
        assert_eq!(c.padding[0], Len::Px(5.0));
        assert_eq!(c.text_align, TextAlign::Right);
        let f = style_of(&dom, &st, "f");
        assert_eq!(f.color, Color::rgb(0, 255, 0));
        assert_eq!(f.font_size, 24.0);
    }

    #[test]
    fn sibling_and_structural_selectors_cascade() {
        let (dom, st) = styles_for(
            "<style>.menu { display: none } .toggle:checked ~ .menu { display: block }
             li:nth-child(odd) { font-weight: 700 } li:not(:first-child) + li { font-size: 20px }</style>
             <div><input type=checkbox class=toggle checked><div id=open class=menu>m</div></div>
             <div><input type=checkbox class=toggle><div id=shut class=menu>m</div></div>
             <ul><li id=a>1</li><li id=b>2</li><li id=c>3</li></ul>",
        );
        assert_eq!(style_of(&dom, &st, "open").display, Display::Block);
        assert_eq!(style_of(&dom, &st, "shut").display, Display::None);
        assert!(style_of(&dom, &st, "a").bold());
        assert!(!style_of(&dom, &st, "b").bold());
        assert_eq!(style_of(&dom, &st, "b").font_size, 16.0);
        assert_eq!(style_of(&dom, &st, "c").font_size, 20.0);
    }

    #[test]
    fn custom_properties_cascade_and_inherit() {
        let (dom, st) = styles_for(
            "<style>:root { --fg: #00f; --size: 20px; --gap: 3px; --fam: Georgia, serif }
             .a { --fg: #f00 }
             .b { --fg: #0f0 !important }
             p { color: var(--fg); font-size: var(--size); margin: var(--gap) 0 var(--gap, 9px) }
             .c { --size: 2em; --gap: var(--size) }
             .d { font: italic var(--size)/1.5 var(--fam) }
             </style>
             <body><p id=p>x</p><div class=a><p id=q>y</p><p id=r class=b style='--fg: #fff'>z</p></div>
             <p id=s class=c>w</p><span id=t class=d>v</span><span id=u style='--Fg: red; color: var(--Fg)'>u</span></body>",
        );
        let p = style_of(&dom, &st, "p");
        assert_eq!(p.color, Color::rgb(0, 0, 255));
        assert_eq!(p.font_size, 20.0);
        assert_eq!(p.margin, [Len::Px(3.0), Len::Px(0.0), Len::Px(3.0), Len::Px(0.0)]);
        assert_eq!(style_of(&dom, &st, "q").color, Color::rgb(255, 0, 0), "inherited from .a");
        assert_eq!(style_of(&dom, &st, "r").color, Color::rgb(0, 255, 0), "!important beats inline");
        // Custom properties are text: `2em` is resolved where it is used.
        let s = style_of(&dom, &st, "s");
        assert_eq!(s.font_size, 32.0);
        assert_eq!(s.margin[0], Len::Px(64.0));
        let t = style_of(&dom, &st, "t");
        assert!(t.italic);
        assert_eq!(t.font_size, 20.0);
        assert_eq!(t.family, Family::Serif);
        assert_eq!(t.line_height, LineHeight::Factor(1.5));
        assert_eq!(style_of(&dom, &st, "u").color, Color::rgb(255, 0, 0), "names are case-sensitive");
    }

    #[test]
    fn var_fallbacks_and_invalid_values() {
        let (dom, st) = styles_for(
            "<style>body { color: #123456 }
             #a { color: var(--missing, var(--also-missing, #0f0)) }
             #b { color: red; color: var(--missing) }
             #c { --bad: banana; margin-left: 5px; margin-left: var(--bad) }
             #d { --e: ; margin: 1px var(--e) 2px }
             #f { font-size: VAR( --nope , 30px ) }
             #g { --a: var(--b); --b: var(--a); --c: var(--a, 7px);
                  margin-top: var(--a, 8px); margin-bottom: var(--c, 9px); margin-right: var(--b, 1px) }
             #h { --self: var(--self, 1px); margin-left: var(--self, 6px) }
             #i { --k: initial; margin-left: var(--k, 1px) }
             #j { --k: inherit; margin-left: var(--k, 1px) }
             #k { margin-left: var(--k) var(--k }
             </style>
             <body><p id=a>a</p><p id=b>b</p><p id=c>c</p><p id=d>d</p><p id=f>f</p><p id=g>g</p>
             <p id=h>h</p><div style='--k: 11px'><p id=i>i</p><p id=j>j</p><p id=k>k</p></div></body>",
        );
        assert_eq!(style_of(&dom, &st, "a").color, Color::rgb(0, 255, 0));
        // Invalid at computed-value time: `unset`, so color inherits.
        assert_eq!(style_of(&dom, &st, "b").color, Color::rgb(0x12, 0x34, 0x56));
        assert_eq!(style_of(&dom, &st, "c").margin[3], Len::Px(0.0));
        assert_eq!(style_of(&dom, &st, "d").margin[1], Len::Px(2.0));
        assert_eq!(style_of(&dom, &st, "f").font_size, 30.0);
        // A cycle makes its members invalid, fallbacks or not; a property
        // that merely refers to one falls back.
        let g = style_of(&dom, &st, "g");
        assert_eq!(g.margin[0], Len::Px(8.0));
        assert_eq!(g.margin[1], Len::Px(1.0));
        assert_eq!(g.margin[2], Len::Px(7.0));
        assert_eq!(style_of(&dom, &st, "h").margin[3], Len::Px(6.0));
        assert_eq!(style_of(&dom, &st, "i").margin[3], Len::Px(1.0));
        assert_eq!(style_of(&dom, &st, "j").margin[3], Len::Px(11.0));
        assert_eq!(style_of(&dom, &st, "k").margin[3], Len::Px(0.0), "unclosed var() is invalid");
    }

    #[test]
    fn hostile_vars_are_bounded() {
        // "Billion laughs": every property doubles the previous one. The
        // 64 KiB value cap makes the long ones invalid.
        let mut css = String::from(":root { --l0: xxxxxxxxxxxxxxxx;");
        for i in 1..60 {
            css += &format!(" --l{i}: var(--l{p}) var(--l{p});", p = i - 1);
        }
        css += " } p { margin-left: var(--l59, 3px); margin-top: var(--l8, 4px) }";
        // Deeply nested fallbacks.
        let nested = "var(--n, ".repeat(10_000) + "5px" + &")".repeat(10_000);
        css += &format!(" #n {{ margin-left: {nested} }}");
        // Hundreds of properties on every element of a deep tree, each
        // re-declared and chained.
        css += " * {";
        for i in 0..250 {
            css += &format!(" --p{i}: var(--p{}, {i}px);", (i + 1) % 250);
        }
        css += " } #deep { margin-left: var(--p3) }";
        let html = format!(
            "<style>{css}</style><p id=p>x</p><p id=n>n</p>{}<i id=deep>d</i>",
            "<div>".repeat(120) + &"<b>b</b>".repeat(3000)
        );
        let (dom, st) = styles_for(&html);
        let p = style_of(&dom, &st, "p");
        assert_eq!(p.margin[3], Len::Px(3.0));
        // A 4 KiB run of x's is no margin: invalid, so unset.
        assert_eq!(p.margin[0], Len::Px(0.0));
        assert_eq!(style_of(&dom, &st, "n").margin[3], Len::Px(0.0));
        // The 250-property cycle is invalid everywhere, whatever survives
        // the budgets.
        assert_eq!(style_of(&dom, &st, "deep").margin[3], Len::Px(0.0));
    }
}
