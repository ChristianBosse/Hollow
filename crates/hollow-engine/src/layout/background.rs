//! Background image layers (`url()` images and gradients): sized,
//! positioned and repeated as CSS Backgrounds 3 says, and recorded as
//! display items that the painter tiles.
//!
//! `background-attachment: fixed` positions against the top of the page
//! (fixed things scroll with the page in Hollow).

use std::sync::Arc;

use super::{Ctx, Deco, DisplayItem, Rect, clamp_extent};
use crate::css::{BgSize, BoxArea, Color, ColorValue, Gradient, Image, Length, RadialSize, Repeat, Shape};
use crate::dom::NodeId;
use crate::style::{BgLayer, Style};

/// What a background layer draws.
#[derive(Clone, Debug)]
pub(crate) enum BgSource {
    /// An index into [`super::Laid::images`].
    Image(u32),
    Gradient(Arc<ResolvedGradient>),
}

/// Gradient stops: positions from 0 to 1, and colors.
type Stops = Vec<(f32, Color)>;

/// A gradient laid out for one tile, in CSS px from the tile's top left.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedGradient {
    pub geom: GradientGeom,
    /// Ascending positions from 0 at the start to 1 at the end.
    pub stops: Stops,
    pub repeating: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum GradientGeom {
    Linear {
        start: (f32, f32),
        end: (f32, f32),
    },
    /// Circles from radius `r0` to `r1` around `center`, squashed
    /// vertically by `ratio` into ellipses.
    Radial {
        center: (f32, f32),
        r0: f32,
        r1: f32,
        ratio: f32,
    },
}

impl Ctx<'_> {
    /// Reserves one display-list slot per image layer that can be drawn.
    pub(super) fn reserve_layers(&mut self, st: &Style, node: NodeId) -> Option<(usize, usize)> {
        let layers = st.bg_layers.clone()?;
        let n = layers.iter().filter(|l| self.drawable(&l.image)).count();
        if n == 0 {
            return None;
        }
        let first = self.reserve();
        for _ in 1..n {
            self.reserve();
        }
        if self.canvas_node == Some(node) {
            self.canvas_layers.extend(first..first + n);
        }
        Some((first, n))
    }

    fn drawable(&self, image: &Image) -> bool {
        match image {
            Image::Url(u) => self.image_ids.contains_key(u.as_str()),
            Image::Gradient(_) => true,
        }
    }

    /// Fills the image layer slots of a box whose border box has corner
    /// `radius`.
    pub(super) fn paint_layers(&mut self, st: &Style, d: &Deco, radius: f32) {
        let (Some((first, n)), Some(layers)) = (d.layers, st.bg_layers.as_ref()) else { return };
        // CSS lists the top layer first; the bottom one paints first.
        let drawable: Vec<&BgLayer> = layers.iter().rev().filter(|l| self.drawable(&l.image)).take(n).collect();
        for (k, layer) in drawable.into_iter().enumerate() {
            let item = self
                .layer_item(st, layer, d, radius)
                .unwrap_or(DisplayItem::Rect { rect: Rect::default(), color: Color { r: 0, g: 0, b: 0, a: 0 } });
            if let Some(o) = self.out.get_mut(first + k) {
                *o = item;
            }
        }
    }

    fn layer_item(&self, st: &Style, layer: &BgLayer, d: &Deco, radius: f32) -> Option<DisplayItem> {
        let area = |b: BoxArea| match b {
            BoxArea::Border => (d.rect, radius),
            BoxArea::Padding => Ctx::inner_box(d.rect, radius, d, false),
            BoxArea::Content => Ctx::inner_box(d.rect, radius, d, true),
        };
        // A fixed background's positioning area is the viewport; Hollow's
        // fixed things scroll with the page, so it is the page's top, at
        // the 16:9 viewport height `vh` units assume.
        let (origin, _) = if layer.fixed {
            (Rect { x: 0.0, y: 0.0, w: self.width, h: self.width * crate::css::VH_PER_VW }, 0.0)
        } else {
            area(layer.origin)
        };
        let (clip, clip_radius) = area(layer.clip);
        let image = match &layer.image {
            Image::Url(u) => Some(*self.image_ids.get(u.as_str())?),
            Image::Gradient(_) => None,
        };
        let intrinsic = image.and_then(|i| self.images.get(i as usize)).map(|p| (p.width() as f32, p.height() as f32));
        let (mut tw, mut th) = tile_size(layer.size, intrinsic, origin.w, origin.h);
        // `round` fits a whole number of tiles.
        if layer.repeat[0] == Repeat::Round && tw > 0.0 && origin.w > 0.0 {
            tw = origin.w / (origin.w / tw).round().max(1.0);
        }
        if layer.repeat[1] == Repeat::Round && th > 0.0 && origin.h > 0.0 {
            th = origin.h / (origin.h / th).round().max(1.0);
        }
        let (tw, th) = (clamp_extent(tw), clamp_extent(th));
        if tw <= 0.0 || th <= 0.0 {
            return None;
        }
        let mut pos = [
            origin.x + resolve(layer.position[0], origin.w - tw),
            origin.y + resolve(layer.position[1], origin.h - th),
        ];
        let mut repeat = layer.repeat.map(|r| r != Repeat::NoRepeat);
        // `space`: as many whole tiles as fit, the first and last touching
        // the edges and the rest spread evenly between; with room for only
        // one, it is placed as if not repeated.
        let mut gap = [0.0f32; 2];
        for (axis, (start, len, t)) in [(origin.x, origin.w, tw), (origin.y, origin.h, th)].into_iter().enumerate() {
            if layer.repeat[axis] == Repeat::Space {
                let n = (len / t).floor();
                if n >= 2.0 {
                    pos[axis] = start;
                    gap[axis] = (len - n * t) / (n - 1.0);
                } else {
                    repeat[axis] = false;
                }
            }
        }
        let [x, y] = pos;
        let source = match (&layer.image, image) {
            (Image::Gradient(g), _) => BgSource::Gradient(Arc::new(resolve_gradient(g, tw, th, st.color)?)),
            (_, Some(i)) => BgSource::Image(i),
            _ => return None,
        };
        Some(DisplayItem::Background {
            area: clip,
            radius: clip_radius,
            tile: Rect { x, y, w: tw, h: th },
            repeat,
            gap,
            image: source,
            fixed: layer.fixed,
        })
    }
}

