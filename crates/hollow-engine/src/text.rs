//! Bundled fonts, font fallback, shaping and glyph outlines.
//!
//! Only the fonts compiled into the binary are ever used, so text measures
//! and draws identically everywhere and a page cannot probe which fonts the
//! machine has: the DejaVu families for CSS `font-family`, and behind them a
//! fixed chain of fallback fonts for scripts DejaVu lacks (Hebrew, Arabic,
//! Devanagari, Thai, CJK, hangul, emoji), searched in the same order for
//! everyone.
//! Outlines are drawn unhinted: hinting output depends on interpreter
//! details we would rather not make part of the fingerprint surface.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::OnceLock;

use harfrust::{Buffer, Direction, Script, ShapeOptions, ShapePlan, ShaperFont};
use skrifa::{DrawMemory, Font, GlyphId, PathSink, ScalerFont};
use tiny_skia::{Path, PathBuilder};
use unicode_bidi::BidiClass;

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

/// Index into [`faces`]: `family * 4 + bold * 2 + italic` for the DejaVu
/// faces, then the fallback faces (see [`FALLBACK_FONTS`]); test fonts follow
/// them, one face each.
pub(crate) type FaceId = u8;

/// The fallback fonts, in face order after the twelve DejaVu faces. Regular
/// then bold where a family has both.
const FALLBACK_FONTS: [&[u8]; 11] = [
    include_bytes!("../fonts/NotoSansHebrew-Regular.ttf"),
    include_bytes!("../fonts/NotoSansHebrew-Bold.ttf"),
    include_bytes!("../fonts/NotoSansArabic-Regular.ttf"),
    include_bytes!("../fonts/NotoSansArabic-Bold.ttf"),
    include_bytes!("../fonts/NotoSansDevanagari-Regular.ttf"),
    include_bytes!("../fonts/NotoSansDevanagari-Bold.ttf"),
    include_bytes!("../fonts/NotoSansThai-Regular.ttf"),
    include_bytes!("../fonts/NotoSansThai-Bold.ttf"),
    include_bytes!("../fonts/DroidSansFallbackFull.ttf"),
    include_bytes!("../fonts/NanumGothic-Regular.ttf"),
    include_bytes!("../fonts/NotoEmoji-VF.ttf"),
];

/// Fallback face ids; add 1 for bold where there is a bold face.
const HEBREW: FaceId = 12;
const ARABIC: FaceId = 14;
const DEVANAGARI: FaceId = 16;
const THAI: FaceId = 18;
/// Droid Sans Fallback: CJK ideographs (simplified and traditional), kana,
/// bopomofo and CJK punctuation. Regular only.
const CJK: FaceId = 20;
/// Nanum Gothic: hangul. Regular only.
const HANGUL: FaceId = 21;
/// Noto Emoji, monochrome outlines.
pub(crate) const EMOJI: FaceId = 22;

/// Number of bundled faces (test fonts come after them).
const BUNDLED_FACES: u8 = 23;

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

    fn covers(&self, c: char) -> bool {
        self.font.charmap().map_unicode(c).is_some()
    }
}

/// All bundled faces (then any test fonts), in [`face_id`] order. Parsed
/// once per process.
pub(crate) fn faces() -> &'static [Face] {
    static FACES: OnceLock<Vec<Face>> = OnceLock::new();
    FACES.get_or_init(|| {
        use dejavu::{sans, sans_mono, serif};
        let dejavu: [&'static [u8]; 12] = [
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
        let mut faces = Vec::with_capacity(usize::from(BUNDLED_FACES));
        let data = dejavu.iter().chain(&FALLBACK_FONTS);
        faces.extend(data.filter_map(|d| Face::new(d).or_else(|| Face::new(dejavu[0]))));
        #[cfg(feature = "test-fonts")]
        faces.extend(test_fonts().iter().filter_map(|(_, d)| Face::new(d).or_else(|| Face::new(dejavu[0]))));
        faces
    })
}

