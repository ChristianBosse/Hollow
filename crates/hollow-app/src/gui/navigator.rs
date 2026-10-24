//! One navigator thread per tab. It owns that tab's renderer and shares the
//! network process with the other tabs, so the window thread never waits on
//! either and a slow page in one tab doesn't hold up the rest.
//!
//! One renderer per page: navigating kills the old renderer process and
//! starts a fresh one (SEC-03). Subresources are fetched by the broker, on
//! the circuit of the page in the address bar (first-party isolation). A
//! background tab can be suspended: its renderer is killed and the page's
//! bytes stay in the broker's memory, so switching back re-renders without
//! touching the network.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, bail};
use hollow_ipc::{BrokerToRenderer, Frame, KeyInput, MAX_STYLESHEET_LEN, RendererToBroker, Viewport};
use url::Url;
use winit::event_loop::EventLoopProxy;

use crate::child::{Child, Kind};
use crate::net::{self, Ledger, Navigation, Net};
use crate::terminal::sanitize;
use crate::{form, policy};

pub const START_URL: &str = "about:hollow";

pub type TabId = u64;

#[derive(Debug)]
pub enum Command {
    Navigate {
        url: Url,
        from_history: bool,
        referrer: Option<Url>,
    },
    Paint {
        scroll_y: f32,
        viewport: Viewport,
    },
    /// A click at page coordinates; `new_tab` for middle and Ctrl+click.
    Click {
        x: f32,
        y: f32,
        new_tab: bool,
    },
    /// A key for the page's focused field.
    Key(KeyInput),
    /// Kill the renderer but keep the page, for tabs out of sight.
    Suspend,
    Shutdown,
}

#[derive(Debug)]
pub enum UiEvent {
    Tab(TabId, TabEvent),
    /// Tor's own status, for every tab.
    NetStatus(String),
    /// A scripted step (development builds with `dev-dump` only).
    #[cfg(feature = "dev-dump")]
    Dev(super::dev_dump::Step),
}

#[derive(Debug)]
pub enum TabEvent {
    Status(String),
    /// A navigation committed to this URL (after redirects).
    Loading {
        url: Url,
        from_history: bool,
    },
    Title(Option<String>),
    Frame(Frame),
    /// The page started a navigation of its own (a link or a form).
    Started,
    /// A navigation ended, shown or failed.
    Finished,
    /// Whether a page field has focus, so keys should go to the page.
    Editing(bool),
    /// A link the user wants in a new tab.
    OpenInNewTab {
        url: Url,
        referrer: Url,
    },
}

/// The network process, once Tor is up, shared by all tabs.
#[derive(Clone)]
pub struct NetSlot(Arc<(Mutex<NetState>, Condvar)>);

enum NetState {
    Starting,
    Ready(Net),
    Failed(String),
    Stopped,
}

impl NetSlot {
    /// Starts Tor on its own thread. The start page needs no network, so
    /// the window shows it while Tor bootstraps.
    pub fn start(proxy: EventLoopProxy<UiEvent>) -> NetSlot {
        let slot = NetSlot(Arc::new((Mutex::new(NetState::Starting), Condvar::new())));
        let ours = slot.clone();
        std::thread::spawn(move || {
            let _ = proxy.send_event(UiEvent::NetStatus("Starting Tor: every session starts fresh, in RAM…".into()));
            let progress = proxy.clone();
            let result = Net::start(Box::new(move |p| {
                let _ = progress.send_event(UiEvent::NetStatus(format!("Tor: {p}")));
            }));
            let status = match &result {
                Ok(_) => "Connected to Tor · Fortress mode: no JavaScript · nothing is written to disk".to_owned(),
                Err(e) => format!("Tor failed to start: {e:#}"),
            };
            let (lock, cvar) = &*ours.0;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            match (&*state, result) {
                // The window closed while Tor was starting.
                (NetState::Stopped, Ok(net)) => net.shutdown(),
                (NetState::Stopped, Err(_)) => {}
                (_, Ok(net)) => *state = NetState::Ready(net),
                (_, Err(e)) => *state = NetState::Failed(format!("{e:#}")),
            }
            cvar.notify_all();
            drop(state);
            let _ = proxy.send_event(UiEvent::NetStatus(sanitize(&status)));
        });
        slot
    }

