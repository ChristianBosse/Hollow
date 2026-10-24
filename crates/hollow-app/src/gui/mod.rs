//! The browser window (Phase 2).
//!
//! The broker owns the window and everything the user types into it. Pages
//! arrive as finished pixels from sandboxed renderers (via one navigator
//! thread per tab); the broker only composes them with its own bars and
//! presents.
//!
//! Privacy details that live here:
//! - The window is excluded from screen capture and Recall (WIN-01).
//! - The OS-visible window title is always "Hollow": page titles never reach
//!   the taskbar, Alt-Tab or anything else that records window titles.
//! - The page area snaps to fixed sizes (letterboxing, FP-02), so the
//!   window size never becomes something a page could measure.
//! - History lives in RAM only, per tab.
//! - Only the most recently used tabs keep a live renderer; the others are
//!   suspended and re-rendered from RAM when shown again.

mod chrome;
mod navigator;

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use anyhow::Context;
use hollow_ipc::{Frame, KeyInput, Viewport};
use softbuffer::Surface;
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Icon, Window, WindowId};

use crate::policy;
use navigator::{Command, NetSlot, START_URL, TabEvent, TabId, UiEvent};

/// Letterbox margins.
const BACKGROUND: u32 = 0x1f1f25;
/// CSS px scrolled per wheel notch or arrow key.
const LINE: f32 = 48.0;
/// Tabs that keep a live renderer process; the rest are suspended.
const LIVE_RENDERERS: usize = 4;
const MAX_TABS: usize = 64;

/// Opens the window at `start` (the start page if `None`).
pub fn run(start: Option<Url>) -> anyhow::Result<bool> {
    hollow_sandbox::detach_own_console();
    let event_loop = EventLoop::<UiEvent>::with_user_event().build().context("creating the window event loop")?;
    let mut app = App::new(event_loop.create_proxy());
    app.start = start;
    event_loop.run_app(&mut app).context("running the window")?;
    app.shut_down();
    match app.fatal.take() {
        Some(e) => Err(e),
        None => Ok(true),
    }
}

struct Geometry {
    scale: f32,
    width: u32,
    height: u32,
    strip: u32,
    top: u32,
    bottom: u32,
    page_x: u32,
    page_y: u32,
    viewport: Viewport,
}

impl Geometry {
    fn of(window: &Window) -> Geometry {
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;
        let strip = (chrome::TAB_STRIP * scale).round() as u32;
        let top = strip + (chrome::TOP_BAR * scale).round() as u32;
        let bottom = (chrome::STATUS_BAR * scale).round() as u32;
        let avail_w = size.width as f32 / scale;
        let avail_h = size.height.saturating_sub(top + bottom) as f32 / scale;
        let viewport =
            Viewport { width: letterbox(avail_w, 200.0, 400.0), height: letterbox(avail_h, 100.0, 200.0), scale };
        let page_w = (viewport.width * scale).round() as u32;
        Geometry {
            scale,
            width: size.width,
            height: size.height,
            strip,
            top,
            bottom,
            page_x: size.width.saturating_sub(page_w) / 2,
            page_y: top,
            viewport,
        }
    }
}

/// Snaps a dimension down to a multiple of `step` CSS px, like Tor Browser's
/// letterboxing, unless the window is too small for that to be usable.
fn letterbox(available: f32, step: f32, min: f32) -> f32 {
    if available < min { available.max(1.0) } else { (available / step).floor() * step }
}

#[derive(Default)]
struct AddressBar {
    text: String,
    focused: bool,
    /// Everything selected: the next keystroke replaces the text.
    selected: bool,
}

/// What the window knows about one tab; the page itself lives in the tab's
/// navigator thread and renderer.
struct Tab {
    id: TabId,
    nav: Sender<Command>,
    thread: JoinHandle<()>,
    url: Option<Url>,
    title: Option<String>,
    status: String,
    loading: bool,
    frame: Option<Frame>,
    scroll_y: f32,
    page_height: f32,
    history: Vec<Url>,
    history_pos: usize,
    /// A field on the page has focus: keys go to the page.
    editing: bool,
    /// Larger is more recent; decides which renderers stay alive.
    last_shown: u64,
    live: bool,
}

