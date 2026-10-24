//! Parsing HTML into the engine's own flat DOM.
//!
//! html5ever does the spec-compliant parsing (with scripting *disabled*, so
//! `<noscript>` content is parsed as real markup). We then copy only what can
//! be shown into an arena: scripts, templates, embedded documents and other
//! inert content never make it in, so later stages can't touch them by
//! accident. Depth, node count and text size are capped here, which bounds
//! the recursion and work of every later stage.

use std::collections::BTreeSet;
use std::sync::Arc;

use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::{QuirksMode, TreeBuilderOpts};
use html5ever::{ParseOpts, driver};
use scraper::{Html, HtmlTreeSink, Node as HtmlNode};
use url::Url;

pub(crate) type NodeId = usize;

/// Deeper elements are flattened: their content is kept, attached to the
/// deepest allowed ancestor, but the elements themselves are dropped.
pub(crate) const MAX_DEPTH: usize = 256;
const MAX_NODES: usize = 200_000;
const MAX_TEXT_BYTES: usize = 4 << 20;
const MAX_ATTRS: usize = 64;
const MAX_ATTR_LEN: usize = 16 * 1024;
const MAX_CLASSES: usize = 32;
const MAX_URL_LEN: usize = 8 * 1024;
const MAX_TITLE_CHARS: usize = 512;
/// Images a page may ask the broker for.
pub(crate) const MAX_IMAGES: usize = 64;

#[derive(Clone, Debug)]
pub(crate) struct Element {
    /// Lowercase local name.
    pub name: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attrs: Vec<(String, String)>,
    pub has_href: bool,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug)]
pub(crate) enum NodeData {
    Root,
    Element(Element),
    Text(String),
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub data: NodeData,
    /// Resolved `href` (links), `src` (images), `action` (forms) or
    /// `formaction` (submit buttons).
    pub url: Option<Url>,
}

/// Nodes in document order; node 0 is the root.
#[derive(Clone, Debug)]
pub(crate) struct Dom {
    pub nodes: Vec<Node>,
    /// The page has no (or an ancient) doctype, so it expects the old
    /// rendering quirks.
    pub quirks: bool,
    /// What relative URLs in the page resolve against (`<base>` included).
    pub base: Arc<Url>,
}

impl Dom {
    pub fn element(&self, id: NodeId) -> Option<&Element> {
        match &self.nodes.get(id)?.data {
            NodeData::Element(e) => Some(e),
            _ => None,
        }
    }
}

pub(crate) struct Parsed {
    pub dom: Dom,
    pub title: Option<String>,
    /// `<style>` and `<link rel=stylesheet>` elements, in document order.
    pub sheets: Vec<SheetSource>,
    pub images: Vec<Url>,
    /// Stylesheets to fetch: every `<link rel~=stylesheet>`, whatever its
    /// `media`, so the fetch set never depends on the viewport.
    pub stylesheet_urls: Vec<Url>,
}

/// Where a stylesheet sits in the cascade.
pub(crate) enum SheetSource {
    Inline { css: String, media: Option<String> },
    Linked { url: Url, media: Option<String> },
}

/// Stylesheets a page may ask the broker for.
pub(crate) const MAX_STYLESHEETS: usize = 32;
/// `<style>` + `<link>` elements considered for the cascade.
const MAX_SHEET_SOURCES: usize = 256;

/// Elements whose whole subtree is dropped. `<noscript>` is deliberately
/// absent: Fortress has no JavaScript, so noscript content is what the page
/// wants shown.
const DROPPED: &[&str] = &[
    "script", "template", "iframe", "embed", "svg", "noembed", "noframes", "frameset", "frame", "param", "source",
    "track", "link", "meta", "area", "datalist", "base", "slot", "applet",
];

enum Target {
    /// Append to this node.
    Node(NodeId),
    /// Not shown (inside `<head>`), but `<title>`, `<style>` and `<base>`
    /// still count.
    Hidden,
}

/// Source documents nested deeper than this (by a cheap tag count) are cut
/// off there. html5ever's tree builder is quadratic in nesting depth for
/// common tags, so 100k nested `<div>`s would otherwise take many seconds.
const MAX_SOURCE_DEPTH: usize = 4096;
/// Larger documents are truncated before parsing.
const MAX_SOURCE_BYTES: usize = 16 << 20;

