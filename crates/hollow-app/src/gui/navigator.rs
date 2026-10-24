//! The navigator thread. It owns the network process and the current page's
//! renderer, so the window thread never waits on either.
//!
//! One renderer per page: navigating kills the old renderer process and
//! starts a fresh one (SEC-03). Subresources are fetched by the broker, on
//! the circuit of the page in the address bar (first-party isolation).

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;

use anyhow::{Context, bail};
use hollow_ipc::{BrokerToRenderer, Frame, MAX_STYLESHEET_LEN, RendererToBroker, Viewport};
use url::Url;
use winit::event_loop::EventLoopProxy;

use crate::child::{Child, Kind};
use crate::net::{Ledger, Net};
use crate::policy;
use crate::terminal::sanitize;

pub const START_URL: &str = "about:hollow";

#[derive(Debug)]
pub enum Command {
    Navigate { url: Url, from_history: bool },
    Paint { scroll_y: f32, viewport: Viewport },
    Click { x: f32, y: f32 },
    Shutdown,
}

#[derive(Debug)]
pub enum UiEvent {
    Status(String),
    /// A navigation committed to this URL (after redirects).
    Loading {
        url: Url,
        from_history: bool,
    },
    Title(Option<String>),
    Frame(Frame),
    /// A scripted step (development builds with `dev-dump` only).
    #[cfg(feature = "dev-dump")]
    Dev(super::dev_dump::Step),
}

pub fn spawn(proxy: EventLoopProxy<UiEvent>, viewport: Viewport) -> (Sender<Command>, JoinHandle<()>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || Navigator { proxy, viewport, net: None, page: None }.run(rx));
    (tx, handle)
}

struct Page {
    renderer: Child,
}

struct Navigator {
    proxy: EventLoopProxy<UiEvent>,
    viewport: Viewport,
    net: Option<Net>,
    page: Option<Page>,
}

impl Navigator {
    fn run(mut self, rx: Receiver<Command>) {
        // The start page needs no network: show it before Tor bootstraps.
        let mut pending = None;
        if let Ok(cmd) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
            match cmd {
                Command::Navigate { url, from_history } if url.as_str() == START_URL => {
                    if let Err(e) = self.navigate(url, from_history) {
                        self.status(&format!("{e:#}"));
                    }
                }
                other => pending = Some(other),
            }
        }

        self.status("Starting Tor: every session starts fresh, in RAM…");
        let proxy = self.proxy.clone();
        match Net::start(Box::new(move |p| {
            let _ = proxy.send_event(UiEvent::Status(format!("Tor: {p}")));
        })) {
            Ok(net) => {
                self.net = Some(net);
                self.status("Connected to Tor · Fortress mode: no JavaScript · nothing is written to disk");
            }
            Err(e) => self.status(&format!("Tor failed to start: {e:#}")),
        }

