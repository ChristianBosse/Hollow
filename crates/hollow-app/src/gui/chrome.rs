//! The browser's own bars, drawn with the same engine as pages, from small
//! HTML snippets the broker writes itself (never from web content).

use hollow_engine::{Document, Layout, Resources};
use url::Url;

use super::navigator::html_escape;

/// Heights in CSS px.
pub const TAB_STRIP: f32 = 34.0;
pub const TOP_BAR: f32 = 42.0;
pub const STATUS_BAR: f32 = 24.0;

/// Tab strip geometry in CSS px, shared by drawing and hit testing so the two
/// can't disagree.
const STRIP_LEFT: f32 = 34.0;
const NEW_TAB_W: f32 = 34.0;
const TAB_MIN_W: f32 = 64.0;
const TAB_MAX_W: f32 = 220.0;
const CLOSE_W: f32 = 22.0;

/// What a tab strip shows for one tab.
pub struct TabLabel<'a> {
    pub title: &'a str,
    pub loading: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripHit {
    Tab(usize),
    Close(usize),
    NewTab,
}

fn tab_width(count: usize, width: f32) -> f32 {
    ((width - STRIP_LEFT - NEW_TAB_W - 8.0) / count.max(1) as f32).clamp(TAB_MIN_W, TAB_MAX_W)
}

/// What's under `x` (CSS px) in a strip of `count` tabs.
pub fn strip_hit(x: f32, count: usize, width: f32) -> Option<StripHit> {
    let w = tab_width(count, width);
    let rel = x - STRIP_LEFT;
    if rel < 0.0 {
        return None;
    }
    let i = (rel / w) as usize;
    if i < count {
        let into = rel - i as f32 * w;
        return Some(if into >= w - CLOSE_W - 4.0 { StripHit::Close(i) } else { StripHit::Tab(i) });
    }
    (rel - count as f32 * w < NEW_TAB_W).then_some(StripHit::NewTab)
}

/// The tab strip, with Hollow's ring at the left (UX-06).
pub fn tab_strip(tabs: &[TabLabel], active: usize, width: f32, scale: f32) -> Bar {
    let w = tab_width(tabs.len(), width);
    let mut html = String::from(
        "<html><body style=\"margin:0;background-color:#0b0b0e;font-family:sans-serif\">         <div style=\"position:absolute;left:10px;top:10px;width:10px;height:10px;border:3px solid #8ef0b4;border-radius:50%\"></div>",
    );
    for (i, tab) in tabs.iter().enumerate() {
        let x = STRIP_LEFT + i as f32 * w;
        let (bg, fg) = if i == active { ("#141418", "#ececf1") } else { ("#18181d", "#8a8a99") };
        let title: String = tab.title.chars().take(80).collect();
        let dot = if tab.loading { "<span style=\"color:#8ef0b4\">● </span>" } else { "" };
        html += &format!(
            "<div style=\"position:absolute;left:{x}px;top:5px;width:{tw}px;height:29px;background-color:{bg};             border-radius:7px 7px 0 0;overflow:hidden\">             <div style=\"position:absolute;left:10px;top:8px;width:{lw}px;height:16px;overflow:hidden;white-space:nowrap;             font-size:12px;color:{fg}\">{dot}{}</div>             <div style=\"position:absolute;right:8px;top:6px;font-size:13px;color:#8a8a99\">×</div></div>",
            html_escape(&title),
            tw = w - 4.0,
            lw = (w - 4.0 - 10.0 - CLOSE_W - 6.0).max(0.0),
        );
    }
    let plus = STRIP_LEFT + tabs.len() as f32 * w;
    html += &format!(
        "<div style=\"position:absolute;left:{plus}px;top:7px;width:26px;height:22px;font-size:17px;color:#8a8a99;         text-align:center\">+</div></body></html>"
    );
    paint(&html, width, TAB_STRIP, scale)
}

pub struct Bar {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn top_bar(address: &str, focused: bool, selected: bool, width: f32, scale: f32) -> Bar {
    let mut shown: String = address.chars().take(240).collect();
    if focused && !selected {
        shown.push('|');
    }
    let address_style = if selected {
        "background-color:#3b5bdb;color:#ffffff"
    } else if focused {
        "background-color:#2b2b33;color:#ffffff"
    } else {
        "color:#ececf1"
    };
    let html = format!(
        "<html><body style=\"margin:0;background-color:#141418\">\
         <div style=\"padding:11px 14px;font-size:14px;font-family:sans-serif;color:#ececf1;background-color:#141418\">\
         <b style=\"color:#8ef0b4\">TOR</b> <span style=\"color:#8a8a99\">FORTRESS</span> &nbsp; \
         <span style=\"{address_style}\">{}</span></div></body></html>",
        html_escape(&shown)
    );
    paint(&html, width, TOP_BAR, scale)
}

pub fn status_bar(title: Option<&str>, status: &str, width: f32, scale: f32) -> Bar {
    let title: String = title.unwrap_or("").chars().take(120).collect();
    let status: String = status.chars().take(200).collect();
    let sep = if title.is_empty() { "" } else { " &nbsp;·&nbsp; " };
    let html = format!(
        "<html><body style=\"margin:0;background-color:#141418\">\
         <div style=\"padding:4px 14px;font-size:12px;font-family:sans-serif;color:#a9a9b8;background-color:#141418\">\
         <b style=\"color:#ececf1\">{}</b>{sep}{}</div></body></html>",
        html_escape(&title),
        html_escape(&status)
    );
    paint(&html, width, STATUS_BAR, scale)
}

fn paint(html: &str, width: f32, height: f32, scale: f32) -> Bar {
    let base = Url::parse("about:hollow").expect("valid URL");
    let doc = Document::parse(html, &base);
    let layout = Layout::new(&doc, &Resources::new(), width.max(200.0));
    let f = layout.paint(0.0, height, scale);
    Bar { width: f.width, height: f.height, rgba: f.rgba }
}
