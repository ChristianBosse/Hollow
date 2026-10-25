//! Floats, absolutely positioned boxes and relative offsets.
//!
//! Floats live in the float context of their block formatting context, in
//! page coordinates; line boxes and new formatting contexts ask it how much
//! room is left at a given height. Absolutely positioned boxes record their
//! static position and wait until their containing block (the nearest
//! positioned ancestor) knows its size; `fixed` boxes wait for the initial
//! containing block, which is the whole page: without scripts a fixed
//! overlay could never be dismissed, so it scrolls away with the page.

use super::{BoxId, Ctx, Flow, Margin, PHASE_FLOAT, Rect, clamp_extent, geometry};
use crate::style::{Clear, Float, Position, Style};

/// Floats one formatting context may hold; later ones are laid out where
/// they stand without floating, so hostile pages can't make placement
/// quadratic.
const MAX_FLOATS: usize = 1000;
/// How often one line may move down past floats looking for room.
pub(super) const MAX_FLOAT_SKIPS: usize = 64;

/// Margin boxes of the floats in one block formatting context.
#[derive(Default)]
pub(super) struct FloatCtx {
    left: Vec<Rect>,
    right: Vec<Rect>,
}

impl FloatCtx {
    /// Bottom of the floats `clear` applies to.
    pub(super) fn bottom(&self, clear: Clear) -> f32 {
        let side = |v: &[Rect]| v.iter().map(|r| r.y + r.h).fold(f32::NEG_INFINITY, f32::max);
        match clear {
            Clear::None => f32::NEG_INFINITY,
            Clear::Left => side(&self.left),
            Clear::Right => side(&self.right),
            Clear::Both => side(&self.left).max(side(&self.right)),
        }
    }

    fn len(&self) -> usize {
        self.left.len() + self.right.len()
    }
}

/// An absolutely positioned box and its static position: where its margin
/// box would have started had it been in flow.
pub(super) struct Pending {
    pub id: BoxId,
    pub x: f32,
    pub y: f32,
    /// Where its margin box would have ended, when the static position's
    /// containing block runs right to left (the box then hangs from there).
    pub right: Option<f32>,
    pub seq: usize,
}