/// The bundled fonts as raw data, for the SVG renderer's font database.
pub(crate) fn fallback_font_data() -> &'static [&'static [u8]] {
    &FALLBACK_FONTS
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

// ---------------------------------------------------------------------------
// Fallback

/// The faces searched for a character, in order: the page's face, the
/// script fonts, DejaVu Sans (whose coverage is wider than Serif's and
/// Mono's), then CJK and emoji. The same for every machine and page.
fn chain(primary: FaceId) -> [FaceId; 9] {
    let (bold, italic) = if primary < 12 { (primary & 2 != 0, primary & 1 != 0) } else { (false, false) };
    let b = bold as u8;
    let sans = face_id(Family::Sans, bold, italic);
    [primary, HEBREW + b, ARABIC + b, DEVANAGARI + b, THAI + b, sans, CJK, HANGUL, EMOJI]
}

fn covers(face: FaceId, c: char) -> bool {
    self::face(face).is_some_and(|f| f.covers(c))
}

/// Characters that belong to the cluster before them and so stay in its
/// face: combining marks, joiners, variation selectors, emoji modifiers and
/// tags, and the invisible format characters shapers hide.
fn joins_previous(c: char) -> bool {
    invisible(c) || (u32::from(c) >= 0x300 && unicode_bidi::bidi_class(c) == BidiClass::NSM)
}

/// Format characters and other default-ignorables, which shapers hide.
fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{1F3FB}'..='\u{1F3FF}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// The face that draws `c` for text in `primary`: the first face in the
/// fallback chain that has it. `prev` is the previous character of the same
/// run and the face it got, `next` the character after `c`. Characters no
/// bundled font has keep the current face (and draw as its missing glyph).
pub(crate) fn pick_face(primary: FaceId, c: char, prev: Option<(char, FaceId)>, next: Option<char>) -> FaceId {
    // A joiner binds the next character too, if the font has it (emoji
    // sequences).
    if let Some((p, f)) = prev
        && (joins_previous(c) || p == '\u{200D}' && covers(f, c))
    {
        return f;
    }
    // Starting a run, invisible characters take the page's face, and so do
    // control characters: they draw as its missing glyph, visibly, not as
    // some fallback font's empty one.
    if c.is_control() || invisible(c) {
        return prev.map_or(primary, |(_, f)| f);
    }
    // Emoji presentation asked for explicitly, or the emoji planes (which
    // the CJK font has a few stray pictographs of).
    let emoji = next == Some('\u{FE0F}') && u32::from(c) >= 0x80 || ('\u{1F000}'..='\u{1FAFF}').contains(&c);
    if emoji && covers(EMOJI, c) {
        return EMOJI;
    }
    if covers(primary, c) {
        return primary;
    }
    chain(primary).into_iter().skip(1).find(|&f| covers(f, c)).unwrap_or(prev.map_or(primary, |(_, f)| f))
}

/// Splits `text` into runs that each draw with one face, as [`pick_face`]
/// chooses them.
pub(crate) fn face_runs(primary: FaceId, text: &str) -> Vec<(Range<usize>, FaceId)> {
    let mut runs: Vec<(Range<usize>, FaceId)> = Vec::new();
    let mut prev = None;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let face = pick_face(primary, c, prev, chars.peek().map(|&(_, n)| n));
        let end = i + c.len_utf8();
        match runs.last_mut() {
            Some((r, f)) if *f == face => r.end = end,
            _ => runs.push((i..end, face)),
        }
        prev = Some((c, face));
    }
    runs
}

/// Whether text with no direction of its own should run right to left: its
/// first strong character is Hebrew, Arabic or another RTL script.
pub(crate) fn guess_rtl(text: &str) -> bool {
    unicode_bidi::get_base_direction(text) == unicode_bidi::Direction::Rtl
}

// ---------------------------------------------------------------------------
// Shaping

/// One shaped glyph, in font units relative to the start of its run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawGlyph {
    pub id: u32,
    /// Advance of the glyphs before this one in logical order (for a
    /// right-to-left run, measured from its right edge). Kept apart from the
    /// GPOS offsets so a run can be split at any cluster boundary.
    pub pen: i32,
    pub adv: i32,
    pub dx: i32,
    /// Up is positive, as in the font.
    pub dy: i32,
    pub cluster: u32,
}

/// A shaped run. Glyphs are in logical order, so the first cluster comes
/// first in either direction.
#[derive(Clone, Debug, Default)]
pub(crate) struct Shaped {
    pub glyphs: Vec<RawGlyph>,
    pub advance: i32,
    pub rtl: bool,
}

impl Shaped {
    /// Where a glyph draws, from the run's left edge: its pen position
    /// plus its offset.
    pub fn x(&self, g: &RawGlyph) -> i32 {
        let pen = if self.rtl { self.advance - g.pen - g.adv } else { g.pen };
        pen.saturating_add(g.dx)
    }
}

/// Shapes words and remembers the results for the lifetime of one layout.
///
/// A `BTreeMap` rather than a `HashMap` so nothing about the cache can depend
/// on a per-process random hash seed.
pub(crate) struct Shaper {
    fonts: Vec<ShaperFont<'static, 'static>>,
    plans: BTreeMap<(FaceId, Option<Script>, bool), ShapePlan>,
    cache: BTreeMap<(FaceId, bool, String), Shaped>,
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

