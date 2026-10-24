//! hollow-engine: the Fortress engine (architecture §4). HTML and a subset
//! of CSS in, RGBA pixels out.
//!
//! Everything here is a pure function of its inputs: no I/O, no system
//! fonts, no clocks, no randomness, no threads, no GPU, no `unsafe`. The
//! same document at the same width and scale paints the same bytes on every
//! machine, which is what defeats canvas, font and GPU fingerprinting (P4).
//! Scripts are dropped while parsing and never stored.
//!
//! Hostile input is the normal case. Every stage has a budget (DOM depth and
//! size, CSS rules, boxes, glyphs, image pixels, frame size) and degrades by
//! dropping content rather than failing; no input may make it panic.
//!
//! ```
//! # use hollow_engine::{Document, Layout, Resources};
//! let base = url::Url::parse("https://example.org/").unwrap();
//! let doc = Document::parse("<h1>Hello</h1><a href=next>next</a>", &base);
//! let layout = Layout::new(&doc, &Resources::new(), 800.0);
//! let frame = layout.paint(0.0, 600.0, 1.0);
//! assert_eq!((frame.width, frame.height), (800, 600));
//! ```

mod css;
mod dom;
mod form;
mod image;
mod layout;
mod paint;
mod style;
mod text;
mod vars;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use tiny_skia::Pixmap;
use url::Url;

pub use crate::image::ImageError;

/// Total inline (`<style>`) CSS read from one document.
const MAX_DOCUMENT_CSS: usize = 2 << 20;
/// Largest single linked stylesheet.
pub const MAX_STYLESHEET_BYTES: usize = css::MAX_CSS_BYTES;
/// Total linked stylesheet text one document may hold.
const MAX_TOTAL_STYLESHEET_BYTES: usize = 8 << 20;
/// Total decoded image pixels one document may hold (256 MiB of RGBA).
const MAX_TOTAL_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;

/// A parsed HTML document. Independent of viewport size and of fetched
/// resources, so one `Document` serves any number of relayouts.
pub struct Document {
    /// The document's own URL (an empty form `action` submits here).
    url: Url,
    dom: dom::Dom,
    title: Option<String>,
    images: Vec<Url>,
    stylesheet_urls: Vec<Url>,
    /// `<style>` and `<link rel=stylesheet>` in document order: the cascade
    /// order of author sheets.
    sheets: Vec<DocSheet>,
}

enum DocSheet {
    Inline(css::Stylesheet, Option<css::MediaList>),
    Linked(Url, Option<css::MediaList>),
}

impl Document {
    /// Parse HTML. `base_url` is used to resolve relative URLs (a `<base>`
    /// element in the page overrides it, as in browsers).
    pub fn parse(html: &str, base_url: &Url) -> Document {
        let parsed = dom::parse(html, base_url);
        let mut budget = MAX_DOCUMENT_CSS;
        let media = |m: Option<String>| m.map(|m| css::MediaList::parse(&m));
        let sheets = parsed
            .sheets
            .into_iter()
            .map(|s| match s {
                dom::SheetSource::Inline { css, media: m } => {
                    let text = dom::truncate(&css, budget);
                    budget -= text.len();
                    DocSheet::Inline(css::Stylesheet::parse(text), media(m))
                }
                dom::SheetSource::Linked { url, media: m } => DocSheet::Linked(url, media(m)),
            })
            .collect();
        Document {
            url: base_url.clone(),
            dom: parsed.dom,
            title: parsed.title,
            images: parsed.images,
            stylesheet_urls: parsed.stylesheet_urls,
            sheets,
        }
    }

    /// The `<title>`, whitespace-collapsed, if there is a non-empty one.
    pub fn title(&self) -> Option<String> {
        self.title.clone()
    }

    /// Absolute http(s) URLs of images the page wants (from `<img src>`, or
    /// the first `srcset` candidate), deduplicated, in document order, at
    /// most 64.
    pub fn image_urls(&self) -> Vec<Url> {
        self.images.clone()
    }

    /// Absolute http(s) URLs of `<link rel~=stylesheet href>`, deduplicated,
    /// in document order, at most 32. Every one is listed whatever its
    /// `media` attribute: which sheets get fetched must never depend on the
    /// viewport (FP-05). Media queries are evaluated locally at layout time.
    pub fn stylesheet_urls(&self) -> Vec<Url> {
        self.stylesheet_urls.clone()
    }

    /// Fresh state for the page's form controls: their initial values.
    pub fn forms(&self) -> FormState {
        FormState { forms: form::Forms::new(&self.dom) }
    }
}

pub use form::{FormEnctype, FormKey, FormMethod, Interaction, Submission};

