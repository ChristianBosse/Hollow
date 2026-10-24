//! Flexbox (CSS Flexible Box Layout Level 1), single- and multi-line.
//!
//! Items are sized along the main axis by the usual algorithm (flex base
//! size, hypothetical size, then growing or shrinking with min/max
//! freezing), laid out once at the origin, and shifted into place. Row
//! items are laid out after their width is known; column items are laid
//! out first to learn their height. Stretching in the cross axis, and
//! growing in a column, only resize the item's own background and borders:
//! block content doesn't move when its box gets taller.

use super::{BoxId, Ctx, Kind, Len, PHASE_INLINE, Span, clamp_extent, geometry};
use crate::style::{Align, FlexDirection, Justify, Style};

struct FlexItem {
    id: BoxId,
    /// Top, right, bottom, left; auto margins count as zero here.
    margin: [f32; 4],
    auto: [bool; 4],
    /// Flex base size and min/max, all border-box main sizes.
    base: f32,
    min: f32,
    max: f32,
    grow: f32,
    shrink: f32,
    /// Hypothetical, then final, border-box main size.
    main: f32,
    frozen: bool,
    /// Border-box cross size and the baseline from the margin-box top,
    /// once laid out.
    cross: f32,
    baseline: Option<f32>,
    span: Option<Span>,
    align: Align,
    /// Whether the item's cross size is `auto`, so stretching may set it.
    cross_auto: bool,
}

impl FlexItem {
    fn main_margins(&self, row: bool) -> f32 {
        if row { self.margin[1] + self.margin[3] } else { self.margin[0] + self.margin[2] }
    }
    fn cross_margins(&self, row: bool) -> f32 {
        if row { self.margin[0] + self.margin[2] } else { self.margin[1] + self.margin[3] }
    }
}

