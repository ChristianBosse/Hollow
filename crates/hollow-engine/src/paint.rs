//! Rasterizing the display list with tiny-skia, on the CPU.
//!
//! Rectangles are snapped to whole device pixels (as browsers do), glyphs
//! and images are antialiased. Everything is plain IEEE arithmetic on f32,
//! with no fused multiply-add and no platform math library calls, so the
//! output is a pure function of the inputs.

use std::collections::BTreeMap;

use tiny_skia::{
    FillRule, FilterQuality, Paint, PathBuilder, PathSegment, Pixmap, PixmapPaint, Rect as SkRect, Transform,
};

use crate::css::Color;
use crate::layout::{DisplayItem, Laid};
use crate::text::{self, FaceId};

/// Largest frame we will allocate: 64 Mpx (256 MiB of RGBA).
const MAX_FRAME_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_FRAME_SIDE: u32 = 16_384;

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
pub(crate) fn paint(laid: &Laid, paths: &GlyphPaths, scroll_y: f32, height: f32, scale: f32) -> (u32, u32, Vec<u8>) {
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

    for it in &laid.items {
        let (top, bottom) = it.y_range();
        if bottom < scroll_y || top > view_bottom {
            continue;
        }
        match it {
            DisplayItem::Rect { rect, color } => {
                if color.a == 0 || rect.w <= 0.0 || rect.h <= 0.0 {
                    continue;
                }
                // Snap to device pixels; keep hairlines at least one pixel.
                let x0 = (rect.x * scale).round();
                let y0 = ((rect.y - scroll_y) * scale).round();
                let x1 = ((rect.x + rect.w) * scale).round().max(x0 + 1.0);
                let y1 = (((rect.y + rect.h) - scroll_y) * scale).round().max(y0 + 1.0);
                if let Some(r) = SkRect::from_ltrb(x0, y0, x1, y1) {
                    let mut p = paint_for(*color);
                    p.anti_alias = false;
                    pixmap.fill_rect(r, &p, Transform::identity(), None);
                }
            }
            DisplayItem::Text { face, size, color, x, y, glyphs } => {
                let Some(f) = text::face(*face) else { continue };
                let k = f.scale(*size);
                let mut pb = PathBuilder::new();
                for g in laid.glyphs.get(glyphs.start as usize..glyphs.end as usize).unwrap_or(&[]) {
                    let Some(path) = paths.get(&(*face, g.id)) else { continue };
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
                    pixmap.fill_path(&path, &paint_for(*color), FillRule::Winding, Transform::identity(), None);
                }
            }
            DisplayItem::Image { rect, image } => {
                let Some(img) = laid.images.get(*image as usize) else { continue };
                if rect.w <= 0.0 || rect.h <= 0.0 {
                    continue;
                }
                let sx = rect.w * scale / img.width() as f32;
                let sy = rect.h * scale / img.height() as f32;
                let ts = Transform::from_row(sx, 0.0, 0.0, sy, rect.x * scale, (rect.y - scroll_y) * scale);
                let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..PixmapPaint::default() };
                pixmap.draw_pixmap(0, 0, (**img).as_ref(), &paint, ts, None);
            }
        }
    }
    (w, h, pixmap.take())
}