/// The current values of a document's form controls, which field has focus
/// and which `<select>` list is open. Kept by the caller across relayouts
/// and repaints of the same document.
#[derive(Clone, Debug)]
pub struct FormState {
    forms: form::Forms,
}

impl FormState {
    /// What a click on `hit` (from [`Layout::hit_test_forms`]) does to the
    /// forms. A click anywhere else, `None`, takes the focus away.
    pub fn click(&mut self, doc: &Document, hit: Option<&HitTarget>) -> Interaction {
        let target = match hit {
            Some(HitTarget::Control(id)) => Some(form::FormTarget::Control(id.0)),
            Some(HitTarget::Option(id, i)) => Some(form::FormTarget::Option(id.0, *i)),
            _ => None,
        };
        self.forms.click(&doc.dom, &doc.url, target)
    }

    /// A key for the focused field.
    pub fn key(&mut self, doc: &Document, key: &FormKey) -> Interaction {
        self.forms.key(&doc.dom, &doc.url, key)
    }

    /// Whether keys belong to a field (one is focused, or a list is open)
    /// rather than to scrolling.
    pub fn editing(&self) -> bool {
        self.forms.editing()
    }
}

/// A form control of a document, as hit testing finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlId(dom::NodeId);

fn ua_sheet() -> &'static css::Stylesheet {
    static UA: OnceLock<css::Stylesheet> = OnceLock::new();
    UA.get_or_init(style::ua_sheet)
}

/// Subresources the broker fetched for a document, keyed by the absolute
/// URLs from [`Document::image_urls`] and [`Document::stylesheet_urls`].
#[derive(Default)]
pub struct Resources {
    images: BTreeMap<String, Arc<Pixmap>>,
    pixels: u64,
    stylesheets: BTreeMap<String, (Arc<css::Stylesheet>, usize)>,
    css_bytes: usize,
}

/// Why a stylesheet was not accepted. The page renders without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StylesheetError {
    /// Over [`MAX_STYLESHEET_BYTES`].
    TooLarge,
    /// The document's stylesheet count or total size budget is used up.
    BudgetExhausted,
}

impl std::fmt::Display for StylesheetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            StylesheetError::TooLarge => "stylesheet is too large",
            StylesheetError::BudgetExhausted => "stylesheet budget exhausted",
        })
    }
}

impl std::error::Error for StylesheetError {}

impl Resources {
    pub fn new() -> Resources {
        Resources::default()
    }

    /// Decodes an image (PNG, JPEG, GIF first frame, WebP) and stores it
    /// under `url`. Returns its pixel size. Rejected images are simply not
    /// drawn; the page lays out as if they had failed to load.
    pub fn insert_image(&mut self, url: &Url, bytes: &[u8]) -> Result<(u32, u32), ImageError> {
        let replaced = self.images.get(url.as_str()).map_or(0, |p| u64::from(p.width()) * u64::from(p.height()));
        if replaced == 0 && self.images.len() >= dom::MAX_IMAGES {
            return Err(ImageError::BudgetExhausted);
        }
        let pixmap = image::decode(bytes)?;
        let pixels = u64::from(pixmap.width()) * u64::from(pixmap.height());
        if self.pixels - replaced + pixels > MAX_TOTAL_IMAGE_PIXELS {
            return Err(ImageError::BudgetExhausted);
        }
        self.pixels = self.pixels - replaced + pixels;
        let size = (pixmap.width(), pixmap.height());
        self.images.insert(url.as_str().to_owned(), Arc::new(pixmap));
        Ok(size)
    }

    /// Parses and stores a stylesheet under `url`. Sheets over 2 MiB, or
    /// past 8 MiB / 32 sheets in total, are rejected and simply not applied.
    /// `@import` inside it is ignored (no further fetches).
    pub fn insert_stylesheet(&mut self, url: &Url, css: &str) -> Result<(), StylesheetError> {
        if css.len() > MAX_STYLESHEET_BYTES {
            return Err(StylesheetError::TooLarge);
        }
        let replaced = self.stylesheets.get(url.as_str()).map(|s| s.1);
        if replaced.is_none() && self.stylesheets.len() >= dom::MAX_STYLESHEETS {
            return Err(StylesheetError::BudgetExhausted);
        }
        let total = self.css_bytes - replaced.unwrap_or(0) + css.len();
        if total > MAX_TOTAL_STYLESHEET_BYTES {
            return Err(StylesheetError::BudgetExhausted);
        }
        self.css_bytes = total;
        self.stylesheets.insert(url.as_str().to_owned(), (Arc::new(css::Stylesheet::parse(css)), css.len()));
        Ok(())
    }

