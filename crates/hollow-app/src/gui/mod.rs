//! The browser window (Phase 2).
//!
//! The broker owns the window and everything the user types into it. Pages
//! arrive as finished pixels from a sandboxed renderer (via the navigator
//! thread); the broker only composes them with its own bars and presents.
//!
//! Privacy details that live here:
//! - The window is excluded from screen capture and Recall (WIN-01).
//! - The OS-visible window title is always "Hollow": page titles never reach
//!   the taskbar, Alt-Tab or anything else that records window titles.
//! - The page area snaps to fixed sizes (letterboxing, FP-02), so the
//!   window size never becomes something a page could measure.
//! - History lives in RAM only.

mod chrome;
mod navigator;

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use anyhow::Context;
use hollow_ipc::{Frame, Viewport};
use softbuffer::Surface;
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

use crate::policy;
use navigator::{Command, START_URL, UiEvent};

/// Letterbox margins.
const BACKGROUND: u32 = 0x1f1f25;
/// CSS px scrolled per wheel notch or arrow key.
const LINE: f32 = 48.0;

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
        let top = (chrome::TOP_BAR * scale).round() as u32;
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

struct App {
    proxy: EventLoopProxy<UiEvent>,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    nav: Option<(Sender<Command>, JoinHandle<()>)>,
    fatal: Option<anyhow::Error>,
    start: Option<Url>,

    url: Option<Url>,
    title: Option<String>,
    status: String,
    frame: Option<Frame>,
    scroll_y: f32,
    page_height: f32,
    history: Vec<Url>,
    history_pos: usize,

    address: AddressBar,
    modifiers: ModifiersState,
    cursor: PhysicalPosition<f64>,
    bars: Option<(String, chrome::Bar, String, chrome::Bar)>,
}