/// A computed length (px, percentage or their sum) against `basis`.
fn resolve(l: Length, basis: f32) -> f32 {
    resolve_opt(l, basis).unwrap_or(0.0)
}

fn resolve_opt(l: Length, basis: f32) -> Option<f32> {
    let v = match l {
        Length::Px(v) => v,
        Length::Pct(p) => basis * p / 100.0,
        Length::Calc(c) => c.px + basis * c.pct / 100.0,
        _ => return None,
    };
    v.is_finite().then_some(v)
}

/// The size of one tile in an `aw` x `ah` positioning area.
fn tile_size(size: BgSize, intrinsic: Option<(f32, f32)>, aw: f32, ah: f32) -> (f32, f32) {
    let intrinsic = intrinsic.filter(|(w, h)| *w > 0.0 && *h > 0.0);
    match size {
        BgSize::Cover | BgSize::Contain => match intrinsic {
            Some((iw, ih)) => {
                let (kx, ky) = (aw / iw, ah / ih);
                let k = if size == BgSize::Cover { kx.max(ky) } else { kx.min(ky) };
                (iw * k, ih * k)
            }
            None => (aw, ah),
        },
        BgSize::Size(w, h) => match (resolve_opt(w, aw), resolve_opt(h, ah), intrinsic) {
            (Some(w), Some(h), _) => (w, h),
            (Some(w), None, Some((iw, ih))) => (w, w * ih / iw),
            (None, Some(h), Some((iw, ih))) => (h * iw / ih, h),
            (Some(w), None, None) => (w, ah),
            (None, Some(h), None) => (aw, h),
            (None, None, Some(i)) => i,
            (None, None, None) => (aw, ah),
        },
    }
}

