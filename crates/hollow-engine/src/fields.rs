//! Drawing form controls' current state over a finished layout: typed
//! values, placeholders, the caret, check marks, the chosen option and the
//! open option list.
//!
//! Layout leaves a [`DisplayItem::Field`] marker where each control sits.
//! Painting with form state copies the display list and inserts the
//! control's contents right after its marker, so they keep the control's
//! place in the paint order and its clip. Typing never relays out the page.

use crate::css::Color;
use crate::dom::NodeId;
use crate::form::{Control, Forms, TextKind};
use crate::layout::{DisplayItem, FieldKind, Laid, PlacedGlyph, Rect};
use crate::paint::GlyphPaths;
use crate::text::{self, FaceId, Shaper};

const PLACEHOLDER: Color = Color::rgb(0x75, 0x75, 0x75);
const ACCENT: Color = Color::rgb(0x3b, 0x5b, 0xdb);
const BORDER: Color = Color::rgb(0x76, 0x76, 0x76);
/// Rows an open `<select>` list shows at once.
const LIST_ROWS: usize = 12;
/// Characters drawn per field; the rest can't be seen anyway.
const MAX_SHOWN: usize = 4096;

/// The open option list: where it is and which options it shows.
pub(crate) struct List {
    pub node: NodeId,
    pub rect: Rect,
    pub row_h: f32,
    pub first: usize,
    pub rows: usize,
}

/// The open `<select>` list's geometry, shared by painting and hit testing.
pub(crate) fn open_list(laid: &Laid, forms: &Forms) -> Option<List> {
    let node = forms.open?;
    let (rect, size) = laid.items.iter().rev().find_map(|it| match it {
        DisplayItem::Field { rect, node: n, size, .. } if *n == node => Some((*rect, *size)),
        _ => None,
    })?;
    let Some(Control::Select { options, .. }) = forms.control(node) else { return None };
    let rows = options.len().min(LIST_ROWS);
    if rows == 0 {
        return None;
    }
    let widest = options.iter().map(|o| o.label.chars().count()).max().unwrap_or(0);
    let w = rect.w.max(widest as f32 * size * 0.6 + 16.0).min(laid.width.max(rect.w));
    // Keep the highlighted option in view.
    let first = forms.highlight.saturating_sub(rows - 1).min(options.len() - rows);
    let row_h = (size * 1.5).max(1.0);
    let x = rect.x.min((laid.width - w).max(0.0));
    Some(List { node, rect: Rect { x, y: rect.y + rect.h, w, h: row_h * rows as f32 }, row_h, first, rows })
}

/// The display list with every control's state drawn in, plus outlines for
/// the glyphs it adds. `None` when the page has no controls.
pub(crate) fn decorate(laid: &Laid, forms: &Forms, paths: &GlyphPaths) -> Option<(Laid, GlyphPaths)> {
    if !laid.items.iter().any(|it| matches!(it, DisplayItem::Field { .. })) {
        return None;
    }
    let mut out = Laid {
        width: laid.width,
        height: laid.height,
        canvas: laid.canvas,
        items: Vec::with_capacity(laid.items.len() + 64),
        clips: Vec::with_capacity(laid.items.len() + 64),
        clip_rects: laid.clip_rects.clone(),
        glyphs: laid.glyphs.clone(),
        links: Vec::new(),
        link_targets: Vec::new(),
        images: laid.images.clone(),
        forms: Forms::default(),
    };
    let mut pen = Pen { shaper: Shaper::new(), out: &mut out };
    for (i, it) in laid.items.iter().enumerate() {
        let clip = laid.clips.get(i).copied().unwrap_or(0);
        pen.out.items.push(it.clone());
        pen.out.clips.push(clip);
        if let DisplayItem::Field { rect, node, kind, face, size, color } = *it {
            let field = FieldStyle { face, size, color };
            pen.field(forms, rect, node, kind, field, clip);
        }
    }
    if let Some(list) = open_list(laid, forms) {
        pen.list(laid, forms, &list);
    }
    let mut extra = GlyphPaths::new();
    for it in &out.items[..] {
        if let DisplayItem::Text { face, glyphs, .. } = it {
            for g in out.glyphs.get(glyphs.start as usize..glyphs.end as usize).unwrap_or(&[]) {
                let key = (*face, g.id);
                if !paths.contains_key(&key)
                    && !extra.contains_key(&key)
                    && let Some(p) = text::outline(*face, g.id)
                {
                    extra.insert(key, p);
                }
            }
        }
    }
    Some((out, extra))
}

#[derive(Clone, Copy)]
struct FieldStyle {
    face: FaceId,
    size: f32,
    color: Color,
}

struct Pen<'a> {
    shaper: Shaper,
    out: &'a mut Laid,
}

