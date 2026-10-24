//! Displaying a page: parse, ask the broker for images, lay out, paint.
//! Everything here works on bytes the broker handed over; nothing is fetched.

use hollow_engine::{Document, FormEnctype, FormKey, FormMethod, FormState, HitTarget, Interaction, Layout, Resources};
use hollow_ipc::{FormEncoding, FormSubmit, Frame, KeyInput, Method, Viewport};
use url::Url;

pub struct Page {
    doc: Document,
    forms: FormState,
    resources: Resources,
    layout: Option<Layout>,
    viewport: Viewport,
    /// Where the last frame was painted, for repaints after a click or key.
    scroll_y: f32,
    /// The second round of resources (images the linked stylesheets use)
    /// has been asked for.
    asked_css_images: bool,
}

impl Page {
    pub fn load(url: &str, content_type: Option<&str>, body: &[u8], viewport: Viewport) -> Page {
        let url = Url::parse(url).unwrap_or_else(|_| Url::parse("about:blank").expect("valid URL"));
        let html = to_html(content_type, body);
        let doc = Document::parse(&html, &url);
        let forms = doc.forms();
        Page {
            doc,
            forms,
            resources: Resources::new(),
            layout: None,
            viewport: sane(viewport),
            scroll_y: 0.0,
            asked_css_images: false,
        }
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn scroll_y(&self) -> f32 {
        self.scroll_y
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

    /// Once, after the first resources: images used by the linked
    /// stylesheets, which could only be known once those arrived. `None`
    /// when there are none (or they were already asked for).
    pub fn stylesheet_images(&mut self) -> Option<Vec<String>> {
        if self.asked_css_images {
            return None;
        }
        self.asked_css_images = true;
        let urls: Vec<String> = self.doc.stylesheet_image_urls(&self.resources).into_iter().map(String::from).collect();
        (!urls.is_empty()).then_some(urls)
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
        self.scroll_y = scroll_y;
        let f = layout.paint_forms(&self.forms, scroll_y, viewport.height, viewport.scale);
        Frame { scroll_y, page_height, width: f.width, height: f.height, rgba: f.rgba }
    }

    /// A click: a link to follow, a form to submit, or a change to the
    /// forms that needs a repaint.
    pub fn click(&mut self, x: f32, y: f32) -> Outcome {
        let hit = self.layout.as_ref().and_then(|l| l.hit_test_forms(&self.forms, x, y));
        let link = match &hit {
            Some(HitTarget::Link(url)) => Some(String::from(url.clone())),
            _ => None,
        };
        let interaction = self.forms.click(&self.doc, hit.as_ref());
        self.outcome(link, interaction)
    }

    pub fn key(&mut self, key: KeyInput) -> Outcome {
        let key = match key {
            KeyInput::Text(t) => FormKey::Text(t),
            KeyInput::Backspace => FormKey::Backspace,
            KeyInput::Delete => FormKey::Delete,
            KeyInput::Enter => FormKey::Enter,
            KeyInput::Left => FormKey::Left,
            KeyInput::Right => FormKey::Right,
            KeyInput::Home => FormKey::Home,
            KeyInput::End => FormKey::End,
            KeyInput::Up => FormKey::Up,
            KeyInput::Down => FormKey::Down,
            KeyInput::Tab => FormKey::Tab,
            KeyInput::BackTab => FormKey::BackTab,
            KeyInput::Escape => FormKey::Escape,
        };
        let interaction = self.forms.key(&self.doc, &key);
        self.outcome(None, interaction)
    }

    fn outcome(&self, link: Option<String>, interaction: Interaction) -> Outcome {
        let (submit, repaint) = match interaction {
            Interaction::None => (None, false),
            Interaction::Repaint => (None, true),
            Interaction::Submit(s) => (
                Some(FormSubmit {
                    action: s.action.into(),
                    method: match s.method {
                        FormMethod::Get => Method::Get,
                        FormMethod::Post => Method::Post,
                    },
                    encoding: match s.enctype {
                        FormEnctype::UrlEncoded => FormEncoding::UrlEncoded,
                        FormEnctype::Multipart => FormEncoding::Multipart,
                        FormEnctype::Plain => FormEncoding::Plain,
                    },
                    entries: s.entries,
                    new_tab: s.new_tab,
                }),
                true,
            ),
        };
        Outcome { link, submit, editing: self.forms.editing(), repaint }
    }
}

pub struct Outcome {
    pub link: Option<String>,
    pub submit: Option<FormSubmit>,
    pub editing: bool,
    pub repaint: bool,
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
