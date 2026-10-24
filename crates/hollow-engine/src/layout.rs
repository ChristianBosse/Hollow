//! Box construction, layout and the display list.
//!
//! Three passes: build a box tree from the DOM (blocks, inline runs, flex
//! containers, tables), lay it out top to bottom for one viewport width, and
//! record what to paint as a flat display list in page coordinates (CSS px).
//! Painting is a separate, cheap step so scrolling never relayouts.
//!
//! Supported: block flow with margin collapsing, borders, heights with
//! min/max, inline text with line breaking and alignment, inline-blocks,
//! lists, images, preformatted text, tables with automatic column sizing,
//! flexbox (`flex.rs`), floats, relative/absolute/fixed positioning,
//! `z-index` and overflow clipping (`position.rs`). Grid containers are laid
//! out as blocks for now.
//!
//! Boxes whose position depends on their own size (flex items, floats,
//! inline-blocks, absolutely positioned boxes) are laid out at the origin
//! first and then moved into place by shifting what they emitted, so every
//! box is laid out exactly once.

mod flex;
mod position;

use std::collections::BTreeMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use tiny_skia::Pixmap;
use url::Url;

use crate::Resources;
use crate::css::Color;
use crate::dom::{Dom, Element, NodeData, NodeId};
use crate::style::{Clear, Display, Float, Len, ListStyle, Style, TextAlign, TextTransform, VAlign, WhiteSpace};
use crate::text::{self, FaceId, Shaper};

use position::FloatCtx;

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
    /// A flex container; the children are its flex items.
    Flex(Vec<BoxId>),
    /// A block-level image or form control.
    Replaced(Item),
    Table {
        captions: Vec<BoxId>,
        rows: Vec<BoxId>,
    },
    Row(Vec<BoxId>),
    Hr,
}

enum Item {
    Text {
        text: String,
        style: NodeId,
        link: Option<u32>,
        bg: Option<Color>,
    },
    Image {
        image: Option<u32>,
        size: ImageSize,
        style: NodeId,
        link: Option<u32>,
    },
    Field {
        w: f32,
        h: f32,
        style: NodeId,
    },
    Break {
        style: NodeId,
    },
    /// An inline-block (or inline-flex) box, laid out as one unit.
    Atomic {
        boxid: BoxId,
        style: NodeId,
    },
    /// An absolutely positioned box. In the line it only marks its static
    /// position; it is laid out with its containing block.
    Abs {
        boxid: BoxId,
        style: NodeId,
    },
}

