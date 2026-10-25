//! hollow-engine: HTML, CSS, images, layout, paint and forms.

use hollow_engine::{Document, FormKey, HitTarget, Interaction, Layout, MAX_STYLESHEET_BYTES, Resources, Submission};
use hollow_ipc::{FormEncoding, FormSubmit, Method};
use url::Url;

use crate::{fields, text};

fn base() -> Url {
    Url::parse("https://fuzz.example/dir/page.html").expect("valid URL")
}

/// A page with a bit of everything the cascade and layout handle, for the
/// stylesheet and image targets to style and draw.
const PAGE: &str = r#"<!doctype html><html lang=en><head><title>t</title>
<link rel=stylesheet href=style.css></head>
<body class="b c" id=top>
<h1 id=h>Head <em>em</em> <code>code</code></h1>
<p class=a>Some <a href=/x>link</a> text, <b>bold</b>, <i>italic</i>, <span style="color:red">span</span>.
<img src=img.png alt=i> <img src=img.png width=37 height=5></p>
<div class="flex row"><div>one</div><div class=a>two</div><div style="flex:2">three</div></div>
<div style="float:left;width:30%"><img src=img.png style="width:100%"></div>
<div style="position:relative"><div style="position:absolute;top:3px;left:4px">abs</div></div>
<ul><li>a<li class=a>b<ol><li>c</ol></ul>
<table border=1><tr><th>h<th colspan=2>h2<tr><td>1<td rowspan=2>2<td>3<tr><td>4<td>5</table>
<pre>  pre
 text</pre><blockquote>q</blockquote><hr>
<form action=/s><input name=q value=v><input type=checkbox name=c checked>
<select name=s><option>1<option selected>2</select><textarea name=t>x</textarea><button>go</button></form>
<div style="display:inline-block;border:2px solid;padding:1em;margin:-1px">ib</div>
<p dir=rtl>rtl عربي 中文 😀</p>
</body></html>"#;

/// `Document::parse` and everything a renderer asks of the result. The
/// engine promises at most 64 image and 32 stylesheet URLs, all http(s).
pub fn html(data: &[u8]) {
    let doc = Document::parse(&text(data), &base());
    let _ = doc.title();
    let images = doc.image_urls();
    let sheets = doc.stylesheet_urls();
    assert!(images.len() <= 64, "{} image URLs", images.len());
    assert!(sheets.len() <= 32, "{} stylesheet URLs", sheets.len());
    for u in images.iter().chain(&sheets) {
        assert!(matches!(u.scheme(), "http" | "https"), "resource URL {u}");
    }
    let _ = doc.forms();
}

/// `Resources::insert_stylesheet`, then the sheet applied to [`PAGE`].
pub fn css(data: &[u8]) {
    let css = text(data);
    let mut res = Resources::new();
    let url = base().join("style.css").expect("valid URL");
    let accepted = res.insert_stylesheet(&url, &css);
    assert_eq!(accepted.is_ok(), css.len() <= MAX_STYLESHEET_BYTES);
    if accepted.is_ok() {
        let doc = Document::parse(PAGE, &base());
        let layout = Layout::new(&doc, &res, 320.0);
        check_frame(&layout, 0.0, 240.0, 1.0);
    }
}

/// `Resources::insert_image`, then the image drawn at several sizes.
pub fn image(data: &[u8]) {
    let mut res = Resources::new();
    let url = base().join("img.png").expect("valid URL");
    let Ok((w, h)) = res.insert_image(&url, data) else { return };
    assert!(w > 0 && h > 0 && u64::from(w) * u64::from(h) <= 16 * 1024 * 1024, "{w}x{h}");
    let doc = Document::parse(PAGE, &base());
    let layout = Layout::new(&doc, &res, 200.0);
    check_frame(&layout, 0.0, 150.0, 1.5);
}

/// A fuzzed page laid out and painted: byte 0 picks the viewport width
/// (1 to 1021 CSS px), byte 1 the scale, the rest is the HTML.
pub fn layout(data: &[u8]) {
    let (width, scale, html) = match data {
        [w, s, rest @ ..] => (1.0 + f32::from(*w) * 4.0, 0.25 + f32::from(*s) / 64.0, rest),
        _ => (320.0, 1.0, data),
    };
    let doc = Document::parse(&text(html), &base());
    let layout = Layout::new(&doc, &Resources::new(), width);
    let height = layout.height();
    assert!(height.is_finite() && height >= 0.0, "page height {height}");
    check_frame(&layout, 0.0, 200.0, scale);
    if height > 200.0 {
        check_frame(&layout, height / 2.0, 200.0, scale);
    }
    for (x, y) in [(1.0, 1.0), (width / 2.0, 10.0), (width - 1.0, height / 3.0), (5.0, height - 1.0)] {
        let _ = layout.hit_test(x, y);
    }
    for link in layout.links() {
        assert!(link.width.is_finite() && link.height.is_finite() && link.x.is_finite() && link.y.is_finite());
    }
}

