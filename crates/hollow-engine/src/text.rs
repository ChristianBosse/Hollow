//! Bundled fonts, shaping and glyph outlines.
//!
//! Only the DejaVu families compiled into the binary are ever used, so text
//! measures and draws identically everywhere and a page cannot probe which
//! fonts the machine has. Outlines are drawn unhinted: hinting output depends
//! on interpreter details we would rather not make part of the fingerprint
//! surface.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use harfrust::{Buffer, Direction, Script, ShapeOptions, ShapePlan, ShaperFont};
use skrifa::{DrawMemory, Font, GlyphId, PathSink, ScalerFont};
use tiny_skia::{Path, PathBuilder};

/// Generic family. Every CSS `font-family` is mapped onto one of these.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Family {
    Sans,
    Serif,
    Mono,
    /// A font registered with [`register_test_fonts`] (index into them).
    #[cfg(feature = "test-fonts")]
    Test(u8),
}

/// Index into [`faces`]: `family * 4 + bold * 2 + italic` for the bundled
/// faces; test fonts follow them, one face each.
pub(crate) type FaceId = u8;

/// Number of bundled faces (test fonts come after them).
#[cfg(feature = "test-fonts")]
const BUNDLED_FACES: u8 = 12;

pub(crate) fn face_id(family: Family, bold: bool, italic: bool) -> FaceId {
    let family = match family {
        Family::Sans => 0,
        Family::Serif => 1,
        Family::Mono => 2,
        #[cfg(feature = "test-fonts")]
        Family::Test(i) => return BUNDLED_FACES + i,
    };
    family * 4 + (bold as u8) * 2 + italic as u8
}

/// A font plus the metrics layout needs, all in font units.
pub(crate) struct Face {
    pub font: Font,
    pub upem: f32,
    /// Above the baseline, positive.
    pub ascent: f32,
    /// Below the baseline, positive.
    pub descent: f32,
    pub line_gap: f32,
    /// Top of the underline stroke relative to the baseline (negative = below).
    pub underline_pos: f32,
    pub underline_thickness: f32,
    pub strike_pos: f32,
    pub strike_thickness: f32,
}

impl Face {
    fn new(data: &'static [u8]) -> Option<Face> {
        let font = Font::new(data, 0)?;
        let upem = f32::from(font.units_per_em().max(16));
        let metrics = font.metrics();
        let line = metrics.typo_line.filter(|_| metrics.use_typo_metrics).or(metrics.hhea_line);
        let (ascent, descent, line_gap) = match line {
            Some(l) => (l.ascender.to_f32(), -l.descender.to_f32(), l.line_gap.to_f32().max(0.0)),
            None => (upem * 0.8, upem * 0.2, 0.0),
        };
        let style = font.style_metrics();
        let (underline_pos, underline_thickness) =
            style.underline.map(|d| (d.position.to_f32(), d.thickness.to_f32())).unwrap_or((-upem * 0.1, upem * 0.05));
        let (strike_pos, strike_thickness) = style
            .strike_through
            .map(|d| (d.position.to_f32(), d.thickness.to_f32()))
            .unwrap_or((upem * 0.3, upem * 0.05));
        Some(Face {
            font,
            upem,
            ascent: ascent.max(0.0),
            descent: descent.max(0.0),
            line_gap,
            underline_pos,
            underline_thickness: underline_thickness.max(1.0),
            strike_pos,
            strike_thickness: strike_thickness.max(1.0),
        })
    }

    /// Scale from font units to CSS px at `size`.
    pub fn scale(&self, size: f32) -> f32 {
        size / self.upem
    }
}

/// All twelve bundled faces (then any test fonts), in [`face_id`] order.
/// Parsed once per process.
pub(crate) fn faces() -> &'static [Face] {
    static FACES: OnceLock<Vec<Face>> = OnceLock::new();
    FACES.get_or_init(|| {
        use dejavu::{sans, sans_mono, serif};
        let data: [&'static [u8]; 12] = [
            sans::regular(),
            sans::oblique(),
            sans::bold(),
            sans::bold_oblique(),
            serif::regular(),
            serif::italic(),
            serif::bold(),
            serif::bold_italic(),
            sans_mono::regular(),
            sans_mono::oblique(),
            sans_mono::bold(),
            sans_mono::bold_oblique(),
        ];
        // The fonts are compiled in, so failing to parse one is a build defect;
        // substitute the regular sans face rather than panicking anyway.
        #[cfg_attr(not(feature = "test-fonts"), allow(unused_mut))]
        let mut faces: Vec<Face> = data.iter().filter_map(|d| Face::new(d).or_else(|| Face::new(data[0]))).collect();
        #[cfg(feature = "test-fonts")]
        faces.extend(test_fonts().iter().filter_map(|(_, d)| Face::new(d).or_else(|| Face::new(data[0]))));
        faces
    })
}

/// Fonts registered for test suites, as (lower-case family name, data).
/// Frozen by the first use of [`faces`].
#[cfg(feature = "test-fonts")]
static TEST_FONTS: OnceLock<Vec<(String, &'static [u8])>> = OnceLock::new();

#[cfg(feature = "test-fonts")]
fn test_fonts() -> &'static [(String, &'static [u8])] {
    TEST_FONTS.get_or_init(Vec::new)
}

