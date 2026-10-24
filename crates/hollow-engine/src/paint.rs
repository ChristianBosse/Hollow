//! Rasterizing the display list with tiny-skia, on the CPU.
//!
//! Rectangles are snapped to whole device pixels (as browsers do), glyphs
//! and images are antialiased. Everything is plain IEEE arithmetic on f32,
//! with no fused multiply-add and no platform math library calls, so the
//! output is a pure function of the inputs.

use std::collections::BTreeMap;

use tiny_skia::{
    FillRule, FilterQuality, GradientStop, IntSize, LinearGradient, Mask, Paint, PathBuilder, PathSegment, Pattern,
    Pixmap, PixmapPaint, Point, RadialGradient, Rect as SkRect, Shader, SpreadMode, Stroke, Transform,
};

use crate::css::Color;
use crate::layout::{BgSource, DisplayItem, GradientGeom, Laid, Rect, ResolvedGradient};
use crate::text::{self, FaceId};

/// Largest frame we will allocate: 64 Mpx (256 MiB of RGBA).
const MAX_FRAME_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_FRAME_SIDE: u32 = 16_384;
/// Largest side of an offscreen gradient tile, in device pixels; larger
/// tiles are drawn at this resolution and scaled.
const MAX_TILE_SIDE: u32 = 1024;

/// Glyph outlines in font units, keyed by (face, glyph).
pub(crate) type GlyphPaths = BTreeMap<(FaceId, u32), tiny_skia::Path>;

pub(crate) fn glyph_paths(laid: &Laid) -> GlyphPaths {
    let mut paths = GlyphPaths::new();
    let mut missing = std::collections::BTreeSet::new();
    for it in &laid.items {
        if let DisplayItem::Text { face, glyphs, .. } = it {
            for g in laid.glyphs.get(glyphs.start as usize..glyphs.end as usize).unwrap_or(&[]) {
                let key = (*face, g.id);
                if paths.contains_key(&key) || missing.contains(&key) {
                    continue;
                }
                match text::outline(*face, g.id) {
                    Some(p) => {
                        paths.insert(key, p);
                    }
                    None => {
                        missing.insert(key);
                    }
                }
            }
        }
    }
    paths
}

fn paint_for(c: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c.r, c.g, c.b, c.a);
    p.anti_alias = true;
    p
}

/// Frame dimensions for a viewport, after clamping.
pub(crate) fn frame_size(width: f32, height: f32, scale: f32) -> (u32, u32) {
    let w = ceil_px(width * scale).clamp(1, MAX_FRAME_SIDE);
    let max_h = (MAX_FRAME_PIXELS / u64::from(w)).min(u64::from(MAX_FRAME_SIDE)) as u32;
    let h = ceil_px(height * scale).clamp(1, max_h.max(1));
    (w, h)
}

fn ceil_px(v: f32) -> u32 {
    if v.is_finite() && v > 0.0 { v.ceil().min(u32::MAX as f32) as u32 } else { 1 }
}

