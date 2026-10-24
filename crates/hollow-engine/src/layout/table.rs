//! Tables, for HTML `<table>` and CSS `display: table` alike.
//!
//! Box construction follows CSS 2.1 §17.2.1: table parts outside a table
//! get an anonymous table around them, rows outside a row group or table
//! and cells outside a row get anonymous parents, and anything else inside
//! a table or row gets an anonymous row or cell. White space between table
//! parts is dropped.
//!
//! Layout sizes columns from their cells' min/max-content widths (or, with
//! `table-layout: fixed`, from the first row), places captions outside the
//! table box, aligns cells vertically once each row's height is known, and
//! gives the rows any extra height the table's own `height` asks for. The
//! collapsing border model is approximated: no spacing or table padding,
//! and where two borders meet only one is drawn.

use super::{
    BoxId, Builder, Ctx, Deco, Flow, InlineCtx, Kid, Kind, LBox, Margin, PHASE_POSITIONED, Rect, Span, clamp_extent,
    geometry,
};
use crate::dom::NodeId;
use crate::style::{Clear, Display, Len, Position, VAlign};

use super::position::FloatCtx;

/// Most columns a table may have; cells past it are dropped.
const MAX_COLUMNS: usize = 256;

/// A table cell with its first column and column span.
type GridCell = (BoxId, usize, usize);

/// Kinds of table part that end a run of children needing an anonymous row.
fn ends_row_run(d: Display) -> bool {
    d == Display::TableRow || d == Display::TableCaption || d == Display::TableColumn || d.is_row_group()
}

impl Builder<'_> {
    /// A table box for `node` whose children are `kids`. When `anon`, the
    /// table is anonymous and `node` is the parent it was made in, whose
    /// style it takes for its inherited properties.
    pub(super) fn build_table(
        &mut self,
        node: NodeId,
        anon: bool,
        kids: &[Kid],
        depth: usize,
        ctx: InlineCtx,
    ) -> Option<BoxId> {
        if self.full() {
            return None;
        }
        let mut captions = Vec::new();
        let (mut head, mut body, mut foot) = (Vec::new(), Vec::new(), Vec::new());
        let mut i = 0;
        while i < kids.len() && !self.full() {
            let kid = kids[i];
            i += 1;
            match self.kid_display(kid) {
                None if self.is_blank(kid) => {}
                Some((_, Display::None | Display::TableColumn)) => {}
                Some((c, Display::TableCaption)) => {
                    captions.extend(self.build_level(c, Display::TableCaption, depth + 1, None, ctx));
                }
                Some((r, Display::TableRow)) => {
                    let k = self.kids(r);
                    body.extend(self.build_row(r, false, &k, depth + 1, ctx));
                }
                Some((g, d)) if d.is_row_group() => {
                    let rows = match d {
                        Display::TableHeaderGroup => &mut head,
                        Display::TableFooterGroup => &mut foot,
                        _ => &mut body,
                    };
                    self.group_rows(g, depth + 1, ctx, rows);
                }
                _ => {
                    // Cells, blocks and text: an anonymous row.
                    let start = i - 1;
                    while i < kids.len() && !self.kid_display(kids[i]).is_some_and(|(_, d)| ends_row_run(d)) {
                        i += 1;
                    }
                    body.extend(self.build_row(node, true, &kids[start..i], depth + 1, ctx));
                }
            }
        }
        head.append(&mut body);
        head.append(&mut foot);
        Some(self.push_box(LBox {
            style: node,
            anon,
            kind: Kind::Table { captions, rows: head },
            marker: None,
            colspan: 1,
        }))
    }

    /// The rows of a row group; anything but rows gets an anonymous row.
    fn group_rows(&mut self, group: NodeId, depth: usize, ctx: InlineCtx, rows: &mut Vec<BoxId>) {
        let kids = self.kids(group);
        let mut i = 0;
        while i < kids.len() && !self.full() {
            let kid = kids[i];
            i += 1;
            match self.kid_display(kid) {
                None if self.is_blank(kid) => {}
                Some((_, Display::None)) => {}
                Some((r, Display::TableRow)) => {
                    let k = self.kids(r);
                    rows.extend(self.build_row(r, false, &k, depth + 1, ctx));
                }
                _ => {
                    let start = i - 1;
                    while i < kids.len() && !self.kid_display(kids[i]).is_some_and(|(_, d)| d == Display::TableRow) {
                        i += 1;
                    }
                    rows.extend(self.build_row(group, true, &kids[start..i], depth + 1, ctx));
                }
            }
        }
    }

    /// A row of `node` holding `kids`; anything but cells gets an anonymous
    /// cell, together with its neighbours up to the next cell.
    fn build_row(&mut self, node: NodeId, anon: bool, kids: &[Kid], depth: usize, ctx: InlineCtx) -> Option<BoxId> {
        if self.full() {
            return None;
        }
        let mut cells = Vec::new();
        let mut i = 0;
        while i < kids.len() && !self.full() {
            let kid = kids[i];
            i += 1;
            match self.kid_display(kid) {
                None if self.is_blank(kid) => {}
                Some((_, Display::None)) => {}
                Some((c, Display::TableCell)) => {
                    cells.extend(self.build_level(c, Display::TableCell, depth + 1, None, ctx));
                }
                _ => {
                    let start = i - 1;
                    while i < kids.len() && !self.kid_display(kids[i]).is_some_and(|(_, d)| d == Display::TableCell) {
                        i += 1;
                    }
                    let kind = self.container_of(node, &kids[start..i], depth + 1, ctx);
                    if !self.full() {
                        cells.push(self.push_box(LBox { style: node, anon: true, kind, marker: None, colspan: 1 }));
                    }
                }
            }
        }
        Some(self.push_box(LBox { style: node, anon, kind: Kind::Row(cells), marker: None, colspan: 1 }))
    }
}