/// See [`crate::register_test_fonts`].
#[cfg(feature = "test-fonts")]
pub(crate) fn register_test_fonts(fonts: Vec<(String, Vec<u8>)>) -> bool {
    if fonts.len() > usize::from(u8::MAX - BUNDLED_FACES)
        || fonts.iter().any(|(_, d)| Font::new(d.clone(), 0).is_none())
    {
        return false;
    }
    // Leaked once per process: faces borrow their data for 'static.
    let fonts = fonts.into_iter().map(|(name, data)| (name.to_ascii_lowercase(), &*data.leak())).collect();
    TEST_FONTS.set(fonts).is_ok()
}

/// The test font registered as `name` (lower case), if any.
#[cfg(feature = "test-fonts")]
pub(crate) fn test_family(name: &str) -> Option<Family> {
    let i = test_fonts().iter().position(|(n, _)| n == name)?;
    u8::try_from(i).ok().map(Family::Test)
}

pub(crate) fn face(id: FaceId) -> Option<&'static Face> {
    faces().get(usize::from(id))
}

/// One shaped glyph, in font units relative to the start of its run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawGlyph {
    pub id: u32,
    /// Pen position before the glyph. Kept apart from the GPOS offsets so a
    /// run can be split at any cluster boundary.
    pub pen: i32,
    pub dx: i32,
    /// Up is positive, as in the font.
    pub dy: i32,
    pub cluster: u32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Shaped {
    pub glyphs: Vec<RawGlyph>,
    pub advance: i32,
}

/// Shapes words and remembers the results for the lifetime of one layout.
///
/// A `BTreeMap` rather than a `HashMap` so nothing about the cache can depend
/// on a per-process random hash seed.
pub(crate) struct Shaper {
    fonts: Vec<ShaperFont<'static, 'static>>,
    plans: BTreeMap<(FaceId, Option<Script>, u8), ShapePlan>,
    cache: BTreeMap<(FaceId, String), Shaped>,
    buffer: Option<Buffer>,
}

impl Shaper {
    pub fn new() -> Shaper {
        Shaper {
            fonts: faces().iter().map(|f| ShaperFont::new(&f.font)).collect(),
            plans: BTreeMap::new(),
            cache: BTreeMap::new(),
            buffer: Some(Buffer::new()),
        }
    }

    pub fn shape(&mut self, face: FaceId, text: &str) -> Shaped {
        if text.is_empty() {
            return Shaped::default();
        }
        let key = (face, text.to_owned());
        if let Some(s) = self.cache.get(&key) {
            return s.clone();
        }
        let shaped = self.shape_uncached(face, text);
        // Bound memory on pages made of millions of distinct words.
        if self.cache.len() < 200_000 {
            self.cache.insert(key, shaped.clone());
        }
        shaped
    }

    fn shape_uncached(&mut self, face: FaceId, text: &str) -> Shaped {
        let Some(font) = self.fonts.get(usize::from(face)).or_else(|| self.fonts.first()) else {
            return Shaped::default();
        };
        let Some(f) = faces().get(usize::from(face)).or_else(|| faces().first()) else {
            return Shaped::default();
        };
        let mut buffer = self.buffer.take().unwrap_or_default();
        buffer.clear();
        buffer.push_str(text);
        buffer.guess_segment_properties();
        let dir = match buffer.direction() {
            Direction::RightToLeft => 1,
            Direction::TopToBottom => 2,
            Direction::BottomToTop => 3,
            _ => 0,
        };
        if dir >= 2 {
            buffer.set_direction(Direction::LeftToRight);
        }
        let plan_key = (face, buffer.script(), dir.min(1));
        let plan = self
            .plans
            .entry(plan_key)
            .or_insert_with(|| ShapePlan::new(&f.font, buffer.direction(), buffer.script(), buffer.language(), &[]));
        let mut out = Shaped::default();
        if harfrust::shape(font, &mut buffer, ShapeOptions::new().plan(Some(plan))).is_ok() {
            let mut x: i32 = 0;
            out.glyphs.reserve(buffer.len());
            for (info, pos) in buffer.glyph_infos().iter().zip(buffer.glyph_positions()) {
                out.glyphs.push(RawGlyph {
                    id: info.glyph_id,
                    pen: x,
                    dx: pos.x_offset,
                    dy: pos.y_offset,
                    cluster: info.cluster,
                });
                x = x.saturating_add(pos.x_advance);
            }
            out.advance = x;
        }
        self.buffer = Some(buffer);
        out
    }
}

/// Glyph outline in font units, y up. `None` for empty glyphs like space.
pub(crate) fn outline(face: FaceId, glyph: u32) -> Option<Path> {
    let f = faces().get(usize::from(face))?;
    let scaler = ScalerFont::unhinted(f.font.clone(), None);
    let outline = scaler.outline(GlyphId::new(glyph))?;
    let mut sink = Sink(PathBuilder::new());
    outline.draw(DrawMemory::Scratch, &mut sink).ok()?;
    sink.0.finish()
}

struct Sink(PathBuilder);

impl PathSink for Sink {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0.quad_to(cx0, cy0, x, y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.cubic_to(cx0, cy0, cx1, cy1, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_faces_load_and_shape() {
        assert_eq!(faces().len(), 12);
        let mut shaper = Shaper::new();
        for id in 0..12 {
            let s = shaper.shape(id, "Hollow");
            assert_eq!(s.glyphs.len(), 6);
            assert!(s.advance > 0);
            assert!(outline(id, s.glyphs[0].id).is_some());
        }
    }

    #[test]
    fn bold_is_wider_than_regular() {
        let mut shaper = Shaper::new();
        let regular = shaper.shape(face_id(Family::Sans, false, false), "Hollow browser");
        let bold = shaper.shape(face_id(Family::Sans, true, false), "Hollow browser");
        assert!(bold.advance > regular.advance);
    }
}
