//! Static SVG (MODE-03): SVG files used as images, and inline `<svg>`
//! elements, rasterized with resvg. No scripts, no animation, no external
//! references: an SVG can only use what is inside it (or, for inline SVG,
//! symbols and gradients defined elsewhere in the same page).
//!
//! Text in SVG uses only the bundled fonts (DejaVu and the fallback fonts),
//! never the system's, so it renders the same everywhere and can't probe
//! installed fonts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use ego_tree::NodeRef;
use scraper::Node as HtmlNode;
use tiny_skia::{Pixmap, Transform};

use crate::image::ImageError;

/// Largest SVG source rendered (file or inline markup).
pub(crate) const MAX_SVG_BYTES: usize = 1 << 20;
/// Elements deep an inline SVG may nest before the rest is dropped.
const MAX_SVG_DEPTH: usize = 128;
/// Total markup kept for elements other inline SVGs may reference by id.
const MAX_DEFS_BYTES: usize = 4 << 20;
/// References followed when gathering another SVG's definitions.
const MAX_REFS: usize = 256;

/// Whether `bytes` look like SVG markup rather than another image format.
pub(crate) fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    let head = String::from_utf8_lossy(head);
    let t = head.trim_start_matches('\u{feff}').trim_start();
    (t.starts_with('<') && head.contains("<svg")) && !t.starts_with("<html") && !t.starts_with("<!DOCTYPE html")
}

fn fonts() -> Arc<resvg::usvg::fontdb::Database> {
    static DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        for data in [
            dejavu::sans::regular(),
            dejavu::sans::bold(),
            dejavu::sans::oblique(),
            dejavu::sans::bold_oblique(),
            dejavu::serif::regular(),
            dejavu::serif::bold(),
            dejavu::serif::italic(),
            dejavu::serif::bold_italic(),
            dejavu::sans_mono::regular(),
            dejavu::sans_mono::bold(),
        ] {
            db.load_font_data(data.to_vec());
        }
        // The same fallback fonts as HTML text, for scripts DejaVu lacks.
        for &data in crate::text::fallback_font_data() {
            db.load_font_source(resvg::usvg::fontdb::Source::Binary(Arc::new(data)));
        }
        db.set_sans_serif_family("DejaVu Sans");
        db.set_serif_family("DejaVu Serif");
        db.set_monospace_family("DejaVu Sans Mono");
        db.set_cursive_family("DejaVu Sans");
        db.set_fantasy_family("DejaVu Sans");
        Arc::new(db)
    })
    .clone()
}

/// Longest side, in pixels, an SVG with no size of its own is drawn at
/// before layout scales it to the room it gets.
const SHAPE_ONLY_SIDE: f32 = 512.0;

/// The root `<svg>` tag's `width`, `height` and `viewBox` attributes.
fn root_size(src: &str) -> (Option<f32>, Option<f32>, Option<(f32, f32)>) {
    let Some(start) = src.find("<svg") else { return (None, None, None) };
    let tag = &src[start..];
    let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
    let attr = |name: &str| {
        let mut rest = tag;
        while let Some(i) = rest.find(name) {
            let before = rest[..i].chars().next_back();
            rest = &rest[i + name.len()..];
            if before.is_some_and(char::is_whitespace) && rest.trim_start().starts_with('=') {
                let v = rest.trim_start()[1..].trim_start();
                let quote = v.chars().next().filter(|c| *c == '"' || *c == '\'');
                let v = match quote {
                    Some(_) => &v[1..],
                    None => v,
                };
                let end =
                    v.find(|c: char| Some(c) == quote || (quote.is_none() && c.is_whitespace())).unwrap_or(v.len());
                return Some(v[..end].trim().to_owned());
            }
        }
        None
    };
    // Only absolute lengths count as a size of its own; `100%` doesn't.
    let length = |v: Option<String>| {
        let v = v?;
        let n = v.trim_end_matches("px");
        n.parse::<f32>().ok().filter(|n| *n > 0.0 && n.is_finite())
    };
    let view_box = attr("viewBox").and_then(|v| {
        let n: Vec<f32> = v.split([' ', ',']).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        (n.len() == 4 && n[2] > 0.0 && n[3] > 0.0).then(|| (n[2], n[3]))
    });
    (length(attr("width")), length(attr("height")), view_box)
}

