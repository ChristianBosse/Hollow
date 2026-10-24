//! Displaying a page: parse, ask the broker for images, lay out, paint.
//! Everything here works on bytes the broker handed over; nothing is fetched.

use hollow_engine::{Document, HitTarget, Layout, Resources};
use hollow_ipc::{Frame, Viewport};
use url::Url;

pub struct Page {
    doc: Document,
    resources: Resources,
    layout: Option<Layout>,
    viewport: Viewport,
}

impl Page {
    pub fn load(url: &str, content_type: Option<&str>, body: &[u8], viewport: Viewport) -> Page {
        let url = Url::parse(url).unwrap_or_else(|_| Url::parse("about:blank").expect("valid URL"));
        let html = to_html(content_type, body);
        Page { doc: Document::parse(&html, &url), resources: Resources::new(), layout: None, viewport: sane(viewport) }
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn title(&self) -> Option<String> {
        self.doc.title()
    }

    pub fn image_urls(&self) -> Vec<String> {
        self.doc.image_urls().into_iter().map(String::from).collect()
    }

    /// Linked stylesheets, all of them whatever their `media`: which sheets
    /// are fetched must never depend on the viewport (FP-05).
    pub fn stylesheet_urls(&self) -> Vec<String> {
        self.doc.stylesheet_urls().into_iter().map(String::from).collect()
    }

    /// Takes whatever the broker managed to fetch; bad or oversized images
    /// and stylesheets are skipped. Styles are computed at layout time, so
    /// the layout is rebuilt.
    pub fn add_resources(&mut self, images: Vec<(String, Vec<u8>)>, stylesheets: Vec<(String, String)>) {
        for (url, bytes) in images {
            if let Ok(url) = Url::parse(&url) {
                let _ = self.resources.insert_image(&url, &bytes);
            }
        }
        for (url, css) in stylesheets {
            if let Ok(url) = Url::parse(&url) {
                let _ = self.resources.insert_stylesheet(&url, &css);
            }
        }
        self.layout = None;
    }

    pub fn paint(&mut self, scroll_y: f32, viewport: Viewport) -> Frame {
        let viewport = sane(viewport);
        if viewport.width != self.viewport.width {
            self.layout = None;
        }
        self.viewport = viewport;
        let layout = self.layout.get_or_insert_with(|| Layout::new(&self.doc, &self.resources, viewport.width));
        let page_height = layout.height();
        let max_scroll = (page_height - viewport.height).max(0.0);
        let scroll_y = if scroll_y.is_finite() { scroll_y.clamp(0.0, max_scroll) } else { 0.0 };
        let f = layout.paint(scroll_y, viewport.height, viewport.scale);
        Frame { scroll_y, page_height, width: f.width, height: f.height, rgba: f.rgba }
    }

    pub fn hit_test(&mut self, x: f32, y: f32) -> Option<String> {
        let layout = self.layout.as_ref()?;
        match layout.hit_test(x, y)? {
            HitTarget::Link(url) => Some(url.into()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

/// The broker's viewport is trusted, but a bad one must not turn into a
/// giant allocation.
fn sane(v: Viewport) -> Viewport {
    let clamp = |x: f32, lo: f32, hi: f32, default: f32| if x.is_finite() { x.clamp(lo, hi) } else { default };
    Viewport {
        width: clamp(v.width, 200.0, 4096.0, 1000.0),
        height: clamp(v.height, 100.0, 4096.0, 700.0),
        scale: clamp(v.scale, 0.5, 4.0, 1.0),
    }
}

/// Fortress shows HTML and plain text. Anything else gets an explanation
/// instead of a parser that was never meant for it.
fn to_html(content_type: Option<&str>, body: &[u8]) -> String {
    let ct = content_type.unwrap_or("text/html").to_ascii_lowercase();
    let text = String::from_utf8_lossy(body);
    if ct.starts_with("text/html") || ct.starts_with("application/xhtml") {
        text.into_owned()
    } else if ct.starts_with("text/") {
        format!("<html><body><pre>{}</pre></body></html>", escape(&text))
    } else {
        format!(
            "<html><head><title>Can't show this file</title></head><body><h1>Can't show this file</h1>\
             <p>Fortress mode displays web pages and text. This is <code>{}</code>.</p></body></html>",
            escape(&ct)
        )
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
