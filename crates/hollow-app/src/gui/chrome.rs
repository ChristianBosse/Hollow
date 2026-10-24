//! The browser's own bars, drawn with the same engine as pages, from small
//! HTML snippets the broker writes itself (never from web content).

use hollow_engine::{Document, Layout, Resources};
use url::Url;

use super::navigator::html_escape;

/// Heights in CSS px.
pub const TOP_BAR: f32 = 42.0;
pub const STATUS_BAR: f32 = 24.0;

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
