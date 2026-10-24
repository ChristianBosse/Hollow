//! A pre-pass that makes XML test files (`.xht`, `.xhtml`, `.xml`) parse with the HTML parser the
//! way browsers parse them as XML. html5ever has no XML mode; this covers what WPT's CSS tests
//! rely on:
//!
//! - `<![CDATA[ ... ]]>` becomes its text (escaped, except inside `<style>`/`<script>`);
//! - `<div/>` becomes `<div></div>` (the HTML parser would leave it open), and `</br>` and other
//!   end tags of void elements are dropped (the HTML parser turns `</br>` into a second `<br>`);
//! - comments inside `<style>` are removed, as XML does (CSS would read through `<!--`);
//! - `<?xml-stylesheet href=...?>` becomes a `<link rel=stylesheet>`, other processing
//!   instructions and the doctype are dropped, and `<!DOCTYPE html>` is prepended (XML documents
//!   never render in quirks mode).
//!
//! What it does not emulate: XML has no implied elements (no `<tbody>`, no auto-closed `<p>`) and is
//! case-sensitive; such tests may still differ.

const VOID: &[&str] =
    &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];
const RAW_TEXT: &[&str] = &["style", "script"];

pub fn to_html(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 64);
    out.push_str("<!DOCTYPE html>");
    let mut i = 0;
    // Inside <style> or <script>: copy up to the end tag.
    let mut raw: Option<&str> = None;
    while i < src.len() {
        let rest = &src[i..];
        if let Some(name) = raw.take() {
            let end = find_ci(rest, &format!("</{name}")).unwrap_or(rest.len());
            push_raw(&mut out, &rest[..end]);
            i += end;
            continue;
        }
        if rest.starts_with("<!--") {
            let end = rest.find("-->").map_or(rest.len(), |e| e + 3);
            out.push_str(&rest[..end]);
            i += end;
        } else if let Some(body) = rest.strip_prefix("<![CDATA[") {
            let end = body.find("]]>").unwrap_or(body.len());
            for c in body[..end].chars() {
                match c {
                    '&' => out.push_str("&amp;"),
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    c => out.push(c),
                }
            }
            i += 9 + (end + 3).min(body.len());
        } else if let Some(body) = rest.strip_prefix("<?") {
            let end = body.find("?>").unwrap_or(body.len());
            if let Some(attrs) = body[..end].strip_prefix("xml-stylesheet") {
                xml_stylesheet(&mut out, attrs);
            }
            i += 2 + (end + 2).min(body.len());
        } else if rest.starts_with("<!") {
            i += doctype_len(rest);
        } else if let Some(body) = rest.strip_prefix("</") {
            let end = body.find('>').map_or(body.len(), |e| e + 1);
            let name = tag_name(body);
            if !VOID.iter().any(|v| v.eq_ignore_ascii_case(name)) {
                out.push_str(&rest[..2 + end]);
            }
            i += 2 + end;
        } else if rest.len() > 1 && rest.as_bytes()[1].is_ascii_alphabetic() {
            let len = tag_len(rest);
            let tag = &rest[..len];
            let name = tag_name(&tag[1..]);
            let self_closing = tag.strip_suffix('>').is_some_and(|t| t.trim_end().ends_with('/'));
            let void = VOID.iter().any(|v| v.eq_ignore_ascii_case(name));
            if self_closing && !void {
                let open = tag.strip_suffix('>').unwrap_or(tag).trim_end();
                out.push_str(open.strip_suffix('/').unwrap_or(open));
                out.push_str("></");
                out.push_str(name);
                out.push('>');
            } else {
                out.push_str(tag);
                if !self_closing {
                    raw = RAW_TEXT.iter().copied().find(|r| r.eq_ignore_ascii_case(name));
                }
            }
            i += len;
        } else {
            let c = rest.chars().next().unwrap_or(' ');
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// Style or script text: CDATA markers stripped, XML comments removed.
fn push_raw(out: &mut String, text: &str) {
    let mut rest = text;
    while !rest.is_empty() {
        let cdata = rest.find("<![CDATA[");
        let comment = rest.find("<!--");
        match (cdata, comment) {
            (Some(c), m) if m.is_none_or(|m| c < m) => {
                out.push_str(&rest[..c]);
                let body = &rest[c + 9..];
                let end = body.find("]]>").unwrap_or(body.len());
                out.push_str(&body[..end]);
                rest = &body[(end + 3).min(body.len())..];
            }
            (_, Some(m)) => {
                out.push_str(&rest[..m]);
                let body = &rest[m + 4..];
                rest = &body[body.find("-->").map_or(body.len(), |e| e + 3)..];
            }
            _ => {
                out.push_str(rest);
                rest = "";
            }
        }
    }
}

/// `<?xml-stylesheet href="a.css" type="text/css"?>` as a `<link>`, unless it is an alternate sheet
/// or not CSS.
fn xml_stylesheet(out: &mut String, attrs: &str) {
    let get = |name: &str| pseudo_attr(attrs, name);
    let css = get("type").is_none_or(|t| t.eq_ignore_ascii_case("text/css"));
    let alternate = get("alternate").is_some_and(|a| a == "yes");
    if let Some(href) = get("href").filter(|_| css && !alternate) {
        out.push_str("<link rel=stylesheet href=\"");
        out.push_str(&href.replace('"', "&quot;"));
        out.push('"');
        if let Some(media) = get("media") {
            out.push_str(" media=\"");
            out.push_str(&media.replace('"', "&quot;"));
            out.push('"');
        }
        out.push('>');
    }
}

fn pseudo_attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attrs;
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim();
        let value = rest[eq + 1..].trim_start();
        let quote = value.chars().next().filter(|q| *q == '"' || *q == '\'')?;
        let end = value[1..].find(quote)? + 1;
        if key == name {
            return Some(&value[1..end]);
        }
        rest = &value[end + 1..];
    }
    None
}