/// A cell after its first layout, waiting for its row's height.
struct Placed {
    cell: BoxId,
    x: f32,
    w: f32,
    /// Border-box height and baseline, both from the row's top.
    h: f32,
    base: f32,
    valign: VAlign,
    deco: Deco,
    /// What the cell's contents produced.
    span: Span,
}

struct RowOut {
    y: f32,
    h: f32,
    /// The baseline of the row's baseline-aligned cells, from its top.
    base: f32,
    cells: Vec<Placed>,
    bg: Option<(usize, crate::css::Color)>,
}

fn baseline_aligned(v: VAlign) -> bool {
    matches!(v, VAlign::Baseline | VAlign::Sub | VAlign::Super)
}

impl Ctx<'_> {
    /// Cells of each row with their start column and span, and the number
    /// of columns.
    pub(super) fn grid(&self, rows: &[BoxId]) -> (Vec<Vec<GridCell>>, usize) {
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

    /// Minimum and maximum widths of each column, and the percentage width
    /// cells ask for (0 for none).
    pub(super) fn columns(&mut self, grid: &[Vec<GridCell>], n: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let mut mins = vec![0.0f32; n];
        let mut maxs = vec![0.0f32; n];
        let mut pcts = vec![0.0f32; n];
        for row in grid {
            for &(c, col, span) in row {
                if span == 1 {
                    let (mn, mx) = self.intrinsic(c);
                    mins[col] = mins[col].max(mn);
                    maxs[col] = maxs[col].max(mx);
                    if !self.boxes[c].anon
                        && let Len::Pct(p) = self.styles[self.boxes[c].style].width
                    {
                        pcts[col] = pcts[col].max(p.clamp(0.0, 100.0));
                    }
                }
            }
        }
        for row in grid {
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
        // Percentages over 100% in total are cut back from the right.
        let mut left = 100.0f32;
        for p in &mut pcts {
            *p = p.min(left);
            left -= *p;
        }
        (mins, maxs, pcts)
    }

    /// Column widths for `table-layout: fixed`: the first row's specified
    /// widths, the rest shared equally.
    fn fixed_widths(&self, grid: &[Vec<GridCell>], n: usize, total: f32) -> Vec<f32> {
        let mut widths: Vec<Option<f32>> = vec![None; n];
        for &(c, col, span) in grid.first().map_or(&[][..], |r| &r[..]) {
            let b = &self.boxes[c];
            let st = &self.styles[b.style];
            if b.anon || !st.width.is_definite() {
                continue;
            }
            let g = geometry(st, false, total);
            let w = st.width.resolve(total) + if st.border_box { 0.0 } else { g.hpad() };
            for slot in &mut widths[col..col + span] {
                *slot = Some(clamp_extent(w / span as f32));
            }
        }
        let fixed: f32 = widths.iter().flatten().sum();
        let free = widths.iter().filter(|w| w.is_none()).count();
        if free == 0 && fixed > 0.0 && fixed < total {
            return widths.iter().map(|w| w.unwrap_or(0.0) * total / fixed).collect();
        }
        let share = if free > 0 { ((total - fixed) / free as f32).max(0.0) } else { 0.0 };
        widths.iter().map(|w| w.unwrap_or(share)).collect()
    }

    pub(super) fn layout_table(&mut self, id: BoxId, x: f32, cb_w: f32, flow: &mut Flow) -> Option<f32> {
        let boxes = self.boxes;
        let styles = self.styles;
        let force_w = self.force_w.take();
        let force_h = self.force_h.take();
        let b = &boxes[id];
        let Kind::Table { captions, rows } = &b.kind else { return None };
        let st = &styles[b.style];
        let anon = b.anon;
        let start = self.mark();
        let g = geometry(st, anon, cb_w);
        let [mt, mut mr, mb, mut ml] = g.margin;
        let collapse = st.border_collapse;
        // The collapsing border model has no table padding and no spacing.
        let padding = if collapse { [0.0; 4] } else { g.padding };
        let [pt, pr, pb, pl] = padding;
        let [bt, br, bb, bl] = g.border;
        if g.auto[3] {
            ml = 0.0;
        }
        if g.auto[1] {
            mr = 0.0;
        }
        let [hs, vs] = if collapse { [0.0; 2] } else { st.border_spacing };
        let (grid, n) = self.grid(rows);
        let (cols_min, cols_max, pcts) = self.columns(&grid, n);
        let chrome = hs * (n as f32 + 1.0) + pl + pr + bl + br;
        let sum_min = cols_min.iter().sum::<f32>() + chrome;
        let mut sum_max = cols_max.iter().sum::<f32>() + chrome;
        // Percentage columns make an automatic table as wide as they need.
        let pct_total: f32 = pcts.iter().sum();
        if pct_total > 0.0 {
            for (i, &p) in pcts.iter().enumerate() {
                if p > 0.0 {
                    sum_max = sum_max.max(cols_max[i] * 100.0 / p + chrome);
                }
            }
            if pct_total < 100.0 {
                let rest: f32 = cols_max.iter().zip(&pcts).filter(|(_, p)| **p <= 0.0).map(|(m, _)| m).sum();
                sum_max = sum_max.max(rest * 100.0 / (100.0 - pct_total) + chrome);
            }
        }
        let avail = (cb_w - ml - mr).max(0.0);
        let adj = if st.border_box { 0.0 } else { pl + pr + bl + br };
        let width = if anon { Len::Auto } else { st.width };
        let mut tw = match force_w {
            Some(w) => w,
            None if width.is_definite() => width.resolve(cb_w) + adj,
            None => sum_max.min(avail),
        };
        if !anon {
            if let Some(max) = st.max_width.filter(|m| m.is_definite()) {
                tw = tw.min(max.resolve(cb_w) + adj);
            }
            if st.min_width.is_definite() {
                tw = tw.max(st.min_width.resolve(cb_w) + adj);
            }
        }
        let widths = if !anon && st.table_fixed && width.is_definite() {
            let w = self.fixed_widths(&grid, n, (tw - chrome).max(0.0));
            tw = tw.max(w.iter().sum::<f32>() + chrome);
            w
        } else {
            tw = tw.max(sum_min);
            column_widths(&cols_min, &cols_max, &pcts, (tw - chrome).max(0.0))
        };
        let tw = clamp_extent(tw);
        if force_w.is_none() {
            let free = cb_w - tw - ml - mr;
            match (g.auto[3], g.auto[1]) {
                (true, true) => ml += (free / 2.0).max(0.0),
                (true, false) => ml += free.max(0.0),
                _ => {}
            }
        }
        if !anon && force_w.is_none() && st.clear != Clear::None {
            self.clear(st.clear, mt, flow);
        }
        let mut tx = x + ml;
        // Tables never overlap floats either.
        if force_w.is_none() {
            let (l, r) = self.band(flow.next() + mt, 1.0, x, x + cb_w);
            if (l > x || r < x + cb_w) && tw <= r - l {
                tx = l + ml;
            }
        }
        let saved = (self.z, self.clip, self.paint_path, self.paint_depth);
        let positioned = !anon && st.positioned();
        if positioned {
            if let Some(z) = st.z_index {
                self.z = z;
            }
            self.enter_atomic(PHASE_POSITIONED);
            self.abs.push(Vec::new());
        }

        flow.margin.push(mt);
        let top = flow.place();
        let mut deco = self.reserve_deco(st, b);
        let mut first_baseline = None;
        // Captions sit outside the table box, above or below it.
        let mut y = self.captions(captions, false, tx, tw, top, &mut first_baseline);
        let table_top = y;
        y += bt + pt;
        let inner_x = tx + bl + pl;
        let inner_w = (tw - pl - pr - bl - br).max(0.0);
        if !rows.is_empty() {
            y += vs;
        }
        let mut out_rows: Vec<RowOut> = Vec::with_capacity(rows.len());
        for (ri, &row) in rows.iter().enumerate() {
            let rb = &boxes[row];
            let row_style = &styles[rb.style];
            let bg = self.bg_slot(row_style, rb.anon);
            let mut cells = Vec::with_capacity(grid[ri].len());
            let mut row_h = 0.0f32;
            let mut base = 0.0f32;
            for (ci, &(cell, col, span)) in grid[ri].iter().enumerate() {
                let cx = inner_x + hs + widths[..col].iter().sum::<f32>() + hs * col as f32;
                let cw = widths[col..col + span].iter().sum::<f32>() + hs * (span as f32 - 1.0);
                let cb = &boxes[cell];
                let cell_style = &styles[cb.style];
                let own = if cb.anon { [0.0; 4] } else { cell_style.border_width };
                let border = if collapse { self.collapsed(&grid, ri, ci, own, g.border, n) } else { own };
                let mut deco = self.reserve_deco(cell_style, cb);
                deco.border = border;
                deco.padding = geometry(cell_style, cb.anon, cw).padding;
                let m = self.mark();
                let (h, cell_base) = self.layout_cell(cell, cx, cw, y, border);
                let mut valign = if cb.anon { VAlign::Baseline } else { cell_style.vertical_align };
                // A cell without a line of its own takes no part in the
                // row's baseline (its baseline is its content box bottom).
                if cell_base.is_none() && baseline_aligned(valign) {
                    valign = VAlign::Top;
                }
                let rel = cell_base.unwrap_or(y + h - geometry(cell_style, cb.anon, cw).padding[2] - border[2]) - y;
                if baseline_aligned(valign) {
                    base = base.max(rel);
                }
                row_h = row_h.max(h);
                cells.push(Placed { cell, x: cx, w: cw, h, base: rel, valign, deco, span: self.span_since(m) });
            }
            // Baseline-aligned cells move down to the row's baseline, which
            // may make the row taller.
            for p in cells.iter().filter(|p| baseline_aligned(p.valign)) {
                row_h = row_h.max(p.h + base - p.base);
            }
            if !rb.anon
                && let Len::Px(h) = row_style.height
            {
                row_h = row_h.max(h);
            }
            out_rows.push(RowOut { y, h: row_h, base, cells, bg });
            y += row_h + vs;
        }
        // A table taller than its rows (`height`) shares the rest out among
        // them, in proportion to their heights.
        let content_h = y + pb + bb - table_top;
        self.table_rows_h.insert(id, content_h);
        let vpad = pt + pb + bt + bb;
        let min_h = force_h
            .or_else(|| {
                (!anon)
                    .then(|| self.definite_height(st.height))
                    .flatten()
                    .map(|h| h + if st.border_box { 0.0 } else { vpad })
            })
            .unwrap_or(0.0);
        let extra = (min_h - content_h).max(0.0);
        let rows_h: f32 = out_rows.iter().map(|r| r.h).sum();
        let mut shift = 0.0f32;
        for (ri, r) in out_rows.iter_mut().enumerate() {
            let share = row_share(extra, rows_h, r.h, rows.len());
            let laid_y = r.y;
            r.y += shift;
            r.h += share;
            for p in &r.cells {
                let cell_style = &styles[boxes[p.cell].style];
                let dy = match p.valign {
                    VAlign::Middle => (r.h - p.h) / 2.0,
                    VAlign::Bottom => r.h - p.h,
                    VAlign::Top => 0.0,
                    _ => r.base - p.base,
                };
                // Aligning the contents leaves absolutely positioned boxes'
                // static positions where they were.
                let mut contents = p.span.clone();
                contents.pending = contents.pending.start..contents.pending.start;
                self.shift_span(&p.span, 0.0, r.y - laid_y);
                self.shift_span(&contents, 0.0, dy);
                let mut d = p.deco;
                d.rect = Rect { x: p.x, y: r.y, w: p.w, h: r.h };
                self.paint_deco(cell_style, &d);
                if ri == 0 && first_baseline.is_none() {
                    first_baseline = Some(r.y + p.base + dy);
                }
            }
            if let Some((slot, color)) = r.bg {
                let rect = Rect { x: inner_x + hs, y: r.y, w: (inner_w - hs * 2.0).max(0.0), h: r.h };
                self.fill(Some(slot), rect, color);
            }
            shift += share;
        }
        let bottom = y + shift + pb + bb + if out_rows.is_empty() { extra } else { 0.0 };
        deco.rect = Rect { x: tx, y: table_top, w: tw, h: bottom - table_top };
        deco.border = g.border;
        deco.padding = padding;
        self.paint_deco(st, &deco);
        self.deco[id] = Some(deco);
        let end = self.captions(captions, true, tx, tw, bottom, &mut first_baseline);
        if positioned {
            self.finish_frame(deco.rect);
        }
        (self.z, self.clip, self.paint_path, self.paint_depth) = saved;
        flow.y = end;
        flow.margin = Margin::default();
        flow.margin.push(mb);
        if !anon && st.position == Position::Relative {
            let (dx, dy) = self.relative_offset(st, cb_w);
            let span = self.span_since(start);
            self.shift_span(&span, dx, dy);
        }
        first_baseline
    }

    /// Lays out the captions on one side, from `y` down; returns where they
    /// end.
    fn captions(
        &mut self,
        captions: &[BoxId],
        bottom: bool,
        x: f32,
        w: f32,
        mut y: f32,
        base: &mut Option<f32>,
    ) -> f32 {
        // Each caption is its own formatting context.
        self.floats.push(FloatCtx::default());
        for &c in captions.iter().filter(|&&c| self.styles[self.boxes[c].style].caption_bottom == bottom) {
            let mut f = Flow::new(y);
            let r = self.layout_box(c, x, w, &mut f);
            if base.is_none() {
                *base = r;
            }
            y = f.y + f.margin.value().max(0.0);
        }
        self.floats.pop();
        y
    }

    /// A cell's borders in the collapsing model: where it meets the table's
    /// border or a neighbour's on its top or left, that one is drawn instead.
    fn collapsed(
        &self,
        grid: &[Vec<GridCell>],
        ri: usize,
        ci: usize,
        own: [f32; 4],
        table: [f32; 4],
        n: usize,
    ) -> [f32; 4] {
        let border_of = |cell: BoxId| {
            let b = &self.boxes[cell];
            if b.anon { [0.0; 4] } else { self.styles[b.style].border_width }
        };
        let (_, col, span) = grid[ri][ci];
        let [mut t, mut r, mut b, mut l] = own;
        let above = ri.checked_sub(1).and_then(|r| grid[r].iter().find(|&&(_, c, s)| c <= col && col < c + s));
        if (ri == 0 && table[0] > 0.0) || above.is_some_and(|&(a, _, _)| border_of(a)[2] > 0.0) {
            t = 0.0;
        }
        if (col == 0 && table[3] > 0.0) || (ci > 0 && border_of(grid[ri][ci - 1].0)[1] > 0.0) {
            l = 0.0;
        }
        if col + span >= n && table[1] > 0.0 {
            r = 0.0;
        }
        if ri + 1 == grid.len() && table[2] > 0.0 {
            b = 0.0;
        }
        [t, r, b, l]
    }

    /// Lays out a cell's contents as an independent formatting context.
    /// Returns its border-box height and the baseline of its first line.
    fn layout_cell(&mut self, cell: BoxId, cx: f32, cw: f32, y: f32, border: [f32; 4]) -> (f32, Option<f32>) {
        let boxes = self.boxes;
        let styles = self.styles;
        let b = &boxes[cell];
        let st = &styles[b.style];
        let g = geometry(st, b.anon, cw);
        let [pt, pr, pb, pl] = g.padding;
        let [bt, br, bb, bl] = border;
        let content_x = cx + bl + pl;
        let content_w = (cw - pl - pr - bl - br).max(0.0);
        let top = y + bt + pt;
        let mut f = Flow::new(top);
        self.floats.push(FloatCtx::default());
        let saved = self.cb_h;
        // A cell's own height is what percentages inside it refer to.
        self.cb_h = match st.height {
            Len::Px(h) if !b.anon => Some((h - if st.border_box { pt + pb + bt + bb } else { 0.0 }).max(0.0)),
            _ => None,
        };
        let baseline = match &b.kind {
            Kind::Table { .. } => self.layout_table(cell, content_x, content_w, &mut f),
            _ => self.layout_contents(cell, content_x, content_w, &mut f, None),
        };
        self.cb_h = saved;
        let floats = self.floats.pop().map_or(0.0, |fc| fc.bottom(Clear::Both));
        let mut end = (f.y + f.margin.value()).max(top).max(floats);
        if !b.anon
            && let Len::Px(h) = st.height
        {
            let h = if st.border_box { h - pt - pb - bt - bb } else { h };
            end = end.max(top + h);
        }
        (end - y + pb + bb, baseline)
    }
}

/// A row's share of a table's extra height.
fn row_share(extra: f32, total: f32, h: f32, rows: usize) -> f32 {
    if extra <= 0.0 {
        0.0
    } else if total > 0.0 {
        extra * h / total
    } else {
        extra / rows.max(1) as f32
    }
}

/// Column widths for an automatic table `total` wide (without spacing):
/// percentage columns get their share first, the others split the rest.
fn column_widths(mins: &[f32], maxs: &[f32], pcts: &[f32], total: f32) -> Vec<f32> {
    if pcts.iter().all(|p| *p <= 0.0) {
        return distribute(mins, maxs, total);
    }
    let mut widths: Vec<f32> =
        pcts.iter().zip(mins).map(|(p, mn)| if *p > 0.0 { (total * p / 100.0).max(*mn) } else { 0.0 }).collect();
    let rest: Vec<usize> = (0..pcts.len()).filter(|&i| pcts[i] <= 0.0).collect();
    let used: f32 = widths.iter().sum();
    let rest_min: f32 = rest.iter().map(|&i| mins[i]).sum();
    if used + rest_min > total {
        // Too much: percentage columns give back what they have over their
        // minimum, in proportion.
        let slack: f32 = (0..pcts.len()).filter(|&i| pcts[i] > 0.0).map(|i| widths[i] - mins[i]).sum();
        let over = (used + rest_min - total).min(slack);
        if slack > 0.0 {
            for i in (0..pcts.len()).filter(|&i| pcts[i] > 0.0) {
                widths[i] -= over * (widths[i] - mins[i]) / slack;
            }
        }
    }
    let used: f32 = widths.iter().sum();
    if rest.is_empty() {
        // Only percentage columns: they stretch to fill the table.
        if used > 0.0 && used < total {
            return widths.iter().map(|w| w * total / used).collect();
        }
        return widths;
    }
    let rest_mins: Vec<f32> = rest.iter().map(|&i| mins[i]).collect();
    let rest_maxs: Vec<f32> = rest.iter().map(|&i| maxs[i]).collect();
    for (k, w) in distribute(&rest_mins, &rest_maxs, (total - used).max(0.0)).into_iter().enumerate() {
        widths[rest[k]] = w;
    }
    widths
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
    fn distributes_column_widths() {
        assert_eq!(distribute(&[10.0, 10.0], &[20.0, 60.0], 80.0), vec![20.0, 60.0]);
        assert_eq!(distribute(&[10.0, 10.0], &[20.0, 60.0], 160.0), vec![40.0, 120.0]);
        assert_eq!(distribute(&[10.0, 10.0], &[20.0, 60.0], 10.0), vec![10.0, 10.0]);
        let mid = distribute(&[10.0, 10.0], &[20.0, 60.0], 50.0);
        assert!((mid[0] - 15.0).abs() < 1e-4 && (mid[1] - 35.0).abs() < 1e-4);
    }

    #[test]
    fn percentage_columns_come_first() {
        assert_eq!(column_widths(&[10.0, 10.0], &[20.0, 60.0], &[25.0, 0.0], 200.0), vec![50.0, 150.0]);
        // Too narrow: the percentage column shrinks toward its minimum.
        let w = column_widths(&[10.0, 100.0], &[20.0, 100.0], &[80.0, 0.0], 150.0);
        assert!((w[0] - 50.0).abs() < 1e-3 && (w[1] - 100.0).abs() < 1e-3, "{w:?}");
        assert_eq!(column_widths(&[0.0, 0.0], &[0.0, 0.0], &[20.0, 30.0], 100.0), vec![40.0, 60.0]);
    }
}