/// Lays a gradient out for a `w` x `h` tile.
fn resolve_gradient(g: &Gradient, w: f32, h: f32, current: Color) -> Option<ResolvedGradient> {
    let color = |c: ColorValue| match c {
        ColorValue::Rgba(c) => c,
        ColorValue::Current => current,
    };
    match g.shape {
        Shape::Linear(_) | Shape::Corner(..) => {
            let (dx, dy) = match g.shape {
                Shape::Corner(sx, sy) => {
                    // Perpendicular to the diagonal between the other two
                    // corners, pointing at the named one.
                    let (vx, vy) = (f32::from(sx) * h, f32::from(sy) * w);
                    let len = (vx * vx + vy * vy).sqrt();
                    if len > 0.0 { (vx / len, vy / len) } else { (0.0, 1.0) }
                }
                Shape::Linear(a) => {
                    let (s, c) = sin_cos_deg(a);
                    (s, -c)
                }
                Shape::Radial { .. } => (0.0, 1.0),
            };
            // The gradient line just reaches the corners.
            let len = (w * dx).abs() + (h * dy).abs();
            let (pos, colors) = stops(g, len, color)?;
            let (lo, hi, stops) = normalize(pos, colors, g.repeating, false)?;
            let (sx, sy) = (w / 2.0 - dx * len / 2.0, h / 2.0 - dy * len / 2.0);
            let at = |t: f32| (sx + dx * len * t, sy + dy * len * t);
            Some(ResolvedGradient {
                geom: GradientGeom::Linear { start: at(lo), end: at(hi) },
                stops,
                repeating: g.repeating,
            })
        }
        Shape::Radial { circle, size, at } => {
            let (cx, cy) = (resolve(at[0], w), resolve(at[1], h));
            let (l, r, t, b) = (cx.abs(), (w - cx).abs(), cy.abs(), (h - cy).abs());
            let (rx, ry) = match size {
                RadialSize::ClosestSide if circle => (l.min(r).min(t).min(b), 0.0),
                RadialSize::FarthestSide if circle => (l.max(r).max(t).max(b), 0.0),
                RadialSize::ClosestSide => (l.min(r), t.min(b)),
                RadialSize::FarthestSide => (l.max(r), t.max(b)),
                RadialSize::ClosestCorner | RadialSize::FarthestCorner => {
                    let far = size == RadialSize::FarthestCorner;
                    let (x, y) = if far { (l.max(r), t.max(b)) } else { (l.min(r), t.min(b)) };
                    if circle {
                        ((x * x + y * y).sqrt(), 0.0)
                    } else {
                        // The ellipse with the side-based aspect ratio that
                        // passes through the corner.
                        (x * std::f32::consts::SQRT_2, y * std::f32::consts::SQRT_2)
                    }
                }
                RadialSize::Explicit(a, b) => (resolve(a, w), resolve(b, h)),
            };
            let ry = if circle { rx } else { ry };
            if !(rx > 0.0 && ry > 0.0 && rx.is_finite() && ry.is_finite()) {
                return None;
            }
            let (pos, colors) = stops(g, rx, color)?;
            let (lo, hi, stops) = normalize(pos, colors, g.repeating, true)?;
            Some(ResolvedGradient {
                geom: GradientGeom::Radial { center: (cx, cy), r0: rx * lo, r1: rx * hi, ratio: ry / rx },
                stops,
                repeating: g.repeating,
            })
        }
    }
}

/// Stop positions as fractions of a gradient line `len` long, with missing
/// ones filled in as CSS says, and the stop colors.
fn stops(g: &Gradient, len: f32, color: impl Fn(ColorValue) -> Color) -> Option<(Vec<f32>, Vec<Color>)> {
    let n = g.stops.len();
    if n == 0 {
        return None;
    }
    let frac = |l: Length| match l {
        Length::Pct(p) => Some(p / 100.0),
        l if len > 0.0 => resolve_opt(l, len).map(|v| v / len),
        _ => Some(0.0),
    };
    let mut pos: Vec<Option<f32>> = g.stops.iter().map(|(_, p)| p.and_then(frac)).collect();
    if pos[0].is_none() {
        pos[0] = Some(0.0);
    }
    if pos[n - 1].is_none() {
        pos[n - 1] = Some(if n == 1 { 0.0 } else { 1.0 });
    }
    // A position before an earlier one moves up to it.
    let mut max = f32::NEG_INFINITY;
    for p in pos.iter_mut().flatten() {
        *p = p.max(max);
        max = *p;
    }
    // Runs without positions are spread evenly between their neighbours.
    let mut out = vec![0.0f32; n];
    let mut i = 0;
    while i < n {
        if let Some(p) = pos[i] {
            out[i] = p;
            i += 1;
            continue;
        }
        let prev = out[i - 1];
        let end = (i..n).find(|&k| pos[k].is_some()).unwrap_or(n - 1);
        let next = pos[end].unwrap_or(prev);
        let gaps = (end - i + 1) as f32;
        for (k, slot) in out.iter_mut().enumerate().take(end).skip(i) {
            *slot = prev + (next - prev) * (k - i + 1) as f32 / gaps;
        }
        i = end;
    }
    Some((out, g.stops.iter().map(|(c, _)| color(*c)).collect()))
}

/// Maps stop positions onto 0..=1 for the painter: the line runs from `lo`
/// to `hi` (the first and last stop when repeating; else 0 and 1 widened to
/// take in every stop). Transparent stops get their neighbours' color, so
/// fading to `transparent` doesn't pass through black.
fn normalize(pos: Vec<f32>, colors: Vec<Color>, repeating: bool, radial: bool) -> Option<(f32, f32, Stops)> {
    let (first, last) = (pos[0], pos[pos.len() - 1]);
    let (mut lo, hi) = if repeating { (first, last) } else { (first.min(0.0), last.max(1.0)) };
    if radial {
        lo = lo.max(0.0);
    }
    let span = hi - lo;
    if span.is_nan() || span <= 1e-6 || !lo.is_finite() || !hi.is_finite() {
        // A degenerate repeating gradient is its average; the last color
        // is close enough.
        let c = *colors.last()?;
        return Some((0.0, 1.0, vec![(0.0, c), (1.0, c)]));
    }
    let mut out = Vec::with_capacity(pos.len() + 2);
    for (i, (&p, &c)) in pos.iter().zip(&colors).enumerate() {
        let t = ((p - lo) / (hi - lo)).clamp(0.0, 1.0);
        if c.a > 0 {
            out.push((t, c));
            continue;
        }
        let near = |k: Option<usize>| k.and_then(|k| colors.get(k)).filter(|c| c.a > 0).copied();
        let before = near(i.checked_sub(1)).unwrap_or(c);
        let after = near(Some(i + 1)).unwrap_or(c);
        out.push((t, Color { a: 0, ..before }));
        if after != before {
            out.push((t, Color { a: 0, ..after }));
        }
    }
    Some((lo, hi, out))
}