impl Ctx<'_> {
    /// Lays out a flex container's items in its content box at (`x`, `y`),
    /// `w` wide and, if definite, `height` tall. Returns the content height
    /// and the first baseline.
    pub(super) fn layout_flex(&mut self, id: BoxId, x: f32, y: f32, w: f32, height: Option<f32>) -> (f32, Option<f32>) {
        let boxes = self.boxes;
        let styles = self.styles;
        let Kind::Flex(children) = &boxes[id].kind else { return (0.0, None) };
        let st = &styles[boxes[id].style];
        let row = st.is_row();
        let reverse = matches!(st.flex_direction, FlexDirection::RowReverse | FlexDirection::ColumnReverse);
        let gap = |l: Len, basis: f32| l.resolve(basis).max(0.0);
        let (gap_main, gap_cross) = if row {
            (gap(st.gap[1], w), gap(st.gap[0], height.unwrap_or(0.0)))
        } else {
            (gap(st.gap[0], height.unwrap_or(0.0)), gap(st.gap[1], w))
        };
        let main_avail = if row { Some(w) } else { height };

        let mut order = Vec::with_capacity(children.len());
        for &c in children {
            if !boxes[c].anon && styles[boxes[c].style].out_of_flow() {
                self.add_pending(c, x, y);
            } else {
                order.push(c);
            }
        }
        order.sort_by_key(|&c| if boxes[c].anon { 0 } else { styles[boxes[c].style].order });
        let saved_cb_h = self.cb_h;
        self.cb_h = height;
        let mut items: Vec<FlexItem> = order.iter().map(|&c| self.flex_item(c, st, row, w, height)).collect();

        // Lines.
        let mut lines: Vec<std::ops::Range<usize>> = Vec::new();
        match main_avail {
            Some(avail) if st.flex_wrap => {
                let mut start = 0;
                let mut used = 0.0f32;
                for (i, it) in items.iter().enumerate() {
                    let outer = it.main + it.main_margins(row);
                    if i > start && used + gap_main + outer > avail {
                        lines.push(start..i);
                        start = i;
                        used = outer;
                    } else {
                        used += if i > start { gap_main + outer } else { outer };
                    }
                }
                lines.push(start..items.len());
            }
            _ => lines.push(0..items.len()),
        }
        if let Some(avail) = main_avail {
            for line in &lines {
                let n = line.len();
                resolve_flexible(&mut items[line.clone()], avail, gap_main * n.saturating_sub(1) as f32, row);
            }
        }

        // Lay out row items now that their widths are known; column items
        // were laid out to measure them and only change their height.
        for it in &mut items {
            if row {
                // Flex items paint like inline-blocks.
                let (_, baseline, span) = self.atomically(PHASE_INLINE, |c| c.layout_detached(it.id, it.main, w, None));
                let d = self.deco[it.id];
                it.cross = d.map_or(0.0, |d| d.rect.h);
                if d.is_none() {
                    // Tables and the like: their emitted extent is the size.
                    it.cross = self.span_height(&span);
                }
                it.baseline = baseline;
                it.span = Some(span);
            } else {
                self.resize_deco(it.id, None, Some(it.main));
            }
        }

        // Cross sizes of the lines; a single line fills a definite container.
        let single_cross = if row { height } else { Some(w) };
        let mut line_cross: Vec<f32> = Vec::with_capacity(lines.len());
        let mut line_base: Vec<f32> = Vec::with_capacity(lines.len());
        for line in &lines {
            let mut size = 0.0f32;
            let mut max_base = 0.0f32;
            if row {
                for it in &items[line.clone()] {
                    if it.align == Align::Baseline {
                        max_base = max_base.max(it.baseline.unwrap_or(it.margin[0] + it.cross));
                    }
                }
            }
            for it in &items[line.clone()] {
                let outer = it.cross + it.cross_margins(row);
                let shift = if row && it.align == Align::Baseline {
                    max_base - it.baseline.unwrap_or(it.margin[0] + it.cross)
                } else {
                    0.0
                };
                size = size.max(outer + shift);
            }
            if lines.len() == 1
                && let Some(c) = single_cross
            {
                size = c;
            }
            line_cross.push(size);
            line_base.push(max_base);
        }

        // Place everything.
        let total_cross: f32 = line_cross.iter().sum::<f32>() + gap_cross * line_cross.len().saturating_sub(1) as f32;
        let mut content_main = 0.0f32;
        let mut cross_pos = 0.0f32;
        let mut first_baseline = None;
        for (li, line) in lines.iter().enumerate() {
            let n = line.len();
            let used: f32 = items[line.clone()].iter().map(|it| it.main + it.main_margins(row)).sum::<f32>()
                + gap_main * n.saturating_sub(1) as f32;
            content_main = content_main.max(used);
            let avail = main_avail.unwrap_or(used);
            let mut free = avail - used;
            let autos: usize = items[line.clone()]
                .iter()
                .map(|it| {
                    let (a, b) = if row { (it.auto[3], it.auto[1]) } else { (it.auto[0], it.auto[2]) };
                    a as usize + b as usize
                })
                .sum();
            let auto_extra = if free > 0.0 && autos > 0 {
                let e = free / autos as f32;
                free = 0.0;
                e
            } else {
                0.0
            };
            let (lead, between) = justify(st.justify_content, free, n);
            let mut pos = lead;
            let lc = line_cross[li];
            for it in &mut items[line.clone()] {
                let (ms, me, auto_s, auto_e) = if row {
                    (it.margin[3], it.margin[1], it.auto[3], it.auto[1])
                } else {
                    (it.margin[0], it.margin[2], it.auto[0], it.auto[2])
                };
                pos += ms + if auto_s { auto_extra } else { 0.0 };
                let mut main_pos = pos;
                pos += it.main + me + if auto_e { auto_extra } else { 0.0 } + gap_main + between;
                if reverse {
                    main_pos = avail - main_pos - it.main;
                }
                // Cross axis.
                let (cs, ce, cauto_s, cauto_e) = if row {
                    (it.margin[0], it.margin[2], it.auto[0], it.auto[2])
                } else {
                    (it.margin[3], it.margin[1], it.auto[3], it.auto[1])
                };
                if it.align == Align::Stretch && it.cross_auto && !cauto_s && !cauto_e && row {
                    let target = self.clamp_cross(it.id, (lc - cs - ce).max(0.0), row, w);
                    if target > it.cross {
                        it.cross = target;
                        self.resize_deco(it.id, None, Some(target));
                    }
                }
                let outer = it.cross + cs + ce;
                let off = match (cauto_s, cauto_e) {
                    (true, true) => (lc - outer) / 2.0,
                    (true, false) => lc - outer,
                    (false, true) => 0.0,
                    _ => match it.align {
                        Align::End => lc - outer,
                        Align::Center => (lc - outer) / 2.0,
                        Align::Baseline if row => line_base[li] - it.baseline.unwrap_or(it.margin[0] + it.cross),
                        _ => 0.0,
                    },
                };
                let cross_at = cross_pos + off + cs;
                let (bx, by) = if row { (x + main_pos, y + cross_at) } else { (x + cross_at, y + main_pos) };
                // Laid out at the origin, border box at (margin-left, margin-top).
                let (dx, dy) = (bx - it.margin[3], by - it.margin[0]);
                if let Some(span) = it.span.take() {
                    self.shift_span(&span, dx, dy);
                }
                if first_baseline.is_none() && li == 0 {
                    first_baseline = it.baseline.map(|b| b + dy);
                }
            }
            cross_pos += lc + gap_cross;
        }
        self.cb_h = saved_cb_h;
        let content_h = if row { total_cross } else { height.unwrap_or(content_main) };
        (clamp_extent(content_h), first_baseline)
    }

    /// Sizes one item; column items are laid out here to learn their height.
    fn flex_item(&mut self, c: BoxId, cont: &Style, row: bool, w: f32, height: Option<f32>) -> FlexItem {
        let boxes = self.boxes;
        let styles = self.styles;
        let b = &boxes[c];
        let cs = &styles[b.style];
        let g = geometry(cs, b.anon, w);
        let (hpad, vpad) = (g.hpad(), g.vpad());
        let align = match (b.anon, cs.align_self) {
            (false, a) if a != Align::Auto => a,
            _ => cont.align_items,
        };
        let (grow, shrink) = if b.anon { (0.0, 1.0) } else { (cs.flex_grow, cs.flex_shrink) };
        let clips = !b.anon && cs.clips();
        let mut it = FlexItem {
            id: c,
            margin: g.margin,
            auto: g.auto,
            base: 0.0,
            min: 0.0,
            max: f32::INFINITY,
            grow,
            shrink,
            main: 0.0,
            frozen: false,
            cross: 0.0,
            baseline: None,
            span: None,
            align,
            cross_auto: true,
        };
        if b.anon {
            it.margin = [0.0; 4];
        }
        let definite = |l: Len, basis: Option<f32>| -> Option<f32> {
            match l {
                Len::Px(v) => Some(v),
                l if l.is_definite() => basis.map(|b| l.resolve(b)),
                _ => None,
            }
        };
        if row {
            let adj = if cs.border_box { 0.0 } else { hpad };
            let specified = (!b.anon).then(|| definite(cs.width, Some(w))).flatten().map(|v| v + adj);
            it.base = match (!b.anon).then(|| definite(cs.flex_basis, Some(w))).flatten() {
                Some(v) => v + adj,
                None => specified.unwrap_or_else(|| self.intrinsic_inner(c).1 + hpad),
            };
            it.min = match (!b.anon).then(|| definite(cs.min_width, Some(w))).flatten() {
                Some(v) => v + adj,
                None if clips => 0.0,
                None => {
                    let content = self.intrinsic_inner(c).0 + hpad;
                    specified.map_or(content, |s| s.min(content))
                }
            };
            if let Some(m) = cs.max_width.filter(|_| !b.anon).and_then(|m| definite(m, Some(w))) {
                it.max = m + adj;
            }
            it.cross_auto = b.anon || !cs.height.is_definite();
        } else {
            let adj_h = if cs.border_box { 0.0 } else { vpad };
            let adj_w = if cs.border_box { 0.0 } else { hpad };
            let avail = (w - it.margin[1] - it.margin[3]).max(0.0);
            let cross = if !b.anon && cs.width.is_definite() {
                cs.width.resolve(w) + adj_w
            } else if align == Align::Stretch && !g.auto[1] && !g.auto[3] {
                self.clamp_cross(c, avail, row, w)
            } else {
                self.shrink_width(c, avail, w)
            };
            it.cross_auto = b.anon || !cs.width.is_definite();
            let (outer_h, baseline, span) = self.atomically(PHASE_INLINE, |s| s.layout_detached(c, cross, w, None));
            let content = (outer_h - it.margin[0] - it.margin[2]).max(0.0);
            it.cross = cross;
            it.baseline = baseline;
            it.span = Some(span);
            it.base = match (!b.anon).then(|| definite(cs.flex_basis, height)).flatten() {
                Some(v) => v + adj_h,
                None => content,
            };
            it.min = match (!b.anon).then(|| definite(cs.min_height, height)).flatten() {
                Some(v) => v + adj_h,
                None if clips => 0.0,
                None => content.min(it.base.max(content)),
            };
            if let Some(m) = cs.max_height.filter(|_| !b.anon).and_then(|m| definite(m, height)) {
                it.max = m + adj_h;
            }
        }
        it.base = clamp_extent(it.base);
        it.main = it.base.min(it.max).max(it.min);
        it
    }

    /// Clamps a stretched cross size by the item's min/max in that axis.
    fn clamp_cross(&self, c: BoxId, size: f32, row: bool, w: f32) -> f32 {
        let b = &self.boxes[c];
        if b.anon {
            return size;
        }
        let cs = &self.styles[b.style];
        let g = geometry(cs, false, w);
        let (min, max, pad) =
            if row { (cs.min_height, cs.max_height, g.vpad()) } else { (cs.min_width, cs.max_width, g.hpad()) };
        let adj = if cs.border_box { 0.0 } else { pad };
        let basis = if row { self.cb_h } else { Some(w) };
        let res = |l: Len| match l {
            Len::Px(v) => Some(v + adj),
            l if l.is_definite() => basis.map(|b| l.resolve(b) + adj),
            _ => None,
        };
        let mut s = size;
        if let Some(m) = max.and_then(res) {
            s = s.min(m);
        }
        if let Some(m) = res(min) {
            s = s.max(m);
        }
        s.max(pad)
    }

    /// Height of what a span painted, for boxes without a recorded border box.
    fn span_height(&self, span: &Span) -> f32 {
        self.out[span.out.clone()]
            .iter()
            .map(|it| it.y_range())
            .fold(None, |acc: Option<(f32, f32)>, (a, b)| Some(acc.map_or((a, b), |(x, y)| (x.min(a), y.max(b)))))
            .map_or(0.0, |(a, b)| (b - a.min(0.0)).max(0.0))
    }
}