    /// The network, waiting for Tor if it's still starting.
    fn wait(&self) -> anyhow::Result<Net> {
        let (lock, cvar) = &*self.0;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        while matches!(*state, NetState::Starting) {
            state = cvar.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        match &*state {
            NetState::Ready(net) => Ok(net.clone()),
            NetState::Failed(e) => bail!("Tor isn't running: {e}"),
            NetState::Starting | NetState::Stopped => bail!("Tor isn't running"),
        }
    }

    pub fn is_starting(&self) -> bool {
        matches!(*self.0.0.lock().unwrap_or_else(|e| e.into_inner()), NetState::Starting)
    }

    pub fn shutdown(&self) {
        let (lock, cvar) = &*self.0;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let NetState::Ready(net) = std::mem::replace(&mut *state, NetState::Stopped) {
            net.shutdown();
        }
        cvar.notify_all();
    }
}

pub fn spawn(
    id: TabId,
    proxy: EventLoopProxy<UiEvent>,
    net: NetSlot,
    viewport: Viewport,
) -> (Sender<Command>, JoinHandle<()>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || Navigator { id, proxy, viewport, net, renderer: None, page: None }.run(rx));
    (tx, handle)
}

/// Everything needed to show a page again without the network.
struct PageData {
    url: Url,
    content_type: Option<String>,
    body: Vec<u8>,
    images: Vec<(String, Vec<u8>)>,
    stylesheets: Vec<(String, String)>,
}

struct Navigator {
    id: TabId,
    proxy: EventLoopProxy<UiEvent>,
    viewport: Viewport,
    net: NetSlot,
    renderer: Option<Child>,
    page: Option<PageData>,
}

impl Navigator {
    fn run(mut self, rx: Receiver<Command>) {
        let mut pending = None;
        loop {
            let cmd = match pending.take() {
                Some(c) => c,
                None => match rx.recv() {
                    Ok(c) => c,
                    Err(_) => break,
                },
            };
            let result = match cmd {
                Command::Navigate { url, from_history, referrer } => {
                    let r = self.navigate(Navigation { url, referrer: referrer.as_ref(), post: None }, from_history);
                    self.send(TabEvent::Finished);
                    r
                }
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
                Command::Click { x, y, new_tab } => self.interact(BrokerToRenderer::Click { x, y }, new_tab),
                Command::Key(key) => self.interact(BrokerToRenderer::Key(key), false),
                Command::Suspend => {
                    self.renderer = None;
                    Ok(())
                }
                Command::Shutdown => break,
            };
            if let Err(e) = result {
                self.status(&format!("{e:#}"));
            }
        }
    }

    fn send(&self, event: TabEvent) {
        let _ = self.proxy.send_event(UiEvent::Tab(self.id, event));
    }

    fn status(&self, s: &str) {
        self.send(TabEvent::Status(sanitize(s)));
    }