impl Item {
    fn style(&self) -> NodeId {
        match self {
            Item::Text { style, .. }
            | Item::Image { style, .. }
            | Item::Field { style, .. }
            | Item::Break { style }
            | Item::Atomic { style, .. }
            | Item::Abs { style, .. } => *style,
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
    height: Len,
    max_width: Option<Len>,
    attr_w: Option<f32>,
    attr_h: Option<f32>,
    intrinsic: Option<(f32, f32)>,
}

impl ImageSize {
    /// Used size, scaled down to fit `avail` as `max-width: 100%` would.
    /// An infinite `avail` asks for the preferred size, where percentages
    /// don't apply.
    fn resolve(&self, avail: f32) -> (f32, f32) {
        let usable = |l: Len| l.is_definite() && (avail.is_finite() || !l.has_pct());
        let ratio = match (self.intrinsic, self.attr_w, self.attr_h) {
            (Some((w, h)), _, _) if w > 0.0 && h > 0.0 => Some(w / h),
            (_, Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some(w / h),
            _ => None,
        };
        let width = usable(self.width).then(|| self.width.resolve(avail));
        let height = match self.height {
            Len::Px(h) => Some(h),
            _ => self.attr_h,
        };
        let (mut w, mut h) = match (width, height) {
            (Some(w), Some(h)) if matches!(self.height, Len::Px(_)) => (w, h),
            (Some(w), _) => (w, ratio.map(|r| w / r).or(height).unwrap_or(w)),
            (None, Some(h)) => (ratio.map(|r| h * r).or(self.attr_w).or(self.intrinsic.map(|i| i.0)).unwrap_or(h), h),
            (None, None) => self.intrinsic.unwrap_or((0.0, 0.0)),
        };
        if let Some(max) = self.max_width.filter(|m| usable(*m)).map(|m| m.resolve(avail))
            && w > max
            && w > 0.0
        {
            h = h * max / w;
            w = max;
        }
        if w > avail && avail > 0.0 && avail.is_finite() {
            h = h * avail / w;
            w = avail;
        }
        (clamp_extent(w), clamp_extent(h))
    }
}

fn clamp_extent(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, MAX_EXTENT) } else { 0.0 }
}

/// Elements drawn by the engine itself rather than from their children.
fn is_replaced(name: &str) -> bool {
    matches!(name, "img" | "input" | "textarea" | "select")
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

    /// The box of a block-level element, or the inside of an atomic inline.
    fn build_level(
        &mut self,
        node: NodeId,
        display: Display,
        depth: usize,
        marker: Option<String>,
        ctx: InlineCtx,
    ) -> Option<BoxId> {
        if self.full() {
            return None;
        }
        let el = self.dom.element(node);
        let name = el.map_or("", |e| e.name.as_str());
        let kind = if name == "hr" {
            Kind::Hr
        } else if let Some(el) = el.filter(|_| is_replaced(name)) {
            let mut items = Vec::new();
            self.replaced(node, el, &mut items, ctx);
            match items.pop() {
                Some(item @ (Item::Image { .. } | Item::Field { .. })) if items.is_empty() => Kind::Replaced(item),
                Some(item) => {
                    items.push(item);
                    Kind::Inline(items)
                }
                None => Kind::Inline(Vec::new()),
            }
        } else {
            match display {
                Display::Table => return self.build_table(node, depth, ctx),
                Display::Flex | Display::InlineFlex => self.build_flex(node, depth, ctx),
                _ => self.build_container(node, depth, ctx),
            }
        };
        let colspan = el
            .and_then(|e| e.attr("colspan"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, MAX_COLSPAN);
        Some(self.push_box(LBox { style: node, anon: false, kind, marker, colspan }))
    }

    fn build_block(&mut self, node: NodeId, depth: usize, marker: Option<String>, ctx: InlineCtx) -> Option<BoxId> {
        let display = self.styles[node].display;
        self.build_level(node, display, depth, marker, ctx)
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

    /// Every child element of a flex container is a flex item, and so is
    /// every run of text between them.
    fn build_flex(&mut self, node: NodeId, depth: usize, ctx: InlineCtx) -> Kind {
        let mut kids = Vec::new();
        let mut run = Vec::new();
        self.last_space = true;
        self.flex_items(node, node, depth, InlineCtx { link: ctx.link, bg: None }, &mut kids, &mut run);
        self.flush(node, &mut kids, &mut run);
        Kind::Flex(kids)
    }

    fn flex_items(
        &mut self,
        container: NodeId,
        node: NodeId,
        depth: usize,
        ctx: InlineCtx,
        kids: &mut Vec<BoxId>,
        run: &mut Vec<Item>,
    ) {
        let dom = self.dom;
        for &child in &dom.nodes[node].children {
            if self.full() {
                return;
            }
            match &dom.nodes[child].data {
                NodeData::Text(t) => {
                    let st = &self.styles[child];
                    let text = transform(&collapse(t, st.white_space, &mut self.last_space), st.text_transform);
                    if !text.is_empty() {
                        run.push(Item::Text { text, style: child, link: ctx.link, bg: ctx.bg });
                    }
                }
                NodeData::Element(el) => {
                    let st = &self.styles[child];
                    let link = self.link_of(child, el, ctx.link);
                    let cctx = InlineCtx { link, ..ctx };
                    match st.display {
                        Display::None => {}
                        Display::Contents => self.flex_items(container, child, depth + 1, cctx, kids, run),
                        _ if depth >= MAX_BLOCK_DEPTH => {
                            self.inline_element(container, child, el, kids, run, cctx, depth);
                        }
                        d => {
                            self.flush(container, kids, run);
                            kids.extend(self.build_level(child, d.blockified(), depth + 1, None, cctx));
                            self.last_space = true;
                        }
                    }
                }
                NodeData::Root => {}
            }
        }
    }

    fn flush(&mut self, container: NodeId, blocks: &mut Vec<BoxId>, items: &mut Vec<Item>) {
        let items = std::mem::take(items);
        self.last_space = true;
        let meaningful = items.iter().any(|i| match i {
            Item::Text { text, style, .. } => {
                !self.styles[*style].white_space.collapses_spaces() || text.chars().any(|c| !c.is_ascii_whitespace())
            }
            Item::Abs { .. } => false,
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
        } else {
            // Only white space and out-of-flow boxes: those become block-level.
            blocks.extend(items.iter().filter_map(|i| match i {
                Item::Abs { boxid, .. } => Some(*boxid),
                _ => None,
            }));
        }
    }

    fn link_of(&mut self, node: NodeId, el: &Element, link: Option<u32>) -> Option<u32> {
        match self.dom.nodes[node].url.as_ref() {
            Some(url) if matches!(el.name.as_str(), "a" | "area") => Some(self.link_id(node, url)),
            _ => link,
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
                    let display = st.display;
                    if display == Display::None {
                        continue;
                    }
                    let link = self.link_of(child, el, ctx.link);
                    let cctx = InlineCtx { link, ..ctx };
                    let block_ctx = InlineCtx { link, bg: None };
                    if depth >= MAX_BLOCK_DEPTH {
                        // Too deep for more box structure: everything flows inline.
                        self.inline_element(container, child, el, blocks, items, cctx, depth);
                        continue;
                    }
                    if st.out_of_flow() {
                        let last_space = self.last_space;
                        if let Some(b) = self.build_level(child, display, depth + 1, None, block_ctx) {
                            items.push(Item::Abs { boxid: b, style: child });
                        }
                        self.last_space = last_space;
                        continue;
                    }
                    if st.float != Float::None {
                        self.flush(container, blocks, items);
                        blocks.extend(self.build_level(child, display, depth + 1, None, block_ctx));
                        self.last_space = true;
                        continue;
                    }
                    match display {
                        Display::None => {}
                        Display::Inline => self.inline_element(container, child, el, blocks, items, cctx, depth),
                        Display::Contents => self.collect(container, child, blocks, items, cctx, depth + 1),
                        Display::InlineBlock | Display::InlineFlex | Display::InlineGrid => {
                            if is_replaced(&el.name) {
                                self.inline_element(container, child, el, blocks, items, cctx, depth);
                            } else if let Some(b) = self.build_level(child, display, depth + 1, None, block_ctx) {
                                items.push(Item::Atomic { boxid: b, style: child });
                                self.last_space = false;
                            }
                        }
                        Display::Table => {
                            self.flush(container, blocks, items);
                            if let Some(b) = self.build_table(child, depth + 1, block_ctx) {
                                blocks.push(b);
                            }
                        }
                        Display::Block
                        | Display::FlowRoot
                        | Display::ListItem
                        | Display::Flex
                        | Display::Grid
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
                            if let Some(b) = self.build_level(child, display, depth + 1, marker, block_ctx) {
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
            name if is_replaced(name) => self.replaced(node, el, items, ctx),
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

    /// Images and form controls, as inline items.
    fn replaced(&mut self, node: NodeId, el: &Element, items: &mut Vec<Item>, ctx: InlineCtx) {
        let st = &self.styles[node];
        let text_item = |b: &mut Self, text: &str, bg: Option<Color>| {
            b.last_space = false;
            Item::Text { text: text.to_owned(), style: node, link: ctx.link, bg }
        };
        match el.name.as_str() {
            "img" => {
                let image = self.dom.nodes[node].url.as_ref().and_then(|u| self.image_id(u));
                let attr = |n: &str| el.attr(n).and_then(crate::style::dimension_px);
                let size = ImageSize {
                    width: st.width,
                    height: st.height,
                    max_width: st.max_width,
                    attr_w: attr("width"),
                    attr_h: attr("height"),
                    intrinsic: image.map(|i| {
                        let p = &self.images[i as usize];
                        (p.width() as f32, p.height() as f32)
                    }),
                };
                let sized = (size.attr_h.is_some() || matches!(size.height, Len::Px(_)))
                    && (st.width.is_definite() || size.attr_w.is_some());
                if image.is_some() || sized {
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
                    "hidden" | "image" | "file" | "range" | "color" => {}
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
            _ => {}
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
                d => captions.extend(self.build_level(child, d.blockified(), depth + 1, None, ctx)),
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
                cells.extend(self.build_level(child, Display::TableCell, depth + 1, None, ctx));
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

#[derive(Clone, Copy, Debug, PartialEq)]
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

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        Rect { x: x0, y: y0, w: (x1 - x0).max(0.0), h: (y1 - y0).max(0.0) }
    }

    fn shifted(self, dx: f32, dy: f32) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, ..self }
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
    /// A rounded rectangle: filled, or outlined when `stroke` > 0.
    Round {
        rect: Rect,
        radius: f32,
        stroke: f32,
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
    fn shift(&mut self, dx: f32, dy: f32) {
        match self {
            DisplayItem::Rect { rect, .. } | DisplayItem::Round { rect, .. } | DisplayItem::Image { rect, .. } => {
                *rect = rect.shifted(dx, dy)
            }
            DisplayItem::Text { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
        }
    }

    /// Vertical extent, for culling and document height.
    pub fn y_range(&self) -> (f32, f32) {
        match self {
            DisplayItem::Rect { rect, .. } | DisplayItem::Round { rect, .. } | DisplayItem::Image { rect, .. } => {
                (rect.y, rect.y + rect.h)
            }
            DisplayItem::Text { y, size, .. } => (y - size * 1.3, y + size * 0.6),
        }
    }
}

/// Everything layout produces; [`crate::Layout`] wraps it.
pub(crate) struct Laid {
    pub width: f32,
    pub height: f32,
    pub canvas: Color,
    /// In paint order.
    pub items: Vec<DisplayItem>,
    /// For each item, its clip rectangle in `clip_rects` (0 is "none").
    pub clips: Vec<u32>,
    pub clip_rects: Vec<Rect>,
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

    /// Where content would start, without placing anything.
    fn next(&self) -> f32 {
        self.y + self.margin.value()
    }
}

enum Atom {
    Word(Vec<Seg>),
    Space { item: u32, w: f32, can_break: bool, collapsible: bool },
    Break,
    Object { item: u32 },
    Abs { item: u32 },
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
    /// An image, form control or inline-block: `base` is its baseline,
    /// measured from its top.
    Object {
        h: f32,
        base: f32,
    },
    Abs,
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
    /// Whether anything but out-of-flow markers is on the line.
    content: bool,
}

impl Line {
    fn trim(&mut self) {
        while matches!(self.frags.last(), Some(Frag { kind: FragKind::Space, .. })) {
            self.frags.pop();
        }
        self.w = self.frags.iter().rev().find(|f| !matches!(f.kind, FragKind::Abs)).map_or(0.0, |f| f.x + f.w);
    }
}

/// An atomic inline laid out at the origin, waiting for its line.
#[derive(Clone)]
struct Measured {
    /// Margin-box size, and the baseline from the margin-box top.
    w: f32,
    h: f32,
    base: f32,
    span: Span,
}

/// The display items, links, clips and pending boxes one subtree emitted,
/// so it can be moved after the fact.
#[derive(Clone, Copy)]
struct Mark {
    out: usize,
    links: usize,
    clips: usize,
    pending: usize,
}

#[derive(Clone, Debug)]
struct Span {
    out: Range<usize>,
    links: Range<usize>,
    clips: Range<usize>,
    pending: Range<usize>,
}

/// Background, borders and clip of a laid-out box, kept so a flex container
/// can stretch the box afterwards.
#[derive(Clone, Copy)]
struct Deco {
    /// Reserved display-list slots: the background, and four borders.
    bg: Option<usize>,
    borders: Option<usize>,
    clip: u32,
    /// Border box.
    rect: Rect,
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
    deco: Vec<Option<Deco>>,
    glyph_budget: usize,
    out: Vec<DisplayItem>,
    /// Paint layer (`z-index`) and clip of each display item.
    item_z: Vec<i32>,
    item_clip: Vec<u32>,
    /// Clip rectangles with their enclosing clip, which they are intersected
    /// with at the end. Index 0 is "no clip".
    clip_rects: Vec<(Rect, u32)>,
    glyphs: Vec<PlacedGlyph>,
    links: Vec<(Rect, u32)>,
    /// The current paint layer and clip.
    z: i32,
    clip: u32,
    /// The float context of each enclosing block formatting context.
    floats: Vec<FloatCtx>,
    /// Absolutely positioned boxes waiting for their containing block, one
    /// list per enclosing positioned box; the first is the initial
    /// containing block's.
    abs: Vec<Vec<position::Pending>>,
    pending_seq: usize,
    /// Definite height of the containing block, for percentage heights.
    cb_h: Option<f32>,
    /// A size the next laid-out box must take, set by whoever placed it
    /// (flex items, floats, inline-blocks, absolutely positioned boxes).
    force_w: Option<f32>,
    force_h: Option<f32>,
    /// The element whose background already paints the canvas.
    canvas_node: Option<NodeId>,
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

    let (canvas, canvas_node) = canvas_color(dom, styles);
    let Builder { boxes, links: link_urls, images, .. } = b;
    let mut ctx = Ctx {
        boxes: &boxes,
        styles,

        shaper: Shaper::new(),
        atoms: (0..boxes.len()).map(|_| None).collect(),
        intrinsic: vec![None; boxes.len()],
        deco: vec![None; boxes.len()],
        glyph_budget: MAX_GLYPHS,
        out: Vec::new(),
        item_z: Vec::new(),
        item_clip: Vec::new(),
        clip_rects: vec![(Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }, 0)],
        glyphs: Vec::new(),
        links: Vec::new(),
        z: 0,
        clip: 0,
        floats: vec![FloatCtx::default()],
        abs: vec![Vec::new()],
        pending_seq: 0,
        cb_h: None,
        force_w: None,
        force_h: None,
        canvas_node,
    };
    let mut flow = Flow::new(0.0);
    for &r in &roots {
        ctx.layout_box(r, 0.0, width, &mut flow);
    }
    let mut height = flow.y + flow.margin.value().max(0.0);
    height = height.max(ctx.floats_bottom(Clear::Both));
    // The initial containing block is the whole page, so `bottom: 0` boxes
    // (cookie banners and the like) end up at its end rather than on top of
    // the content.
    ctx.finish_frame(Rect { x: 0.0, y: 0.0, w: width, h: clamp_extent(height) });
    // Boxes and images that overflow the flow (negative margins, wide
    // content) still extend the page; text extents are only estimates.
    for it in &ctx.out {
        if !matches!(it, DisplayItem::Text { .. }) {
            height = height.max(it.y_range().1);
        }
    }
    let Ctx { out, item_z, item_clip, clip_rects, glyphs, links, .. } = ctx;
    // Paint order: by layer, then in document order.
    let mut order: Vec<usize> = (0..out.len()).collect();
    order.sort_by_key(|&i| item_z[i]);
    let mut slots: Vec<Option<DisplayItem>> = out.into_iter().map(Some).collect();
    let items: Vec<DisplayItem> = order.iter().filter_map(|&i| slots[i].take()).collect();
    let clips: Vec<u32> = order.iter().map(|&i| item_clip[i]).collect();
    // Nested clips intersect; a clip is always created after its parent.
    let mut resolved: Vec<Rect> = Vec::with_capacity(clip_rects.len());
    for (i, (r, parent)) in clip_rects.iter().enumerate() {
        let p = *parent as usize;
        resolved.push(if i == 0 || p == 0 || p >= i { *r } else { r.intersect(&resolved[p]) });
    }
    Laid {
        width,
        height: clamp_extent(height),
        canvas,
        items,
        clips,
        clip_rects: resolved,
        glyphs,
        links,
        link_urls,
        images: images.clone(),
    }
}

/// The root's background (or else body's) paints the whole canvas, per CSS.
/// Returns the color and the element it came from.
fn canvas_color(dom: &Dom, styles: &[Style]) -> (Color, Option<NodeId>) {
    let find = |name: &str| {
        dom.nodes.iter().enumerate().find_map(|(i, n)| match &n.data {
            NodeData::Element(e) if e.name == name => styles[i].background.map(|c| (c, i)),
            _ => None,
        })
    };
    let Some((c, node)) = find("html").or_else(|| find("body")) else { return (Color::WHITE, None) };
    // The canvas is always opaque: blend a translucent color over white.
    let blend = |v: u8| ((u16::from(v) * u16::from(c.a) + 255 * (255 - u16::from(c.a)) + 127) / 255) as u8;
    (Color::rgb(blend(c.r), blend(c.g), blend(c.b)), Some(node))
}

struct BoxGeom {
    margin: [f32; 4],
    padding: [f32; 4],
    border: [f32; 4],
    /// Which margins are `auto` (they count as 0 here).
    auto: [bool; 4],
}

impl BoxGeom {
    fn hpad(&self) -> f32 {
        self.padding[1] + self.padding[3] + self.border[1] + self.border[3]
    }
    fn vpad(&self) -> f32 {
        self.padding[0] + self.padding[2] + self.border[0] + self.border[2]
    }
}

fn geometry(st: &Style, anon: bool, cb_w: f32) -> BoxGeom {
    if anon {
        return BoxGeom { margin: [0.0; 4], padding: [0.0; 4], border: [0.0; 4], auto: [false; 4] };
    }
    let r = |l: Len| {
        let v = l.resolve(cb_w);
        if v.is_finite() { v.clamp(-MAX_EXTENT, MAX_EXTENT) } else { 0.0 }
    };
    BoxGeom {
        margin: st.margin.map(r),
        padding: st.padding.map(|l| r(l).max(0.0)),
        border: st.border_width,
        auto: st.margin.map(|m| m == Len::Auto),
    }
}

/// Whether a box establishes a new block formatting context: its floats
/// stay inside, and its margins never collapse with its children's.
fn new_context(st: &Style, kind: &Kind) -> bool {
    st.float != Float::None
        || st.out_of_flow()
        || st.clips()
        || matches!(
            st.display,
            Display::FlowRoot
                | Display::InlineBlock
                | Display::InlineFlex
                | Display::InlineGrid
                | Display::Flex
                | Display::Grid
                | Display::TableCell
        )
        || matches!(kind, Kind::Flex(_))
}

impl Ctx<'_> {
    fn push(&mut self, it: DisplayItem) {
        self.out.push(it);
        self.item_z.push(self.z);
        self.item_clip.push(self.clip);
    }

    fn reserve(&mut self) -> usize {
        self.push(DisplayItem::Rect { rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }, color: Color::WHITE });
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

    fn mark(&self) -> Mark {
        Mark { out: self.out.len(), links: self.links.len(), clips: self.clip_rects.len(), pending: self.pending_seq }
    }

    fn span_since(&self, m: Mark) -> Span {
        Span {
            out: m.out..self.out.len(),
            links: m.links..self.links.len(),
            clips: m.clips..self.clip_rects.len(),
            pending: m.pending..self.pending_seq,
        }
    }

    /// Moves everything a subtree emitted, including absolutely positioned
    /// descendants still waiting for their containing block.
    fn shift_span(&mut self, s: &Span, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        for it in &mut self.out[s.out.clone()] {
            it.shift(dx, dy);
        }
        for l in &mut self.links[s.links.clone()] {
            l.0 = l.0.shifted(dx, dy);
        }
        for c in &mut self.clip_rects[s.clips.clone()] {
            c.0 = c.0.shifted(dx, dy);
        }
        for frame in &mut self.abs {
            for p in frame.iter_mut().filter(|p| s.pending.contains(&p.seq)) {
                p.x += dx;
                p.y += dy;
            }
        }
    }

    fn new_clip(&mut self) -> u32 {
        self.clip_rects.push((Rect { x: 0.0, y: 0.0, w: MAX_EXTENT, h: MAX_EXTENT }, self.clip));
        (self.clip_rects.len() - 1) as u32
    }

    /// Reserves slots for a box's background and borders, so they paint
    /// under its contents.
    fn reserve_deco(&mut self, st: &Style, b: &LBox) -> (Option<usize>, Option<usize>) {
        if b.anon || !st.visible {
            return (None, None);
        }
        let bg = match st.background {
            Some(c) if c.a > 0 && !st.masked && self.canvas_node != Some(b.style) => Some(self.reserve()),
            _ => None,
        };
        let borders = if st.border_width.iter().any(|w| *w > 0.0) {
            let first = self.reserve();
            for _ in 0..3 {
                self.reserve();
            }
            Some(first)
        } else {
            None
        };
        (bg, borders)
    }

    fn paint_deco(&mut self, st: &Style, d: &Deco) {
        let r = d.rect;
        let radius = st.radius.resolve(r.w.min(r.h)).clamp(0.0, r.w.min(r.h) / 2.0);
        let mut set = |slot: usize, it: DisplayItem| {
            if let Some(o) = self.out.get_mut(slot) {
                *o = it;
            }
        };
        if let (Some(slot), Some(color)) = (d.bg, st.background) {
            set(
                slot,
                if radius > 0.0 {
                    DisplayItem::Round { rect: r, radius, stroke: 0.0, color }
                } else {
                    DisplayItem::Rect { rect: r, color }
                },
            );
        }
        if let Some(first) = d.borders {
            let [t, right, b, l] = st.border_width;
            let c = st.border_color;
            let uniform = t == right && t == b && t == l && c.iter().all(|x| *x == c[0]);
            let empty = DisplayItem::Rect { rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }, color: c[0] };
            if radius > 0.0 && uniform {
                set(first, DisplayItem::Round { rect: r, radius, stroke: t, color: c[0] });
                for i in 1..4 {
                    set(first + i, empty.clone());
                }
            } else {
                let mid = (r.h - t - b).max(0.0);
                let rects = [
                    Rect { x: r.x, y: r.y, w: r.w, h: t },
                    Rect { x: r.x + r.w - right, y: r.y + t, w: right, h: mid },
                    Rect { x: r.x, y: r.y + r.h - b, w: r.w, h: b },
                    Rect { x: r.x, y: r.y + t, w: l, h: mid },
                ];
                for (i, rect) in rects.into_iter().enumerate() {
                    let w = st.border_width[i];
                    set(first + i, if w > 0.0 { DisplayItem::Rect { rect, color: c[i] } } else { empty.clone() });
                }
            }
        }
        if d.clip != 0
            && let Some(c) = self.clip_rects.get_mut(d.clip as usize)
        {
            let [t, right, b, l] = st.border_width;
            c.0 = Rect { x: r.x + l, y: r.y + t, w: (r.w - l - right).max(0.0), h: (r.h - t - b).max(0.0) };
        }
    }

    /// Gives an already laid-out box a new border-box size, as flex
    /// stretching does. Its contents stay where they are.
    fn resize_deco(&mut self, id: BoxId, w: Option<f32>, h: Option<f32>) {
        let Some(mut d) = self.deco[id] else { return };
        if let Some(w) = w {
            d.rect.w = w.max(0.0);
        }
        if let Some(h) = h {
            d.rect.h = h.max(0.0);
        }
        self.deco[id] = Some(d);
        let st = &self.styles[self.boxes[id].style];
        self.paint_deco(st, &d);
    }

    fn layout_box(&mut self, id: BoxId, x: f32, cb_w: f32, flow: &mut Flow) -> Option<f32> {
        let b = &self.boxes[id];
        let st = &self.styles[b.style];
        if !b.anon {
            if st.out_of_flow() {
                self.add_pending(id, x, flow.next());
                return None;
            }
            if st.float != Float::None {
                self.layout_float(id, x, cb_w, flow);
                return None;
            }
        }
        self.layout_in_flow(id, x, cb_w, flow)
    }

    fn layout_in_flow(&mut self, id: BoxId, x: f32, cb_w: f32, flow: &mut Flow) -> Option<f32> {
        match &self.boxes[id].kind {
            Kind::Table { .. } => self.layout_table(id, x, cb_w, flow),
            _ => self.layout_block(id, x, cb_w, flow),
        }
    }

    /// Lays a box out at the origin with a given border-box width and
    /// returns (margin-box height, baseline, what it emitted). The caller
    /// moves it into place with [`Ctx::shift_span`].
    fn layout_detached(
        &mut self,
        id: BoxId,
        border_w: f32,
        cb_w: f32,
        force_h: Option<f32>,
    ) -> (f32, Option<f32>, Span) {
        let start = self.mark();
        let mut f = Flow::new(0.0);
        self.force_w = Some(border_w);
        self.force_h = force_h;
        let baseline = self.layout_in_flow(id, 0.0, cb_w, &mut f);
        self.force_w = None;
        self.force_h = None;
        (f.y + f.margin.value().max(0.0), baseline, self.span_since(start))
    }

    /// Content width of a block in flow, and whether something other than
    /// the containing block decided it.
    fn used_width(&mut self, id: BoxId, st: &Style, avail: f32, cb_w: f32, hpad: f32) -> (f32, bool) {
        let b = &self.boxes[id];
        let auto = (avail - hpad).max(0.0);
        if b.anon {
            return (auto, false);
        }
        let adj = if st.border_box { hpad } else { 0.0 };
        let mut w = auto;
        let mut explicit = false;
        if st.width.is_definite() {
            w = st.width.resolve(cb_w) - adj;
            explicit = true;
        } else if matches!(st.width, Len::MinContent | Len::MaxContent | Len::FitContent) {
            let (mn, mx) = self.intrinsic_inner(id);
            w = match st.width {
                Len::MinContent => mn,
                Len::MaxContent => mx,
                _ => mx.min(auto).max(mn),
            };
            explicit = true;
        } else if let Kind::Replaced(item) = &b.kind {
            w = self.replaced_size(item, auto, None).0;
            explicit = true;
        }
        if let Some(max) = st.max_width.filter(|m| m.is_definite()) {
            let m = max.resolve(cb_w) - adj;
            if w > m {
                w = m;
                explicit = true;
            }
        }
        if st.min_width.is_definite() {
            let m = st.min_width.resolve(cb_w) - adj;
            if w < m {
                w = m;
                explicit = true;
            }
        }
        (clamp_extent(w), explicit)
    }

    /// Border-box width of a shrink-to-fit box (floats, inline-blocks,
    /// absolutely positioned boxes): the specified width if there is one,
    /// else its content's preferred width within `avail` (which excludes
    /// margins).
    fn shrink_width(&mut self, id: BoxId, avail: f32, cb_w: f32) -> f32 {
        let st = &self.styles[self.boxes[id].style];
        let g = geometry(st, self.boxes[id].anon, cb_w);
        let hpad = g.hpad();
        let adj = if st.border_box { 0.0 } else { hpad };
        let mut w = if st.width.is_definite() {
            st.width.resolve(cb_w) + adj
        } else {
            let (mn, mx) = self.intrinsic_inner(id);
            let fit = match st.width {
                Len::MinContent => mn,
                Len::MaxContent => mx,
                _ => mx.min(avail - hpad).max(mn),
            };
            fit + hpad
        };
        if let Some(max) = st.max_width.filter(|m| m.is_definite()) {
            w = w.min(max.resolve(cb_w) + adj);
        }
        if st.min_width.is_definite() {
            w = w.max(st.min_width.resolve(cb_w) + adj);
        }
        clamp_extent(w.max(hpad))
    }

    /// Resolves a height against the containing block's definite height.
    fn definite_height(&self, l: Len) -> Option<f32> {
        match l {
            Len::Px(v) => Some(v),
            l if l.has_pct() => self.cb_h.map(|h| l.resolve(h)),
            _ => None,
        }
    }

    fn layout_block(&mut self, id: BoxId, x: f32, cb_w: f32, flow: &mut Flow) -> Option<f32> {
        let boxes = self.boxes;
        let styles = self.styles;
        let b = &boxes[id];
        let st = &styles[b.style];
        let force_w = self.force_w.take();
        let force_h = self.force_h.take();
        let start = self.mark();
        let g = geometry(st, b.anon, cb_w);
        let [mt, mut mr, mb, mut ml] = g.margin;
        let [pt, _, pb, pl] = g.padding;
        let [bt, br, bb, bl] = g.border;
        let (hpad, vpad) = (g.hpad(), g.vpad());
        let placed = force_w.is_some();
        let bfc = !b.anon && (placed || new_context(st, &b.kind));
        if g.auto[3] {
            ml = 0.0;
        }
        if g.auto[1] {
            mr = 0.0;
        }
        let (mut w, explicit) = match force_w {
            Some(fw) => ((fw - hpad).max(0.0), true),
            None => self.used_width(id, st, cb_w - ml - mr, cb_w, hpad),
        };
        if explicit && !placed {
            let free = cb_w - w - hpad - ml - mr;
            match (g.auto[3], g.auto[1]) {
                (true, true) => ml += (free / 2.0).max(0.0),
                (true, false) => ml += free.max(0.0),
                _ => {}
            }
        }
        // Boxes placed by someone else (floats, flex items, ...) were
        // cleared by them, and are laid out at the origin meanwhile.
        if !b.anon && !placed && st.clear != Clear::None {
            self.clear(st.clear, flow);
        }
        let mut box_x = x + ml;
        // A new formatting context never overlaps floats: it moves in next
        // to them, narrowing if its width is automatic.
        if bfc && !placed {
            let (l, r) = self.band(flow.next(), 1.0, x, x + cb_w);
            if l > x || r < x + cb_w {
                box_x = l + ml;
                if !explicit {
                    w = (r - l - ml - mr - hpad).max(0.0);
                }
            }
        }
        let content_x = box_x + bl + pl;

        // Paint layer and clip for this box and its descendants.
        let saved = (self.cb_h, self.z, self.clip);
        let positioned = !b.anon && st.positioned();
        if positioned {
            if let Some(z) = st.z_index {
                self.z = z;
            }
            self.abs.push(Vec::new());
        }
        flow.margin.push(mt);
        let (bg, borders) = self.reserve_deco(st, b);
        let clip = if !b.anon && st.clips() {
            let c = self.new_clip();
            self.clip = c;
            c
        } else {
            0
        };
        let collapse_top = pt <= 0.0 && bt <= 0.0 && !bfc;
        let mut inner = if collapse_top {
            let f = Flow { y: flow.y, margin: flow.margin, first_place: None };
            flow.margin = Margin::default();
            f
        } else {
            let top = flow.place();
            Flow::new(top + bt + pt)
        };
        let explicit_top = (!collapse_top).then_some(inner.y - pt - bt);

        let adj = if st.border_box { vpad } else { 0.0 };
        let height = force_h
            .map(|h| (h - vpad).max(0.0))
            .or_else(|| (!b.anon).then(|| self.definite_height(st.height)).flatten().map(|h| (h - adj).max(0.0)));
        let min_h = if b.anon { 0.0 } else { self.definite_height(st.min_height).map_or(0.0, |h| (h - adj).max(0.0)) };
        let max_h = if b.anon {
            f32::INFINITY
        } else {
            st.max_height.and_then(|l| self.definite_height(l)).map_or(f32::INFINITY, |h| (h - adj).max(0.0))
        };
        let definite = height.map(|h| h.min(max_h).max(min_h));
        if !b.anon {
            self.cb_h = definite;
        }
        if bfc {
            self.floats.push(FloatCtx::default());
        }

        let first_baseline = self.layout_contents(id, content_x, w, &mut inner, definite);

        let floats_bottom = if bfc { self.floats.pop().map(|f| f.bottom(Clear::Both)) } else { None };
        let collapsed = explicit_top.is_none()
            && inner.first_place.is_none()
            && pb <= 0.0
            && bb <= 0.0
            && definite.is_none_or(|h| h <= 0.0)
            && min_h <= 0.0;
        let (border_top, border_bottom) = if collapsed {
            // Nothing placed and nothing separating the margins: the box
            // collapses through, and its margins join the surrounding ones.
            let y = inner.y + inner.margin.value().max(0.0);
            flow.y = inner.y;
            flow.margin = inner.margin;
            flow.margin.push(mb);
            (y, y)
        } else {
            let content_top = match explicit_top {
                Some(t) => t + bt + pt,
                None => match inner.first_place {
                    Some(y) => y,
                    None => inner.place(),
                },
            };
            let border_top = content_top - bt - pt;
            if collapse_top && flow.first_place.is_none() {
                flow.first_place = Some(border_top);
            }
            let pass_through = !bfc && pb <= 0.0 && bb <= 0.0 && definite.is_none() && min_h <= 0.0;
            let mut content_h = if pass_through { inner.y - content_top } else { inner.place() - content_top };
            if let Some(fb) = floats_bottom {
                content_h = content_h.max(fb - content_top);
            }
            content_h = match definite {
                Some(h) => h,
                None => content_h.min(max_h).max(min_h),
            };
            let bottom = content_top + clamp_extent(content_h) + pb + bb;
            flow.y = bottom;
            flow.margin = if pass_through { inner.margin } else { Margin::default() };
            flow.margin.push(mb);
            (border_top, bottom)
        };

        let deco = Deco {
            bg,
            borders,
            clip,
            rect: Rect { x: box_x, y: border_top, w: w + hpad, h: border_bottom - border_top },
        };
        self.paint_deco(st, &deco);
        self.deco[id] = Some(deco);
        if positioned {
            let r = deco.rect;
            let pad = Rect { x: r.x + bl, y: r.y + bt, w: (r.w - bl - br).max(0.0), h: (r.h - bt - bb).max(0.0) };
            self.finish_frame(pad);
        }
        (self.cb_h, self.z, self.clip) = saved;
        self.marker(b, st, content_x, border_top + bt + pt, first_baseline);
        if !b.anon && st.position == crate::style::Position::Relative {
            let (dx, dy) = self.relative_offset(st, cb_w);
            let span = self.span_since(start);
            self.shift_span(&span, dx, dy);
            return first_baseline.map(|y| y + dy);
        }
        first_baseline
    }

    /// Lays out a box's contents in its content box. Returns the first
    /// baseline.
    fn layout_contents(&mut self, id: BoxId, x: f32, w: f32, inner: &mut Flow, definite: Option<f32>) -> Option<f32> {
        let boxes = self.boxes;
        let st = &self.styles[boxes[id].style];
        match &boxes[id].kind {
            Kind::Block(children) | Kind::Row(children) => {
                let mut fb = None;
                for &c in children {
                    let r = self.layout_box(c, x, w, inner);
                    if fb.is_none() {
                        fb = r;
                    }
                }
                fb
            }
            Kind::Inline(_) => self.layout_inline(id, x, w, inner),
            Kind::Flex(_) => {
                let top = inner.place();
                let (h, baseline) = self.layout_flex(id, x, top, w, definite);
                inner.y = top + h;
                baseline
            }
            Kind::Replaced(item) => {
                let top = inner.place();
                let (iw, ih) = self.replaced_size(item, w, definite);
                if st.visible {
                    let rect = Rect { x, y: top, w: iw, h: ih };
                    self.draw_object(item, rect, None);
                }
                inner.y = top + ih;
                Some(top + ih)
            }
            Kind::Hr => {
                let top = inner.place();
                inner.y = top + 2.0;
                if st.visible && st.background.is_none() && st.border_width.iter().all(|b| *b == 0.0) {
                    let light = Color { a: st.color.a / 2, ..st.color };
                    self.push(DisplayItem::Rect { rect: Rect { x, y: top, w, h: 1.0 }, color: st.color });
                    self.push(DisplayItem::Rect { rect: Rect { x, y: top + 1.0, w, h: 1.0 }, color: light });
                }
                None
            }
            Kind::Table { .. } => None,
        }
    }

    /// Size of an image or form control given the width it may take; a
    /// definite `height` wins over the aspect ratio.
    fn replaced_size(&self, item: &Item, avail: f32, height: Option<f32>) -> (f32, f32) {
        match item {
            Item::Image { size, .. } => {
                let mut s = *size;
                if let Some(h) = height {
                    s.height = Len::Px(h);
                }
                s.resolve(avail)
            }
            Item::Field { w, h, style } => {
                let st = &self.styles[*style];
                let fits = st.width.is_definite() && (avail.is_finite() || !st.width.has_pct());
                let w = if fits { st.width.resolve(avail) } else { *w };
                (clamp_extent(w), height.unwrap_or(*h))
            }
            _ => (0.0, 0.0),
        }
    }

    /// Draws an image or form control into `rect`.
    fn draw_object(&mut self, item: &Item, rect: Rect, _st: Option<&Style>) {
        match item {
            Item::Image { image: Some(img), .. } => self.push(DisplayItem::Image { rect, image: *img }),
            Item::Field { .. } => {
                let border = Color::rgb(0x76, 0x76, 0x76);
                self.push(DisplayItem::Rect { rect, color: border });
                if rect.w > 2.0 && rect.h > 2.0 {
                    let inner = Rect { x: rect.x + 1.0, y: rect.y + 1.0, w: rect.w - 2.0, h: rect.h - 2.0 };
                    self.push(DisplayItem::Rect { rect: inner, color: Color::WHITE });
                }
            }
            _ => {}
        }
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
        self.push(DisplayItem::Text {
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
                Item::Image { .. } | Item::Field { .. } | Item::Atomic { .. } => {
                    flush_word(&mut atoms, &mut word);
                    atoms.push(Atom::Object { item: i });
                }
                Item::Abs { .. } => {
                    flush_word(&mut atoms, &mut word);
                    atoms.push(Atom::Abs { item: i });
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

    /// Size and baseline (from the top) of an object on a line `avail` wide.
    fn object_size(&self, item: &Item, measured: Option<&Measured>, avail: f32) -> (f32, f32, f32) {
        if let Some(m) = measured {
            return (m.w, m.h, m.base);
        }
        let (w, h) = match item {
            Item::Image { size, .. } => size.resolve(avail),
            Item::Field { .. } => self.replaced_size(item, avail, None),
            _ => (0.0, 0.0),
        };
        let st = &self.styles[item.style()];
        let base = match st.vertical_align {
            VAlign::Middle => h / 2.0 + st.font_size * 0.25,
            _ => h,
        };
        (w, h, base)
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
        // Inline-blocks are laid out first, at the origin, and moved onto
        // their line once it is placed.
        let mut measured: Vec<Option<Measured>> = Vec::with_capacity(items.len());
        for item in items {
            measured.push(match item {
                Item::Atomic { boxid, .. } => Some(self.measure_atomic(*boxid, w)),
                _ => None,
            });
        }
        let block_style = &styles[b.style];
        let strut = text_metrics(block_style).2.max(1.0);
        let mut y = flow.next();
        let mut placed = false;
        let mut first = None;
        let mut breaker = Breaker::new();
        let mut skips = 0;
        loop {
            let (l, r) = self.band(y, strut, x, x + w);
            let avail = r - l;
            let saved = breaker.clone();
            let Some(line) = breaker.next(self, &atoms, items, &measured, avail) else { break };
            // Narrowed by floats and still too wide: try below the next one.
            if line.w > avail + 0.01
                && (l > x || r < x + w)
                && skips < position::MAX_FLOAT_SKIPS
                && let Some(ny) = self.next_float_bottom(y)
            {
                y = ny;
                breaker = saved;
                skips += 1;
                continue;
            }
            if !placed {
                // Resolve the pending margin only once there is a line.
                let top = flow.place();
                y = y.max(top);
                placed = true;
            }
            let (h, baseline) = self.emit_line(&line, &atoms, items, &measured, block_style, l, y, avail);
            if line.content {
                first.get_or_insert(baseline);
            }
            y += h;
        }
        if placed {
            flow.y = y;
        }
        first
    }

    fn measure_atomic(&mut self, boxid: BoxId, avail: f32) -> Measured {
        let st = &self.styles[self.boxes[boxid].style];
        let g = geometry(st, false, avail);
        let [_, mr, _, ml] = g.margin;
        let bw = self.shrink_width(boxid, avail - ml - mr, avail);
        let (h, baseline, span) = self.layout_detached(boxid, bw, avail, None);
        // Without in-flow lines (or when clipping), the bottom margin edge
        // is the baseline.
        let base = baseline.filter(|_| !st.clips()).unwrap_or(h);
        Measured { w: clamp_extent(bw + ml + mr), h, base, span }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_line(
        &mut self,
        line: &Line,
        atoms: &[Atom],
        items: &[Item],
        measured: &[Option<Measured>],
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
                FragKind::Object { h, base } => {
                    above = above.max(base - shift);
                    below = below.max(h - base + shift);
                }
                FragKind::Abs => {}
            }
        }
        // A line holding only out-of-flow markers takes no room.
        let height = if line.content || line.frags.is_empty() { (above + below).max(0.0) } else { 0.0 };
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
            if let FragKind::Abs = f.kind {
                if let Item::Abs { boxid, .. } = item {
                    self.add_pending(*boxid, fx, top);
                }
                continue;
            }
            if let FragKind::Object { base, .. } = f.kind
                && let Some(m) = &measured[f.item as usize]
            {
                // Inline-blocks draw themselves (and their own links).
                let span = m.span.clone();
                self.shift_span(&span, fx, by - base);
                continue;
            }
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
                        self.push(DisplayItem::Rect { rect: Rect { x: fx, y: by - a, w: f.w, h: a + d }, color: *bg });
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
                            self.push(DisplayItem::Text {
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
                            self.push(DisplayItem::Rect { rect, color: st.color });
                        }
                        if st.line_through {
                            let t = (fc.strike_thickness * k).max(1.0);
                            let rect = Rect { x: fx, y: by - fc.strike_pos * k, w: f.w, h: t };
                            self.push(DisplayItem::Rect { rect, color: st.color });
                        }
                    }
                }
                FragKind::Object { h, base } => {
                    let rect = Rect { x: fx, y: by - base, w: f.w, h: *h };
                    self.draw_object(item, rect, Some(st));
                }
                FragKind::Abs => {}
            }
        }
        (height, baseline)
    }

    // -- Intrinsic sizes ------------------------------------------------------

    /// (min-content, max-content) width of a box's content box.
    fn intrinsic_inner(&mut self, id: BoxId) -> (f32, f32) {
        if let Some(v) = self.intrinsic[id] {
            return v;
        }
        // Breaks cycles through hostile structures; real values overwrite it.
        self.intrinsic[id] = Some((0.0, 0.0));
        let boxes = self.boxes;
        let styles = self.styles;
        let b = &boxes[id];
        let st = &styles[b.style];
        let (mn, mx) = match &b.kind {
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
                            let (omn, omx) = match &items[*item as usize] {
                                Item::Atomic { boxid, .. } => self.intrinsic(*boxid),
                                it @ (Item::Image { .. } | Item::Field { .. }) => {
                                    let w = self.object_size(it, None, f32::INFINITY).0;
                                    (w, w)
                                }
                                _ => (0.0, 0.0),
                            };
                            line += omx;
                            mn = mn.max(omn);
                            word = 0.0;
                            glue = false;
                        }
                        Atom::Abs { .. } => {}
                    }
                }
                (mn, mx.max(line))
            }
            Kind::Block(children) | Kind::Row(children) => {
                let row = matches!(b.kind, Kind::Row(_));
                let mut acc = (0.0f32, 0.0f32);
                // Floats in a row sit side by side.
                let mut floats = 0.0f32;
                for &c in children {
                    let cs = &styles[boxes[c].style];
                    if !boxes[c].anon && cs.out_of_flow() {
                        continue;
                    }
                    let (cmn, cmx) = self.intrinsic(c);
                    if row {
                        acc = (acc.0 + cmn, acc.1 + cmx);
                    } else if !boxes[c].anon && cs.float != Float::None {
                        floats += cmx;
                        acc = (acc.0.max(cmn), acc.1.max(floats));
                    } else {
                        floats = 0.0;
                        acc = (acc.0.max(cmn), acc.1.max(cmx));
                    }
                }
                acc
            }
            Kind::Flex(children) => {
                let row = st.is_row();
                let gap = if row { st.gap[1] } else { st.gap[0] };
                let gap = if let Len::Px(g) = gap { g.max(0.0) } else { 0.0 };
                let mut acc = (0.0f32, 0.0f32);
                let mut n = 0;
                for &c in children {
                    if !boxes[c].anon && styles[boxes[c].style].out_of_flow() {
                        continue;
                    }
                    let (cmn, cmx) = self.intrinsic(c);
                    n += 1;
                    acc = if !row {
                        (acc.0.max(cmn), acc.1.max(cmx))
                    } else if st.flex_wrap {
                        (acc.0.max(cmn), acc.1 + cmx)
                    } else {
                        (acc.0 + cmn, acc.1 + cmx)
                    };
                }
                if row && n > 1 {
                    let gaps = gap * (n - 1) as f32;
                    acc.1 += gaps;
                    if !st.flex_wrap {
                        acc.0 += gaps;
                    }
                }
                acc
            }
            Kind::Replaced(item) => {
                let w = self.replaced_size(item, f32::INFINITY, None).0;
                (w, w)
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
        let v = (clamp_extent(mn), clamp_extent(mx.max(mn)));
        self.intrinsic[id] = Some(v);
        v
    }

    /// (min-content, max-content) margin-box width of a box.
    fn intrinsic(&mut self, id: BoxId) -> (f32, f32) {
        let (mut mn, mut mx) = self.intrinsic_inner(id);
        let b = &self.boxes[id];
        if b.anon {
            return (mn, mx);
        }
        let st = &self.styles[b.style];
        let table = matches!(b.kind, Kind::Table { .. });
        let px = |l: Len| if let Len::Px(v) = l { v } else { 0.0 };
        let pad = px(st.padding[1]).max(0.0) + px(st.padding[3]).max(0.0) + st.border_width[1] + st.border_width[3];
        if let Len::Px(w) = st.width {
            let w = if st.border_box && !table { w - pad } else { w };
            let w = w.max(0.0);
            mn = if table { mn.max(w) } else { w };
            mx = mn.max(w);
        }
        if let Some(Len::Px(m)) = st.max_width {
            let m = if st.border_box && !table { m - pad } else { m }.max(0.0);
            mx = mx.min(m);
            mn = mn.min(m);
        }
        if let Len::Px(m) = st.min_width {
            let m = if st.border_box && !table { m - pad } else { m }.max(0.0);
            mn = mn.max(m);
            mx = mx.max(m);
        }
        if !table {
            mn += pad;
            mx += pad;
        }
        let margins = px(st.margin[1]) + px(st.margin[3]);
        mn += margins.max(0.0);
        mx += margins.max(0.0);
        (clamp_extent(mn), clamp_extent(mx.max(mn)))
    }

    // -- Tables ---------------------------------------------------------------

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
        let force_w = self.force_w.take();
        self.force_h = None;
        let b = &boxes[id];
        let Kind::Table { captions, rows } = &b.kind else { return None };
        let st = &styles[b.style];
        let start = self.mark();
        let g = geometry(st, false, cb_w);
        let [mt, mut mr, mb, mut ml] = g.margin;
        let [pt, pr, pb, pl] = g.padding;
        let [bt, br, bb, bl] = g.border;
        if g.auto[3] {
            ml = 0.0;
        }
        if g.auto[1] {
            mr = 0.0;
        }
        let spacing = st.border_spacing;
        let (cols_min, cols_max, n) = self.columns(rows);
        let (grid, _) = self.grid(rows);
        let chrome = spacing * (n as f32 + 1.0) + pl + pr + bl + br;
        let sum_min = cols_min.iter().sum::<f32>() + chrome;
        let sum_max = cols_max.iter().sum::<f32>() + chrome;
        let avail = (cb_w - ml - mr).max(0.0);
        let mut tw = match (force_w, st.width) {
            (Some(w), _) => w,
            (None, l) if l.is_definite() => l.resolve(cb_w),
            _ => sum_max.min(avail),
        };
        if let Some(max) = st.max_width.filter(|m| m.is_definite()) {
            tw = tw.min(max.resolve(cb_w));
        }
        let tw = clamp_extent(tw.max(sum_min));
        if force_w.is_none() {
            let free = cb_w - tw - ml - mr;
            match (g.auto[3], g.auto[1]) {
                (true, true) => ml += (free / 2.0).max(0.0),
                (true, false) => ml += free.max(0.0),
                _ => {}
            }
        }
        if force_w.is_none() && st.clear != Clear::None {
            self.clear(st.clear, flow);
        }
        let widths = distribute(&cols_min, &cols_max, (tw - chrome).max(0.0));
        let mut tx = x + ml;
        // Tables never overlap floats either.
        if force_w.is_none() {
            let (l, r) = self.band(flow.next() + mt, 1.0, x, x + cb_w);
            if (l > x || r < x + cb_w) && tw <= r - l {
                tx = l + ml;
            }
        }
        let saved = (self.z, self.clip);
        let positioned = st.positioned();
        if positioned {
            if let Some(z) = st.z_index {
                self.z = z;
            }
            self.abs.push(Vec::new());
        }

        flow.margin.push(mt);
        let top = flow.place();
        let (bg, borders) = self.reserve_deco(st, b);
        let mut y = top + bt + pt;
        let inner_x = tx + bl + pl;
        let inner_w = (tw - pl - pr - bl - br).max(0.0);
        let mut first_baseline = None;
        // The table is its own formatting context.
        self.floats.push(FloatCtx::default());
        for &c in captions {
            let mut f = Flow::new(y);
            let r = self.layout_box(c, inner_x, inner_w, &mut f);
            first_baseline = first_baseline.or(r);
            y = f.y + f.margin.value().max(0.0);
        }
        self.floats.pop();
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
                deco: (Option<usize>, Option<usize>),
                /// What the cell's contents produced.
                span: Span,
            }
            let mut placed: Vec<Placed> = Vec::new();
            let mut row_h = 0.0f32;
            for &(cell, col, span) in &grid[ri] {
                let cx = inner_x + spacing + widths[..col].iter().sum::<f32>() + spacing * col as f32;
                let cw = widths[col..col + span].iter().sum::<f32>() + spacing * (span as f32 - 1.0);
                let cell_style = &styles[boxes[cell].style];
                let deco = self.reserve_deco(cell_style, &boxes[cell]);
                let m = self.mark();
                let (h, baseline) = self.layout_cell(cell, cx, cw, y);
                row_h = row_h.max(h);
                placed.push(Placed { cell, x: cx, w: cw, h, baseline, deco, span: self.span_since(m) });
            }
            for p in placed {
                let cell_style = &styles[boxes[p.cell].style];
                // Cells are vertically aligned after the row height is known,
                // by shifting what they already emitted.
                let dy = match cell_style.vertical_align {
                    VAlign::Middle => (row_h - p.h) / 2.0,
                    VAlign::Bottom => row_h - p.h,
                    _ => 0.0,
                };
                if dy > 0.0 {
                    self.shift_span(&p.span, 0.0, dy);
                }
                let d = Deco { bg: p.deco.0, borders: p.deco.1, clip: 0, rect: Rect { x: p.x, y, w: p.w, h: row_h } };
                self.paint_deco(cell_style, &d);
                if ri == 0 && first_baseline.is_none() {
                    first_baseline = p.baseline.map(|b| b + dy);
                }
            }
            if let Some((slot, color)) = row_bg {
                let rect = Rect { x: inner_x + spacing, y, w: (inner_w - spacing * 2.0).max(0.0), h: row_h };
                self.fill(Some(slot), rect, color);
            }
            y += row_h + spacing;
        }
        let bottom = y + pb + bb;
        let deco = Deco { bg, borders, clip: 0, rect: Rect { x: tx, y: top, w: tw, h: bottom - top } };
        self.paint_deco(st, &deco);
        self.deco[id] = Some(deco);
        if positioned {
            self.finish_frame(deco.rect);
        }
        (self.z, self.clip) = saved;
        flow.y = bottom;
        flow.margin = Margin::default();
        flow.margin.push(mb);
        if st.position == crate::style::Position::Relative {
            let (dx, dy) = self.relative_offset(st, cb_w);
            let span = self.span_since(start);
            self.shift_span(&span, dx, dy);
        }
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
        let [bt, br, bb, bl] = g.border;
        let content_x = cx + bl + pl;
        let content_w = (cw - pl - pr - bl - br).max(0.0);
        let top = y + bt + pt;
        let mut f = Flow::new(top);
        self.floats.push(FloatCtx::default());
        let saved = self.cb_h;
        self.cb_h = None;
        let baseline = match &b.kind {
            Kind::Table { .. } => self.layout_table(cell, content_x, content_w, &mut f),
            _ => self.layout_contents(cell, content_x, content_w, &mut f, None),
        };
        self.cb_h = saved;
        let floats = self.floats.pop().map_or(0.0, |fc| fc.bottom(Clear::Both));
        let mut end = (f.y + f.margin.value()).max(top).max(floats);
        if let Len::Px(h) = st.height {
            let h = if st.border_box { h - pt - pb - bt - bb } else { h };
            end = end.max(top + h);
        }
        (end - y + pb + bb, baseline)
    }
}

/// Line breaking, one line at a time, so each line can get its own width
/// (lines next to floats are shorter).
#[derive(Clone)]
struct Breaker {
    /// Next atom.
    ai: usize,
    /// The unplaced rest of a word that was broken inside.
    rest: Vec<Piece>,
    break_ok: bool,
}

impl Breaker {
    fn new() -> Breaker {
        Breaker { ai: 0, rest: Vec::new(), break_ok: true }
    }

    fn next(
        &mut self,
        ctx: &Ctx<'_>,
        atoms: &[Atom],
        items: &[Item],
        measured: &[Option<Measured>],
        avail: f32,
    ) -> Option<Line> {
        let styles = ctx.styles;
        if self.ai >= atoms.len() && self.rest.is_empty() {
            return None;
        }
        let mut cur = Line::default();
        if !self.rest.is_empty() {
            let pieces = std::mem::take(&mut self.rest);
            let Atom::Word(segs) = &atoms[pieces[0].atom as usize] else { return None };
            if self.place_word(&mut cur, pieces, segs, avail, true) {
                return Some(cur);
            }
        }
        while self.ai < atoms.len() {
            match &atoms[self.ai] {
                Atom::Word(segs) => {
                    let wraps = styles[items[segs[0].item as usize].style()].white_space.wraps();
                    let pieces: Vec<Piece> = segs
                        .iter()
                        .enumerate()
                        .map(|(si, s)| Piece {
                            atom: self.ai as u32,
                            seg: si as u32,
                            g0: 0,
                            g1: s.glyphs.len() as u32,
                            x0: 0.0,
                            w: s.w,
                        })
                        .collect();
                    let w: f32 = pieces.iter().map(|p| p.w).sum();
                    // Whether the line may break here is up to the space
                    // before the word, not the word itself; and what has to
                    // fit is everything up to the next break opportunity.
                    if self.break_ok && cur.content && cur.w + unbreakable(atoms, self.ai, w, avail - cur.w) > avail {
                        cur.trim();
                        return Some(cur);
                    }
                    self.ai += 1;
                    if self.place_word(&mut cur, pieces, segs, avail, wraps) {
                        return Some(cur);
                    }
                }
                Atom::Space { item, w, can_break, collapsible } => {
                    if !(!cur.content && *collapsible) {
                        cur.frags.push(Frag { item: *item, x: cur.w, w: *w, kind: FragKind::Space });
                        cur.w += *w;
                    }
                    self.break_ok = *can_break;
                    self.ai += 1;
                }
                Atom::Break => {
                    self.ai += 1;
                    self.break_ok = true;
                    cur.trim();
                    cur.content = true;
                    return Some(cur);
                }
                Atom::Object { item } => {
                    let it = &items[*item as usize];
                    let (w, h, base) = ctx.object_size(it, measured[*item as usize].as_ref(), avail);
                    let wraps = styles[it.style()].white_space.wraps();
                    if (wraps || self.break_ok) && cur.content && cur.w + w > avail {
                        cur.trim();
                        return Some(cur);
                    }
                    self.ai += 1;
                    cur.frags.push(Frag { item: *item, x: cur.w, w, kind: FragKind::Object { h, base } });
                    cur.w += w;
                    cur.content = true;
                    self.break_ok = true;
                }
                Atom::Abs { item } => {
                    self.ai += 1;
                    cur.frags.push(Frag { item: *item, x: cur.w, w: 0.0, kind: FragKind::Abs });
                }
            }
        }
        cur.trim();
        (!cur.frags.is_empty()).then_some(cur)
    }

    /// Places a word, breaking inside it if no line could hold it. Returns
    /// true when the line is full.
    fn place_word(&mut self, cur: &mut Line, mut pieces: Vec<Piece>, segs: &[Seg], avail: f32, wraps: bool) -> bool {
        let w: f32 = pieces.iter().map(|p| p.w).sum();
        if wraps && !cur.content && w > avail && avail > 0.0 {
            let atoms_seg = |p: &Piece| &segs[p.seg as usize];
            let (head, tail) = split_pieces(&pieces, atoms_seg, avail);
            if !head.is_empty() && !tail.is_empty() {
                place_pieces(cur, &head, segs);
                self.rest = tail;
                self.break_ok = false;
                return true;
            }
            pieces = if head.is_empty() { tail } else { head };
        }
        place_pieces(cur, &pieces, segs);
        self.break_ok = false;
        false
    }
}

/// Width from the word at `ai` (`w` wide) to the next break opportunity,
/// counted only until it exceeds `room`.
fn unbreakable(atoms: &[Atom], ai: usize, w: f32, room: f32) -> f32 {
    let mut total = w;
    for a in atoms.iter().skip(ai + 1) {
        if total > room {
            break;
        }
        match a {
            Atom::Word(segs) => total += segs.iter().map(|s| s.w).sum::<f32>(),
            Atom::Space { w, can_break: false, .. } => total += w,
            _ => break,
        }
    }
    total
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
        line.content = true;
    }
}

/// Splits a word so the head fits in `avail`, breaking between clusters.
/// The head always gets at least one cluster so breaking makes progress.
fn split_pieces<'s>(pieces: &[Piece], seg_of: impl Fn(&Piece) -> &'s Seg, avail: f32) -> (Vec<Piece>, Vec<Piece>) {
    let mut head = Vec::new();
    let mut acc = 0.0f32;
    for (pi, p) in pieces.iter().enumerate() {
        if acc + p.w <= avail {
            head.push(*p);
            acc += p.w;
            continue;
        }
        let seg = seg_of(p);
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
            height: Len::Auto,
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
        let s = ImageSize { height: Len::Px(20.0), ..s };
        assert_eq!(s.resolve(1000.0), (40.0, 20.0));
    }
}