    /// Shapes `text` in one face, right to left if `rtl`. Text that needs
    /// several faces is split with [`face_runs`] first.
    pub fn shape(&mut self, face: FaceId, text: &str, rtl: bool) -> Shaped {
        if text.is_empty() {
            return Shaped { rtl, ..Shaped::default() };
        }
        let key = (face, rtl, text.to_owned());
        if let Some(s) = self.cache.get(&key) {
            return s.clone();
        }
        let shaped = self.shape_uncached(face, text, rtl);
        // Bound memory on pages made of millions of distinct words.
        if self.cache.len() < 200_000 {
            self.cache.insert(key, shaped.clone());
        }
        shaped
    }

    fn shape_uncached(&mut self, face: FaceId, text: &str, rtl: bool) -> Shaped {
        let mut out = Shaped { rtl, ..Shaped::default() };
        let Some(font) = self.fonts.get(usize::from(face)).or_else(|| self.fonts.first()) else {
            return out;
        };
        let Some(f) = faces().get(usize::from(face)).or_else(|| faces().first()) else {
            return out;
        };
        let mut buffer = self.buffer.take().unwrap_or_default();
        buffer.clear();
        buffer.push_str(text);
        // The direction comes from the bidi algorithm; only the script (and
        // language) is guessed from the text.
        buffer.set_direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight });
        buffer.guess_segment_properties();
        let plan_key = (face, buffer.script(), rtl);
        let plan = self
            .plans
            .entry(plan_key)
            .or_insert_with(|| ShapePlan::new(&f.font, buffer.direction(), buffer.script(), buffer.language(), &[]));
        if harfrust::shape(font, &mut buffer, ShapeOptions::new().plan(Some(plan))).is_ok() {
            out.glyphs.reserve(buffer.len());
            for (info, pos) in buffer.glyph_infos().iter().zip(buffer.glyph_positions()) {
                out.glyphs.push(RawGlyph {
                    id: info.glyph_id,
                    pen: 0,
                    adv: pos.x_advance,
                    dx: pos.x_offset,
                    dy: pos.y_offset,
                    cluster: info.cluster,
                });
            }
            // A right-to-left run comes out in visual order.
            if rtl {
                out.glyphs.reverse();
            }
            let mut x: i32 = 0;
            for g in &mut out.glyphs {
                g.pen = x;
                x = x.saturating_add(g.adv);
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
        assert_eq!(faces().len(), usize::from(BUNDLED_FACES));
        let mut shaper = Shaper::new();
        for id in 0..12 {
            let s = shaper.shape(id, "Hollow", false);
            assert_eq!(s.glyphs.len(), 6);
            assert!(s.advance > 0);
            assert!(outline(id, s.glyphs[0].id).is_some());
        }
    }

    #[test]
    fn bold_is_wider_than_regular() {
        let mut shaper = Shaper::new();
        let regular = shaper.shape(face_id(Family::Sans, false, false), "Hollow browser", false);
        let bold = shaper.shape(face_id(Family::Sans, true, false), "Hollow browser", false);
        assert!(bold.advance > regular.advance);
    }

    #[test]
    fn fallback_covers_the_scripts() {
        let serif = face_id(Family::Serif, false, false);
        let bold = face_id(Family::Serif, true, false);
        for (c, face) in [
            ('a', serif),
            ('\u{05D0}', HEBREW),
            ('\u{0628}', ARABIC),
            ('\u{0915}', DEVANAGARI),
            ('\u{0E01}', THAI),
            ('\u{4E2D}', CJK),
            ('\u{570B}', CJK),
            ('\u{3042}', CJK),
            ('\u{30A2}', CJK),
            ('\u{D55C}', HANGUL),
            ('\u{1F600}', EMOJI),
        ] {
            assert_eq!(pick_face(serif, c, None, None), face, "{c:?}");
        }
        assert_eq!(pick_face(bold, '\u{0628}', None, None), ARABIC + 1);
        // The same characters in DejaVu Sans stay in DejaVu Sans.
        let sans = face_id(Family::Sans, false, false);
        assert_eq!(pick_face(sans, '\u{0628}', None, None), sans);
        // Emoji sequences stay together.
        let runs = face_runs(serif, "a\u{1F44D}\u{1F3FD}\u{1F468}\u{200D}\u{1F469}b");
        assert_eq!(runs.iter().map(|r| r.1).collect::<Vec<_>>(), [serif, EMOJI, serif]);
    }

    #[test]
    fn rtl_runs_are_in_logical_order() {
        let mut shaper = Shaper::new();
        let s = shaper.shape(HEBREW, "\u{05D0}\u{05D1}\u{05D2}", true);
        assert_eq!(s.glyphs.iter().map(|g| g.cluster).collect::<Vec<_>>(), [0, 2, 4]);
        // The first letter draws rightmost.
        assert!(s.x(&s.glyphs[0]) > s.x(&s.glyphs[2]));
        assert_eq!(s.x(&s.glyphs[2]), 0);
    }
}