/// Rasterizes SVG source at its own size (from `width`/`height`, else the
/// `viewBox`), premultiplied like every other decoded image.
#[cfg(test)]
pub(crate) fn render(bytes: &[u8]) -> Result<Pixmap, ImageError> {
    render_sized(bytes).map(|(p, _)| p)
}

/// Like [`render`], also saying whether the SVG has only a shape (a
/// `viewBox` but no width or height of its own). Such an SVG is drawn with
/// its longest side at 512 px for layout to scale; one with neither is 300
/// by 150, CSS's default object size.
pub(crate) fn render_sized(bytes: &[u8]) -> Result<(Pixmap, bool), ImageError> {
    if bytes.len() > MAX_SVG_BYTES {
        return Err(ImageError::TooLarge);
    }
    let (width, height, view_box) = root_size(&String::from_utf8_lossy(&bytes[..bytes.len().min(8192)]));
    let shape_only = (width.is_none() || height.is_none()) && view_box.is_some();
    let default_size = match (width, height, view_box) {
        (None, None, Some((w, h))) => {
            let k = SHAPE_ONLY_SIDE / w.max(h);
            (w * k, h * k)
        }
        (Some(w), None, Some((vw, vh))) => (w, w * vh / vw),
        (None, Some(h), Some((vw, vh))) => (h * vw / vh, h),
        _ => (300.0, 150.0),
    };
    // A size given on one side only is still a size of its own.
    let shape_only = shape_only && width.is_none() && height.is_none();
    let options = resvg::usvg::Options {
        default_size: resvg::usvg::Size::from_wh(default_size.0, default_size.1)
            .unwrap_or(resvg::usvg::Size::from_wh(300.0, 150.0).expect("valid size")),
        font_family: "DejaVu Sans".into(),
        fontdb: fonts(),
        // Only images embedded as data: URLs; never files or the network.
        image_href_resolver: resvg::usvg::ImageHrefResolver {
            resolve_data: resvg::usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        ..resvg::usvg::Options::default()
    };
    let tree = resvg::usvg::Tree::from_data(bytes, &options).map_err(|_| ImageError::Decode)?;
    let size = tree.size();
    let (w, h) = (size.width().ceil(), size.height().ceil());
    if !(w >= 1.0 && h >= 1.0) {
        return Err(ImageError::Decode);
    }
    if w > 16_384.0 || h > 16_384.0 || u64::from(w as u32) * u64::from(h as u32) > crate::image::MAX_IMAGE_PIXELS {
        return Err(ImageError::TooLarge);
    }
    let mut pixmap = Pixmap::new(w as u32, h as u32).ok_or(ImageError::TooLarge)?;
    resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());
    Ok((pixmap, shape_only))
}

fn escape(s: &str, attr: bool, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// Writes an inline `<svg>` subtree back out as standalone SVG markup, at
/// most `limit` bytes of it. Elements with an `id` are also remembered in
/// `defs`, so other inline SVGs on the page can `<use>` them.
pub(crate) fn serialize(root: NodeRef<'_, HtmlNode>, defs: &mut BTreeMap<String, String>, limit: usize) -> String {
    enum Step<'a> {
        Open(NodeRef<'a, HtmlNode>, usize),
        Close(String, usize, Option<String>),
    }
    let mut out = String::new();
    let mut defs_bytes: usize = defs.values().map(String::len).sum();
    let mut stack = vec![Step::Open(root, 0)];
    while let Some(step) = stack.pop() {
        match step {
            Step::Open(node, depth) => match node.value() {
                HtmlNode::Element(e) if depth <= MAX_SVG_DEPTH && out.len() < limit => {
                    let start = out.len();
                    let name = e.name();
                    out.push('<');
                    out.push_str(name);
                    let mut has_ns = false;
                    let mut id = None;
                    for (k, v) in e.attrs() {
                        has_ns |= k == "xmlns";
                        if k == "id" {
                            id = Some(v.to_owned());
                        }
                        out.push(' ');
                        out.push_str(k);
                        out.push_str("=\"");
                        escape(v, true, &mut out);
                        out.push('"');
                    }
                    if depth == 0 && !has_ns {
                        out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
                    }
                    out.push('>');
                    let id = id.filter(|_| depth > 0).map(|i| (i, start));
                    stack.push(Step::Close(name.to_owned(), start, id.map(|(i, _)| i)));
                    for child in node.children().rev() {
                        stack.push(Step::Open(child, depth + 1));
                    }
                }
                HtmlNode::Text(t) if out.len() < limit => escape(t, false, &mut out),
                _ => {}
            },
            Step::Close(name, start, id) => {
                out.push_str("</");
                out.push_str(&name);
                out.push('>');
                if let Some(id) = id
                    && !defs.contains_key(&id)
                    && defs_bytes + (out.len() - start) <= MAX_DEFS_BYTES
                {
                    defs_bytes += out.len() - start;
                    defs.insert(id, out[start..].to_owned());
                }
            }
        }
    }
    out
}

/// `#id` references in markup: `href="#x"` and `url(#x)`.
fn refs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (pat, end) in [("href=\"#", '"'), ("url(#", ')')] {
        let mut rest = s;
        while let Some(i) = rest.find(pat) {
            rest = &rest[i + pat.len()..];
            let id: String = rest.chars().take_while(|c| *c != end && *c != '"' && *c != '\'').collect();
            if !id.is_empty() && id.len() < 256 {
                out.push(id);
            }
        }
    }
    out
}