/// Returns the prefix of `html` whose approximate element nesting stays
/// within [`MAX_SOURCE_DEPTH`]. Only a safety valve: elements that HTML
/// closes implicitly (`<p>`, `<li>`, `<td>`, ...) and void elements are not
/// counted, so ordinary pages are never affected.
fn limit_nesting(html: &str) -> &str {
    const VOID_OR_IMPLIED: &[&str] = &[
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr", "param",
        "keygen", "p", "li", "dt", "dd", "option", "optgroup", "tr", "td", "th", "tbody", "thead", "tfoot", "colgroup",
        "rb", "rt", "rp", "html", "head", "body", "!doctype",
    ];
    const RAW: &[&str] = &["script", "style", "textarea", "title", "xmp", "plaintext", "noembed", "noframes", "iframe"];
    let b = html.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if html[i..].starts_with("<!--") {
            i = html[i + 4..].find("-->").map_or(b.len(), |e| i + 4 + e + 3);
            continue;
        }
        let closing = b.get(i + 1) == Some(&b'/');
        let start = i + 1 + closing as usize;
        let mut end = start;
        while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'!' || b[end] == b'-') {
            end += 1;
        }
        if end == start || !b[start].is_ascii_alphabetic() && b[start] != b'!' {
            i += 1;
            continue;
        }
        let name = html[start..end].to_ascii_lowercase();
        let tag_end = html[end..].find('>').map_or(b.len(), |e| end + e + 1);
        let self_closing = tag_end >= 2 && b.get(tag_end - 2) == Some(&b'/');
        if closing {
            if !VOID_OR_IMPLIED.contains(&name.as_str()) {
                depth = depth.saturating_sub(1);
            }
        } else if RAW.contains(&name.as_str()) {
            // Skip raw text so markup-looking content inside is not counted.
            let close = format!("</{name}");
            let from = tag_end.min(b.len());
            i = b[from..]
                .windows(close.len())
                .position(|w| w.eq_ignore_ascii_case(close.as_bytes()))
                .map_or(b.len(), |e| from + e);
            continue;
        } else if !self_closing && !VOID_OR_IMPLIED.contains(&name.as_str()) {
            depth += 1;
            if depth > MAX_SOURCE_DEPTH {
                return &html[..i];
            }
        }
        i = tag_end;
    }
    html
}