/// Returns (width, height, premultiplied RGBA).
/// `extra` holds outlines of glyphs added after layout (form field values).
pub(crate) fn paint(
    laid: &Laid,
    paths: &GlyphPaths,
    extra: &GlyphPaths,
    scroll_y: f32,
    height: f32,
    scale: f32,
) -> (u32, u32, Vec<u8>) {
    let scale = if scale.is_finite() && scale > 0.0 { scale.clamp(0.1, 8.0) } else { 1.0 };
    let scroll_y = if scroll_y.is_finite() { scroll_y.clamp(-1e7, 1e7) } else { 0.0 };
    let height = if height.is_finite() { height.max(0.0) } else { 0.0 };
    let (w, h) = frame_size(laid.width, height, scale);
    let Some(mut pixmap) = Pixmap::new(w, h) else {
        return (w, h, vec![255; (w as usize) * (h as usize) * 4]);
    };
    let c = laid.canvas;
    pixmap.fill(tiny_skia::Color::from_rgba8(c.r, c.g, c.b, 255));
    let view_bottom = scroll_y + h as f32 / scale;
    let device = |r: &Rect| -> [f32; 4] {
        [
            (r.x * scale).round(),
            ((r.y - scroll_y) * scale).round(),
            ((r.x + r.w) * scale).round(),
            ((r.y + r.h - scroll_y) * scale).round(),
        ]
    };
    // Masks for clips that cut through glyphs or images, made on first use.
    let mut masks: BTreeMap<u32, Mask> = BTreeMap::new();

    for (i, it) in laid.items.iter().enumerate() {
        let (top, bottom) = it.y_range();
        if bottom < scroll_y || top > view_bottom {
            continue;
        }
        let clip_id = laid.clips.get(i).copied().unwrap_or(0);
        let clip = laid.clip_rects.get(clip_id as usize).filter(|_| clip_id != 0).map(device);
        if let Some([cx0, cy0, cx1, cy1]) = clip
            && (cx1 <= cx0 || cy1 <= cy0 || cy1 <= 0.0 || cy0 >= h as f32)
        {
            continue;
        }
        // For a device-space box: None if clipped away entirely, else
        // whether it needs the mask.
        let coverage = |b: [f32; 4]| -> Option<bool> {
            match clip {
                None => Some(false),
                Some(c) if b[2] <= c[0] || b[0] >= c[2] || b[3] <= c[1] || b[1] >= c[3] => None,
                Some(c) => Some(b[0] < c[0] || b[1] < c[1] || b[2] > c[2] || b[3] > c[3]),
            }
        };
        let make_mask = |masks: &mut BTreeMap<u32, Mask>| -> bool {
            if !masks.contains_key(&clip_id)
                && let Some(m) = clip.and_then(|c| rect_mask(w, h, c))
            {
                masks.insert(clip_id, m);
            }
            masks.contains_key(&clip_id)
        };
        match it {
            DisplayItem::Rect { rect, color } => {
                if color.a == 0 || rect.w <= 0.0 || rect.h <= 0.0 {
                    continue;
                }
                // Snap to device pixels; keep hairlines at least one pixel.
                let mut x0 = (rect.x * scale).round();
                let mut y0 = ((rect.y - scroll_y) * scale).round();
                let mut x1 = ((rect.x + rect.w) * scale).round().max(x0 + 1.0);
                let mut y1 = (((rect.y + rect.h) - scroll_y) * scale).round().max(y0 + 1.0);
                if let Some([cx0, cy0, cx1, cy1]) = clip {
                    x0 = x0.max(cx0);
                    y0 = y0.max(cy0);
                    x1 = x1.min(cx1);
                    y1 = y1.min(cy1);
                    if x1 <= x0 || y1 <= y0 {
                        continue;
                    }
                }
                if let Some(r) = SkRect::from_ltrb(x0, y0, x1, y1) {
                    let mut p = paint_for(*color);
                    p.anti_alias = false;
                    pixmap.fill_rect(r, &p, Transform::identity(), None);
                }
            }
            DisplayItem::Round { rect, radius, stroke, color } => {
                if color.a == 0 || rect.w <= 0.0 || rect.h <= 0.0 {
                    continue;
                }
                let b = device(rect);
                let Some(masked) = coverage(b) else { continue };
                if masked && !make_mask(&mut masks) {
                    continue;
                }
                let mask = if masked { masks.get(&clip_id) } else { None };
                let inset = if *stroke > 0.0 { stroke * scale / 2.0 } else { 0.0 };
                let r = (radius * scale - inset).max(0.0);
                let Some(path) = rounded_rect(b[0] + inset, b[1] + inset, b[2] - inset, b[3] - inset, r) else {
                    continue;
                };
                let p = paint_for(*color);
                if *stroke > 0.0 {
                    let s = Stroke { width: stroke * scale, ..Stroke::default() };
                    pixmap.stroke_path(&path, &p, &s, Transform::identity(), mask);
                } else {
                    pixmap.fill_path(&path, &p, FillRule::Winding, Transform::identity(), mask);
                }
            }
            DisplayItem::Text { face, size, color, x, y, glyphs } => {
                let Some(f) = text::face(*face) else { continue };
                let k = f.scale(*size);
                let run = laid.glyphs.get(glyphs.start as usize..glyphs.end as usize).unwrap_or(&[]);
                let mut mask = None;
                if clip.is_some() {
                    let (gx0, gx1) =
                        run.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), g| (a.min(g.x), b.max(g.x)));
                    let b = [
                        (x + gx0.min(0.0)) * scale,
                        (top - scroll_y) * scale,
                        (x + gx1.max(0.0) + size) * scale,
                        (bottom - scroll_y) * scale,
                    ];
                    let Some(masked) = coverage(b) else { continue };
                    if masked {
                        if !make_mask(&mut masks) {
                            continue;
                        }
                        mask = masks.get(&clip_id);
                    }
                }
                let mut pb = PathBuilder::new();
                for g in run {
                    let Some(path) = paths.get(&(*face, g.id)).or_else(|| extra.get(&(*face, g.id))) else { continue };
                    let ox = (x + g.x) * scale;
                    let oy = (y + g.y - scroll_y) * scale;
                    let s = k * scale;
                    // Font units are y-up; the page is y-down.
                    let map = |p: tiny_skia::Point| (ox + p.x * s, oy - p.y * s);
                    for seg in path.segments() {
                        match seg {
                            PathSegment::MoveTo(p) => {
                                let (a, b) = map(p);
                                pb.move_to(a, b);
                            }
                            PathSegment::LineTo(p) => {
                                let (a, b) = map(p);
                                pb.line_to(a, b);
                            }
                            PathSegment::QuadTo(p1, p) => {
                                let ((a, b), (c, d)) = (map(p1), map(p));
                                pb.quad_to(a, b, c, d);
                            }
                            PathSegment::CubicTo(p1, p2, p) => {
                                let ((a, b), (c, d), (e, f)) = (map(p1), map(p2), map(p));
                                pb.cubic_to(a, b, c, d, e, f);
                            }
                            PathSegment::Close => pb.close(),
                        }
                    }
                }
                if let Some(path) = pb.finish() {
                    pixmap.fill_path(&path, &paint_for(*color), FillRule::Winding, Transform::identity(), mask);
                }
            }
            DisplayItem::Image { rect, image } => {
                let Some(img) = laid.images.get(*image as usize) else { continue };
                if rect.w <= 0.0 || rect.h <= 0.0 {
                    continue;
                }
                let Some(masked) = coverage(device(rect)) else { continue };
                if masked && !make_mask(&mut masks) {
                    continue;
                }
                let mask = if masked { masks.get(&clip_id) } else { None };
                let sx = rect.w * scale / img.width() as f32;
                let sy = rect.h * scale / img.height() as f32;
                let ts = Transform::from_row(sx, 0.0, 0.0, sy, rect.x * scale, (rect.y - scroll_y) * scale);
                let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..PixmapPaint::default() };
                pixmap.draw_pixmap(0, 0, (**img).as_ref(), &paint, ts, mask);
            }
            // A marker for the form painter; drawn by the items it adds.
            DisplayItem::Field { .. } => {}
            DisplayItem::Quad { points, color } => {
                if color.a == 0 {
                    continue;
                }
                // Corners snap to device pixels, like rectangles do.
                let snap = |p: &(f32, f32)| ((p.0 * scale).round(), ((p.1 - scroll_y) * scale).round());
                let pts: Vec<(f32, f32)> = points.iter().map(snap).collect();
                let b =
                    pts.iter().fold([f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY], |b, p| {
                        [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)]
                    });
                if b[2] <= b[0] || b[3] <= b[1] {
                    continue;
                }
                let Some(masked) = coverage(b) else { continue };
                if masked && !make_mask(&mut masks) {
                    continue;
                }
                let mask = if masked { masks.get(&clip_id) } else { None };
                let mut pb = PathBuilder::new();
                pb.move_to(pts[0].0, pts[0].1);
                for p in &pts[1..] {
                    pb.line_to(p.0, p.1);
                }
                pb.close();
                if let Some(path) = pb.finish() {
                    pixmap.fill_path(&path, &paint_for(*color), FillRule::Winding, Transform::identity(), mask);
                }
            }
            DisplayItem::Background { area, radius, tile, repeat, image, .. } => {
                if area.w <= 0.0 || area.h <= 0.0 || tile.w <= 0.0 || tile.h <= 0.0 {
                    continue;
                }
                // Tiles fill the area in the repeating axes; otherwise there
                // is just the one.
                let mut fill = *area;
                if !repeat[0] {
                    let x0 = fill.x.max(tile.x);
                    fill.w = ((fill.x + fill.w).min(tile.x + tile.w) - x0).max(0.0);
                    fill.x = x0;
                }
                if !repeat[1] {
                    let y0 = fill.y.max(tile.y);
                    fill.h = ((fill.y + fill.h).min(tile.y + tile.h) - y0).max(0.0);
                    fill.y = y0;
                }
                if fill.w <= 0.0 || fill.h <= 0.0 {
                    continue;
                }
                let b = device(&fill);
                let Some(masked) = coverage(b) else { continue };
                if masked && !make_mask(&mut masks) {
                    continue;
                }
                let mask = if masked { masks.get(&clip_id) } else { None };
                let rounded = *radius > 0.0 && fill == *area;
                let path = if rounded {
                    rounded_rect(b[0], b[1], b[2], b[3], radius * scale)
                } else {
                    SkRect::from_ltrb(b[0], b[1], b[2].max(b[0] + 1.0), b[3].max(b[1] + 1.0))
                        .map(PathBuilder::from_rect)
                };
                let Some(path) = path else { continue };
                // The tile in device space.
                let (ox, oy) = (tile.x * scale, (tile.y - scroll_y) * scale);
                let (tw, th) = (tile.w * scale, tile.h * scale);
                let repeats = repeat[0] || repeat[1];
                // Kept alive for the pattern that borrows it.
                let tile_pixmap;
                let mut paint = Paint { anti_alias: rounded, ..Paint::default() };
                match image {
                    BgSource::Image(id) => {
                        let Some(img) = laid.images.get(*id as usize) else { continue };
                        let ts =
                            Transform::from_row(tw / img.width() as f32, 0.0, 0.0, th / img.height() as f32, ox, oy);
                        let spread = if repeats { SpreadMode::Repeat } else { SpreadMode::Pad };
                        paint.shader = Pattern::new((**img).as_ref(), spread, FilterQuality::Bilinear, 1.0, ts);
                    }
                    BgSource::Gradient(g) => {
                        let covers = |a0: f32, a1: f32, t0: f32, t1: f32| t0 <= a0 && t1 >= a1;
                        let whole = (!repeat[0] || covers(fill.x, fill.x + fill.w, tile.x, tile.x + tile.w))
                            && (!repeat[1] || covers(fill.y, fill.y + fill.h, tile.y, tile.y + tile.h));
                        if whole {
                            let ts = Transform::from_row(scale, 0.0, 0.0, scale, ox, oy);
                            let Some(shader) = gradient_shader(g, ts) else { continue };
                            paint.shader = shader;
                        } else {
                            // Smaller tiles that repeat: draw one, then tile it.
                            let (pw, ph) = (tile_px(tw), tile_px(th));
                            let Some(mut p) = Pixmap::new(pw, ph) else { continue };
                            let k = Transform::from_scale(pw as f32 / tile.w, ph as f32 / tile.h);
                            let Some(shader) = gradient_shader(g, k) else { continue };
                            let full = SkRect::from_xywh(0.0, 0.0, pw as f32, ph as f32);
                            if let Some(r) = full {
                                p.fill_rect(r, &Paint { shader, ..Paint::default() }, Transform::identity(), None);
                            }
                            tile_pixmap = p;
                            let ts = Transform::from_row(tw / pw as f32, 0.0, 0.0, th / ph as f32, ox, oy);
                            let q = FilterQuality::Bilinear;
                            paint.shader = Pattern::new(tile_pixmap.as_ref(), SpreadMode::Repeat, q, 1.0, ts);
                        }
                    }
                }
                pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), mask);
            }
        }
    }
    (w, h, pixmap.take())
}

