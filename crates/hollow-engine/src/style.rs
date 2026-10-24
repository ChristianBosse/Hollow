//! Computed styles and the cascade.
//!
//! Styles are computed per layout, because `@media` rules depend on the
//! viewport width. Percentages stay unresolved until layout knows the
//! containing block.

use crate::css::{
    self, Color, ColorValue, Decl, Declaration, FontSize, FontWeight, Length, LineHeightValue, Matched, MediaList,
    Prop, Stylesheet, Wide,
};
use crate::dom::{Dom, Element, NodeData, NodeId};
use crate::text::Family;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Display {
    None,
    Inline,
    Block,
    ListItem,
    Table,
    TableRowGroup,
    TableRow,
    TableCell,
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
    Auto,
}

impl Len {
    pub fn resolve(self, basis: f32) -> f32 {
        match self {
            Len::Px(v) => v,
            Len::Pct(p) => basis * p / 100.0,
            Len::Auto => 0.0,
        }
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
    pub text_transform: TextTransform,
    pub border_spacing: f32,
    // Not inherited.
    pub display: Display,
    pub background: Option<Color>,
    /// Text decorations propagate to all descendants, so these are the union
    /// of the element's own and its ancestors'.
    pub underline: bool,
    pub line_through: bool,
    /// Top, right, bottom, left.
    pub margin: [Len; 4],
    pub padding: [Len; 4],
    pub width: Len,
    pub max_width: Option<Len>,
    pub vertical_align: VAlign,
    pub border_box: bool,
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
            text_transform: TextTransform::None,
            border_spacing: 0.0,
            display: Display::Inline,
            background: None,
            underline: false,
            line_through: false,
            margin: [Len::Px(0.0); 4],
            padding: [Len::Px(0.0); 4],
            width: Len::Auto,
            max_width: None,
            vertical_align: VAlign::Baseline,
            border_box: false,
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
            text_transform: parent.text_transform,
            border_spacing: parent.border_spacing,
            ..Style::initial()
        }
    }

    pub fn bold(&self) -> bool {
        self.weight >= 600
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
table { display: table; border-spacing: 2px; box-sizing: border-box }
caption { display: block; text-align: center }
thead, tbody, tfoot { display: table-row-group }
tr { display: table-row }
td, th { display: table-cell; padding: 1px; vertical-align: middle }
th { font-weight: bold; text-align: center }
col, colgroup { display: none }
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
button { background-color: #e9e9ed; color: black }
summary { font-weight: bold }
"#;

pub(crate) fn ua_sheet() -> Stylesheet {
    Stylesheet::parse(UA_CSS)
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

/// Computes a style for every node, for a viewport `width` CSS px wide.
/// Text nodes get their parent's style, which is what layout uses for them.
pub(crate) fn compute(dom: &Dom, ua: &Stylesheet, author: &[AuthorSheet<'_>], width: f32) -> Vec<Style> {
    let author: Vec<&Stylesheet> =
        author.iter().filter(|s| s.media.is_none_or(|m| m.matches(width))).map(|s| s.sheet).collect();
    let mut styles: Vec<Style> = Vec::with_capacity(dom.nodes.len());
    let mut root_font_size = DEFAULT_FONT_SIZE;
    let mut matched: Vec<Matched<'_>> = Vec::new();
    // The arena is in document order, so a parent is always computed before
    // its children and ancestors can be gathered by walking parent links.
    for (id, node) in dom.nodes.iter().enumerate() {
        let parent_style = node.parent.and_then(|p| styles.get(p)).cloned();
        let style = match &node.data {
            NodeData::Element(el) => {
                let parent = parent_style.unwrap_or_else(Style::initial);
                let chain = ancestor_chain(dom, id);
                matched.clear();
                ua.matching(&chain, width, 0, &mut matched);
                let ua_count = matched.len();
                // Each sheet gets its own order range, so later sheets win ties.
                for (i, sheet) in author.iter().enumerate() {
                    sheet.matching(&chain, width, (i as u32 + 1).saturating_mul(1 << 16), &mut matched);
                }
                let hints = presentational_hints(el, &chain);
                let inline = el.attr("style").map(css::parse_declarations).unwrap_or_default();
                let s = cascade(&parent, &matched, ua_count, &hints, &inline, root_font_size);
                if el.name == "html" && node.parent.is_some_and(|p| matches!(dom.nodes[p].data, NodeData::Root)) {
                    root_font_size = s.font_size;
                }
                s
            }
            _ => parent_style.unwrap_or_else(Style::initial),
        };
        styles.push(style);
    }
    styles
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

fn cascade(
    parent: &Style,
    matched: &[Matched<'_>],
    ua_count: usize,
    hints: &[Decl],
    inline: &[Declaration],
    root_font_size: f32,
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

    let mut s = Style::inherit_from(parent);
    // Font size first: em units in every other property refer to it.
    for &(_, _, _, _, d) in &list {
        match d {
            Decl::FontSize(fs) => s.font_size = font_size(*fs, parent.font_size, root_font_size),
            Decl::Wide(Prop::FontSize, w) => {
                s.font_size = if *w == Wide::Initial { DEFAULT_FONT_SIZE } else { parent.font_size }
            }
            _ => {}
        }
    }
    let em = s.font_size;
    let len = |l: Length| -> Len {
        let px = |v: f32| Len::Px(v.clamp(-MAX_EXTENT, MAX_EXTENT));
        match l {
            Length::Px(v) => px(v),
            Length::Em(v) => px(v * em),
            Length::Rem(v) => px(v * root_font_size),
            Length::Pct(p) => Len::Pct(p.clamp(-1000.0, 1000.0)),
            Length::Auto => Len::Auto,
        }
    };
    let mut underline = false;
    let mut line_through = false;
    for &(_, _, _, _, d) in &list {
        match *d {
            Decl::Display(v) => s.display = v,
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
                underline = u;
                line_through = l;
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
            Decl::TextTransform(t) => s.text_transform = t,
            Decl::Margin(side, l) => s.margin[side & 3] = len(l),
            Decl::Padding(side, l) => s.padding[side & 3] = len(l),
            Decl::Width(l) => s.width = len(l),
            Decl::MaxWidth(l) => s.max_width = l.map(len),
            Decl::VerticalAlign(v) => s.vertical_align = v,
            Decl::BorderBox(v) => s.border_box = v,
            Decl::BorderSpacing(l) => s.border_spacing = len(l).resolve(0.0).clamp(0.0, 1000.0),
            Decl::Wide(p, w) => apply_wide(&mut s, parent, p, w, &mut underline, &mut line_through),
        }
    }
    s.underline = parent.underline || underline;
    s.line_through = parent.line_through || line_through;
    s
}

fn apply_wide(s: &mut Style, parent: &Style, p: Prop, w: Wide, underline: &mut bool, line_through: &mut bool) {
    let inherit = match w {
        Wide::Inherit => true,
        Wide::Initial => false,
        Wide::Unset => p.inherited(),
    };
    let src = if inherit { parent.clone() } else { Style::initial() };
    match p {
        Prop::Display => s.display = src.display,
        Prop::Color => s.color = src.color,
        Prop::Background => s.background = src.background,
        Prop::FontSize => {}
        Prop::FontWeight => s.weight = src.weight,
        Prop::FontStyle => s.italic = src.italic,
        Prop::FontFamily => s.family = src.family,
        Prop::TextAlign => s.text_align = src.text_align,
        Prop::TextDecoration => {
            *underline = false;
            *line_through = false;
        }
        Prop::LineHeight => s.line_height = src.line_height,
        Prop::WhiteSpace => s.white_space = src.white_space,
        Prop::ListStyle => s.list_style = src.list_style,
        Prop::Visibility => s.visible = src.visible,
        Prop::TextTransform => s.text_transform = src.text_transform,
        Prop::Margin(i) => s.margin[i & 3] = src.margin[i & 3],
        Prop::Padding(i) => s.padding[i & 3] = src.padding[i & 3],
        Prop::Width => s.width = src.width,
        Prop::MaxWidth => s.max_width = src.max_width,
        Prop::VerticalAlign => s.vertical_align = src.vertical_align,
        Prop::BoxSizing => s.border_box = src.border_box,
        Prop::BorderSpacing => s.border_spacing = src.border_spacing,
    }
}

fn resolve_color(c: ColorValue, current: Color) -> Color {
    match c {
        ColorValue::Rgba(c) => c,
        ColorValue::Current => current,
    }
}

fn font_size(fs: FontSize, parent: f32, root: f32) -> f32 {
    let px = match fs {
        FontSize::Px(v) => v,
        FontSize::Larger => parent * 1.2,
        FontSize::Smaller => parent / 1.2,
        FontSize::Length(Length::Px(v)) => v,
        FontSize::Length(Length::Em(v)) => parent * v,
        FontSize::Length(Length::Rem(v)) => root * v,
        FontSize::Length(Length::Pct(p)) => parent * p / 100.0,
        FontSize::Length(Length::Auto) => parent,
    };
    px.clamp(0.0, MAX_FONT_SIZE)
}

/// Legacy presentational attributes (`bgcolor`, `width`, `align`, `<font>`,
/// ...). Still common on old and minimalist sites; they sit just below
/// author CSS in the cascade.
fn presentational_hints(el: &Element, chain: &[&Element]) -> Vec<Decl> {
    let mut out = Vec::new();
    let name = el.name.as_str();
    let color = |v: &str| css::parse_color(&v.trim().to_ascii_lowercase()).or_else(|| legacy_color(v));
    if let Some(c) = el.attr("bgcolor").and_then(color) {
        out.push(Decl::Background(c));
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
            "baseline" => Some(VAlign::Top),
            _ => None,
        };
        out.extend(va.map(Decl::VerticalAlign));
    }
    if name == "table"
        && let Some(sp) = el.attr("cellspacing").and_then(|v| v.trim().parse::<f32>().ok()).filter(|v| v.is_finite())
    {
        out.push(Decl::BorderSpacing(Length::Px(sp.clamp(0.0, 1000.0))));
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
        (parsed.dom, styles)
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
}