        loop {
            let cmd = match pending.take() {
                Some(c) => c,
                None => match rx.recv() {
                    Ok(c) => c,
                    Err(_) => break,
                },
            };
            let result = match cmd {
                Command::Navigate { url, from_history } => self.navigate(url, from_history),
                Command::Paint { mut scroll_y, mut viewport } => {
                    // Scrolling sends bursts; only the latest position matters.
                    loop {
                        match rx.try_recv() {
                            Ok(Command::Paint { scroll_y: s, viewport: v }) => (scroll_y, viewport) = (s, v),
                            Ok(other) => {
                                pending = Some(other);
                                break;
                            }
                            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                        }
                    }
                    self.viewport = viewport;
                    self.paint(scroll_y)
                }
                Command::Click { x, y } => self.click(x, y),
                Command::Shutdown => break,
            };
            if let Err(e) = result {
                self.status(&format!("{e:#}"));
            }
        }
        self.page = None;
        if let Some(net) = self.net.take() {
            net.shutdown();
        }
    }

    fn status(&self, s: &str) {
        let _ = self.proxy.send_event(UiEvent::Status(sanitize(s)));
    }

    fn navigate(&mut self, url: Url, from_history: bool) -> anyhow::Result<()> {
        // The old page's renderer dies here; the new page gets a fresh one.
        self.page = None;
        let (url, content_type, body) = if url.as_str() == START_URL {
            (url, Some("text/html".to_owned()), START_PAGE.as_bytes().to_vec())
        } else {
            policy::check(&url)?;
            self.status(&format!("Loading {url} …"));
            let net = self.net.as_mut().context("Tor isn't running")?;
            match net.fetch_page(url.clone(), &mut |_| {}) {
                Ok((final_url, resp)) => (final_url, resp.header("content-type").map(str::to_owned), resp.body),
                Err(e) => (url, Some("text/html".to_owned()), error_page(&format!("{e:#}")).into_bytes()),
            }
        };
        let _ = self.proxy.send_event(UiEvent::Loading { url: url.clone(), from_history });

        let mut renderer = Child::spawn(Kind::Fortress)?;
        renderer.send(&BrokerToRenderer::Load { url: url.to_string(), content_type, body, viewport: self.viewport })?;
        match renderer.recv::<RendererToBroker>()? {
            RendererToBroker::Loaded { title } => {
                let _ = self.proxy.send_event(UiEvent::Title(title.map(|t| sanitize(&t))));
            }
            other => bail!("unexpected reply from renderer: {}", short(&other)),
        }
        let (want_images, want_sheets) = match renderer.recv::<RendererToBroker>()? {
            RendererToBroker::NeedResources { images, stylesheets } => (images, stylesheets),
            other => bail!("unexpected reply from renderer: {}", short(&other)),
        };
        let fetched = self.fetch_resources(&url, &want_images, &want_sheets)?;
        let summary = fetched.summary(want_images.len(), want_sheets.len());
        renderer.send(&BrokerToRenderer::Resources { images: fetched.images, stylesheets: fetched.stylesheets })?;
        let frame = expect_frame(&mut renderer)?;
        let _ = self.proxy.send_event(UiEvent::Frame(frame));
        self.page = Some(Page { renderer });
        if url.as_str() != START_URL {
            self.status(&format!("Done{summary} · circuit: {}", policy::isolation_key(&url)));
        }
        Ok(())
    }

    /// The renderer asks; the broker decides. Only URLs that pass the same
    /// policy as navigation are fetched, all together, on the page's circuit,
    /// and only responses of the right type are handed back.
    fn fetch_resources(&mut self, page: &Url, images: &[String], sheets: &[String]) -> anyhow::Result<Fetched> {
        let allowed = |list: &[String]| -> Vec<Url> {
            list.iter().filter_map(|u| Url::parse(u).ok()).filter(|u| policy::check(u).is_ok()).collect()
        };
        let (image_urls, sheet_urls) = (allowed(images), allowed(sheets));
        let mut fetched = Fetched::default();
        if page.as_str() == START_URL || self.net.is_none() || image_urls.len() + sheet_urls.len() == 0 {
            return Ok(fetched);
        }
        self.status(&format!("Loading {} images and {} stylesheets…", image_urls.len(), sheet_urls.len()));
        let Some(net) = self.net.as_mut() else { return Ok(fetched) };
        let all: Vec<Url> = image_urls.iter().chain(&sheet_urls).cloned().collect();
        // Failures go to stderr for developers; the status bar shows counts.
        let responses = net.fetch_subresources(&all, &policy::isolation_key(page), &mut |event| {
            if let Ledger::Failed { url, reason } = event {
                eprintln!("hollow: {url} failed: {reason}");
            }
        })?;
        for (i, (url, resp)) in all.into_iter().zip(responses).enumerate() {
            let Some(resp) = resp else { continue };
            let ct = resp.header("content-type").unwrap_or("").to_ascii_lowercase();
            if i < image_urls.len() {
                if ct.starts_with("image/") {
                    fetched.images.push((url.to_string(), resp.body));
                }
            } else if (ct.is_empty() || ct.starts_with("text/css")) && resp.body.len() <= MAX_STYLESHEET_LEN {
                fetched.stylesheets.push((url.to_string(), String::from_utf8_lossy(&resp.body).into_owned()));
            }
        }
        Ok(fetched)
    }

    fn paint(&mut self, scroll_y: f32) -> anyhow::Result<()> {
        let Some(page) = self.page.as_mut() else { return Ok(()) };
        page.renderer.send(&BrokerToRenderer::Paint { scroll_y, viewport: self.viewport })?;
        let frame = expect_frame(&mut page.renderer)?;
        let _ = self.proxy.send_event(UiEvent::Frame(frame));
        Ok(())
    }

    fn click(&mut self, x: f32, y: f32) -> anyhow::Result<()> {
        let Some(page) = self.page.as_mut() else { return Ok(()) };
        page.renderer.send(&BrokerToRenderer::HitTest { x, y })?;
        let link = match page.renderer.recv::<RendererToBroker>()? {
            RendererToBroker::Hit(link) => link,
            other => bail!("unexpected reply from renderer: {}", short(&other)),
        };
        let Some(link) = link else { return Ok(()) };
        // What the renderer says is a link is just a suggestion.
        let url = Url::parse(&link).with_context(|| format!("bad link {}", sanitize(&link)))?;
        match policy::check(&url) {
            Ok(()) => self.navigate(url, false),
            Err(e) => {
                self.status(&format!("Blocked {}: {e}", sanitize(url.as_str())));
                Ok(())
            }
        }
    }
}

