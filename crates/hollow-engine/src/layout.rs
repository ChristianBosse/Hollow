//! Box construction, layout and the display list.
//!
//! Three passes: build a box tree from the DOM (blocks, inline runs,
//! tables), lay it out top to bottom for one viewport width, and record
//! what to paint as a flat display list in page coordinates (CSS px).
//! Painting is a separate, cheap step so scrolling never relayouts.
//!
//! Supported: block flow with margin collapsing, inline text with line
//! breaking and alignment, lists, images, `<br>`, preformatted text, and
//! tables with automatic column sizing. Not supported: floats,
//! positioning, flexbox/grid (laid out as blocks), borders, heights.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use tiny_skia::Pixmap;
use url::Url;

use crate::Resources;
use crate::css::Color;
use crate::dom::{Dom, Element, NodeData, NodeId};
use crate::style::{Display, Len, ListStyle, Style, TextAlign, TextTransform, VAlign, WhiteSpace};
use crate::text::{self, FaceId, Shaper};

pub(crate) const MAX_BOXES: usize = 100_000;
pub(crate) const MAX_GLYPHS: usize = 1_000_000;
/// Block nesting deeper than this is laid out inline instead. Bounds the
/// recursion of layout well below the DOM depth cap.
const MAX_BLOCK_DEPTH: usize = 128;
const MAX_COLUMNS: usize = 256;
const MAX_COLSPAN: usize = 64;
const MAX_EXTENT: f32 = 1_000_000.0;
/// Images without a decodable resource and without explicit dimensions
/// collapse to their alt text; with dimensions they keep their space.
const FIELD_WIDTH: f32 = 150.0;

type BoxId = usize;
/// A table cell with its first column and column span.
type GridCell = (BoxId, usize, usize);

struct LBox {
    /// Node whose computed style applies. Anonymous boxes use their parent's.
    style: NodeId,
    anon: bool,
    kind: Kind,
    marker: Option<String>,
    colspan: usize,
}

enum Kind {
    Block(Vec<BoxId>),
    Inline(Vec<Item>),
    Table { captions: Vec<BoxId>, rows: Vec<BoxId> },
    Row(Vec<BoxId>),
    Hr,
}

enum Item {
    Text { text: String, style: NodeId, link: Option<u32>, bg: Option<Color> },
    Image { image: Option<u32>, size: ImageSize, style: NodeId, link: Option<u32> },
    Field { w: f32, h: f32, style: NodeId },
    Break { style: NodeId },
}

impl Item {
    fn style(&self) -> NodeId {
        match self {
            Item::Text { style, .. }
            | Item::Image { style, .. }
            | Item::Field { style, .. }
            | Item::Break { style } => *style,
        }
    }
    fn link(&self) -> Option<u32> {
        match self {
            Item::Text { link, .. } | Item::Image { link, .. } => *link,
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct ImageSize {
    width: Len,
    max_width: Option<Len>,
    attr_w: Option<f32>,
    attr_h: Option<f32>,
    intrinsic: Option<(f32, f32)>,
}

impl ImageSize {
    /// Used size, scaled down to fit `avail` as `max-width: 100%` would.
    fn resolve(&self, avail: f32) -> (f32, f32) {
        let ratio = match (self.intrinsic, self.attr_w, self.attr_h) {
            (Some((w, h)), _, _) if w > 0.0 && h > 0.0 => Some(w / h),
            (_, Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some(w / h),
            _ => None,
        };
        let width = match self.width {
            Len::Auto => None,
            l => Some(l.resolve(avail)),
        };
        let (mut w, mut h) = match (width, self.attr_h) {
            (Some(w), _) => (w, ratio.map(|r| w / r).or(self.attr_h).unwrap_or(w)),
            (None, Some(h)) => (ratio.map(|r| h * r).or(self.intrinsic.map(|i| i.0)).unwrap_or(h), h),
            (None, None) => self.intrinsic.unwrap_or((0.0, 0.0)),
        };
        if let Some(max) = self.max_width.map(|m| m.resolve(avail))
            && w > max
            && w > 0.0
        {
            h = h * max / w;
            w = max;
        }
        if w > avail && avail > 0.0 {
            h = h * avail / w;
            w = avail;
        }
        (clamp_extent(w), clamp_extent(h))
    }
}

fn clamp_extent(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, MAX_EXTENT) } else { 0.0 }
}

// ---------------------------------------------------------------------------
// Box tree construction

#[derive(Clone, Copy, Default)]
struct InlineCtx {
    link: Option<u32>,
    bg: Option<Color>,
}

struct Builder<'a> {
    dom: &'a Dom,
    styles: &'a [Style],
    resources: &'a Resources,
    boxes: Vec<LBox>,
    links: Vec<Url>,
    link_ids: BTreeMap<NodeId, u32>,
    images: Vec<Arc<Pixmap>>,
    image_ids: BTreeMap<String, u32>,
    /// Whether the last character added to the current inline run was a
    /// collapsible space, for collapsing across element boundaries.
    last_space: bool,
}

impl Builder<'_> {
    fn full(&self) -> bool {
        self.boxes.len() >= MAX_BOXES
    }

    fn push_box(&mut self, b: LBox) -> BoxId {
        self.boxes.push(b);
        self.boxes.len() - 1
    }