impl Tab {
    fn send(&self, cmd: Command) {
        let _ = self.nav.send(cmd);
    }

    fn label(&self) -> String {
        match (&self.title, &self.url) {
            (Some(t), _) if !t.trim().is_empty() => t.clone(),
            (_, Some(u)) if u.as_str() == START_URL => "Hollow".into(),
            (_, Some(u)) => u.host_str().unwrap_or(u.as_str()).to_owned(),
            (_, None) => "New tab".into(),
        }
    }
}

struct App {
    proxy: EventLoopProxy<UiEvent>,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    net: Option<NetSlot>,
    fatal: Option<anyhow::Error>,
    start: Option<Url>,

    tabs: Vec<Tab>,
    active: usize,
    next_id: TabId,
    clock: u64,
    net_status: String,

    address: AddressBar,
    modifiers: ModifiersState,
    cursor: PhysicalPosition<f64>,
    bars: Option<Bars>,
}

/// The rendered bars and what they were rendered from.
struct Bars {
    strip_key: String,
    strip: chrome::Bar,
    top_key: String,
    top: chrome::Bar,
    bottom_key: String,
    bottom: chrome::Bar,
}

impl App {
    fn new(proxy: EventLoopProxy<UiEvent>) -> App {
        App {
            proxy,
            window: None,
            surface: None,
            net: None,
            fatal: None,
            start: None,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            clock: 0,
            net_status: String::new(),
            address: AddressBar::default(),
            modifiers: ModifiersState::empty(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            bars: None,
        }
    }

    fn tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    fn tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    fn viewport(&self) -> Option<Viewport> {
        self.window.as_ref().map(|w| Geometry::of(w).viewport)
    }

    fn redraw_later(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Opens a tab at `url`, in front or behind the current one.
    fn open_tab(&mut self, url: Url, referrer: Option<Url>, activate: bool) {
        let (Some(viewport), Some(net)) = (self.viewport(), self.net.clone()) else { return };
        if self.tabs.len() >= MAX_TABS {
            if let Some(t) = self.tab_mut() {
                t.status = format!("At most {MAX_TABS} tabs; close some first");
            }
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let (nav, thread) = navigator::spawn(id, self.proxy.clone(), net, viewport);
        let tab = Tab {
            id,
            nav,
            thread,
            url: None,
            title: None,
            status: String::new(),
            loading: false,
            frame: None,
            scroll_y: 0.0,
            page_height: 0.0,
            history: Vec::new(),
            history_pos: 0,
            editing: false,
            last_shown: 0,
            live: true,
        };
        // New tabs open next to the current one, like other browsers.
        let at = if self.tabs.is_empty() { 0 } else { self.active + 1 };
        self.tabs.insert(at, tab);
        if activate || self.tabs.len() == 1 {
            self.activate(at);
        } else if at <= self.active {
            self.active += 1;
        }
        self.navigate_tab(at, url, false, referrer);
    }

    fn activate(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        self.clock += 1;
        let clock = self.clock;
        let viewport = self.viewport();
        let tab = &mut self.tabs[index];
        tab.last_shown = clock;
        tab.live = true;
        if let Some(viewport) = viewport {
            tab.send(Command::Paint { scroll_y: tab.scroll_y, viewport });
        }
        self.address = AddressBar { text: self.current_address(), focused: false, selected: false };
        self.suspend_old_tabs();
        self.redraw_later();
    }

    /// Keeps renderers only for the most recently shown tabs.
    fn suspend_old_tabs(&mut self) {
        let mut order: Vec<usize> = (0..self.tabs.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(self.tabs[i].last_shown));
        for &i in order.iter().skip(LIVE_RENDERERS) {
            let tab = &mut self.tabs[i];
            if tab.live && !tab.loading {
                tab.live = false;
                tab.send(Command::Suspend);
            }
        }
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(index);
        tab.send(Command::Shutdown);
        // The thread exits on its own once its current step finishes.
        drop(tab.thread);
        if self.tabs.is_empty() {
            // Closing the last tab never ends the session by accident.
            self.open_tab(Url::parse(START_URL).expect("valid URL"), None, true);
            return;
        }
        let next = if index < self.active || self.active >= self.tabs.len() {
            self.active.saturating_sub(1)
        } else {
            self.active
        };
        self.activate(next.min(self.tabs.len() - 1));
    }

    fn navigate_tab(&mut self, index: usize, url: Url, from_history: bool, referrer: Option<Url>) {
        let Some(tab) = self.tabs.get_mut(index) else { return };
        tab.status = format!("Loading {url} …");
        tab.loading = true;
        tab.live = true;
        tab.send(Command::Navigate { url, from_history, referrer });
        self.redraw_later();
    }

    fn navigate(&mut self, url: Url, from_history: bool) {
        self.navigate_tab(self.active, url, from_history, None);
    }

    fn submit_address(&mut self) {
        let input = self.address.text.trim().to_owned();
        let parsed = if input == START_URL {
            Url::parse(START_URL).map_err(|e| e.to_string())
        } else {
            policy::parse_input(&input).map_err(|e| e.to_string())
        };
        match parsed {
            Ok(url) => {
                self.address.focused = false;
                self.navigate(url, false);
            }
            Err(e) => {
                if let Some(t) = self.tab_mut() {
                    t.status = e;
                }
            }
        }
        self.redraw_later();
    }

    fn go_history(&mut self, delta: isize) {
        let Some(tab) = self.tab_mut() else { return };
        let Some(pos) = tab.history_pos.checked_add_signed(delta).filter(|p| *p < tab.history.len()) else { return };
        tab.history_pos = pos;
        let url = tab.history[pos].clone();
        self.navigate(url, true);
    }

    fn scroll_by(&mut self, dy: f32) {
        let Some(viewport) = self.viewport() else { return };
        let Some(tab) = self.tab_mut() else { return };
        let max = (tab.page_height - viewport.height).max(0.0);
        let next = (tab.scroll_y + dy).clamp(0.0, max);
        if next != tab.scroll_y {
            tab.scroll_y = next;
            tab.send(Command::Paint { scroll_y: next, viewport });
        }
    }

    fn repaint_page(&self) {
        if let (Some(viewport), Some(tab)) = (self.viewport(), self.tab()) {
            tab.send(Command::Paint { scroll_y: tab.scroll_y, viewport });
        }
    }

    fn current_address(&self) -> String {
        self.tab().and_then(|t| t.url.as_ref()).map(|u| u.to_string()).unwrap_or_default()
    }

    fn new_tab_with_address(&mut self) {
        self.open_tab(Url::parse(START_URL).expect("valid URL"), None, true);
        self.address = AddressBar { text: String::new(), focused: true, selected: false };
    }

    fn handle_key(&mut self, event: &KeyEvent) {
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let shift = self.modifiers.shift_key();
        match event.logical_key.as_ref() {
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("l") => {
                self.address = AddressBar { text: self.current_address(), focused: true, selected: true };
            }
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("t") => self.new_tab_with_address(),
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("w") => self.close_tab(self.active),
            Key::Named(NamedKey::Tab) if ctrl => {
                let n = self.tabs.len();
                if n > 1 {
                    self.activate(if shift { (self.active + n - 1) % n } else { (self.active + 1) % n });
                }
            }
            Key::Character(c) if ctrl && c.len() == 1 && ('1'..='9').contains(&c.chars().next().unwrap_or(' ')) => {
                let digit = c.chars().next().and_then(|d| d.to_digit(10)).unwrap_or(1) as usize;
                // Ctrl+9 is always the last tab.
                let index = if digit == 9 { self.tabs.len().saturating_sub(1) } else { digit - 1 };
                self.activate(index);
            }
            Key::Named(NamedKey::ArrowLeft) if alt => self.go_history(-1),
            Key::Named(NamedKey::ArrowRight) if alt => self.go_history(1),
            Key::Named(NamedKey::F5) => {
                if let Some(url) = self.tab().and_then(|t| t.url.clone()) {
                    self.navigate(url, true);
                }
            }
            _ if !self.address.focused && self.tab().is_some_and(|t| t.editing) => {
                if let Some(key) = page_key(event, ctrl, shift)
                    && let Some(tab) = self.tab()
                {
                    tab.send(Command::Key(key));
                }
            }
            _ if self.address.focused => match event.logical_key.as_ref() {
                Key::Named(NamedKey::Enter) => self.submit_address(),
                Key::Named(NamedKey::Escape) => {
                    self.address = AddressBar { text: self.current_address(), focused: false, selected: false };
                }
                Key::Named(NamedKey::Backspace) => {
                    if self.address.selected {
                        self.address.text.clear();
                    } else {
                        self.address.text.pop();
                    }
                    self.address.selected = false;
                }
                _ => {
                    if let Some(text) = event.text.as_ref().filter(|_| !ctrl) {
                        let typed: String = text.chars().filter(|c| !c.is_control()).collect();
                        if !typed.is_empty() {
                            if self.address.selected {
                                self.address.text.clear();
                                self.address.selected = false;
                            }
                            if self.address.text.len() + typed.len() <= 8192 {
                                self.address.text.push_str(&typed);
                            }
                        }
                    }
                }
            },
            Key::Named(NamedKey::PageDown | NamedKey::Space) => self.scroll_by(self.page_step()),
            Key::Named(NamedKey::PageUp) => self.scroll_by(-self.page_step()),
            Key::Named(NamedKey::ArrowDown) => self.scroll_by(LINE),
            Key::Named(NamedKey::ArrowUp) => self.scroll_by(-LINE),
            Key::Named(NamedKey::Home) => {
                let y = self.tab().map_or(0.0, |t| t.scroll_y);
                self.scroll_by(-y);
            }
            Key::Named(NamedKey::End) => {
                let h = self.tab().map_or(0.0, |t| t.page_height);
                self.scroll_by(h);
            }
            _ => {}
        }
        self.redraw_later();
    }

    fn page_step(&self) -> f32 {
        self.viewport().map_or(600.0, |v| v.height * 0.9)
    }

    fn click(&mut self, button: MouseButton) {
        let Some(window) = &self.window else { return };
        let g = Geometry::of(window);
        let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
        let middle = button == MouseButton::Middle;
        if cy < g.strip as f32 {
            let width_css = g.width as f32 / g.scale;
            match chrome::strip_hit(cx / g.scale, self.tabs.len(), width_css) {
                Some(chrome::StripHit::Tab(i)) if middle => self.close_tab(i),
                Some(chrome::StripHit::Tab(i)) => self.activate(i),
                Some(chrome::StripHit::Close(i)) => self.close_tab(i),
                Some(chrome::StripHit::NewTab) => self.new_tab_with_address(),
                None => {}
            }
        } else if cy < g.top as f32 {
            if !middle {
                self.address = AddressBar { text: self.current_address(), focused: true, selected: true };
            }
        } else {
            self.address.focused = false;
            let page_w = g.viewport.width * g.scale;
            let page_h = g.viewport.height * g.scale;
            let (px, py) = (cx - g.page_x as f32, cy - g.page_y as f32);
            let new_tab = middle || self.modifiers.control_key();
            if (0.0..page_w).contains(&px)
                && (0.0..page_h).contains(&py)
                && let Some(tab) = self.tab()
            {
                tab.send(Command::Click { x: px / g.scale, y: py / g.scale + tab.scroll_y, new_tab });
            }
        }
        self.redraw_later();
    }

    /// Re-renders the bars only when what they show has changed.
    fn update_bars(&mut self, g: &Geometry) {
        let width_css = g.width as f32 / g.scale;
        let labels: Vec<(String, bool)> = self.tabs.iter().map(|t| (t.label(), t.loading)).collect();
        let strip_key = format!("{labels:?}|{}|{}|{}", self.active, g.width, g.scale);
        let address = self.address_text();
        let top_key = format!("{address}|{}|{}|{}|{}", self.address.focused, self.address.selected, g.width, g.scale);
        let title = self.tab().and_then(|t| t.title.clone());
        let status = match self.tab() {
            Some(t) if !t.status.is_empty() => t.status.clone(),
            _ => self.net_status.clone(),
        };
        let bottom_key = format!("{title:?}|{status}|{}|{}", g.width, g.scale);
        let old = self.bars.take();
        let (strip, top, bottom) = match old {
            Some(b) => (
                (b.strip_key == strip_key).then_some(b.strip),
                (b.top_key == top_key).then_some(b.top),
                (b.bottom_key == bottom_key).then_some(b.bottom),
            ),
            None => (None, None, None),
        };
        let strip = strip.unwrap_or_else(|| {
            let tabs: Vec<chrome::TabLabel> =
                labels.iter().map(|(title, loading)| chrome::TabLabel { title, loading: *loading }).collect();
            chrome::tab_strip(&tabs, self.active, width_css, g.scale)
        });
        let top = top.unwrap_or_else(|| {
            chrome::top_bar(&address, self.address.focused, self.address.selected, width_css, g.scale)
        });
        let bottom = bottom.unwrap_or_else(|| chrome::status_bar(title.as_deref(), &status, width_css, g.scale));
        self.bars = Some(Bars { strip_key, strip, top_key, top, bottom_key, bottom });
    }

    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let g = Geometry::of(&window);
        self.update_bars(&g);
        let Some(surface) = self.surface.as_mut() else { return };
        let (Some(w), Some(h)) = (NonZeroU32::new(g.width), NonZeroU32::new(g.height)) else { return };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else { return };
        buf.fill(BACKGROUND);

        if let Some(b) = &self.bars {
            blit(&mut buf, g.width, g.height, &b.strip.rgba, b.strip.width, b.strip.height, 0, 0, g.strip);
            blit(&mut buf, g.width, g.height, &b.top.rgba, b.top.width, b.top.height, 0, g.strip, g.top - g.strip);
            let bottom_y = g.height.saturating_sub(g.bottom);
            blit(&mut buf, g.width, g.height, &b.bottom.rgba, b.bottom.width, b.bottom.height, 0, bottom_y, g.bottom);
        }
        if let Some(f) = self.tabs.get(self.active).and_then(|t| t.frame.as_ref()) {
            let room = g.height.saturating_sub(g.bottom).saturating_sub(g.page_y);
            blit(&mut buf, g.width, g.height, &f.rgba, f.width, f.height, g.page_x, g.page_y, room);
        }
        #[cfg(feature = "dev-dump")]
        dev_dump::write(&buf, g.width, g.height);
        let _ = buf.present();
    }

    fn address_text(&self) -> String {
        if self.address.focused { self.address.text.clone() } else { self.current_address() }
    }

    fn tab_event(&mut self, id: TabId, event: TabEvent) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else { return };
        let active = index == self.active;
        let tab = &mut self.tabs[index];
        match event {
            TabEvent::Status(s) => tab.status = s,
            TabEvent::Started => tab.loading = true,
            TabEvent::Finished => tab.loading = false,
            TabEvent::Editing(e) => tab.editing = e,
            TabEvent::Loading { url, from_history } => {
                if !from_history {
                    tab.history.truncate(tab.history_pos + 1);
                    if tab.history.last() != Some(&url) {
                        tab.history.push(url.clone());
                    }
                    tab.history_pos = tab.history.len() - 1;
                }
                tab.url = Some(url);
                tab.title = None;
                tab.scroll_y = 0.0;
                if active && !self.address.focused {
                    self.address = AddressBar { text: self.current_address(), focused: false, selected: false };
                }
            }
            TabEvent::Title(t) => tab.title = t,
            TabEvent::Frame(f) => {
                tab.scroll_y = f.scroll_y;
                tab.page_height = f.page_height;
                tab.frame = Some(f);
            }
            TabEvent::OpenInNewTab { url, referrer } => self.open_tab(url, Some(referrer), false),
        }
    }

    fn shut_down(&mut self) {
        for tab in self.tabs.drain(..) {
            tab.send(Command::Shutdown);
            let _ = tab.thread.join();
        }
        if let Some(net) = self.net.take() {
            net.shutdown();
        }
    }
}

/// A key for a focused page field, if it is one fields use. Browser
/// shortcuts were handled before this is asked.
fn page_key(event: &KeyEvent, ctrl: bool, shift: bool) -> Option<KeyInput> {
    Some(match event.logical_key.as_ref() {
        Key::Named(NamedKey::Backspace) => KeyInput::Backspace,
        Key::Named(NamedKey::Delete) => KeyInput::Delete,
        Key::Named(NamedKey::Enter) => KeyInput::Enter,
        Key::Named(NamedKey::ArrowLeft) => KeyInput::Left,
        Key::Named(NamedKey::ArrowRight) => KeyInput::Right,
        Key::Named(NamedKey::ArrowUp) => KeyInput::Up,
        Key::Named(NamedKey::ArrowDown) => KeyInput::Down,
        Key::Named(NamedKey::Home) => KeyInput::Home,
        Key::Named(NamedKey::End) => KeyInput::End,
        Key::Named(NamedKey::Tab) if shift => KeyInput::BackTab,
        Key::Named(NamedKey::Tab) => KeyInput::Tab,
        Key::Named(NamedKey::Escape) => KeyInput::Escape,
        _ if ctrl => return None,
        _ => {
            let text: String = event.text.as_ref()?.chars().filter(|c| !c.is_control()).take(256).collect();
            if text.is_empty() {
                return None;
            }
            KeyInput::Text(text)
        }
    })
}

/// Hollow's ring (UX-06) as the window icon, drawn here rather than shipped
/// as a file.
fn ring_icon() -> Option<Icon> {
    const N: u32 = 32;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
            let d = (dx * dx + dy * dy).sqrt();
            // Anti-aliased ring between radius 9 and 14.
            let a = ((d - 8.5).clamp(0.0, 1.0) * (14.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
            rgba.extend_from_slice(&[0x8e, 0xf0, 0xb4, a]);
        }
    }
    Icon::from_rgba(rgba, N, N).ok()
}

/// Development only, behind the off-by-default `dev-dump` feature: the window
/// is excluded from screen capture, so this is how a developer sees what it
/// draws. Writes each composed frame to `$HOLLOW_DEV_DUMP/frame-NNNN.png`.
/// Never part of a release or `cargo xtask dist` build.
///
/// `$HOLLOW_DEV_SCRIPT` drives the window without touching the desktop:
/// `wait:SECS;goto:URL;scroll:DY;click:X,Y;middle:X,Y;newtab;tab:N;closetab;quit`
/// (X, Y in window pixels), run through the same code paths as real input.
#[cfg(feature = "dev-dump")]
mod dev_dump {
    use std::sync::atomic::{AtomicU32, Ordering};