    fn navigate(&mut self, nav: Navigation, from_history: bool) -> anyhow::Result<()> {
        // The old page's renderer dies here; the new page gets a fresh one.
        self.renderer = None;
        self.page = None;
        let url = nav.url.clone();
        let (url, content_type, body) = if url.as_str() == START_URL {
            (url, Some("text/html".to_owned()), START_PAGE.as_bytes().to_vec())
        } else {
            policy::check(&url)?;
            if self.net.is_starting() {
                self.status(&format!("Waiting for Tor before loading {url} …"));
            }
            let net = self.net.wait()?;
            self.status(&format!("Loading {url} …"));
            match net.fetch_page(nav, &mut |_| {}) {
                Ok((final_url, resp)) => match net::onion_location(&final_url, &resp) {
                    // The site has an onion service: use it (NET-09). If it
                    // fails, the HTTPS page we already have is shown.
                    Some(onion) => {
                        self.status(&format!("This site has an onion service; loading {onion} …"));
                        let nav = Navigation { url: onion, referrer: None, post: None };
                        match net.fetch_page(nav, &mut |_| {}) {
                            Ok((u, r)) => (u, r.header("content-type").map(str::to_owned), r.body),
                            Err(_) => (final_url, resp.header("content-type").map(str::to_owned), resp.body),
                        }
                    }
                    None => (final_url, resp.header("content-type").map(str::to_owned), resp.body),
                },
                Err(e) => (url, Some("text/html".to_owned()), error_page(&format!("{e:#}")).into_bytes()),
            }
        };
        self.send(TabEvent::Loading { url: url.clone(), from_history });
        let page = PageData { url, content_type, body, images: vec![], stylesheets: vec![] };
        self.show(page, true)
    }