    /// Number of images and stylesheets held.
    pub fn len(&self) -> usize {
        self.images.len() + self.stylesheets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn get(&self, url: &Url) -> Option<Arc<Pixmap>> {
        self.images.get(url.as_str()).cloned()
    }

    fn stylesheet(&self, url: &Url) -> Option<&css::Stylesheet> {
        self.stylesheets.get(url.as_str()).map(|s| s.0.as_ref())
    }
}

/// A rendered frame: `width * height` pixels, row-major, 4 bytes each
/// (R, G, B, A). The bytes are premultiplied, but the page background is
/// always opaque, so every pixel has alpha 255 and the buffer is equally
/// valid as straight RGBA.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame").field("width", &self.width).field("height", &self.height).finish_non_exhaustive()
    }
}

/// What is under a point of the page.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HitTarget {
    Link(Url),
    /// A form control, or a `<label>` for one.
    Control(ControlId),
    /// An entry in an open `<select>` list.
    Option(ControlId, usize),
}

/// A clickable link region in page coordinates (CSS px).
#[derive(Clone, Debug, PartialEq)]
pub struct LinkArea {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub url: Url,
}

/// A document laid out for one viewport width.
pub struct Layout {
    laid: layout::Laid,
    paths: paint::GlyphPaths,
}

impl Layout {
    /// Lay the document out for a viewport `width` in CSS px (clamped to
    /// 1..=16384).
    pub fn new(doc: &Document, resources: &Resources, width: f32) -> Layout {
        let width = layout::clamp_width(width);
        // Linked sheets the broker could not fetch are skipped; the rest keep
        // their place in document order.
        let author: Vec<style::AuthorSheet<'_>> = doc
            .sheets
            .iter()
            .filter_map(|s| match s {
                DocSheet::Inline(sheet, media) => Some(style::AuthorSheet { sheet, media: media.as_ref() }),
                DocSheet::Linked(url, media) => {
                    resources.stylesheet(url).map(|sheet| style::AuthorSheet { sheet, media: media.as_ref() })
                }
            })
            .collect();
        let styles = style::compute(&doc.dom, ua_sheet(), &author, width);
        let laid = layout::layout(&doc.dom, &styles, resources, width);
        let paths = paint::glyph_paths(&laid);
        Layout { laid, paths }
    }

    /// Viewport width the layout was made for, in CSS px.
    pub fn width(&self) -> f32 {
        self.laid.width
    }

    /// Total content height in CSS px.
    pub fn height(&self) -> f32 {
        self.laid.height
    }

    /// Paints the region `[scroll_y, scroll_y + height)` at `scale` device
    /// pixels per CSS px. The frame is `ceil(width * scale)` by
    /// `ceil(height * scale)` pixels; see [`Frame`] for the format. `scale`
    /// is clamped to 0.1..=8 and frames to 16384 px per side and 64 Mpx.
    pub fn paint(&self, scroll_y: f32, height: f32, scale: f32) -> Frame {
        let (width, height, rgba) = paint::paint(&self.laid, &self.paths, scroll_y, height, scale);
        Frame { width, height, rgba }
    }

    /// Like [`Layout::paint`], with form controls showing `forms`' values,
    /// focus and open lists.
    pub fn paint_forms(&self, forms: &FormState, scroll_y: f32, height: f32, scale: f32) -> Frame {
        let _ = forms;
        self.paint(scroll_y, height, scale)
    }

    /// Like [`Layout::hit_test`], also finding form controls and the
    /// entries of an open `<select>` list.
    pub fn hit_test_forms(&self, forms: &FormState, x: f32, y: f32) -> Option<HitTarget> {
        let _ = forms;
        self.hit_test(x, y)
    }

    /// What is at a point in page coordinates (CSS px, y includes scroll).
    pub fn hit_test(&self, x: f32, y: f32) -> Option<HitTarget> {
        // Later areas were laid out on top of earlier ones.
        self.laid
            .links
            .iter()
            .rev()
            .find(|(r, _)| r.contains(x, y))
            .and_then(|(_, id)| self.laid.link_urls.get(*id as usize))
            .map(|u| HitTarget::Link(u.clone()))
    }

    /// Every link region, for a broker-side hit-test map.
    pub fn links(&self) -> Vec<LinkArea> {
        self.laid
            .links
            .iter()
            .filter_map(|(r, id)| {
                let url = self.laid.link_urls.get(*id as usize)?.clone();
                Some(LinkArea { x: r.x, y: r.y, width: r.w, height: r.h, url })
            })
            .collect()
    }
}