/// Paints and checks the documented frame size and format.
fn check_frame(layout: &Layout, scroll_y: f32, height: f32, scale: f32) {
    let frame = layout.paint(scroll_y, height, scale);
    assert_eq!(frame.rgba.len(), frame.width as usize * frame.height as usize * 4);
    assert!(frame.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "a pixel is not opaque");
}

/// A page's forms driven by a script of clicks and keys: `html SEP script`.
/// Each script byte picks an action; some take the next byte as an argument
/// (which control, which option, how much text). Every submission goes
/// through the broker's encoder, as it would for real.
pub fn forms(data: &[u8]) {
    let [html, script] = fields(data, 2)[..] else { unreachable!() };
    let doc = Document::parse(&text(html), &base());
    let mut state = doc.forms();
    let controls = state.controls(&doc);
    let mut script = script.iter().copied();
    let mut steps = 0;
    while let Some(op) = script.next() {
        steps += 1;
        if steps > 2_000 {
            break;
        }
        let mut arg = || usize::from(script.next().unwrap_or(0));
        let interaction = match op % 18 {
            0..=3 if !controls.is_empty() => {
                let id = controls[arg() % controls.len()];
                state.click(&doc, Some(&HitTarget::Control(id)))
            }
            4 if !controls.is_empty() => {
                let id = controls[arg() % controls.len()];
                state.click(&doc, Some(&HitTarget::Option(id, arg())))
            }
            5 => state.click(&doc, None),
            6 => {
                // Typed text: the next byte is its length in bytes.
                let n = arg();
                let bytes: Vec<u8> = (0..n).map(|_| arg() as u8).collect();
                state.key(&doc, &FormKey::Text(text(&bytes)))
            }
            7 => state.key(&doc, &FormKey::Text("x\u{e9}\u{1F600}\n".repeat(arg() + 1))),
            op => state.key(&doc, &key(op)),
        };
        let _ = state.editing();
        if let Interaction::Submit(s) = interaction {
            submit(s);
        }
    }
    let layout = Layout::new(&doc, &Resources::new(), 300.0);
    let frame = layout.paint_forms(&state, 0.0, 200.0, 1.0);
    assert_eq!(frame.rgba.len(), frame.width as usize * frame.height as usize * 4);
    for (x, y) in [(10.0, 10.0), (100.0, 50.0), (150.0, 120.0), (290.0, 199.0)] {
        let hit = layout.hit_test_forms(&state, x, y);
        if let Interaction::Submit(s) = state.click(&doc, hit.as_ref()) {
            submit(s);
        }
    }
}

fn key(op: u8) -> FormKey {
    match op % 11 {
        0 => FormKey::Backspace,
        1 => FormKey::Delete,
        2 => FormKey::Enter,
        3 => FormKey::Left,
        4 => FormKey::Right,
        5 => FormKey::Home,
        6 => FormKey::End,
        7 => FormKey::Up,
        8 => FormKey::Down,
        9 => {
            if op & 0x80 == 0 {
                FormKey::Tab
            } else {
                FormKey::BackTab
            }
        }
        _ => FormKey::Escape,
    }
}

/// A submission as the renderer forwards it, encoded as the broker does.
fn submit(s: Submission) {
    assert!(s.entries.len() <= hollow_ipc::MAX_FORM_ENTRIES, "{} entries", s.entries.len());
    let submit = FormSubmit {
        action: s.action.into(),
        method: match s.method {
            hollow_engine::FormMethod::Get => Method::Get,
            hollow_engine::FormMethod::Post => Method::Post,
        },
        encoding: match s.enctype {
            hollow_engine::FormEnctype::UrlEncoded => FormEncoding::UrlEncoded,
            hollow_engine::FormEnctype::Multipart => FormEncoding::Multipart,
            hollow_engine::FormEnctype::Plain => FormEncoding::Plain,
        },
        entries: s.entries,
        new_tab: s.new_tab,
    };
    let _ = hollow_app::form::to_request(submit);
}

#[cfg(test)]
mod tests {
    #[test]
    fn page_lays_out() {
        super::css(b"p { color: red }");
        super::layout(b"\x50\x40<p>hi");
        super::forms(b"<form><input name=q><input type=submit></form>\xff\x00\x00\x06\x03abc\x02");
    }
}