    /// Starts a renderer for `page` and paints it at the top. With `fetch`,
    /// what the renderer asks for is fetched and remembered; without, the
    /// remembered resources are handed over again.
    fn show(&mut self, mut page: PageData, fetch: bool) -> anyhow::Result<()> {
        self.renderer = None;
        let mut renderer = Child::spawn(Kind::Fortress)?;
        renderer.send(&BrokerToRenderer::Load {
            url: page.url.to_string(),
            content_type: page.content_type.clone(),
            body: page.body.clone(),
            viewport: self.viewport,
        })?;
        match renderer.recv::<RendererToBroker>()? {
            RendererToBroker::Loaded { title } => self.send(TabEvent::Title(title.map(|t| sanitize(&t)))),
            other => bail!("unexpected reply from renderer: {}", short(&other)),
        }
        // At most two rounds: what the page asks for, then the images its
        // stylesheets use. Without `fetch`, the remembered resources are
        // handed over again each round.
        let (mut asked_images, mut asked_sheets, mut got_images, mut got_sheets) = (0, 0, 0, 0);
        let mut round = 0;
        let frame = loop {
            let (want_images, want_sheets) = match renderer.recv::<RendererToBroker>()? {
                RendererToBroker::NeedResources { images, stylesheets } if round < 2 => (images, stylesheets),
                RendererToBroker::Frame(f) if round > 0 => break f,
                RendererToBroker::Error(e) => bail!("renderer failed: {}", sanitize(&e)),
                other => bail!("unexpected reply from renderer: {}", short(&other)),
            };
            round += 1;
            if fetch {
                let fetched = self.fetch_resources(&page.url, &want_images, &want_sheets)?;
                (asked_images, asked_sheets) = (asked_images + want_images.len(), asked_sheets + want_sheets.len());
                (got_images, got_sheets) = (got_images + fetched.images.len(), got_sheets + fetched.stylesheets.len());
                page.images.extend(fetched.images);
                page.stylesheets.extend(fetched.stylesheets);
            }
            renderer.send(&BrokerToRenderer::Resources {
                images: page.images.clone(),
                stylesheets: page.stylesheets.clone(),
            })?;
        };
        let summary = fetch.then(|| {
            let mut s = String::new();
            if asked_images > 0 {
                s += &format!(" · images {got_images}/{asked_images}");
            }
            if asked_sheets > 0 {
                s += &format!(" · stylesheets {got_sheets}/{asked_sheets}");
            }
            s
        });
        self.send(TabEvent::Frame(frame));
        self.renderer = Some(renderer);
        if let Some(summary) = summary.filter(|_| page.url.as_str() != START_URL) {
            self.status(&format!("Done{summary} · circuit: {}", policy::isolation_key(&page.url)));
        }
        self.page = Some(page);
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
        if page.as_str() == START_URL || image_urls.len() + sheet_urls.len() == 0 {
            return Ok(fetched);
        }
        let Ok(net) = self.net.wait() else { return Ok(fetched) };
        self.status(&format!("Loading {} images and {} stylesheets…", image_urls.len(), sheet_urls.len()));
        let all: Vec<Url> = image_urls.iter().chain(&sheet_urls).cloned().collect();
        // Failures go to stderr for developers; the status bar shows counts.
        let responses = net.fetch_subresources(&all, page, &mut |event| {
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

    /// The renderer, restarted from the remembered page if it was suspended.
    fn renderer(&mut self) -> anyhow::Result<Option<&mut Child>> {
        if self.renderer.is_none()
            && let Some(page) = self.page.take()
        {
            self.show(page, false)?;
        }
        Ok(self.renderer.as_mut())
    }

    fn paint(&mut self, scroll_y: f32) -> anyhow::Result<()> {
        let viewport = self.viewport;
        let Some(renderer) = self.renderer()? else { return Ok(()) };
        renderer.send(&BrokerToRenderer::Paint { scroll_y, viewport })?;
        let frame = expect_frame(renderer)?;
        self.send(TabEvent::Frame(frame));
        Ok(())
    }

    /// Sends a click or key to the renderer and acts on what it says: show
    /// the repainted frame, follow a link or submit a form.
    fn interact(&mut self, msg: BrokerToRenderer, new_tab: bool) -> anyhow::Result<()> {
        let Some(renderer) = self.renderer()? else { return Ok(()) };
        renderer.send(&msg)?;
        let (link, submit, editing, repaint) = match renderer.recv::<RendererToBroker>()? {
            RendererToBroker::Interacted { link, submit, editing, repaint } => (link, submit, editing, repaint),
            other => bail!("unexpected reply from renderer: {}", short(&other)),
        };
        if repaint {
            let frame = expect_frame(renderer)?;
            self.send(TabEvent::Frame(frame));
        }
        self.send(TabEvent::Editing(editing));
        let Some(current) = self.page.as_ref().map(|p| p.url.clone()) else { return Ok(()) };
        let (url, post) = if let Some(submit) = submit {
            // The renderer's submission is checked and encoded here.
            let req = form::to_request(submit)?;
            if req.new_tab {
                self.send(TabEvent::OpenInNewTab { url: req.url, referrer: current });
                return Ok(());
            }
            (req.url, req.post)
        } else {
            let Some(link) = link else { return Ok(()) };
            // What the renderer says is a link is just a suggestion.
            let url = Url::parse(&link).with_context(|| format!("bad link {}", sanitize(&link)))?;
            if let Err(e) = policy::check(&url) {
                self.status(&format!("Blocked {}: {e}", sanitize(url.as_str())));
                return Ok(());
            }
            if new_tab {
                self.send(TabEvent::OpenInNewTab { url, referrer: current });
                return Ok(());
            }
            (url, None)
        };
        self.send(TabEvent::Editing(false));
        self.send(TabEvent::Started);
        let result = self.navigate(Navigation { url, referrer: Some(&current), post }, false);
        self.send(TabEvent::Finished);
        result
    }
}

#[derive(Default)]
struct Fetched {
    images: Vec<(String, Vec<u8>)>,
    stylesheets: Vec<(String, String)>,
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
        RendererToBroker::Interacted { .. } => "Interacted",
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
<li><b>Ctrl+L</b>: address bar &nbsp; <b>Enter</b>: go &nbsp; <b>Esc</b>: cancel. Words instead of an address search DuckDuckGo's onion service.</li>
<li><b>Ctrl+T</b>: new tab &nbsp; <b>Ctrl+W</b>: close tab &nbsp; <b>Ctrl+Tab</b>: next tab &nbsp; <b>Middle click</b> or <b>Ctrl+click</b>: open a link in a new tab</li>
<li><b>Alt+Left / Alt+Right</b>: back / forward</li>
<li><b>Mouse wheel, Page Up/Down, Space</b>: scroll</li>
</ul>
<p>This is a preview build. It has not been audited: don't rely on it to protect anyone yet.</p>
</body></html>"#;