    fn build_block(&mut self, node: NodeId, depth: usize, marker: Option<String>, ctx: InlineCtx) -> Option<BoxId> {
        if self.full() {
            return None;
        }
        let is_hr = self.dom.element(node).is_some_and(|e| e.name == "hr");
        let kind = if is_hr { Kind::Hr } else { self.build_container(node, depth, ctx) };
        let colspan = self
            .dom
            .element(node)
            .and_then(|e| e.attr("colspan"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, MAX_COLSPAN);
        Some(self.push_box(LBox { style: node, anon: false, kind, marker, colspan }))
    }

    /// The contents of a block container: either one inline run, or block
    /// children with anonymous blocks wrapping any inline runs between them.
    fn build_container(&mut self, node: NodeId, depth: usize, ctx: InlineCtx) -> Kind {
        let mut blocks = Vec::new();
        let mut items = Vec::new();
        self.last_space = true;
        self.collect(node, node, &mut blocks, &mut items, InlineCtx { link: ctx.link, bg: None }, depth);
        if blocks.is_empty() {
            return Kind::Inline(items);
        }
        self.flush(node, &mut blocks, &mut items);
        Kind::Block(blocks)
    }

    fn flush(&mut self, container: NodeId, blocks: &mut Vec<BoxId>, items: &mut Vec<Item>) {
        let items = std::mem::take(items);
        self.last_space = true;
        let meaningful = items.iter().any(|i| match i {
            Item::Text { text, style, .. } => {
                !self.styles[*style].white_space.collapses_spaces() || text.chars().any(|c| !c.is_ascii_whitespace())
            }
            _ => true,
        });
        if meaningful && !self.full() {
            let id = self.push_box(LBox {
                style: container,
                anon: true,
                kind: Kind::Inline(items),
                marker: None,
                colspan: 1,
            });
            blocks.push(id);
        }
    }

    fn collect(
        &mut self,
        container: NodeId,
        node: NodeId,
        blocks: &mut Vec<BoxId>,
        items: &mut Vec<Item>,
        ctx: InlineCtx,
        depth: usize,
    ) {
        let dom = self.dom;
        let mut counter: i64 = dom
            .element(node)
            .filter(|e| e.name == "ol")
            .and_then(|e| e.attr("start"))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1);
        for &child in &dom.nodes[node].children {
            if self.full() {
                return;
            }
            match &dom.nodes[child].data {
                NodeData::Text(t) => {
                    let st = &self.styles[child];
                    let text = transform(&collapse(t, st.white_space, &mut self.last_space), st.text_transform);
                    if !text.is_empty() {
                        items.push(Item::Text { text, style: child, link: ctx.link, bg: ctx.bg });
                    }
                }
                NodeData::Element(el) => {
                    let st = &self.styles[child];
                    let mut display = st.display;
                    if display == Display::None {
                        continue;
                    }
                    if depth >= MAX_BLOCK_DEPTH {
                        display = Display::Inline;
                    }
                    let link = match dom.nodes[child].url.as_ref() {
                        Some(url) if matches!(el.name.as_str(), "a" | "area") => Some(self.link_id(child, url)),
                        _ => ctx.link,
                    };
                    match display {
                        Display::None => {}
                        Display::Inline => {
                            self.inline_element(container, child, el, blocks, items, InlineCtx { link, ..ctx }, depth)
                        }
                        Display::Table => {
                            self.flush(container, blocks, items);
                            if let Some(b) = self.build_table(child, depth + 1, InlineCtx { link, bg: None }) {
                                blocks.push(b);
                            }
                        }
                        Display::Block
                        | Display::ListItem
                        | Display::TableRowGroup
                        | Display::TableRow
                        | Display::TableCell => {
                            self.flush(container, blocks, items);
                            let marker = if display == Display::ListItem {
                                if let Some(v) = el.attr("value").and_then(|v| v.trim().parse().ok()) {
                                    counter = v;
                                }
                                let m = marker_text(st.list_style, counter);
                                counter = counter.saturating_add(1);
                                m
                            } else {
                                None
                            };
                            if let Some(b) = self.build_block(child, depth + 1, marker, InlineCtx { link, bg: None }) {
                                blocks.push(b);
                            }
                        }
                    }
                }
                NodeData::Root => {}
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn inline_element(
        &mut self,
        container: NodeId,
        node: NodeId,
        el: &Element,
        blocks: &mut Vec<BoxId>,
        items: &mut Vec<Item>,
        ctx: InlineCtx,
        depth: usize,
    ) {
        let st = &self.styles[node];
        let text_item = |b: &mut Self, text: &str, bg: Option<Color>| {
            b.last_space = false;
            Item::Text { text: text.to_owned(), style: node, link: ctx.link, bg }
        };
        match el.name.as_str() {
            "br" => {
                self.last_space = true;
                items.push(Item::Break { style: node });
            }
            "img" => {
                let image = self.dom.nodes[node].url.as_ref().and_then(|u| self.image_id(u));
                let attr = |n: &str| el.attr(n).and_then(crate::style::dimension_px);
                let size = ImageSize {
                    width: st.width,
                    max_width: st.max_width,
                    attr_w: attr("width"),
                    attr_h: attr("height"),
                    intrinsic: image.map(|i| {
                        let p = &self.images[i as usize];
                        (p.width() as f32, p.height() as f32)
                    }),
                };
                let has_box = image.is_some() || (size.attr_h.is_some() && st.width != Len::Auto);
                if has_box {
                    self.last_space = false;
                    items.push(Item::Image { image, size, style: node, link: ctx.link });
                } else if let Some(alt) = el.attr("alt").map(crate::dom::collapse_whitespace).filter(|a| !a.is_empty())
                {
                    items.push(text_item(self, &alt, ctx.bg));
                }
            }
            "input" => {
                let ty = el.attr("type").unwrap_or("text").trim().to_ascii_lowercase();
                let gray = Some(Color::rgb(0xe9, 0xe9, 0xed));
                match ty.as_str() {
                    "submit" | "button" | "reset" => {
                        let default = if ty == "reset" {
                            "Reset"
                        } else if ty == "submit" {
                            "Submit"
                        } else {
                            ""
                        };
                        let label = el.attr("value").unwrap_or(default);
                        if !label.is_empty() {
                            items.push(text_item(self, label, gray));
                        }
                    }
                    "checkbox" | "radio" => {
                        self.last_space = false;
                        items.push(Item::Field { w: 13.0, h: 13.0, style: node });
                    }
                    "image" | "file" | "range" | "color" => {}
                    _ => {
                        self.last_space = false;
                        let h = (st.font_size * 1.25).max(1.0);
                        items.push(Item::Field { w: FIELD_WIDTH, h, style: node });
                    }
                }
            }
            "textarea" => {
                self.last_space = false;
                items.push(Item::Field { w: FIELD_WIDTH * 1.4, h: st.font_size * 2.5, style: node });
            }
            "select" => {
                let label = self.first_option_text(node);
                items.push(text_item(self, &format!("{label} \u{25BE}"), Some(Color::rgb(0xe9, 0xe9, 0xed))));
            }
            "wbr" => items.push(text_item(self, "\u{200B}", None)),
            "q" => {
                items.push(text_item(self, "\u{201C}", ctx.bg));
                self.collect(
                    container,
                    node,
                    blocks,
                    items,
                    InlineCtx { bg: st.background.or(ctx.bg), ..ctx },
                    depth + 1,
                );
                items.push(text_item(self, "\u{201D}", ctx.bg));
            }
            _ => {
                let bg = st.background.or(ctx.bg);
                self.collect(container, node, blocks, items, InlineCtx { bg, ..ctx }, depth + 1);
            }
        }
    }

    fn first_option_text(&self, select: NodeId) -> String {
        let dom = self.dom;
        let mut first = None;
        let mut stack = vec![select];
        while let Some(n) = stack.pop() {
            for &c in dom.nodes[n].children.iter().rev() {
                if let Some(e) = dom.element(c) {
                    if e.name == "option" {
                        let text: String = dom.nodes[c]
                            .children
                            .iter()
                            .filter_map(|&t| match &dom.nodes[t].data {
                                NodeData::Text(s) => Some(s.as_str()),
                                _ => None,
                            })
                            .collect();
                        let text = crate::dom::collapse_whitespace(&text);
                        if e.attr("selected").is_some() {
                            return text;
                        }
                        // Children are visited in reverse, so keep overwriting.
                        first = Some(text);
                    } else if e.name == "optgroup" {
                        stack.push(c);
                    }
                }
            }
        }
        first.unwrap_or_default()
    }

    fn build_table(&mut self, node: NodeId, depth: usize, ctx: InlineCtx) -> Option<BoxId> {
        if self.full() {
            return None;
        }
        let dom = self.dom;
        let mut captions = Vec::new();
        let mut rows = Vec::new();
        let mut loose = Vec::new();
        for &child in &dom.nodes[node].children {
            if dom.element(child).is_none() {
                continue;
            }
            match self.styles[child].display {
                Display::None => {}
                Display::TableRow => {
                    self.flush_loose(node, &mut loose, &mut rows);
                    rows.extend(self.build_row(child, depth + 1, ctx));
                }
                Display::TableRowGroup => {
                    self.flush_loose(node, &mut loose, &mut rows);
                    for &gc in &dom.nodes[child].children {
                        if dom.element(gc).is_none() {
                            continue;
                        }
                        match self.styles[gc].display {
                            Display::None => {}
                            Display::TableCell => loose.extend(self.build_block(gc, depth + 2, None, ctx)),
                            _ => {
                                self.flush_loose(node, &mut loose, &mut rows);
                                rows.extend(self.build_row(gc, depth + 2, ctx));
                            }
                        }
                    }
                    self.flush_loose(node, &mut loose, &mut rows);
                }
                Display::TableCell => loose.extend(self.build_block(child, depth + 1, None, ctx)),
                _ => captions.extend(self.build_block(child, depth + 1, None, ctx)),
            }
        }
        self.flush_loose(node, &mut loose, &mut rows);
        Some(self.push_box(LBox {
            style: node,
            anon: false,
            kind: Kind::Table { captions, rows },
            marker: None,
            colspan: 1,
        }))
    }

    fn flush_loose(&mut self, table: NodeId, loose: &mut Vec<BoxId>, rows: &mut Vec<BoxId>) {
        if !loose.is_empty() {
            let cells = std::mem::take(loose);
            rows.push(self.push_box(LBox {
                style: table,
                anon: true,
                kind: Kind::Row(cells),
                marker: None,
                colspan: 1,
            }));
        }
    }

    fn build_row(&mut self, node: NodeId, depth: usize, ctx: InlineCtx) -> Option<BoxId> {
        if self.full() {
            return None;
        }
        let dom = self.dom;
        let mut cells = Vec::new();
        for &child in &dom.nodes[node].children {
            if dom.element(child).is_some() && self.styles[child].display != Display::None {
                cells.extend(self.build_block(child, depth + 1, None, ctx));
            }
        }
        Some(self.push_box(LBox { style: node, anon: false, kind: Kind::Row(cells), marker: None, colspan: 1 }))
    }

    fn link_id(&mut self, node: NodeId, url: &Url) -> u32 {
        if let Some(&id) = self.link_ids.get(&node) {
            return id;
        }
        let id = self.links.len() as u32;
        self.links.push(url.clone());
        self.link_ids.insert(node, id);
        id
    }

    fn image_id(&mut self, url: &Url) -> Option<u32> {
        if let Some(&id) = self.image_ids.get(url.as_str()) {
            return Some(id);
        }
        let pixmap = self.resources.get(url)?;
        let id = self.images.len() as u32;
        self.images.push(pixmap);
        self.image_ids.insert(url.as_str().to_owned(), id);
        Some(id)
    }
}

/// CSS white-space processing for one text node. `last_space` carries
/// across nodes so `a <b> b</b>` collapses to a single space.
pub(crate) fn collapse(text: &str, ws: WhiteSpace, last_space: &mut bool) -> String {
    let mut out = String::with_capacity(text.len());
    match ws {
        WhiteSpace::Pre | WhiteSpace::PreWrap => {
            out.push_str(text);
            if !text.is_empty() {
                *last_space = false;
            }
        }
        WhiteSpace::Normal | WhiteSpace::NoWrap | WhiteSpace::PreLine => {
            for c in text.chars() {
                match c {
                    '\n' if ws == WhiteSpace::PreLine => {
                        while out.ends_with(' ') {
                            out.pop();
                        }
                        out.push('\n');
                        *last_space = true;
                    }
                    ' ' | '\t' | '\n' | '\r' | '\x0C' => {
                        if !*last_space {
                            out.push(' ');
                            *last_space = true;
                        }
                    }
                    c => {
                        out.push(c);
                        *last_space = false;
                    }
                }
            }
        }
    }
    out
}

fn transform(text: &str, t: TextTransform) -> String {
    match t {
        TextTransform::None => text.to_owned(),
        TextTransform::Upper => text.to_uppercase(),
        TextTransform::Lower => text.to_lowercase(),
        TextTransform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut start = true;
            for c in text.chars() {
                if start && c.is_alphabetic() {
                    out.extend(c.to_uppercase());
                } else {
                    out.push(c);
                }
                start = c.is_whitespace();
            }
            out
        }
    }
}

fn marker_text(style: ListStyle, n: i64) -> Option<String> {
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
    Some(match style {
        ListStyle::None => return None,
        ListStyle::Disc => "\u{2022} ".into(),
        ListStyle::Circle => "\u{25E6} ".into(),
        ListStyle::Square => "\u{25AA} ".into(),
        ListStyle::Decimal => format!("{n}. "),
        ListStyle::LowerAlpha => format!("{}. ", alpha(n, b'a')),
        ListStyle::UpperAlpha => format!("{}. ", alpha(n, b'A')),
        ListStyle::LowerRoman => format!("{}. ", roman(n)),
        ListStyle::UpperRoman => format!("{}. ", roman(n).to_uppercase()),
    })
}

// ---------------------------------------------------------------------------
// Display list

#[derive(Clone, Copy, Debug)]
pub(crate) struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PlacedGlyph {
    pub id: u32,
    /// Relative to the run origin, CSS px, y down.
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug)]
pub(crate) enum DisplayItem {
    Rect {
        rect: Rect,
        color: Color,
    },
    /// A run of glyphs from one face; `glyphs` indexes [`Laid::glyphs`].
    Text {
        face: FaceId,
        size: f32,
        color: Color,
        x: f32,
        y: f32,
        glyphs: std::ops::Range<u32>,
    },
    Image {
        rect: Rect,
        image: u32,
    },
}

impl DisplayItem {
    fn shift(&mut self, dy: f32) {
        match self {
            DisplayItem::Rect { rect, .. } | DisplayItem::Image { rect, .. } => rect.y += dy,
            DisplayItem::Text { y, .. } => *y += dy,
        }
    }