/// Adds the definitions an inline SVG references but doesn't contain (a
/// sprite sheet's `<symbol>`s, shared gradients) from elsewhere on the page.
pub(crate) fn with_page_defs(markup: String, defs: &BTreeMap<String, String>) -> String {
    let mut extra = String::new();
    let mut seen = BTreeSet::new();
    let mut queue = refs(&markup);
    let mut followed = 0;
    while let Some(id) = queue.pop() {
        if followed >= MAX_REFS || extra.len() + markup.len() > MAX_SVG_BYTES {
            break;
        }
        followed += 1;
        if markup.contains(&format!(" id=\"{id}\"")) || !seen.insert(id.clone()) {
            continue;
        }
        if let Some(def) = defs.get(&id) {
            extra.push_str(def);
            queue.extend(refs(def));
        }
    }
    if extra.is_empty() {
        return markup;
    }
    match markup.rfind("</svg>") {
        Some(end) => format!("{}<defs>{extra}</defs>{}", &markup[..end], &markup[end..]),
        None => markup,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_at_its_own_size_with_bundled_fonts_only() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
            <rect width="20" height="20" fill="#ff0000"/><text x="22" y="15" font-size="12">Hi</text></svg>"##;
        assert!(looks_like_svg(svg));
        let p = render(svg).unwrap();
        assert_eq!((p.width(), p.height()), (40, 20));
        assert_eq!(p.pixel(5, 5).map(|c| (c.red(), c.alpha())), Some((255, 255)));
        let text_ink = (22..40)
            .flat_map(|x| (0..20).map(move |y| (x, y)))
            .filter(|&(x, y)| p.pixel(x, y).is_some_and(|c| c.alpha() > 0));
        assert!(text_ink.count() > 10, "text drew with a bundled font");
    }

    #[test]
    fn refuses_external_and_oversized_content() {
        assert!(!looks_like_svg(b"\x89PNG\r\n\x1a\n"));
        assert!(!looks_like_svg(b"<!DOCTYPE html><html><svg></svg></html>"));
        // An external image reference draws nothing (and fetches nothing).
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
            <image href="file:///C:/Windows/win.ini" width="10" height="10"/></svg>"#;
        let p = render(svg).unwrap();
        assert!(p.pixels().iter().all(|c| c.alpha() == 0));
        let huge = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="100000"></svg>"#;
        assert_eq!(render(huge).err(), Some(ImageError::TooLarge));
    }

    #[test]
    fn page_definitions_are_pulled_in() {
        let mut defs = BTreeMap::new();
        defs.insert("i".to_owned(), "<symbol id=\"i\"><path fill=\"url(#g)\"/></symbol>".to_owned());
        defs.insert("g".to_owned(), "<linearGradient id=\"g\"/>".to_owned());
        let out = with_page_defs("<svg><use href=\"#i\"></use></svg>".into(), &defs);
        assert!(out.contains("<symbol id=\"i\">") && out.contains("<linearGradient id=\"g\"/>"));
        assert!(out.ends_with("</defs></svg>"));
    }
}