#[derive(Default)]
struct Fetched {
    images: Vec<(String, Vec<u8>)>,
    stylesheets: Vec<(String, String)>,
}

impl Fetched {
    fn summary(&self, images: usize, sheets: usize) -> String {
        let mut s = String::new();
        if images > 0 {
            s += &format!(" · images {}/{images}", self.images.len());
        }
        if sheets > 0 {
            s += &format!(" · stylesheets {}/{sheets}", self.stylesheets.len());
        }
        s
    }
}

fn expect_frame(renderer: &mut Child) -> anyhow::Result<Frame> {
    match renderer.recv::<RendererToBroker>()? {
        RendererToBroker::Frame(f) => Ok(f),
        RendererToBroker::Error(e) => bail!("renderer failed: {}", sanitize(&e)),
        other => bail!("unexpected reply from renderer: {}", short(&other)),
    }
}

/// A message summary that never dumps pixels or page text into the UI.
fn short(msg: &RendererToBroker) -> &'static str {
    match msg {
        RendererToBroker::Rendered(_) => "Rendered",
        RendererToBroker::Loaded { .. } => "Loaded",
        RendererToBroker::NeedResources { .. } => "NeedResources",
        RendererToBroker::Frame(_) => "Frame",
        RendererToBroker::Hit(_) => "Hit",
        RendererToBroker::SelfTestReport(_) => "SelfTestReport",
        RendererToBroker::Error(_) => "Error",
    }
}

pub fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            c => c.to_string(),
        })
        .collect()
}

fn error_page(message: &str) -> String {
    format!(
        "<html><head><title>Couldn't load this page</title></head><body style=\"background-color:#fbfbfd\">\
         <h1>Couldn't load this page</h1><p>{}</p>\
         <p>Nothing was sent outside Tor. Check the address, or try again: every request uses Tor, \
         and some sites block Tor exits.</p></body></html>",
        html_escape(message)
    )
}

const START_PAGE: &str = r#"<html><head><title>Hollow</title></head>
<body style="background-color:#fbfbfd;color:#1d1d24">
<h1>Hollow</h1>
<p><b>Nothing to see. Nothing to take.</b></p>
<p>You are browsing in <b>Fortress mode</b>: every page is fetched through Tor and drawn by Hollow's own engine, with no JavaScript, in a sandbox that has no network access of its own. Nothing is written to disk: closing Hollow forgets everything, including your Tor guards.</p>
<h2>Try</h2>
<ul>
<li><a href="https://check.torproject.org/">check.torproject.org</a>: confirms you're on Tor</li>
<li><a href="http://2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion/">The Tor Project, as an onion service</a></li>
<li><a href="https://en.wikipedia.org/wiki/Tor_(network)">Wikipedia: Tor (network)</a></li>
</ul>
<h2>Keys</h2>
<ul>
<li><b>Ctrl+L</b>: address bar &nbsp; <b>Enter</b>: go &nbsp; <b>Esc</b>: cancel</li>
<li><b>Alt+Left / Alt+Right</b>: back / forward</li>
<li><b>Mouse wheel, Page Up/Down, Space</b>: scroll</li>
</ul>
<p>This is a preview build. It has not been audited: don't rely on it to protect anyone yet.</p>
</body></html>"#;