pub(crate) fn parse(html: &str, base_url: &Url) -> Parsed {
    let html = limit_nesting(truncate(html, MAX_SOURCE_BYTES));
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts { scripting_enabled: false, ..Default::default() },
        ..Default::default()
    };
    let parsed: Html = driver::parse_document(HtmlTreeSink::new(Html::new_document()), opts).one(html);

    let mut dom = Dom {
        nodes: vec![Node { parent: None, children: Vec::new(), data: NodeData::Root, url: None }],
        quirks: parsed.quirks_mode == QuirksMode::Quirks,
        base: Arc::new(base_url.clone()),
    };
    let mut base = base_url.clone();
    let mut base_seen = false;
    let mut title: Option<String> = None;
    let mut sheets = Vec::new();
    let mut stylesheet_urls: Vec<Url> = Vec::new();
    let mut images = Vec::new();
    let mut image_set = BTreeSet::new();
    let mut text_bytes = 0usize;

    // Explicit stack instead of recursion: html5ever accepts arbitrarily deep
    // documents and we must not overflow ours.
    let mut stack: Vec<(ego_tree::NodeRef<'_, HtmlNode>, Target, usize)> = Vec::new();
    for child in parsed.tree.root().children().rev() {
        stack.push((child, Target::Node(0), 1));
    }
    while let Some((node, target, depth)) = stack.pop() {
        if dom.nodes.len() >= MAX_NODES {
            break;
        }
        match node.value() {
            HtmlNode::Text(t) => {
                let Target::Node(parent) = target else { continue };
                if text_bytes >= MAX_TEXT_BYTES {
                    continue;
                }
                let s = truncate(t, MAX_TEXT_BYTES - text_bytes);
                text_bytes += s.len();
                // Flattening can make text nodes adjacent; merge them.
                if let Some(&last) = dom.nodes[parent].children.last()
                    && let NodeData::Text(prev) = &mut dom.nodes[last].data
                {
                    prev.push_str(s);
                    continue;
                }
                push(&mut dom, parent, NodeData::Text(s.to_owned()), None);
            }
            HtmlNode::Element(e) => {
                let name = e.name().to_ascii_lowercase();
                match name.as_str() {
                    "title" => {
                        if title.is_none() {
                            let t = collapse_whitespace(&node.descendants().filter_map(text_of).collect::<String>());
                            if !t.is_empty() {
                                title = Some(t.chars().take(MAX_TITLE_CHARS).collect());
                            }
                        }
                        continue;
                    }
                    "style" => {
                        let ty = e.attr("type").map(|t| t.trim().to_ascii_lowercase());
                        if ty.as_deref().is_none_or(|t| t.is_empty() || t == "text/css")
                            && sheets.len() < MAX_SHEET_SOURCES
                        {
                            let css = node.children().filter_map(text_of).collect::<String>();
                            sheets.push(SheetSource::Inline { css, media: e.attr("media").map(str::to_owned) });
                        }
                        continue;
                    }
                    "link" => {
                        let rel = e.attr("rel").unwrap_or("").to_ascii_lowercase();
                        let mut rel = rel.split_ascii_whitespace();
                        let is_sheet = rel.clone().any(|r| r == "stylesheet");
                        // Alternate sheets are fetched (the fetch set must not depend
                        // on page details either) but not applied, as in browsers.
                        let alternate = rel.any(|r| r == "alternate");
                        // A `data:` sheet is part of the page: decoded here,
                        // never fetched.
                        if is_sheet
                            && !alternate
                            && sheets.len() < MAX_SHEET_SOURCES
                            && let Some(href) = e.attr("href").map(str::trim)
                            && href.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("data:"))
                        {
                            if let Some((mime, bytes)) = crate::data_url::decode(href)
                                && (mime.is_empty() || mime == "text/css")
                            {
                                let css = String::from_utf8_lossy(&bytes).into_owned();
                                sheets.push(SheetSource::Inline { css, media: e.attr("media").map(str::to_owned) });
                            }
                            continue;
                        }
                        if is_sheet
                            && let Some(url) = e
                                .attr("href")
                                .and_then(|h| resolve(&base, h))
                                .filter(|u| matches!(u.scheme(), "http" | "https"))
                        {
                            if !stylesheet_urls.contains(&url) && stylesheet_urls.len() < MAX_STYLESHEETS {
                                stylesheet_urls.push(url.clone());
                            }
                            if !alternate && sheets.len() < MAX_SHEET_SOURCES {
                                sheets.push(SheetSource::Linked { url, media: e.attr("media").map(str::to_owned) });
                            }
                        }
                        continue;
                    }
                    "base" => {
                        if !base_seen && let Some(href) = e.attr("href") {
                            base_seen = true;
                            if let Some(u) = resolve(&base, href).filter(|u| matches!(u.scheme(), "http" | "https")) {
                                base = u;
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
                if DROPPED.contains(&name.as_str())
                    || e.attr("hidden").is_some()
                    || (name == "dialog" && e.attr("open").is_none())
                {
                    continue;
                }
                let target = match target {
                    _ if name == "head" => Target::Hidden,
                    Target::Hidden => Target::Hidden,
                    Target::Node(parent) if depth > MAX_DEPTH => Target::Node(parent),
                    Target::Node(parent) => {
                        let el = element(&name, e.attrs());
                        let url = match name.as_str() {
                            "a" | "area" => e.attr("href").and_then(|h| resolve(&base, h)),
                            "img" => image_source(e).and_then(|s| resolve(&base, s)),
                            // Form targets; an empty `action` means the page itself.
                            "form" => e.attr("action").and_then(|h| resolve(&base, h)),
                            "button" | "input" => e.attr("formaction").and_then(|h| resolve(&base, h)),
                            // Shown as an image when it is one; otherwise its
                            // children are the fallback.
                            "object" => e.attr("data").and_then(|s| resolve(&base, s)),
                            _ => None,
                        };
                        let image_type = || {
                            e.attr("type")
                                .is_some_and(|t| t.trim().get(..6).is_some_and(|p| p.eq_ignore_ascii_case("image/")))
                        };
                        if (name == "img" || (name == "object" && image_type()))
                            && let Some(u) = &url
                            && matches!(u.scheme(), "http" | "https")
                            && images.len() < MAX_IMAGES
                            && image_set.insert(u.as_str().to_owned())
                        {
                            images.push(u.clone());
                        }
                        Target::Node(push(&mut dom, parent, NodeData::Element(el), url))
                    }
                };
                // A closed <details> shows only its summary.
                let closed_details = name == "details" && e.attr("open").is_none();
                for child in node.children().rev() {
                    if closed_details
                        && !matches!(child.value(), HtmlNode::Element(c) if c.name().eq_ignore_ascii_case("summary"))
                    {
                        continue;
                    }
                    let t = match &target {
                        Target::Node(n) => Target::Node(*n),
                        Target::Hidden => Target::Hidden,
                    };
                    stack.push((child, t, depth + 1));
                }
            }
            _ => {}
        }
    }
    dom.base = Arc::new(base);
    Parsed { dom, title, sheets, images, stylesheet_urls }
}

fn push(dom: &mut Dom, parent: NodeId, data: NodeData, url: Option<Url>) -> NodeId {
    let id = dom.nodes.len();
    dom.nodes.push(Node { parent: Some(parent), children: Vec::new(), data, url });
    dom.nodes[parent].children.push(id);
    id
}

fn text_of(n: ego_tree::NodeRef<'_, HtmlNode>) -> Option<&str> {
    match n.value() {
        HtmlNode::Text(t) => Some(t),
        _ => None,
    }
}

fn element<'a>(name: &str, attrs: impl Iterator<Item = (&'a str, &'a str)>) -> Element {
    let mut el = Element { name: name.to_owned(), id: None, classes: Vec::new(), attrs: Vec::new(), has_href: false };
    for (n, v) in attrs.take(MAX_ATTRS) {
        let n = n.to_ascii_lowercase();
        let v = truncate(v, MAX_ATTR_LEN);
        match n.as_str() {
            "id" if !v.is_empty() => el.id = Some(v.to_owned()),
            "class" => el.classes = v.split_ascii_whitespace().take(MAX_CLASSES).map(str::to_owned).collect(),
            "href" => el.has_href = true,
            _ => {}
        }
        el.attrs.push((n, v.to_owned()));
    }
    el
}

/// `src`, or the first candidate of `srcset` when there is no `src`.
fn image_source(e: &scraper::node::Element) -> Option<&str> {
    match e.attr("src").map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(s),
        None => e.attr("srcset")?.split(',').next()?.split_ascii_whitespace().next(),
    }
}

/// Resolves a URL from the page. `javascript:` URLs resolve to nothing:
/// there is nothing that could ever run them.
fn resolve(base: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    let data = href.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("data:"));
    let max = if data { crate::data_url::MAX_DATA_URL_BYTES } else { MAX_URL_LEN };
    if href.is_empty() || href.len() > max {
        return None;
    }
    let url = base.join(href).ok()?;
    (url.scheme() != "javascript").then_some(url)
}