    /// Vertical extent, for culling and document height.
    pub fn y_range(&self) -> (f32, f32) {
        match self {
            DisplayItem::Rect { rect, .. } | DisplayItem::Image { rect, .. } => (rect.y, rect.y + rect.h),
            DisplayItem::Text { y, size, .. } => (y - size * 1.3, y + size * 0.6),
        }
    }
}

/// Everything layout produces; [`crate::Layout`] wraps it.
pub(crate) struct Laid {
    pub width: f32,
    pub height: f32,
    pub canvas: Color,
    pub items: Vec<DisplayItem>,
    pub glyphs: Vec<PlacedGlyph>,
    pub links: Vec<(Rect, u32)>,
    pub link_urls: Vec<Url>,
    pub images: Vec<Arc<Pixmap>>,
}

// ---------------------------------------------------------------------------
// Layout

/// Collapsible vertical margins: the largest positive plus the most
/// negative, per CSS 2.
#[derive(Clone, Copy, Default)]
struct Margin {
    pos: f32,
    neg: f32,
}

impl Margin {
    fn push(&mut self, m: f32) {
        if m > 0.0 {
            self.pos = self.pos.max(m);
        } else {
            self.neg = self.neg.min(m);
        }
    }
    fn value(self) -> f32 {
        self.pos + self.neg
    }
}

/// The block-direction cursor of one formatting context.
struct Flow {
    y: f32,
    margin: Margin,
    /// Where the first in-flow content was placed, for a parent whose top
    /// margin collapses through it.
    first_place: Option<f32>,
}

impl Flow {
    fn new(y: f32) -> Flow {
        Flow { y, margin: Margin::default(), first_place: None }
    }

    /// Resolves the pending margin and returns the y where content starts.
    fn place(&mut self) -> f32 {
        let y = self.y + self.margin.value();
        self.margin = Margin::default();
        self.y = y;
        self.first_place.get_or_insert(y);
        y
    }
}

enum Atom {
    Word(Vec<Seg>),
    Space { item: u32, w: f32, can_break: bool, collapsible: bool },
    Break,
    Object { item: u32 },
}

struct Seg {
    item: u32,
    face: FaceId,
    size: f32,
    glyphs: Vec<Glyph>,
    w: f32,
}

#[derive(Clone, Copy)]
struct Glyph {
    id: u32,
    /// Pen position before this glyph, px from the segment start.
    pen: f32,
    dx: f32,
    dy: f32,
    cluster: u32,
}

/// A slice of one segment of a word, as placed on a line.
#[derive(Clone, Copy)]
struct Piece {
    atom: u32,
    seg: u32,
    g0: u32,
    g1: u32,
    x0: f32,
    w: f32,
}

enum FragKind {
    Text(Piece),
    Space,
    Object { h: f32 },
}

struct Frag {
    item: u32,
    x: f32,
    w: f32,
    kind: FragKind,
}

#[derive(Default)]
struct Line {
    frags: Vec<Frag>,
    w: f32,
}

impl Line {
    fn trim(&mut self) {
        while matches!(self.frags.last(), Some(Frag { kind: FragKind::Space, .. })) {
            self.frags.pop();
        }
        self.w = self.frags.last().map_or(0.0, |f| f.x + f.w);
    }
}

/// Vertical metrics of one inline piece of text: (ascent, descent, line height).
fn text_metrics(st: &Style) -> (f32, f32, f32) {
    let Some(f) = text::face(text::face_id(st.family, st.bold(), st.italic)) else {
        return (st.font_size * 0.8, st.font_size * 0.2, st.font_size * 1.2);
    };
    let k = f.scale(st.font_size);
    let normal = (f.ascent + f.descent + f.line_gap) / f.upem;
    (f.ascent * k, f.descent * k, st.line_height_px(normal).clamp(0.0, MAX_EXTENT))
}

fn baseline_shift(st: &Style) -> f32 {
    match st.vertical_align {
        VAlign::Sub => st.font_size * 0.25,
        VAlign::Super => -st.font_size * 0.5,
        _ => 0.0,
    }
}

fn face_of(st: &Style) -> FaceId {
    text::face_id(st.family, st.bold(), st.italic)
}

struct Ctx<'a> {
    boxes: &'a [LBox],
    styles: &'a [Style],
    shaper: Shaper,
    atoms: Vec<Option<Rc<Vec<Atom>>>>,
    intrinsic: Vec<Option<(f32, f32)>>,
    glyph_budget: usize,
    out: Vec<DisplayItem>,
    glyphs: Vec<PlacedGlyph>,
    links: Vec<(Rect, u32)>,
}