impl App {
    fn new(proxy: EventLoopProxy<UiEvent>) -> App {
        App {
            proxy,
            window: None,
            surface: None,
            nav: None,
            fatal: None,
            start: None,
            url: None,
            title: None,
            status: String::new(),
            frame: None,
            scroll_y: 0.0,
            page_height: 0.0,
            history: Vec::new(),
            history_pos: 0,
            address: AddressBar::default(),
            modifiers: ModifiersState::empty(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            bars: None,
        }
    }

    fn send(&self, cmd: Command) {
        if let Some((tx, _)) = &self.nav {
            let _ = tx.send(cmd);
        }
    }

    fn redraw_later(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn navigate(&mut self, url: Url, from_history: bool) {
        self.status = format!("Loading {url} …");
        self.send(Command::Navigate { url, from_history });
        self.redraw_later();
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
            Err(e) => self.status = e,
        }
        self.redraw_later();
    }

    fn go_history(&mut self, delta: isize) {
        let Some(pos) = self.history_pos.checked_add_signed(delta).filter(|p| *p < self.history.len()) else { return };
        self.history_pos = pos;
        let url = self.history[pos].clone();
        self.navigate(url, true);
    }

    fn scroll_by(&mut self, dy: f32) {
        let Some(window) = &self.window else { return };
        let viewport = Geometry::of(window).viewport;
        let max = (self.page_height - viewport.height).max(0.0);
        let next = (self.scroll_y + dy).clamp(0.0, max);
        if next != self.scroll_y {
            self.scroll_y = next;
            self.send(Command::Paint { scroll_y: next, viewport });
        }
    }

    fn repaint_page(&self) {
        if let Some(window) = &self.window {
            self.send(Command::Paint { scroll_y: self.scroll_y, viewport: Geometry::of(window).viewport });
        }
    }

    fn current_address(&self) -> String {
        self.url.as_ref().map(|u| u.to_string()).unwrap_or_default()
    }

    fn handle_key(&mut self, event: &KeyEvent) {
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        match event.logical_key.as_ref() {
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("l") => {
                self.address = AddressBar { text: self.current_address(), focused: true, selected: true };
            }
            Key::Named(NamedKey::ArrowLeft) if alt => self.go_history(-1),
            Key::Named(NamedKey::ArrowRight) if alt => self.go_history(1),
            Key::Named(NamedKey::F5) => {
                if let Some(url) = self.url.clone() {
                    self.navigate(url, true);
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
            Key::Named(NamedKey::Home) => self.scroll_by(-self.scroll_y),
            Key::Named(NamedKey::End) => self.scroll_by(self.page_height),
            _ => {}
        }
        self.redraw_later();
    }

    fn page_step(&self) -> f32 {
        self.window.as_ref().map_or(600.0, |w| Geometry::of(w).viewport.height * 0.9)
    }

    fn click(&mut self) {
        let Some(window) = &self.window else { return };
        let g = Geometry::of(window);
        let (cx, cy) = (self.cursor.x as f32, self.cursor.y as f32);
        if cy < g.top as f32 {
            self.address = AddressBar { text: self.current_address(), focused: true, selected: true };
        } else {
            self.address.focused = false;
            let page_w = g.viewport.width * g.scale;
            let page_h = g.viewport.height * g.scale;
            let (px, py) = (cx - g.page_x as f32, cy - g.page_y as f32);
            if (0.0..page_w).contains(&px) && (0.0..page_h).contains(&py) {
                self.send(Command::Click { x: px / g.scale, y: py / g.scale + self.scroll_y });
            }
        }
        self.redraw_later();
    }

    /// Re-renders the bars only when what they show has changed.
    fn update_bars(&mut self, g: &Geometry) {
        let address = self.address_text();
        let top_key = format!("{address}|{}|{}|{}|{}", self.address.focused, self.address.selected, g.width, g.scale);
        let bottom_key = format!("{:?}|{}|{}|{}", self.title, self.status, g.width, g.scale);
        if self.bars.as_ref().is_some_and(|(tk, _, bk, _)| *tk == top_key && *bk == bottom_key) {
            return;
        }
        let width_css = g.width as f32 / g.scale;
        let top = chrome::top_bar(&address, self.address.focused, self.address.selected, width_css, g.scale);
        let bottom = chrome::status_bar(self.title.as_deref(), &self.status, width_css, g.scale);
        self.bars = Some((top_key, top, bottom_key, bottom));
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

        if let Some((_, top, _, bottom)) = &self.bars {
            blit(&mut buf, g.width, g.height, &top.rgba, top.width, top.height, 0, 0, g.top);
            let bottom_y = g.height.saturating_sub(g.bottom);
            blit(&mut buf, g.width, g.height, &bottom.rgba, bottom.width, bottom.height, 0, bottom_y, g.bottom);
        }
        if let Some(f) = &self.frame {
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

    fn shut_down(&mut self) {
        if let Some((tx, handle)) = self.nav.take() {
            let _ = tx.send(Command::Shutdown);
            let _ = handle.join();
        }
    }
}

/// Development only, behind the off-by-default `dev-dump` feature: the window
/// is excluded from screen capture, so this is how a developer sees what it
/// draws. Writes each composed frame to `$HOLLOW_DEV_DUMP/frame-NNNN.png`.
/// Never part of a release or `cargo xtask dist` build.
///
/// `$HOLLOW_DEV_SCRIPT` drives the window without touching the desktop:
/// `wait:SECS;goto:URL;scroll:DY;click:X,Y` (X, Y in window pixels), run
/// through the same code paths as real input.
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
        Quit,
    }

    pub fn start_script(proxy: EventLoopProxy<UiEvent>) {
        let Ok(script) = std::env::var("HOLLOW_DEV_SCRIPT") else { return };
        std::thread::spawn(move || {
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
                        let (x, y) = arg.split_once(',').unwrap_or(("0", "0"));
                        Step::Click(x.parse().unwrap_or(0.0), y.parse().unwrap_or(0.0))
                    }
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
        if !capture_protected {
            self.status = "Warning: this window could not be hidden from screen capture".into();
        }
        let (tx, handle) = navigator::spawn(self.proxy.clone(), Geometry::of(&window).viewport);
        self.window = Some(window);
        self.nav = Some((tx, handle));
        let start = self.start.take().unwrap_or_else(|| Url::parse(START_URL).expect("valid URL"));
        self.navigate(start, false);
        #[cfg(feature = "dev-dump")]
        dev_dump::start_script(self.proxy.clone());
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Status(s) => self.status = s,
            UiEvent::Loading { url, from_history } => {
                if !from_history {
                    self.history.truncate(self.history_pos + 1);
                    if self.history.last() != Some(&url) {
                        self.history.push(url.clone());
                    }
                    self.history_pos = self.history.len() - 1;
                }
                self.url = Some(url);
                self.title = None;
                self.scroll_y = 0.0;
            }
            UiEvent::Title(t) => self.title = t,
            UiEvent::Frame(f) => {
                self.scroll_y = f.scroll_y;
                self.page_height = f.page_height;
                self.frame = Some(f);
            }
            #[cfg(feature = "dev-dump")]
            UiEvent::Dev(step) => match step {
                dev_dump::Step::Goto(input) => {
                    self.address = AddressBar { text: input, focused: true, selected: false };
                    self.submit_address();
                }
                dev_dump::Step::Scroll(dy) => self.scroll_by(dy),
                dev_dump::Step::Click(x, y) => {
                    self.cursor = PhysicalPosition::new(x, y);
                    self.click();
                }
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
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => self.click(),
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
