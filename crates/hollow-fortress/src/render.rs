//! Phase 1 "rendering": parse the HTML and extract its title, visible text
//! and links. Script contents are never touched, let alone run.

use hollow_ipc::RenderedPage;
use scraper::{ElementRef, Html, Node, Selector};

const MAX_TEXT: usize = 512 * 1024;
const MAX_LINKS: usize = 5_000;

/// Elements whose content is never shown. `<noscript>` is *not* here:
/// Fortress has no JavaScript, so noscript content is what the page wants
/// shown.
const HIDDEN: &[&str] = &["script", "style", "template", "head", "svg", "iframe", "object", "embed"];

pub fn render(_url: &str, content_type: Option<&str>, body: &[u8]) -> RenderedPage {
    let is_html = content_type.is_none_or(|ct| {
        let ct = ct.to_ascii_lowercase();
        ct.starts_with("text/html") || ct.starts_with("application/xhtml")
    });
    let source = String::from_utf8_lossy(body);
    if !is_html {
        return RenderedPage { title: None, text: truncate(collapse_whitespace(&source), MAX_TEXT), links: vec![] };
    }

    let doc = Html::parse_document(&source);
    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| doc.select(&sel).next())
        .map(|t| truncate(collapse_whitespace(&t.text().collect::<String>()), 512))
        .filter(|t| !t.is_empty());

    let mut text = String::new();
    collect_text(doc.root_element(), &mut text);

    let links = Selector::parse("a[href]")
        .map(|sel| {
            doc.select(&sel)
                .filter_map(|a| a.value().attr("href"))
                .map(|h| h.trim())
                .filter(|h| !h.is_empty() && h.len() <= 4096)
                .take(MAX_LINKS)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    RenderedPage { title, text: truncate(collapse_whitespace(&text), MAX_TEXT), links }
}

fn collect_text(el: ElementRef<'_>, out: &mut String) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) if HIDDEN.contains(&e.name()) => {}
            Node::Element(e) => {
                let block = matches!(
                    e.name(),
                    "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "section" | "article"
                );
                if block {
                    out.push('\n');
                }
                if let Some(child_el) = ElementRef::wrap(child) {
                    collect_text(child_el, out);
                }
                if block {
                    out.push('\n');
                }
            }
            _ => {}
        }
    }
}

/// Collapses runs of spaces within lines and drops blank lines.
fn collapse_whitespace(s: &str) -> String {
    s.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_title_text_and_links_but_never_scripts() {
        let html = br#"<html><head><title> Hello  Hollow </title><script>steal()</script></head>
            <body><h1>Heading</h1><p>Some <b>bold</b> text.</p><script>alert(1)</script>
            <noscript>No JS here</noscript><a href="/next">next</a><a href="">empty</a></body></html>"#;
        let page = render("https://example.org/", Some("text/html; charset=utf-8"), html);
        assert_eq!(page.title.as_deref(), Some("Hello Hollow"));
        assert!(page.text.contains("Heading"));
        assert!(page.text.contains("Some bold text."));
        assert!(page.text.contains("No JS here"));
        assert!(!page.text.contains("steal"));
        assert!(!page.text.contains("alert"));
        assert_eq!(page.links, vec!["/next".to_string()]);
    }

    #[test]
    fn plain_text_is_passed_through() {
        let page = render("https://example.org/a.txt", Some("text/plain"), b"line one\n\n  line   two ");
        assert_eq!(page.text, "line one\nline two");
        assert!(page.title.is_none());
    }
}