/// Viewport widths are clamped to 1..=16384 CSS px; NaN becomes 1024.
pub(crate) fn clamp_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(1.0, 16_384.0)
    } else if width > 0.0 {
        16_384.0
    } else {
        1024.0
    }
}

pub(crate) fn layout(dom: &Dom, styles: &[Style], resources: &Resources, width: f32) -> Laid {
    let width = clamp_width(width);
    let mut b = Builder {
        dom,
        styles,
        resources,
        boxes: Vec::new(),
        links: Vec::new(),
        link_ids: BTreeMap::new(),
        images: Vec::new(),
        image_ids: BTreeMap::new(),
        last_space: true,
    };
    // The root element(s): normally just <html>.
    let mut roots = Vec::new();
    let mut items = Vec::new();
    b.collect(0, 0, &mut roots, &mut items, InlineCtx::default(), 0);
    b.flush(0, &mut roots, &mut items);

    let canvas = canvas_color(dom, styles);
    let Builder { boxes, links: link_urls, images, .. } = b;
    let mut ctx = Ctx {
        boxes: &boxes,
        styles,
        shaper: Shaper::new(),
        atoms: (0..boxes.len()).map(|_| None).collect(),
        intrinsic: vec![None; boxes.len()],
        glyph_budget: MAX_GLYPHS,
        out: Vec::new(),
        glyphs: Vec::new(),
        links: Vec::new(),
    };
    let mut flow = Flow::new(0.0);
    for &r in &roots {
        ctx.layout_box(r, 0.0, width, &mut flow);
    }
    // Boxes and images that overflow the flow (negative margins, wide
    // content) still extend the page; text extents are only estimates.
    let mut height = flow.y + flow.margin.value().max(0.0);
    for it in &ctx.out {
        if !matches!(it, DisplayItem::Text { .. }) {
            height = height.max(it.y_range().1);
        }
    }
    let Ctx { out, glyphs, links, .. } = ctx;
    Laid { width, height: clamp_extent(height), canvas, items: out, glyphs, links, link_urls, images: images.clone() }
}

/// The root's background (or else body's) paints the whole canvas, per CSS.
fn canvas_color(dom: &Dom, styles: &[Style]) -> Color {
    let find = |name: &str| {
        dom.nodes.iter().enumerate().find_map(|(i, n)| match &n.data {
            NodeData::Element(e) if e.name == name => styles[i].background,
            _ => None,
        })
    };
    let c = find("html").or_else(|| find("body")).unwrap_or(Color::WHITE);
    // The canvas is always opaque: blend a translucent color over white.
    let blend = |v: u8| ((u16::from(v) * u16::from(c.a) + 255 * (255 - u16::from(c.a)) + 127) / 255) as u8;
    Color::rgb(blend(c.r), blend(c.g), blend(c.b))
}

struct BoxGeom {
    margin: [f32; 4],
    padding: [f32; 4],
    auto_left: bool,
    auto_right: bool,
}

fn geometry(st: &Style, anon: bool, cb_w: f32) -> BoxGeom {
    if anon {
        return BoxGeom { margin: [0.0; 4], padding: [0.0; 4], auto_left: false, auto_right: false };
    }
    let r = |l: Len| {
        let v = l.resolve(cb_w);
        if v.is_finite() { v.clamp(-MAX_EXTENT, MAX_EXTENT) } else { 0.0 }
    };
    BoxGeom {
        margin: st.margin.map(r),
        padding: st.padding.map(|l| r(l).max(0.0)),
        auto_left: st.margin[3] == Len::Auto,
        auto_right: st.margin[1] == Len::Auto,
    }
}