    use winit::event_loop::EventLoopProxy;

    use super::UiEvent;

    static N: AtomicU32 = AtomicU32::new(0);

    #[derive(Debug)]
    pub enum Step {
        Goto(String),
        Scroll(f32),
        Click(f64, f64),
        Middle(f64, f64),
        NewTab,
        Tab(usize),
        CloseTab,
        Quit,
    }

    pub fn start_script(proxy: EventLoopProxy<UiEvent>) {
        let Ok(script) = std::env::var("HOLLOW_DEV_SCRIPT") else { return };
        std::thread::spawn(move || {
            let point = |arg: &str| {
                let (x, y) = arg.split_once(',').unwrap_or(("0", "0"));
                (x.parse().unwrap_or(0.0), y.parse().unwrap_or(0.0))
            };
            for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                let (op, arg) = step.split_once(':').unwrap_or((step, ""));
                let action = match op {
                    "wait" => {
                        std::thread::sleep(std::time::Duration::from_secs_f32(arg.parse().unwrap_or(1.0)));
                        continue;
                    }
                    "goto" => Step::Goto(arg.to_owned()),
                    "scroll" => Step::Scroll(arg.parse().unwrap_or(0.0)),
                    "click" => {
                        let (x, y) = point(arg);
                        Step::Click(x, y)
                    }
                    "middle" => {
                        let (x, y) = point(arg);
                        Step::Middle(x, y)
                    }
                    "newtab" => Step::NewTab,
                    "tab" => Step::Tab(arg.parse().unwrap_or(0)),
                    "closetab" => Step::CloseTab,
                    "quit" => Step::Quit,
                    _ => continue,
                };
                if proxy.send_event(UiEvent::Dev(action)).is_err() {
                    break;
                }
            }
        });
    }