/// Device size of an offscreen gradient tile: at least one pixel, and not
/// so large that a hostile page can make us allocate much.
fn tile_px(v: f32) -> u32 {
    if v.is_finite() { v.ceil().clamp(1.0, MAX_TILE_SIDE as f32) as u32 } else { 1 }
}

/// A tiny-skia shader for a laid-out gradient; `ts` maps the tile's CSS px
/// to device pixels.
fn gradient_shader(g: &ResolvedGradient, ts: Transform) -> Option<Shader<'static>> {
    let stops: Vec<GradientStop> =
        g.stops.iter().map(|(p, c)| GradientStop::new(*p, tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a))).collect();
    let mode = if g.repeating { SpreadMode::Repeat } else { SpreadMode::Pad };
    match g.geom {
        GradientGeom::Linear { start, end } => {
            let (a, b) = (Point::from_xy(start.0, start.1), Point::from_xy(end.0, end.1));
            LinearGradient::new(a, b, stops, mode, ts)
        }
        GradientGeom::Radial { center, r0, r1, ratio } => {
            let ts = ts.pre_translate(center.0, center.1).pre_scale(1.0, ratio);
            let o = Point::from_xy(0.0, 0.0);
            RadialGradient::new(o, r0, o, r1, stops, mode, ts)
        }
    }
}