pub(crate) fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

pub(crate) fn collapse_whitespace(s: &str) -> String {
    s.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.org/dir/page.html").unwrap()
    }

    fn all_text(dom: &Dom) -> String {
        dom.nodes
            .iter()
            .filter_map(|n| match &n.data {
                NodeData::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn drops_scripts_keeps_noscript() {
        let p = parse(
            "<html><head><title> A  title </title><script>evil()</script><style>p{color:red}</style></head>
             <body><script>alert(1)</script><noscript><p>No <b>JS</b></p></noscript><template>tpl</template>
             <p hidden>hidden</p><iframe src=x>frame</iframe><svg><text>svg</text></svg>visible</body></html>",
            &base(),
        );
        assert_eq!(p.title.as_deref(), Some("A title"));
        assert!(matches!(p.sheets.as_slice(), [SheetSource::Inline { css, .. }] if css == "p{color:red}"));
        let text = all_text(&p.dom);
        assert!(text.contains("No JS"), "{text}");
        assert!(text.contains("visible"));
        for bad in ["evil", "alert", "tpl", "hidden", "frame", "svg", "A title"] {
            assert!(!text.contains(bad), "{bad} leaked into {text}");
        }
        // noscript content is real markup, not a raw text blob.
        assert!(p.dom.nodes.iter().any(|n| matches!(&n.data, NodeData::Element(e) if e.name == "b")));
    }

    #[test]
    fn resolves_urls_and_collects_images() {
        let p = parse(
            "<base href='https://cdn.example.net/x/'><a href='../a'>a</a><a href='javascript:alert(1)'>j</a>
             <img src=one.png><img src=one.png><img srcset='two.png 2x, three.png 3x'><img src='data:image/png;base64,AA'>",
            &base(),
        );
        let urls: Vec<&str> = p.images.iter().map(Url::as_str).collect();
        assert_eq!(urls, vec!["https://cdn.example.net/x/one.png", "https://cdn.example.net/x/two.png"]);
        let links: Vec<Option<&str>> = p
            .dom
            .nodes
            .iter()
            .filter(|n| matches!(&n.data, NodeData::Element(e) if e.name == "a"))
            .map(|n| n.url.as_ref().map(Url::as_str))
            .collect();
        assert_eq!(links, vec![Some("https://cdn.example.net/a"), None]);
    }

    #[test]
    fn collects_stylesheet_links_in_order() {
        let p = parse(
            "<head><link rel=stylesheet href=a.css media=print><style>p{}</style>
             <link rel='Alternate StyleSheet' href=alt.css><link rel=icon href=i.png>
             <link rel=stylesheet href=a.css><link rel=stylesheet href='javascript:x'></head>
             <body><link rel=stylesheet href=/late.css></body>",
            &base(),
        );
        let urls: Vec<&str> = p.stylesheet_urls.iter().map(Url::as_str).collect();
        assert_eq!(
            urls,
            vec!["https://example.org/dir/a.css", "https://example.org/dir/alt.css", "https://example.org/late.css"]
        );
        let order: Vec<String> = p
            .sheets
            .iter()
            .map(|s| match s {
                SheetSource::Inline { .. } => "style".to_string(),
                SheetSource::Linked { url, media } => format!("{} {:?}", url.path(), media),
            })
            .collect();
        assert_eq!(
            order,
            vec!["/dir/a.css Some(\"print\")", "style", "/dir/a.css None", "/late.css None"],
            "alternate sheets are fetched but not applied"
        );
        let many: String = (0..100).map(|i| format!("<link rel=stylesheet href=/s{i}.css>")).collect();
        assert_eq!(parse(&many, &base()).stylesheet_urls.len(), MAX_STYLESHEETS);
    }

    #[test]
    fn nesting_safety_valve() {
        let ok = "<ul>".to_string() + &"<li>item".repeat(10_000) + "</ul><p>end";
        assert_eq!(limit_nesting(&ok).len(), ok.len());
        let deep = "<div>".repeat(10_000);
        assert_eq!(limit_nesting(&deep).len(), MAX_SOURCE_DEPTH * 5);
        let raw = "<script>".to_string() + &"<div>".repeat(10_000) + "</script><p>x";
        assert_eq!(limit_nesting(&raw).len(), raw.len());
    }

    #[test]
    fn image_list_is_capped() {
        let html: String = (0..200).map(|i| format!("<img src=/i{i}.png>")).collect();
        assert_eq!(parse(&html, &base()).images.len(), MAX_IMAGES);
    }

    #[test]
    fn deep_nesting_is_flattened() {
        let html = "<div>".repeat(3000) + "deep text" + &"</div>".repeat(3000);
        let p = parse(&html, &base());
        assert!(all_text(&p.dom).contains("deep text"));
        let mut max_depth = 0;
        for (i, _) in p.dom.nodes.iter().enumerate() {
            let mut d = 0;
            let mut cur = p.dom.nodes[i].parent;
            while let Some(c) = cur {
                d += 1;
                cur = p.dom.nodes[c].parent;
            }
            max_depth = max_depth.max(d);
        }
        assert!(max_depth <= MAX_DEPTH + 1, "{max_depth}");
    }

    #[test]
    fn closed_details_shows_only_summary() {
        let p = parse(
            "<details><summary>Sum</summary>Body</details><details open><summary>S2</summary>B2</details>",
            &base(),
        );
        let text = all_text(&p.dom);
        assert!(text.contains("Sum") && !text.contains("Body"));
        assert!(text.contains("S2") && text.contains("B2"));
    }
}