impl Ctx<'_> {
    fn reserve(&mut self) -> usize {
        self.out.push(DisplayItem::Rect { rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }, color: Color::WHITE });
        self.out.len() - 1
    }

    fn fill(&mut self, slot: Option<usize>, rect: Rect, color: Color) {
        if let Some(i) = slot
            && let Some(it) = self.out.get_mut(i)
        {
            *it = DisplayItem::Rect { rect, color };
        }
    }

    fn bg_slot(&mut self, st: &Style, anon: bool) -> Option<(usize, Color)> {
        match st.background {
            Some(c) if !anon && st.visible && c.a > 0 => Some((self.reserve(), c)),
            _ => None,
        }
    }

    fn layout_box(&mut self, id: BoxId, x: f32, cb_w: f32, flow: &mut Flow) -> Option<f32> {
        let boxes = self.boxes;
        match &boxes[id].kind {
            Kind::Table { .. } => self.layout_table(id, x, cb_w, flow),
            _ => self.layout_block(id, x, cb_w, flow),
        }
    }

    fn layout_block(&mut self, id: BoxId, x: f32, cb_w: f32, flow: &mut Flow) -> Option<f32> {
        let boxes = self.boxes;
        let styles = self.styles;
        let b = &boxes[id];
        let st = &styles[b.style];
        let g = geometry(st, b.anon, cb_w);
        let [mt, mut mr, mb, mut ml] = g.margin;
        let [pt, pr, pb, pl] = g.padding;
        if g.auto_left {
            ml = 0.0;
        }
        if g.auto_right {
            mr = 0.0;
        }
        let auto_w = (cb_w - ml - mr - pl - pr).max(0.0);
        let mut w = auto_w;
        let mut explicit = false;
        if !b.anon && st.width != Len::Auto {
            w = st.width.resolve(cb_w);
            if st.border_box {
                w -= pl + pr;
            }
            explicit = true;
        }
        if !b.anon
            && let Some(max) = st.max_width
        {
            let mut m = max.resolve(cb_w);
            if st.border_box {
                m -= pl + pr;
            }
            if w > m {
                w = m;
                explicit = true;
            }
        }
        let w = clamp_extent(w);
        if explicit {
            let free = cb_w - w - pl - pr - ml - mr;
            match (g.auto_left, g.auto_right) {
                (true, true) => ml += (free / 2.0).max(0.0),
                (true, false) => ml += free.max(0.0),
                _ => {}
            }
        }
        let content_x = x + ml + pl;
        let box_x = x + ml;

        flow.margin.push(mt);
        let bg = self.bg_slot(st, b.anon);
        let collapse_top = pt <= 0.0;
        let mut inner = if collapse_top {
            let f = Flow { y: flow.y, margin: flow.margin, first_place: None };
            flow.margin = Margin::default();
            f
        } else {
            let top = flow.place();
            Flow::new(top + pt)
        };
        let explicit_top = (!collapse_top).then_some(inner.y - pt);

        let first_baseline = match &b.kind {
            Kind::Block(children) | Kind::Row(children) => {
                let mut fb = None;
                for &c in children {
                    let r = self.layout_box(c, content_x, w, &mut inner);
                    if fb.is_none() {
                        fb = r;
                    }
                }
                fb
            }
            Kind::Inline(_) => self.layout_inline(id, content_x, w, &mut inner),
            Kind::Hr => {
                let top = inner.place();
                inner.y = top + 2.0;
                if st.visible && st.background.is_none() {
                    let light = Color { a: st.color.a / 2, ..st.color };
                    self.out
                        .push(DisplayItem::Rect { rect: Rect { x: content_x, y: top, w, h: 1.0 }, color: st.color });
                    self.out
                        .push(DisplayItem::Rect { rect: Rect { x: content_x, y: top + 1.0, w, h: 1.0 }, color: light });
                }
                None
            }
            Kind::Table { .. } => None,
        };

        let top = explicit_top.or(inner.first_place);
        let Some(top) = top else {
            // Nothing placed and nothing separating the margins: the box
            // collapses through, and its margins join the surrounding ones.
            if pb > 0.0 {
                let top = inner.place();
                flow.y = top + pb;
                flow.margin = Margin::default();
                self.fill(
                    bg.map(|b| b.0),
                    Rect { x: box_x, y: top, w: w + pl + pr, h: pb },
                    bg.map_or(Color::WHITE, |b| b.1),
                );
            } else {
                flow.y = inner.y;
                flow.margin = inner.margin;
            }
            flow.margin.push(mb);
            self.marker(b, st, content_x, inner.y, first_baseline);
            return first_baseline;
        };
        if collapse_top && flow.first_place.is_none() {
            flow.first_place = Some(top);
        }
        let bottom = if pb > 0.0 {
            let end = inner.place();
            flow.margin = Margin::default();
            end + pb
        } else {
            flow.margin = inner.margin;
            inner.y
        };
        flow.y = bottom;
        flow.margin.push(mb);
        if let Some((slot, color)) = bg {
            self.fill(Some(slot), Rect { x: box_x, y: top, w: w + pl + pr, h: (bottom - top).max(0.0) }, color);
        }
        self.marker(b, st, content_x, top, first_baseline);
        first_baseline
    }

    /// List marker, hanging to the left of the content box.
    fn marker(&mut self, b: &LBox, st: &Style, content_x: f32, top: f32, baseline: Option<f32>) {
        let Some(text) = &b.marker else { return };
        if !st.visible {
            return;
        }
        let (ascent, _, _) = text_metrics(st);
        let baseline = baseline.unwrap_or(top + ascent);
        let face = face_of(st);
        let Some(f) = text::face(face) else { return };
        let shaped = self.shaper.shape(face, text);
        let k = f.scale(st.font_size);
        let w = shaped.advance as f32 * k;
        let start = self.glyphs.len() as u32;
        for g in &shaped.glyphs {
            self.glyphs.push(PlacedGlyph { id: g.id, x: (g.pen + g.dx) as f32 * k, y: -(g.dy as f32) * k });
        }
        let end = self.glyphs.len() as u32;
        self.out.push(DisplayItem::Text {
            face,
            size: st.font_size,
            color: st.color,
            x: content_x - w,
            y: baseline,
            glyphs: start..end,
        });
    }

    // -- Inline formatting ---------------------------------------------------

    fn atoms(&mut self, id: BoxId) -> Rc<Vec<Atom>> {
        if let Some(a) = &self.atoms[id] {
            return a.clone();
        }
        let boxes = self.boxes;
        let atoms = match &boxes[id].kind {
            Kind::Inline(items) => Rc::new(self.tokenize(items)),
            _ => Rc::new(Vec::new()),
        };
        self.atoms[id] = Some(atoms.clone());
        atoms
    }

    fn tokenize(&mut self, items: &[Item]) -> Vec<Atom> {
        let styles = self.styles;
        let mut atoms = Vec::new();
        let mut word: Vec<Seg> = Vec::new();
        let mut col = 0usize;
        for (i, item) in items.iter().enumerate() {
            let i = i as u32;
            match item {
                Item::Text { text, style, .. } => {
                    let st = &styles[*style];
                    let ws = st.white_space;
                    let face = face_of(st);
                    let space_w = self.space_width(face, st.font_size);
                    let mut run = String::new();
                    for c in text.chars() {
                        match c {
                            '\n' if ws.keeps_newlines() => {
                                self.push_run(&mut word, &mut run, i, face, st.font_size);
                                flush_word(&mut atoms, &mut word);
                                atoms.push(Atom::Break);
                                col = 0;
                            }
                            '\r' if ws.keeps_newlines() => {}
                            ' ' | '\t' | '\n' | '\r' | '\x0C' if ws.collapses_spaces() => {
                                self.push_run(&mut word, &mut run, i, face, st.font_size);
                                flush_word(&mut atoms, &mut word);
                                atoms.push(Atom::Space {
                                    item: i,
                                    w: space_w,
                                    can_break: ws.wraps(),
                                    collapsible: true,
                                });
                            }
                            ' ' | '\t' if ws == WhiteSpace::PreWrap => {
                                let n = if c == '\t' { 8 - col % 8 } else { 1 };
                                col += n;
                                self.push_run(&mut word, &mut run, i, face, st.font_size);
                                flush_word(&mut atoms, &mut word);
                                atoms.push(Atom::Space {
                                    item: i,
                                    w: space_w * n as f32,
                                    can_break: true,
                                    collapsible: false,
                                });
                            }
                            '\t' => {
                                let n = 8 - col % 8;
                                col += n;
                                run.extend(std::iter::repeat_n(' ', n));
                            }
                            '\u{200B}' => {
                                self.push_run(&mut word, &mut run, i, face, st.font_size);
                                flush_word(&mut atoms, &mut word);
                                atoms.push(Atom::Space { item: i, w: 0.0, can_break: ws.wraps(), collapsible: false });
                            }
                            c => {
                                run.push(c);
                                col += 1;
                            }
                        }
                    }
                    self.push_run(&mut word, &mut run, i, face, st.font_size);
                }
                Item::Break { .. } => {
                    flush_word(&mut atoms, &mut word);
                    atoms.push(Atom::Break);
                    col = 0;
                }
                Item::Image { .. } | Item::Field { .. } => {
                    flush_word(&mut atoms, &mut word);
                    atoms.push(Atom::Object { item: i });
                }
            }
        }
        flush_word(&mut atoms, &mut word);
        atoms
    }

    fn space_width(&mut self, face: FaceId, size: f32) -> f32 {
        let Some(f) = text::face(face) else { return size * 0.3 };
        self.shaper.shape(face, " ").advance as f32 * f.scale(size)
    }

    fn push_run(&mut self, word: &mut Vec<Seg>, run: &mut String, item: u32, face: FaceId, size: f32) {
        if run.is_empty() {
            return;
        }
        let text = std::mem::take(run);
        if self.glyph_budget == 0 {
            return;
        }
        let Some(f) = text::face(face) else { return };
        let shaped = self.shaper.shape(face, &text);
        let n = shaped.glyphs.len().min(self.glyph_budget);
        self.glyph_budget -= n;
        let k = f.scale(size);
        let glyphs: Vec<Glyph> = shaped
            .glyphs
            .iter()
            .take(n)
            .map(|g| Glyph {
                id: g.id,
                pen: g.pen as f32 * k,
                dx: g.dx as f32 * k,
                dy: g.dy as f32 * k,
                cluster: g.cluster,
            })
            .collect();
        let w = if n == shaped.glyphs.len() { shaped.advance as f32 * k } else { glyphs.last().map_or(0.0, |g| g.pen) };
        word.push(Seg { item, face, size, glyphs, w: clamp_extent(w) });
    }

    fn break_lines(&mut self, atoms: &[Atom], items: &[Item], avail: f32) -> Vec<Line> {
        let styles = self.styles;
        let mut lines = Vec::new();
        let mut cur = Line::default();
        let mut break_ok = true;
        for (ai, atom) in atoms.iter().enumerate() {
            match atom {
                Atom::Word(segs) => {
                    let wraps = styles[items[segs[0].item as usize].style()].white_space.wraps();
                    let mut pieces: Vec<Piece> = segs
                        .iter()
                        .enumerate()
                        .map(|(si, s)| Piece {
                            atom: ai as u32,
                            seg: si as u32,
                            g0: 0,
                            g1: s.glyphs.len() as u32,
                            x0: 0.0,
                            w: s.w,
                        })
                        .collect();
                    let mut w: f32 = pieces.iter().map(|p| p.w).sum();
                    if wraps && break_ok && !cur.frags.is_empty() && cur.w + w > avail {
                        cur.trim();
                        lines.push(std::mem::take(&mut cur));
                    }
                    // Too long for any line: break inside the word.
                    while wraps && cur.frags.is_empty() && w > avail && avail > 0.0 {
                        let (head, tail) = split_pieces(&pieces, atoms, avail);
                        if tail.is_empty() || head.is_empty() {
                            break;
                        }
                        place_pieces(&mut cur, &head, segs);
                        lines.push(std::mem::take(&mut cur));
                        pieces = tail;
                        w = pieces.iter().map(|p| p.w).sum();
                    }
                    place_pieces(&mut cur, &pieces, segs);
                    break_ok = false;
                }
                Atom::Space { item, w, can_break, collapsible } => {
                    if !(cur.frags.is_empty() && *collapsible) {
                        cur.frags.push(Frag { item: *item, x: cur.w, w: *w, kind: FragKind::Space });
                        cur.w += *w;
                    }
                    break_ok = *can_break;
                }
                Atom::Break => {
                    cur.trim();
                    lines.push(std::mem::take(&mut cur));
                    break_ok = true;
                }
                Atom::Object { item } => {
                    let it = &items[*item as usize];
                    let (w, h) = match it {
                        Item::Image { size, .. } => size.resolve(avail),
                        Item::Field { w, h, .. } => (*w, *h),
                        _ => (0.0, 0.0),
                    };
                    let wraps = styles[it.style()].white_space.wraps();
                    if wraps && !cur.frags.is_empty() && cur.w + w > avail {
                        cur.trim();
                        lines.push(std::mem::take(&mut cur));
                    }
                    cur.frags.push(Frag { item: *item, x: cur.w, w, kind: FragKind::Object { h } });
                    cur.w += w;
                    break_ok = true;
                }
            }
        }
        cur.trim();
        if !cur.frags.is_empty() {
            lines.push(cur);
        }
        lines
    }

    fn layout_inline(&mut self, id: BoxId, x: f32, w: f32, flow: &mut Flow) -> Option<f32> {
        let boxes = self.boxes;
        let styles = self.styles;
        let b = &boxes[id];
        let Kind::Inline(items) = &b.kind else { return None };
        let atoms = self.atoms(id);
        if atoms.is_empty() {
            return None;
        }
        let lines = self.break_lines(&atoms, items, w);
        if lines.is_empty() {
            return None;
        }
        let block_style = &styles[b.style];
        let mut y = flow.place();
        let mut first = None;
        for line in &lines {
            let (h, baseline) = self.emit_line(line, &atoms, items, block_style, x, y, w);
            first.get_or_insert(baseline);
            y += h;
        }
        flow.y = y;
        first
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_line(
        &mut self,
        line: &Line,
        atoms: &[Atom],
        items: &[Item],
        block: &Style,
        x: f32,
        top: f32,
        avail: f32,
    ) -> (f32, f32) {
        let styles = self.styles;
        let half_leading = |st: &Style| {
            let (a, d, lh) = text_metrics(st);
            let half = (lh - (a + d)) / 2.0;
            (a + half, d + half)
        };
        // The strut: every line is at least as tall as the block's own font.
        let (mut above, mut below) = half_leading(block);
        for f in &line.frags {
            let st = &styles[items[f.item as usize].style()];
            let shift = baseline_shift(st);
            match f.kind {
                FragKind::Text(_) | FragKind::Space => {
                    let (a, d) = half_leading(st);
                    above = above.max(a - shift);
                    below = below.max(d + shift);
                }
                FragKind::Object { h } => above = above.max(h - shift),
            }
        }
        let height = (above + below).max(0.0);
        let baseline = top + above;
        let offset = match block.text_align {
            TextAlign::Left => 0.0,
            TextAlign::Center => ((avail - line.w) / 2.0).max(0.0),
            TextAlign::Right => (avail - line.w).max(0.0),
        };
        for f in &line.frags {
            let item = &items[f.item as usize];
            let st = &styles[item.style()];
            let fx = x + offset + f.x;
            let by = baseline + baseline_shift(st);
            if let Some(link) = item.link()
                && st.visible
            {
                self.links.push((Rect { x: fx, y: top, w: f.w, h: height }, link));
            }
            if !st.visible {
                continue;
            }
            match &f.kind {
                FragKind::Text(_) | FragKind::Space => {
                    let face = face_of(st);
                    let (a, d, _) = text_metrics(st);
                    if let Item::Text { bg: Some(bg), .. } = item {
                        self.out
                            .push(DisplayItem::Rect { rect: Rect { x: fx, y: by - a, w: f.w, h: a + d }, color: *bg });
                    }
                    if let FragKind::Text(p) = &f.kind
                        && let Atom::Word(segs) = &atoms[p.atom as usize]
                    {
                        let seg = &segs[p.seg as usize];
                        let start = self.glyphs.len() as u32;
                        for g in &seg.glyphs[p.g0 as usize..p.g1 as usize] {
                            self.glyphs.push(PlacedGlyph { id: g.id, x: g.pen + g.dx - p.x0, y: -g.dy });
                        }
                        let end = self.glyphs.len() as u32;
                        if st.color.a > 0 {
                            self.out.push(DisplayItem::Text {
                                face: seg.face,
                                size: seg.size,
                                color: st.color,
                                x: fx,
                                y: by,
                                glyphs: start..end,
                            });
                        }
                    }
                    if let Some(fc) = text::face(face) {
                        let k = fc.scale(st.font_size);
                        if st.underline {
                            let t = (fc.underline_thickness * k).max(1.0);
                            let rect = Rect { x: fx, y: by - fc.underline_pos * k, w: f.w, h: t };
                            self.out.push(DisplayItem::Rect { rect, color: st.color });
                        }
                        if st.line_through {
                            let t = (fc.strike_thickness * k).max(1.0);
                            let rect = Rect { x: fx, y: by - fc.strike_pos * k, w: f.w, h: t };
                            self.out.push(DisplayItem::Rect { rect, color: st.color });
                        }
                    }
                }
                FragKind::Object { h } => {
                    let rect = Rect { x: fx, y: by - h, w: f.w, h: *h };
                    match item {
                        Item::Image { image: Some(img), .. } => self.out.push(DisplayItem::Image { rect, image: *img }),
                        Item::Field { .. } => {
                            let border = Color::rgb(0x76, 0x76, 0x76);
                            self.out.push(DisplayItem::Rect { rect, color: border });
                            if f.w > 2.0 && *h > 2.0 {
                                let inner = Rect { x: rect.x + 1.0, y: rect.y + 1.0, w: rect.w - 2.0, h: rect.h - 2.0 };
                                self.out.push(DisplayItem::Rect { rect: inner, color: Color::WHITE });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        (height, baseline)
    }

    // -- Tables ---------------------------------------------------------------

    /// (min-content, max-content) border-box width of a box, including its
    /// horizontal margins.
    fn intrinsic(&mut self, id: BoxId) -> (f32, f32) {
        if let Some(v) = self.intrinsic[id] {
            return v;
        }
        let boxes = self.boxes;
        let styles = self.styles;
        let b = &boxes[id];
        let st = &styles[b.style];
        let (mut mn, mut mx) = match &b.kind {
            Kind::Inline(items) => {
                let atoms = self.atoms(id);
                let (mut mn, mut mx, mut line, mut word) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                let mut glue = false;
                for a in atoms.iter() {
                    match a {
                        Atom::Word(segs) => {
                            let w: f32 = segs.iter().map(|s| s.w).sum();
                            let nowrap = !styles[items[segs[0].item as usize].style()].white_space.wraps();
                            word = if glue || nowrap { word + w } else { w };
                            line += w;
                            mn = mn.max(word);
                            glue = nowrap;
                        }
                        Atom::Space { w, can_break, .. } => {
                            line += w;
                            if *can_break {
                                word = 0.0;
                                glue = false;
                            } else {
                                word += w;
                                glue = true;
                            }
                        }
                        Atom::Break => {
                            mx = mx.max(line);
                            line = 0.0;
                            word = 0.0;
                            glue = false;
                        }
                        Atom::Object { item } => {
                            let w = match &items[*item as usize] {
                                Item::Image { size, .. } => size.resolve(0.0).0,
                                Item::Field { w, .. } => *w,
                                _ => 0.0,
                            };
                            line += w;
                            mn = mn.max(w);
                            word = 0.0;
                            glue = false;
                        }
                    }
                }
                (mn, mx.max(line))
            }
            Kind::Block(children) | Kind::Row(children) => {
                let row = matches!(b.kind, Kind::Row(_));
                let mut acc = (0.0f32, 0.0f32);
                for &c in children {
                    let (cmn, cmx) = self.intrinsic(c);
                    acc = if row { (acc.0 + cmn, acc.1 + cmx) } else { (acc.0.max(cmn), acc.1.max(cmx)) };
                }
                acc
            }
            Kind::Table { captions, rows } => {
                let (cols_min, cols_max, n) = self.columns(rows);
                let sp = st.border_spacing * (n as f32 + 1.0);
                let mut mn: f32 = cols_min.iter().sum::<f32>() + sp;
                let mut mx: f32 = cols_max.iter().sum::<f32>() + sp;
                for &c in captions {
                    let (cmn, cmx) = self.intrinsic(c);
                    mn = mn.max(cmn);
                    mx = mx.max(cmx);
                }
                (mn, mx)
            }
            Kind::Hr => (0.0, 0.0),
        };
        if !b.anon {
            let px = |l: Len| if let Len::Px(v) = l { v } else { 0.0 };
            let pad = px(st.padding[1]).max(0.0) + px(st.padding[3]).max(0.0);
            if let Len::Px(w) = st.width {
                let w = if st.border_box && !matches!(b.kind, Kind::Table { .. }) { w - pad } else { w };
                let w = w.max(0.0);
                mn = if matches!(b.kind, Kind::Table { .. }) { mn.max(w) } else { w };
                mx = mn.max(w);
            }
            if !matches!(b.kind, Kind::Table { .. }) {
                mn += pad;
                mx += pad;
            }
            let margins = px(st.margin[1]) + px(st.margin[3]);
            mn += margins.max(0.0);
            mx += margins.max(0.0);
        }
        let v = (clamp_extent(mn), clamp_extent(mx.max(mn)));
        self.intrinsic[id] = Some(v);
        v
    }

    /// Cells of each row with their start column and span.
    fn grid(&self, rows: &[BoxId]) -> (Vec<Vec<GridCell>>, usize) {
        let boxes = self.boxes;
        let mut grid = Vec::with_capacity(rows.len());
        let mut n = 0;
        for &r in rows {
            let mut cells = Vec::new();
            if let Kind::Row(cs) = &boxes[r].kind {
                let mut col = 0;
                for &c in cs {
                    if col >= MAX_COLUMNS {
                        break;
                    }
                    let span = boxes[c].colspan.clamp(1, MAX_COLUMNS - col);
                    cells.push((c, col, span));
                    col += span;
                }
                n = n.max(col);
            }
            grid.push(cells);
        }
        (grid, n)
    }

    fn columns(&mut self, rows: &[BoxId]) -> (Vec<f32>, Vec<f32>, usize) {
        let (grid, n) = self.grid(rows);
        let mut mins = vec![0.0f32; n];
        let mut maxs = vec![0.0f32; n];
        for row in &grid {
            for &(c, col, span) in row {
                if span == 1 {
                    let (mn, mx) = self.intrinsic(c);
                    mins[col] = mins[col].max(mn);
                    maxs[col] = maxs[col].max(mx);
                }
            }
        }
        for row in &grid {
            for &(c, col, span) in row {
                if span > 1 {
                    let (mn, mx) = self.intrinsic(c);
                    let range = col..col + span;
                    let have_min: f32 = mins[range.clone()].iter().sum();
                    if mn > have_min {
                        let add = (mn - have_min) / span as f32;
                        mins[range.clone()].iter_mut().for_each(|m| *m += add);
                    }
                    let have_max: f32 = maxs[range.clone()].iter().sum();
                    if mx > have_max {
                        let add = (mx - have_max) / span as f32;
                        maxs[range].iter_mut().for_each(|m| *m += add);
                    }
                }
            }
        }
        for i in 0..n {
            maxs[i] = maxs[i].max(mins[i]);
        }
        (mins, maxs, n)
    }

    fn layout_table(&mut self, id: BoxId, x: f32, cb_w: f32, flow: &mut Flow) -> Option<f32> {
        let boxes = self.boxes;
        let styles = self.styles;
        let b = &boxes[id];
        let Kind::Table { captions, rows } = &b.kind else { return None };
        let st = &styles[b.style];
        let g = geometry(st, false, cb_w);
        let [mt, mut mr, mb, mut ml] = g.margin;
        let [pt, pr, pb, pl] = g.padding;
        if g.auto_left {
            ml = 0.0;
        }
        if g.auto_right {
            mr = 0.0;
        }
        let spacing = st.border_spacing;
        let (cols_min, cols_max, n) = self.columns(rows);
        let (grid, _) = self.grid(rows);
        let chrome = spacing * (n as f32 + 1.0) + pl + pr;
        let sum_min = cols_min.iter().sum::<f32>() + chrome;
        let sum_max = cols_max.iter().sum::<f32>() + chrome;
        let avail = (cb_w - ml - mr).max(0.0);
        let mut tw = match st.width {
            Len::Auto => sum_max.min(avail),
            l => l.resolve(cb_w),
        };
        if let Some(max) = st.max_width {
            tw = tw.min(max.resolve(cb_w));
        }
        let tw = clamp_extent(tw.max(sum_min));
        let free = cb_w - tw - ml - mr;
        match (g.auto_left, g.auto_right) {
            (true, true) => ml += (free / 2.0).max(0.0),
            (true, false) => ml += free.max(0.0),
            _ => {}
        }
        let widths = distribute(&cols_min, &cols_max, (tw - chrome).max(0.0));
        let tx = x + ml;

        flow.margin.push(mt);
        let top = flow.place();
        let bg = self.bg_slot(st, false);
        let mut y = top + pt;
        let mut first_baseline = None;
        for &c in captions {
            let mut f = Flow::new(y);
            let r = self.layout_box(c, tx + pl, (tw - pl - pr).max(0.0), &mut f);
            first_baseline = first_baseline.or(r);
            y = f.y + f.margin.value().max(0.0);
        }
        if !rows.is_empty() {
            y += spacing;
        }
        for (ri, &row) in rows.iter().enumerate() {
            let rb = &boxes[row];
            let row_style = &styles[rb.style];
            let row_bg = self.bg_slot(row_style, rb.anon);
            struct Placed {
                cell: BoxId,
                x: f32,
                w: f32,
                h: f32,
                baseline: Option<f32>,
                bg: Option<(usize, Color)>,
                /// Display items and links the cell's contents produced.
                items: std::ops::Range<usize>,
                links: std::ops::Range<usize>,
            }
            let mut placed: Vec<Placed> = Vec::new();
            let mut row_h = 0.0f32;
            for &(cell, col, span) in &grid[ri] {
                let cx = tx + pl + spacing + widths[..col].iter().sum::<f32>() + spacing * col as f32;
                let cw = widths[col..col + span].iter().sum::<f32>() + spacing * (span as f32 - 1.0);
                let cell_style = &styles[boxes[cell].style];
                let cell_bg = self.bg_slot(cell_style, false);
                let (items_from, links_from) = (self.out.len(), self.links.len());
                let (h, baseline) = self.layout_cell(cell, cx, cw, y);
                row_h = row_h.max(h);
                placed.push(Placed {
                    cell,
                    x: cx,
                    w: cw,
                    h,
                    baseline,
                    bg: cell_bg,
                    items: items_from..self.out.len(),
                    links: links_from..self.links.len(),
                });
            }
            for p in placed {
                // Cells are vertically aligned after the row height is known,
                // by shifting what they already emitted.
                let dy = match styles[boxes[p.cell].style].vertical_align {
                    VAlign::Middle => (row_h - p.h) / 2.0,
                    VAlign::Bottom => row_h - p.h,
                    _ => 0.0,
                };
                if dy > 0.0 {
                    for it in &mut self.out[p.items] {
                        it.shift(dy);
                    }
                    for l in &mut self.links[p.links] {
                        l.0.y += dy;
                    }
                }
                if let Some((slot, color)) = p.bg {
                    self.fill(Some(slot), Rect { x: p.x, y, w: p.w, h: row_h }, color);
                }
                if ri == 0 && first_baseline.is_none() {
                    first_baseline = p.baseline.map(|b| b + dy);
                }
            }
            if let Some((slot, color)) = row_bg {
                let rect =
                    Rect { x: tx + pl + spacing, y, w: (tw - chrome + spacing * (n as f32 - 1.0)).max(0.0), h: row_h };
                self.fill(Some(slot), rect, color);
            }
            y += row_h + spacing;
        }
        let bottom = y + pb;
        if let Some((slot, color)) = bg {
            self.fill(Some(slot), Rect { x: tx, y: top, w: tw, h: bottom - top }, color);
        }
        flow.y = bottom;
        flow.margin = Margin::default();
        flow.margin.push(mb);
        first_baseline
    }

    /// Lays out a cell's contents as an independent formatting context.
    /// Returns (border-box height, first baseline).
    fn layout_cell(&mut self, cell: BoxId, cx: f32, cw: f32, y: f32) -> (f32, Option<f32>) {
        let boxes = self.boxes;
        let styles = self.styles;
        let b = &boxes[cell];
        let st = &styles[b.style];
        let g = geometry(st, false, cw);
        let [pt, pr, pb, pl] = g.padding;
        let content_x = cx + pl;
        let content_w = (cw - pl - pr).max(0.0);
        let mut f = Flow::new(y + pt);
        let baseline = match &b.kind {
            Kind::Inline(_) => self.layout_inline(cell, content_x, content_w, &mut f),
            Kind::Block(children) | Kind::Row(children) => {
                let mut fb = None;
                for &c in children {
                    let r = self.layout_box(c, content_x, content_w, &mut f);
                    fb = fb.or(r);
                }
                fb
            }
            Kind::Table { .. } => self.layout_table(cell, content_x, content_w, &mut f),
            Kind::Hr => None,
        };
        let end = (f.y + f.margin.value()).max(y + pt);
        (end - y + pb, baseline)
    }
}

fn flush_word(atoms: &mut Vec<Atom>, word: &mut Vec<Seg>) {
    if !word.is_empty() {
        atoms.push(Atom::Word(std::mem::take(word)));
    }
}

fn place_pieces(line: &mut Line, pieces: &[Piece], segs: &[Seg]) {
    for p in pieces {
        let item = segs[p.seg as usize].item;
        line.frags.push(Frag { item, x: line.w, w: p.w, kind: FragKind::Text(*p) });
        line.w += p.w;
    }
}

/// Splits a word so the head fits in `avail`, breaking between clusters.
/// The head always gets at least one cluster so breaking makes progress.
fn split_pieces(pieces: &[Piece], atoms: &[Atom], avail: f32) -> (Vec<Piece>, Vec<Piece>) {
    let mut head = Vec::new();
    let mut acc = 0.0f32;
    for (pi, p) in pieces.iter().enumerate() {
        if acc + p.w <= avail {
            head.push(*p);
            acc += p.w;
            continue;
        }
        let Atom::Word(segs) = &atoms[p.atom as usize] else { break };
        let seg = &segs[p.seg as usize];
        let end_pen = |k: u32| seg.glyphs.get(k as usize).map_or(seg.w, |g| g.pen);
        let mut cut = None;
        for k in p.g0 + 1..p.g1 {
            let g = &seg.glyphs[k as usize];
            if g.cluster == seg.glyphs[k as usize - 1].cluster {
                continue;
            }
            if acc + (g.pen - p.x0) <= avail {
                cut = Some(k);
            } else {
                break;
            }
        }
        if cut.is_none() && head.is_empty() {
            cut = (p.g0 + 1..p.g1)
                .find(|&k| seg.glyphs[k as usize].cluster != seg.glyphs[k as usize - 1].cluster)
                .or((p.g1 > p.g0 + 1).then_some(p.g1));
        }
        let mut tail = Vec::new();
        match cut {
            Some(k) if k < p.g1 => {
                let x_cut = end_pen(k);
                head.push(Piece { g1: k, w: x_cut - p.x0, ..*p });
                tail.push(Piece { g0: k, x0: x_cut, w: p.w - (x_cut - p.x0), ..*p });
            }
            Some(_) => head.push(*p),
            None => tail.push(*p),
        }
        tail.extend_from_slice(&pieces[pi + 1..]);
        return (head, tail);
    }
    (head, Vec::new())
}

/// Distributes `total` over columns: everyone gets their minimum, then the
/// remainder in proportion to how much more each column would like.
fn distribute(mins: &[f32], maxs: &[f32], total: f32) -> Vec<f32> {
    let smin: f32 = mins.iter().sum();
    let smax: f32 = maxs.iter().sum();
    if total >= smax {
        let extra = total - smax;
        if smax > 0.0 {
            maxs.iter().map(|m| m + extra * m / smax).collect()
        } else {
            let n = maxs.len().max(1) as f32;
            maxs.iter().map(|_| total / n).collect()
        }
    } else if total > smin && smax > smin {
        let t = (total - smin) / (smax - smin);
        mins.iter().zip(maxs).map(|(mn, mx)| mn + (mx - mn) * t).collect()
    } else {
        mins.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_whitespace_across_nodes() {
        let mut last = true;
        assert_eq!(collapse("  hello \n\t world  ", WhiteSpace::Normal, &mut last), "hello world ");
        assert_eq!(collapse("  again", WhiteSpace::Normal, &mut last), "again");
        let mut last = false;
        assert_eq!(collapse(" a  b ", WhiteSpace::Pre, &mut last), " a  b ");
        let mut last = true;
        assert_eq!(collapse("a  \n  b", WhiteSpace::PreLine, &mut last), "a\nb");
    }

    #[test]
    fn markers() {
        assert_eq!(marker_text(ListStyle::Decimal, 3).unwrap(), "3. ");
        assert_eq!(marker_text(ListStyle::LowerAlpha, 28).unwrap(), "ab. ");
        assert_eq!(marker_text(ListStyle::UpperRoman, 1994).unwrap(), "MCMXCIV. ");
        assert_eq!(marker_text(ListStyle::None, 1), None);
        assert_eq!(marker_text(ListStyle::LowerRoman, 0).unwrap(), "0. ");
        assert_eq!(marker_text(ListStyle::UpperAlpha, i64::MIN).unwrap(), format!("{}. ", i64::MIN));
    }

    #[test]
    fn distributes_column_widths() {
        assert_eq!(distribute(&[10.0, 10.0], &[20.0, 60.0], 80.0), vec![20.0, 60.0]);
        assert_eq!(distribute(&[10.0, 10.0], &[20.0, 60.0], 160.0), vec![40.0, 120.0]);
        assert_eq!(distribute(&[10.0, 10.0], &[20.0, 60.0], 10.0), vec![10.0, 10.0]);
        let mid = distribute(&[10.0, 10.0], &[20.0, 60.0], 50.0);
        assert!((mid[0] - 15.0).abs() < 1e-4 && (mid[1] - 35.0).abs() < 1e-4);
    }

    #[test]
    fn image_size_keeps_aspect_and_fits() {
        let s = ImageSize {
            width: Len::Auto,
            max_width: None,
            attr_w: None,
            attr_h: None,
            intrinsic: Some((400.0, 200.0)),
        };
        assert_eq!(s.resolve(1000.0), (400.0, 200.0));
        assert_eq!(s.resolve(100.0), (100.0, 50.0));
        let s = ImageSize { width: Len::Px(200.0), ..s };
        assert_eq!(s.resolve(1000.0), (200.0, 100.0));
        let s = ImageSize { width: Len::Auto, attr_h: Some(50.0), ..s };
        assert_eq!(s.resolve(1000.0), (100.0, 50.0));
    }
}