/// Leading space and space between items for `justify-content`.
fn justify(j: Justify, free: f32, n: usize) -> (f32, f32) {
    let n_f = n.max(1) as f32;
    if free <= 0.0 {
        return match j {
            Justify::End => (free, 0.0),
            Justify::Center | Justify::SpaceAround | Justify::SpaceEvenly => (free / 2.0, 0.0),
            _ => (0.0, 0.0),
        };
    }
    match j {
        Justify::Start | Justify::Stretch => (0.0, 0.0),
        Justify::End => (free, 0.0),
        Justify::Center => (free / 2.0, 0.0),
        Justify::SpaceBetween if n > 1 => (0.0, free / (n - 1) as f32),
        Justify::SpaceBetween => (0.0, 0.0),
        Justify::SpaceAround => (free / n_f / 2.0, free / n_f),
        Justify::SpaceEvenly => (free / (n_f + 1.0), free / (n_f + 1.0)),
    }
}

/// Grows or shrinks one line's items to fill `avail` (CSS Flexbox §9.7).
fn resolve_flexible(items: &mut [FlexItem], avail: f32, gaps: f32, row: bool) {
    let hypo_total: f32 = items.iter().map(|it| it.main + it.main_margins(row)).sum::<f32>() + gaps;
    let growing = hypo_total < avail;
    for it in items.iter_mut() {
        let factor = if growing { it.grow } else { it.shrink };
        it.frozen = factor == 0.0 || (growing && it.base > it.main) || (!growing && it.base < it.main);
    }
    for _ in 0..=items.len() {
        if items.iter().all(|it| it.frozen) {
            break;
        }
        let used: f32 =
            items.iter().map(|it| if it.frozen { it.main } else { it.base } + it.main_margins(row)).sum::<f32>() + gaps;
        let mut free = avail - used;
        let unfrozen = || items.iter().filter(|it| !it.frozen);
        if growing {
            let sum: f32 = unfrozen().map(|it| it.grow).sum();
            if sum < 1.0 {
                free *= sum;
            }
            for it in items.iter_mut().filter(|it| !it.frozen) {
                it.main = if sum > 0.0 { it.base + free * it.grow / sum } else { it.base };
            }
        } else {
            let sum: f32 = unfrozen().map(|it| it.shrink * it.base).sum();
            for it in items.iter_mut().filter(|it| !it.frozen) {
                it.main = if sum > 0.0 { it.base + free * it.shrink * it.base / sum } else { it.base };
            }
        }
        let mut total = 0.0f32;
        for it in items.iter().filter(|it| !it.frozen) {
            total += it.main.min(it.max).max(it.min) - it.main;
        }
        if total.abs() < 0.01 {
            for it in items.iter_mut() {
                it.frozen = true;
            }
            break;
        }
        for it in items.iter_mut().filter(|it| !it.frozen) {
            let clamped = it.main.min(it.max).max(it.min);
            if (total > 0.0 && clamped > it.main) || (total < 0.0 && clamped < it.main) {
                it.main = clamped;
                it.frozen = true;
            }
        }
    }
    for it in items.iter_mut() {
        it.main = clamp_extent(it.main.min(it.max).max(it.min));
    }
}