/// The sine and cosine of an angle in degrees, with basic arithmetic only:
/// the platform's math library could round differently on another machine.
pub(crate) fn sin_cos_deg(deg: f32) -> (f32, f32) {
    let d = f64::from(deg).rem_euclid(360.0);
    let quadrant = (d / 90.0).floor();
    let r = d - quadrant * 90.0;
    // Taylor series on [0, 45] degrees, mirrored for the rest.
    let series = |deg: f64| {
        let x = deg * (std::f64::consts::PI / 180.0);
        let x2 = x * x;
        let s = x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0 * (1.0 - x2 / 110.0)))));
        let c = 1.0 - x2 / 2.0 * (1.0 - x2 / 12.0 * (1.0 - x2 / 30.0 * (1.0 - x2 / 56.0 * (1.0 - x2 / 90.0))));
        (s, c)
    };
    let (s, c) = if r <= 45.0 {
        series(r)
    } else {
        let (s, c) = series(90.0 - r);
        (c, s)
    };
    let (s, c) = match quadrant as u8 {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    };
    (s as f32, c as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_trigonometry() {
        assert_eq!(sin_cos_deg(0.0), (0.0, 1.0));
        assert_eq!(sin_cos_deg(90.0), (1.0, 0.0));
        assert_eq!(sin_cos_deg(180.0), (0.0, -1.0));
        assert_eq!(sin_cos_deg(-90.0), (-1.0, 0.0));
        for deg in [1.0f32, 30.0, 45.0, 60.0, 123.4, 200.0, 359.0] {
            let (s, c) = sin_cos_deg(deg);
            let rad = deg.to_radians();
            assert!((s - rad.sin()).abs() < 1e-6 && (c - rad.cos()).abs() < 1e-6, "{deg}");
        }
    }

    #[test]
    fn tile_sizes() {
        let auto = BgSize::Size(Length::Auto, Length::Auto);
        assert_eq!(tile_size(auto, Some((10.0, 20.0)), 100.0, 100.0), (10.0, 20.0));
        assert_eq!(tile_size(auto, None, 100.0, 50.0), (100.0, 50.0));
        assert_eq!(tile_size(BgSize::Cover, Some((10.0, 20.0)), 100.0, 100.0), (100.0, 200.0));
        assert_eq!(tile_size(BgSize::Contain, Some((10.0, 20.0)), 100.0, 100.0), (50.0, 100.0));
        let half = BgSize::Size(Length::Pct(50.0), Length::Auto);
        assert_eq!(tile_size(half, Some((10.0, 20.0)), 100.0, 100.0), (50.0, 100.0));
    }

    #[test]
    fn gradient_stops_fill_in() {
        let red = ColorValue::Rgba(Color::rgb(255, 0, 0));
        let g = Gradient {
            shape: Shape::Linear(90.0),
            repeating: false,
            stops: vec![
                (red, None),
                (red, Some(Length::Px(50.0))),
                (red, None),
                (red, None),
                (red, Some(Length::Pct(10.0))),
            ],
        };
        let (pos, _) = stops(&g, 100.0, |_| Color::BLACK).unwrap();
        assert_eq!(pos, vec![0.0, 0.5, 0.5, 0.5, 0.5]);
        let g = Gradient { stops: vec![(red, None), (red, None), (red, None)], ..g };
        assert_eq!(stops(&g, 100.0, |_| Color::BLACK).unwrap().0, vec![0.0, 0.5, 1.0]);
        // A linear gradient to the right spans the box's width.
        let r = resolve_gradient(&g, 200.0, 50.0, Color::BLACK).unwrap();
        match r.geom {
            GradientGeom::Linear { start, end } => {
                assert!((start.0 - 0.0).abs() < 1e-3 && (end.0 - 200.0).abs() < 1e-3, "{start:?} {end:?}");
                assert!((start.1 - 25.0).abs() < 1e-3 && (end.1 - 25.0).abs() < 1e-3);
            }
            _ => panic!(),
        }
    }
}