/// A frame-sized mask that is opaque inside the device rectangle `c`.
fn rect_mask(w: u32, h: u32, c: [f32; 4]) -> Option<Mask> {
    let clampi = |v: f32, max: u32| if v.is_finite() { v.clamp(0.0, max as f32) as usize } else { 0 };
    let (x0, x1) = (clampi(c[0], w), clampi(c[2], w));
    let (y0, y1) = (clampi(c[1], h), clampi(c[3], h));
    let mut data = vec![0u8; (w as usize) * (h as usize)];
    for row in y0..y1 {
        let start = row * w as usize;
        data[start + x0..start + x1.max(x0)].fill(255);
    }
    Mask::from_vec(data, IntSize::from_wh(w, h)?)
}

/// A rectangle with circular corners of radius `r`, in device pixels.
fn rounded_rect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Option<tiny_skia::Path> {
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0).max(0.0);
    // Cubic approximation of a quarter circle.
    let k = r * 0.552_284_8;
    let mut pb = PathBuilder::new();
    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.cubic_to(x1 - r + k, y0, x1, y0 + r - k, x1, y0 + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.cubic_to(x0 + r - k, y1, x0, y1 - r + k, x0, y1 - r);
    pb.line_to(x0, y0 + r);
    pb.cubic_to(x0, y0 + r - k, x0 + r - k, y0, x0 + r, y0);
    pb.close();
    pb.finish()
}