    pub fn write(buf: &[u32], w: u32, h: u32) {
        let Some(dir) = std::env::var_os("HOLLOW_DEV_DUMP") else { return };
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::path::Path::new(&dir).join(format!("frame-{n:04}.png"));
        let rgb: Vec<u8> = buf.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
        let Ok(file) = std::fs::File::create(path) else { return };
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        if let Ok(mut writer) = enc.write_header() {
            let _ = writer.write_image_data(&rgb);
        }
    }
}

/// Copies an RGBA image into the 0RGB surface at (x0, y0), at most
/// `max_rows` rows, clipped to the surface.
#[allow(clippy::too_many_arguments)]
fn blit(dst: &mut [u32], dw: u32, dh: u32, src: &[u8], sw: u32, sh: u32, x0: u32, y0: u32, max_rows: u32) {
    let rows = sh.min(max_rows).min(dh.saturating_sub(y0));
    let cols = sw.min(dw.saturating_sub(x0)) as usize;
    for row in 0..rows {
        let s = (row * sw) as usize * 4;
        let d = ((y0 + row) * dw + x0) as usize;
        let (Some(src_row), Some(dst_row)) = (src.get(s..s + cols * 4), dst.get_mut(d..d + cols)) else { return };
        for (px, out) in src_row.as_chunks::<4>().0.iter().zip(dst_row) {
            *out = (u32::from(px[0]) << 16) | (u32::from(px[1]) << 8) | u32::from(px[2]);
        }
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Hollow")
            .with_window_icon(ring_icon())
            .with_inner_size(LogicalSize::new(1240.0, 860.0))
            .with_min_inner_size(LogicalSize::new(480.0, 320.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                self.fatal = Some(anyhow::anyhow!("creating the window: {e}"));
                event_loop.exit();
                return;
            }
        };
        let capture_protected = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => hollow_sandbox::exclude_window_from_capture(h.hwnd.get()).is_ok(),
            _ => false,
        };
        let surface = softbuffer::Context::new(window.clone()).and_then(|ctx| Surface::new(&ctx, window.clone()));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                self.fatal = Some(anyhow::anyhow!("creating the drawing surface: {e}"));
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self.net = Some(NetSlot::start(self.proxy.clone()));
        let start = self.start.take().unwrap_or_else(|| Url::parse(START_URL).expect("valid URL"));
        self.open_tab(start, None, true);
        if !capture_protected && let Some(t) = self.tab_mut() {
            t.status = "Warning: this window could not be hidden from screen capture".into();
        }
        #[cfg(feature = "dev-dump")]
        dev_dump::start_script(self.proxy.clone());
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::NetStatus(s) => self.net_status = s,
            UiEvent::Tab(id, event) => self.tab_event(id, event),
            #[cfg(feature = "dev-dump")]
            UiEvent::Dev(step) => match step {
                dev_dump::Step::Goto(input) => {
                    self.address = AddressBar { text: input, focused: true, selected: false };
                    self.submit_address();
                }
                dev_dump::Step::Scroll(dy) => self.scroll_by(dy),
                dev_dump::Step::Click(x, y) => {
                    self.cursor = PhysicalPosition::new(x, y);
                    self.click(MouseButton::Left);
                }
                dev_dump::Step::Middle(x, y) => {
                    self.cursor = PhysicalPosition::new(x, y);
                    self.click(MouseButton::Middle);
                }
                dev_dump::Step::NewTab => self.new_tab_with_address(),
                dev_dump::Step::Tab(i) => self.activate(i),
                dev_dump::Step::CloseTab => self.close_tab(self.active),
                dev_dump::Step::Quit => _event_loop.exit(),
            },
        }
        self.redraw_later();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.repaint_page();
                self.redraw_later();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => self.cursor = position,
            WindowEvent::MouseInput { state: ElementState::Pressed, button, .. }
                if matches!(button, MouseButton::Left | MouseButton::Middle) =>
            {
                self.click(button)
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32);
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * LINE,
                    MouseScrollDelta::PixelDelta(p) => -(p.y as f32) / scale,
                };
                self.scroll_by(dy);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => self.handle_key(&event),
            _ => {}
        }
    }
}