impl Pen<'_> {
    fn push(&mut self, item: DisplayItem, clip: u32) {
        self.out.items.push(item);
        self.out.clips.push(clip);
    }

    /// A clip for a control's inside, within whatever already clips it.
    fn clip_inside(&mut self, inner: Rect, parent: u32) -> u32 {
        let r = match self.out.clip_rects.get(parent as usize).filter(|_| parent != 0) {
            Some(p) => inner.intersect(p),
            None => inner,
        };
        self.out.clip_rects.push(r);
        (self.out.clip_rects.len() - 1) as u32
    }

    /// Shapes `s` and returns its glyph range and width in CSS px.
    fn glyphs(&mut self, s: &str, f: FieldStyle) -> (std::ops::Range<u32>, f32) {
        let Some(face) = text::face(f.face) else { return (0..0, 0.0) };
        let k = face.scale(f.size);
        let shaped = self.shaper.shape(f.face, s);
        let start = self.out.glyphs.len() as u32;
        for g in &shaped.glyphs {
            self.out.glyphs.push(PlacedGlyph { id: g.id, x: (g.pen + g.dx) as f32 * k, y: -(g.dy as f32) * k });
        }
        (start..self.out.glyphs.len() as u32, shaped.advance as f32 * k)
    }

    fn width(&mut self, s: &str, f: FieldStyle) -> f32 {
        let Some(face) = text::face(f.face) else { return 0.0 };
        self.shaper.shape(f.face, s).advance as f32 * face.scale(f.size)
    }

    fn text(&mut self, s: &str, f: FieldStyle, x: f32, baseline: f32, clip: u32) -> f32 {
        let (glyphs, w) = self.glyphs(s, f);
        self.push(DisplayItem::Text { face: f.face, size: f.size, color: f.color, x, y: baseline, glyphs }, clip);
        w
    }

    fn field(&mut self, forms: &Forms, rect: Rect, node: NodeId, kind: FieldKind, f: FieldStyle, clip: u32) {
        let Some(control) = forms.control(node) else { return };
        let focused = forms.focused == Some(node);
        // The marker is the content box; border and padding come from CSS.
        let inner = rect;
        let (ascent, descent) = text::face(f.face).map_or((f.size * 0.8, f.size * 0.2), |face| {
            (face.ascent * face.scale(f.size), face.descent * face.scale(f.size))
        });
        match (kind, control) {
            (FieldKind::Text | FieldKind::Area, Control::Text { kind: tk, value, caret, placeholder, .. }) => {
                let inside = self.clip_inside(inner, clip);
                let line_h = (ascent + descent).max(f.size * 1.2);
                let pad = 1.0;
                let shown: String = match tk {
                    TextKind::Password => "\u{2022}".repeat(value.chars().count().min(MAX_SHOWN)),
                    _ => value.chars().take(MAX_SHOWN).collect(),
                };
                if shown.is_empty() && !placeholder.is_empty() {
                    let ph = FieldStyle { color: PLACEHOLDER, ..f };
                    let baseline = if kind == FieldKind::Area {
                        inner.y + pad + ascent
                    } else {
                        inner.y + (inner.h + ascent - descent) / 2.0
                    };
                    self.text(placeholder, ph, inner.x + pad, baseline, inside);
                }
                // Lines and where the caret is among them.
                let caret = (*caret).min(shown.chars().count());
                let before: String = shown.chars().take(caret).collect();
                let caret_line = before.matches('\n').count();
                let caret_col: String = before.rsplit('\n').next().unwrap_or("").to_owned();
                let caret_x = self.width(&caret_col, f);
                let lines: Vec<&str> = if kind == FieldKind::Area { shown.split('\n').collect() } else { vec![&shown] };
                // Scroll so the caret stays visible.
                let room_w = (inner.w - 2.0 * pad).max(1.0);
                let dx = if focused { (caret_x - room_w).max(0.0) } else { 0.0 };
                let room_rows = ((inner.h - 2.0 * pad) / line_h).floor().max(1.0) as usize;
                let first_row =
                    if kind == FieldKind::Area && focused { caret_line.saturating_sub(room_rows - 1) } else { 0 };
                let top = if kind == FieldKind::Area {
                    inner.y + pad + ascent
                } else {
                    inner.y + (inner.h + ascent - descent) / 2.0
                };
                for (row, line) in lines.iter().enumerate().skip(first_row).take(room_rows + 1) {
                    let baseline = top + (row - first_row) as f32 * line_h;
                    if !line.is_empty() {
                        self.text(line, f, inner.x + pad - dx, baseline, inside);
                    }
                }
                if focused {
                    let baseline = top + caret_line.saturating_sub(first_row) as f32 * line_h;
                    let caret_rect =
                        Rect { x: inner.x + pad - dx + caret_x, y: baseline - ascent, w: 1.0, h: ascent + descent };
                    self.push(DisplayItem::Rect { rect: caret_rect, color: f.color }, inside);
                }
            }
            (FieldKind::Check, Control::Check { checked: true, .. }) => {
                let mark =
                    Rect { x: rect.x + 3.0, y: rect.y + 3.0, w: (rect.w - 6.0).max(1.0), h: (rect.h - 6.0).max(1.0) };
                self.push(DisplayItem::Rect { rect: mark, color: ACCENT }, clip);
            }
            (FieldKind::Radio, Control::Check { checked: true, .. }) => {
                let d = (rect.w.min(rect.h) / 2.0).max(1.0);
                let dot = Rect { x: rect.x + (rect.w - d) / 2.0, y: rect.y + (rect.h - d) / 2.0, w: d, h: d };
                self.push(DisplayItem::Round { rect: dot, radius: d / 2.0, stroke: 0.0, color: ACCENT }, clip);
            }
            (FieldKind::List, Control::Select { options, selected, .. }) => {
                let inside = self.clip_inside(inner, clip);
                let row_h = f.size * 1.2;
                let rows = (inner.h / row_h).floor().max(1.0) as usize;
                // Show the first chosen option's row, as browsers scroll to it.
                let first = selected.iter().position(|s| *s).unwrap_or(0).saturating_sub(rows - 1);
                for (row, (o, on)) in options.iter().zip(selected).enumerate().skip(first).take(rows) {
                    let y = inner.y + (row - first) as f32 * row_h;
                    let mut style = f;
                    if *on {
                        let band = Rect { x: inner.x, y, w: inner.w, h: row_h };
                        self.push(DisplayItem::Rect { rect: band, color: Color::rgb(0xce, 0xce, 0xd4) }, inside);
                    }
                    if o.disabled {
                        style.color = PLACEHOLDER;
                    }
                    let baseline = y + (row_h + ascent - descent) / 2.0;
                    self.text(&o.label, style, inner.x + 4.0, baseline, inside);
                }
            }
            (FieldKind::Select, Control::Select { options, selected, .. }) => {
                let inside = self.clip_inside(inner, clip);
                let label =
                    selected.iter().position(|s| *s).and_then(|i| options.get(i)).map_or("", |o| o.label.as_str());
                let baseline = inner.y + (inner.h + ascent - descent) / 2.0;
                self.text(label, f, inner.x + 4.0, baseline, inside);
                let arrow_w = self.width("\u{25BE}", f);
                self.text("\u{25BE}", f, inner.x + inner.w - arrow_w - 4.0, baseline, inside);
            }
            _ => {}
        }
        if focused {
            // Around the UA border and padding (3 px) for text fields.
            let out = if matches!(kind, FieldKind::Check | FieldKind::Radio) { 1.0 } else { 4.0 };
            let ring = Rect { x: rect.x - out, y: rect.y - out, w: rect.w + 2.0 * out, h: rect.h + 2.0 * out };
            let radius = if kind == FieldKind::Radio { ring.w.min(ring.h) / 2.0 } else { 2.0 };
            self.push(DisplayItem::Round { rect: ring, radius, stroke: 2.0, color: ACCENT }, clip);
        }
    }

    fn list(&mut self, laid: &Laid, forms: &Forms, list: &List) {
        let Some(Control::Select { options, .. }) = forms.control(list.node) else { return };
        let Some(f) = laid.items.iter().rev().find_map(|it| match it {
            DisplayItem::Field { node, face, size, color, .. } if *node == list.node => {
                Some(FieldStyle { face: *face, size: *size, color: *color })
            }
            _ => None,
        }) else {
            return;
        };
        let r = list.rect;
        // Drawn last and unclipped: the list floats over everything.
        let border = Rect { x: r.x - 1.0, y: r.y - 1.0, w: r.w + 2.0, h: r.h + 2.0 };
        self.push(DisplayItem::Rect { rect: border, color: BORDER }, 0);
        self.push(DisplayItem::Rect { rect: r, color: Color::WHITE }, 0);
        let (ascent, descent) = text::face(f.face).map_or((f.size * 0.8, f.size * 0.2), |face| {
            (face.ascent * face.scale(f.size), face.descent * face.scale(f.size))
        });
        for row in 0..list.rows {
            let i = list.first + row;
            let Some(option) = options.get(i) else { break };
            let y = r.y + row as f32 * list.row_h;
            let mut style = f;
            if i == forms.highlight {
                self.push(DisplayItem::Rect { rect: Rect { x: r.x, y, w: r.w, h: list.row_h }, color: ACCENT }, 0);
                style.color = Color::WHITE;
            } else if option.disabled {
                style.color = PLACEHOLDER;
            }
            let baseline = y + (list.row_h + ascent - descent) / 2.0;
            let row_clip = self.clip_inside(Rect { x: r.x, y, w: r.w, h: list.row_h }, 0);
            self.text(&option.label, style, r.x + 6.0, baseline, row_clip);
        }
    }
}

/// The option row of list box `node` at page `y`, if `node` is one.
pub(crate) fn list_box_row(laid: &Laid, forms: &Forms, node: NodeId, y: f32) -> Option<usize> {
    let (rect, size) = laid.items.iter().rev().find_map(|it| match it {
        DisplayItem::Field { rect, node: n, size, kind: FieldKind::List, .. } if *n == node => Some((*rect, *size)),
        _ => None,
    })?;
    let Some(Control::Select { options, selected, .. }) = forms.control(node) else { return None };
    let row_h = size * 1.2;
    let rows = (rect.h / row_h).floor().max(1.0) as usize;
    let first = selected.iter().position(|s| *s).unwrap_or(0).saturating_sub(rows - 1);
    let row = first + ((y - rect.y) / row_h).floor().max(0.0) as usize;
    (row < options.len()).then_some(row)
}