impl Ctx<'_> {
    /// The free horizontal band `[left, right)` within `[x0, x1)` for
    /// content between `y` and `y + h`.
    pub(super) fn band(&self, y: f32, h: f32, x0: f32, x1: f32) -> (f32, f32) {
        let Some(fc) = self.floats.last() else { return (x0, x1) };
        let overlaps = |r: &&Rect| r.y < y + h && r.y + r.h > y && r.h > 0.0;
        let l = fc.left.iter().filter(overlaps).map(|r| r.x + r.w).fold(x0, f32::max);
        let r = fc.right.iter().filter(overlaps).map(|r| r.x).fold(x1, f32::min);
        (l.min(x1), r.max(l.min(x1)))
    }

    /// The nearest float bottom below `y`, where the band may widen.
    pub(super) fn next_float_bottom(&self, y: f32) -> Option<f32> {
        let fc = self.floats.last()?;
        fc.left.iter().chain(&fc.right).map(|r| r.y + r.h).filter(|b| *b > y).reduce(f32::min)
    }

    pub(super) fn floats_bottom(&self, clear: Clear) -> f32 {
        self.floats.last().map_or(f32::NEG_INFINITY, |f| f.bottom(clear))
    }

    /// Clearance for a box with top margin `mt` (CSS 2.1 9.5.2): if its
    /// border edge would start above the floats `clear` names, it starts
    /// at their bottom instead (true is returned), and margins above it no
    /// longer collapse with its own.
    pub(super) fn clear(&mut self, clear: Clear, mt: f32, flow: &mut Flow) -> bool {
        let bottom = self.floats_bottom(clear);
        let mut m = flow.margin;
        m.push(mt);
        if bottom <= flow.y + m.value() {
            return false;
        }
        // A parent whose top margin would have collapsed through this box
        // starts where it would have without it. (Whether there is
        // clearance only looks at this box's own top margin, not at margins
        // collapsing through it from its first children.)
        let start = flow.next();
        flow.first_place.get_or_insert(start);
        flow.y = bottom - mt;
        flow.margin = Margin::default();
        true
    }

    pub(super) fn layout_float(&mut self, id: BoxId, x: f32, cb_w: f32, flow: &mut Flow) {
        let st = &self.styles[self.boxes[id].style];
        if self.floats.last().is_some_and(|f| f.len() >= MAX_FLOATS) {
            self.layout_in_flow(id, x, cb_w, flow);
            return;
        }
        let g = geometry(st, false, cb_w);
        let [_, mr, _, ml] = g.margin;
        let bw = self.shrink_width(id, cb_w - ml - mr, cb_w);
        let outer_w = clamp_extent(bw + ml + mr);
        let (outer_h, _, span) = self.atomically(PHASE_FLOAT, |c| c.layout_detached(id, bw, cb_w, None));
        // Never above the current line, an earlier float, or what it clears.
        let floor = self.floats.last().map_or(f32::NEG_INFINITY, |f| {
            f.left.iter().chain(&f.right).map(|r| r.y).fold(f32::NEG_INFINITY, f32::max)
        });
        let mut y = flow.next().max(floor).max(self.floats_bottom(st.clear));
        let left = st.float == Float::Left;
        let mut fx;
        let mut tries = 0;
        loop {
            let (l, r) = self.band(y, outer_h.max(0.01), x, x + cb_w);
            fx = if left { l } else { r - outer_w };
            let fits = r - l >= outer_w - 0.01 || (l <= x && r >= x + cb_w);
            if fits || tries >= MAX_FLOAT_SKIPS {
                break;
            }
            match self.next_float_bottom(y) {
                Some(ny) => y = ny,
                None => break,
            }
            tries += 1;
        }
        let fx = if left { fx.max(x) } else { fx };
        self.shift_span(&span, fx, y);
        if let Some(fc) = self.floats.last_mut() {
            let rect = Rect { x: fx, y, w: outer_w, h: outer_h };
            if left { fc.left.push(rect) } else { fc.right.push(rect) }
        }
    }

    pub(super) fn add_pending(&mut self, id: BoxId, x: f32, y: f32) {
        self.add_pending_rtl(id, x, y, None);
    }

    pub(super) fn add_pending_rtl(&mut self, id: BoxId, x: f32, y: f32, right: Option<f32>) {
        let seq = self.pending_seq;
        self.pending_seq += 1;
        let fixed = self.styles[self.boxes[id].style].position == Position::Fixed;
        let frame = if fixed { self.abs.first_mut() } else { self.abs.last_mut() };
        if let Some(frame) = frame {
            frame.push(Pending { id, x, y, right, seq });
        }
    }

    /// Lays out the boxes waiting for the containing block `cb` (a padding
    /// box) and drops its frame. The frame stays on the stack meanwhile so
    /// `fixed` boxes found inside still have somewhere to go.
    pub(super) fn finish_frame(&mut self, cb: Rect) {
        loop {
            let batch = match self.abs.last_mut() {
                Some(f) if !f.is_empty() => std::mem::take(f),
                _ => break,
            };
            for p in batch {
                self.layout_abs(&p, cb);
            }
            // Only the initial containing block can gain boxes meanwhile.
            if self.abs.len() > 1 {
                break;
            }
        }
        if self.abs.len() > 1 {
            self.abs.pop();
        }
    }

    fn layout_abs(&mut self, p: &Pending, cb: Rect) {
        let styles = self.styles;
        let st = &styles[self.boxes[p.id].style];
        let g = geometry(st, false, cb.w);
        let [mt, mut mr, mb, mut ml] = g.margin;
        let hpad = g.hpad();
        let inset = |i: usize, basis: f32| st.inset[i].is_definite().then(|| st.inset[i].resolve(basis));
        let (top, right, bottom, left) = (inset(0, cb.h), inset(1, cb.w), inset(2, cb.h), inset(3, cb.w));
        let adj = if st.border_box { 0.0 } else { hpad };
        let mut bw = match (left, right) {
            _ if st.width.is_definite() => st.width.resolve(cb.w) + adj,
            (Some(l), Some(r)) if st.width == crate::style::Len::Auto => cb.w - l - r - ml - mr,
            _ => self.shrink_width(p.id, cb.w - left.unwrap_or(0.0) - right.unwrap_or(0.0) - ml - mr, cb.w),
        };
        if let Some(max) = st.max_width.filter(|m| m.is_definite()) {
            bw = bw.min(max.resolve(cb.w) + adj);
        }
        if st.min_width.is_definite() {
            bw = bw.max(st.min_width.resolve(cb.w) + adj);
        }
        let bw = clamp_extent(bw.max(hpad));
        // `left: 0; right: 0; margin: auto` centers.
        if let (Some(l), Some(r)) = (left, right)
            && g.auto[1]
            && g.auto[3]
        {
            let free = (cb.w - l - r - bw).max(0.0);
            ml = free / 2.0;
            mr = free / 2.0;
        }
        let fixed_h = match (top, bottom) {
            (Some(t), Some(b)) if !st.height.is_definite() => Some((cb.h - t - b - mt - mb).max(0.0)),
            _ => None,
        };
        let saved = self.cb_h;
        self.cb_h = Some(cb.h);
        let (outer_h, _, span) = self.layout_detached(p.id, bw, cb.w, fixed_h);
        self.cb_h = saved;
        let bh = (outer_h - mt - mb).max(0.0);
        // Over-constrained, `left` gives in a right-to-left box.
        let bx = match (left, right) {
            (Some(_), Some(r)) if st.rtl => cb.x + cb.w - r - mr - bw,
            (Some(l), _) => cb.x + l + ml,
            (None, Some(r)) => cb.x + cb.w - r - mr - bw,
            (None, None) => p.right.map_or(p.x + ml, |r| r - mr - bw),
        };
        let by = match (top, bottom) {
            (Some(t), _) => cb.y + t + mt,
            (None, Some(b)) => cb.y + cb.h - b - mb - bh,
            (None, None) => p.y + mt,
        };
        // It was laid out at the origin, with its border box at (margin-left,
        // margin-top) and auto margins as zero.
        let laid_ml = if g.auto[3] { 0.0 } else { g.margin[3] };
        self.shift_span(&span, bx - laid_ml, by - mt);
    }

    /// `position: relative` offsets.
    pub(super) fn relative_offset(&self, st: &Style, cb_w: f32) -> (f32, f32) {
        let def = |i: usize, basis: f32| st.inset[i].is_definite().then(|| st.inset[i].resolve(basis));
        let cb_h = self.cb_h.unwrap_or(0.0);
        let (left, right) = (def(3, cb_w), def(1, cb_w).map(|r| -r));
        // With both given, the containing block's direction says which wins.
        let dx = if self.cb_rtl { right.or(left) } else { left.or(right) }.unwrap_or(0.0);
        let dy = def(0, cb_h).or(def(2, cb_h).map(|b| -b)).unwrap_or(0.0);
        (dx, dy)
    }
}