/// Length of `<!DOCTYPE ...>` (or any other `<!...>`), including an internal subset in `[...]`.
fn doctype_len(rest: &str) -> usize {
    let close = rest.find('>').unwrap_or(rest.len());
    match rest.find('[').filter(|&b| b < close) {
        Some(b) => rest[b..].find("]").and_then(|e| rest[b + e..].find('>').map(|g| b + e + g + 1)),
        None => None,
    }
    .unwrap_or((close + 1).min(rest.len()))
}

/// Length of a start tag beginning at `rest[0] == '<'`, skipping `>` inside quoted values.
fn tag_len(rest: &str) -> usize {
    let mut quote = None;
    for (i, c) in rest.char_indices().skip(1) {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return i + 1,
            _ => {}
        }
    }
    rest.len()
}

fn tag_name(s: &str) -> &str {
    let end = s.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.'))).unwrap_or(s.len());
    &s[..end]
}

/// Byte offset of `needle` (ASCII) in `hay`, ignoring ASCII case.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    (0..h.len().saturating_sub(n.len() - 1)).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::to_html;

    #[test]
    fn cdata_and_self_closing() {
        let src = "<?xml version=\"1.0\"?>\n<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\" \"x.dtd\">\
                   <html><head><style type=\"text/css\"><![CDATA[ div > p { color: red } ]]></style></head>\
                   <body><div class=\"a/b\"/><p>x<br/>y</br></p><![CDATA[<b>&]]></body></html>";
        let html = to_html(src);
        assert_eq!(
            html,
            "<!DOCTYPE html>\n<html><head><style type=\"text/css\"> div > p { color: red } </style></head>\
             <body><div class=\"a/b\"></div><p>x<br/>y</p>&lt;b&gt;&amp;</body></html>"
        );
    }

    #[test]
    fn stylesheet_processing_instruction_and_internal_subset() {
        let src = "<?xml-stylesheet href=\"a.css\" type=\"text/css\"?><?xml-stylesheet href='alt.css' alternate=\"yes\"?>\
                   <!DOCTYPE html [ <!ENTITY e \"x\"> ]><html><style><!-- p { color: red } --> b {}</style></html>";
        assert_eq!(
            to_html(src),
            "<!DOCTYPE html><link rel=stylesheet href=\"a.css\"><html><style> b {}</style></html>"
        );
    }
}
