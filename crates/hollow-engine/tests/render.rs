//! End-to-end tests through the public API: HTML in, pixels and hit-tests out.

use hollow_engine::{Document, Frame, HitTarget, Layout, Resources};
use url::Url;

fn base() -> Url {
    Url::parse("https://example.org/dir/").unwrap()
}

fn layout(html: &str, width: f32) -> Layout {
    Layout::new(&Document::parse(html, &base()), &Resources::new(), width)
}

fn pixel(f: &Frame, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * f.width + x) * 4) as usize;
    [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2], f.rgba[i + 3]]
}

/// Counts pixels that are not pure white in the rectangle [x0, x1) x [y0, y1).
fn ink(f: &Frame, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
    let mut n = 0;
    for y in y0..y1.min(f.height) {
        for x in x0..x1.min(f.width) {
            if pixel(f, x, y)[..3] != [255, 255, 255] {
                n += 1;
            }
        }
    }
    n
}

fn link_at(l: &Layout, x: f32, y: f32) -> Option<String> {
    match l.hit_test(x, y) {
        Some(HitTarget::Link(u)) => Some(u.to_string()),
        _ => None,
    }
}

const PLAIN: &str = "<body style='margin:0'>";

#[test]
fn text_paints_where_expected() {
    let l = layout(&format!("{PLAIN}<p style='margin:0'>Hello, world</p>"), 400.0);
    let f = l.paint(0.0, 100.0, 1.0);
    assert_eq!((f.width, f.height), (400, 100));
    assert_eq!(f.rgba.len(), 400 * 100 * 4);
    assert!(ink(&f, 0, 0, 150, 20) > 50, "text should be drawn in the top-left corner");
    assert_eq!(ink(&f, 0, 30, 400, 100), 0, "nothing below the single line");
    assert_eq!(ink(&f, 200, 0, 400, 100), 0, "nothing right of the short text");
    assert!(f.rgba.chunks(4).all(|p| p[3] == 255), "frames are opaque");
}

#[test]
fn background_and_scale() {
    let html = format!("{PLAIN}<div style='background-color:#ff0000; width:100px; padding:20px'></div>");
    let l = layout(&html, 300.0);
    assert_eq!(l.height(), 40.0);
    let f = l.paint(0.0, 60.0, 1.0);
    assert_eq!(pixel(&f, 70, 20), [255, 0, 0, 255]);
    assert_eq!(pixel(&f, 139, 39), [255, 0, 0, 255]);
    assert_eq!(pixel(&f, 140, 20), [255, 255, 255, 255]);
    assert_eq!(pixel(&f, 70, 40), [255, 255, 255, 255]);
    let f2 = l.paint(0.0, 60.0, 2.0);
    assert_eq!((f2.width, f2.height), (600, 120));
    assert_eq!(pixel(&f2, 279, 79), [255, 0, 0, 255]);
    assert_eq!(pixel(&f2, 280, 79), [255, 255, 255, 255]);
    // Scrolling moves the content up.
    let f3 = l.paint(30.0, 30.0, 1.0);
    assert_eq!(pixel(&f3, 70, 5), [255, 0, 0, 255]);
    assert_eq!(pixel(&f3, 70, 15), [255, 255, 255, 255]);
}

#[test]
fn body_background_fills_canvas() {
    let l = layout("<body style='background:#000080'><p>x</p></body>", 200.0);
    let f = l.paint(0.0, 500.0, 1.0);
    assert_eq!(pixel(&f, 199, 499), [0, 0, 128, 255]);
}

#[test]
fn link_hit_test() {
    let l = layout(&format!("{PLAIN}<p style='margin:0'><a href='next.html?q=1'>Next page</a> plain text</p>"), 600.0);
    assert_eq!(link_at(&l, 5.0, 8.0).as_deref(), Some("https://example.org/dir/next.html?q=1"));
    assert_eq!(link_at(&l, 500.0, 8.0), None);
    assert_eq!(link_at(&l, 5.0, 300.0), None);
    let links = l.links();
    assert_eq!(links.len(), 3, "one area per word and per space inside the link: {links:?}");
    assert!(links.iter().all(|a| a.url.as_str() == "https://example.org/dir/next.html?q=1"));
}

#[test]
fn block_link_and_javascript_links() {
    let l = layout(
        &format!(
            "{PLAIN}<a href='/card'><div style='padding:10px'>Card</div></a><p><a href='javascript:x()'>js</a></p>"
        ),
        400.0,
    );
    assert_eq!(link_at(&l, 15.0, 15.0).as_deref(), Some("https://example.org/card"));
    assert!(l.links().iter().all(|a| a.url.scheme() == "https"));
}

#[test]
fn heights_grow_with_content() {
    let one = layout("<p>Paragraph</p>", 400.0).height();
    let ten = layout(&"<p>Paragraph</p>".repeat(10), 400.0).height();
    assert!(one > 0.0);
    assert!(ten > one * 5.0, "{one} {ten}");
    let words = "lorem ipsum dolor sit amet ".repeat(40);
    let wide = layout(&format!("<p>{words}</p>"), 1000.0).height();
    let narrow = layout(&format!("<p>{words}</p>"), 200.0).height();
    assert!(narrow > wide * 3.0, "{wide} {narrow}");
}

#[test]
fn display_none_and_hidden_remove_content() {
    let shown = layout(&format!("{PLAIN}<p>one</p><p class=x>two</p><p>three</p>"), 400.0);
    let hidden =
        layout(&format!("<style>.x {{ display: none }}</style>{PLAIN}<p>one</p><p class=x>two</p><p>three</p>"), 400.0);
    let attr = layout(&format!("{PLAIN}<p>one</p><p hidden>two</p><p>three</p>"), 400.0);
    assert!(hidden.height() < shown.height());
    assert_eq!(hidden.height(), attr.height());
    let script = layout(&format!("{PLAIN}<p>one</p><script>document.write('two')</script><p>three</p>"), 400.0);
    assert_eq!(script.height(), hidden.height());
}

#[test]
fn visibility_hidden_keeps_space_but_paints_nothing() {
    let l = layout(&format!("{PLAIN}<p style='visibility:hidden;margin:0'><a href=/x>Invisible</a></p>"), 300.0);
    assert!(l.height() > 10.0);
    assert_eq!(ink(&l.paint(0.0, 50.0, 1.0), 0, 0, 300, 50), 0);
    assert_eq!(link_at(&l, 5.0, 5.0), None);
}

#[test]
fn noscript_is_rendered() {
    let with = layout("<noscript><p>Enable nothing</p></noscript>", 400.0);
    assert!(with.height() > 20.0);
}

#[test]
fn line_breaking_respects_width() {
    let words: String = (0..60).map(|i| format!("<a href='/w{i}'>word{i}</a> ")).collect();
    let l = layout(&format!("{PLAIN}<p style='margin:0'>{words}</p>"), 300.0);
    let links = l.links();
    let mut rows: Vec<f32> = links.iter().map(|a| a.y).collect();
    rows.dedup();
    assert!(rows.len() > 5, "expected several lines, got {}", rows.len());
    for a in &links {
        assert!(a.x + a.width <= 300.0 + 0.01, "{a:?} overflows");
    }
    // Words stay in document order: left to right, top to bottom.
    for w in links.windows(2) {
        assert!(w[1].y > w[0].y || (w[1].y == w[0].y && w[1].x > w[0].x));
    }
}

#[test]
fn long_words_break() {
    let word = "x".repeat(300);
    let l = layout(&format!("{PLAIN}<p style='margin:0'><a href=/long>{word}</a></p>"), 200.0);
    let links = l.links();
    assert!(links.len() > 3, "the word should be split over several lines");
    assert!(links.iter().all(|a| a.x + a.width <= 200.0 + 0.01));
}

#[test]
fn pre_preserves_whitespace() {
    let l = layout(&format!("{PLAIN}<pre style='margin:0'>a\n\n\nb</pre>"), 300.0);
    let one = layout(&format!("{PLAIN}<pre style='margin:0'>a   b</pre>"), 300.0);
    assert!((l.height() - one.height() * 4.0).abs() < 0.01, "{} {}", l.height(), one.height());
    let collapsed = layout(&format!("{PLAIN}<p style='margin:0'>a\n\n\nb</p>"), 300.0);
    let single = layout(&format!("{PLAIN}<p style='margin:0'>a</p>"), 300.0);
    assert_eq!(collapsed.height(), single.height());
}

#[test]
fn text_align_center() {
    let l = layout(&format!("{PLAIN}<p style='text-align:center;margin:0'><a href=/c>mid</a></p>"), 800.0);
    let a = &l.links()[0];
    assert!((a.x + a.width / 2.0 - 400.0).abs() < 1.0, "{a:?}");
}

#[test]
fn margins_collapse() {
    let l = layout(
        &format!("{PLAIN}<div style='margin-top:20px'><p style='margin:30px 0 0'><a href=/x>x</a></p></div>"),
        400.0,
    );
    assert_eq!(l.links()[0].y, 30.0);
    let siblings = layout(
        &format!("{PLAIN}<p style='margin:0 0 25px'>a</p><p style='margin:10px 0 0'><a href=/y>y</a></p>"),
        400.0,
    );
    let first_line = layout(&format!("{PLAIN}<p style='margin:0'>a</p>"), 400.0).height();
    assert_eq!(siblings.links()[0].y, first_line + 25.0);
}

#[test]
fn centered_max_width_container() {
    let l = layout(&format!("{PLAIN}<div style='max-width:200px;margin:0 auto'><a href=/x>x</a></div>"), 600.0);
    assert_eq!(l.links()[0].x, 200.0);
}

#[test]
fn table_cells_side_by_side() {
    let l = layout(
        &format!(
            "{PLAIN}<table><tr><td><a href=/a>Alpha</a></td><td><a href=/b>Beta</a></td></tr>
             <tr><td colspan=2><a href=/c>Gamma spans both</a></td></tr></table>"
        ),
        600.0,
    );
    let links = l.links();
    let find = |p: &str| links.iter().find(|a| a.url.path() == p).unwrap().clone();
    let (a, b, c) = (find("/a"), find("/b"), find("/c"));
    assert_eq!(a.y, b.y);
    assert!(b.x >= a.x + a.width);
    assert!(c.y > a.y);
}

#[test]
fn list_markers_are_drawn() {
    let l = layout(&format!("{PLAIN}<ul style='margin:0'><li>item</li></ul>"), 300.0);
    let f = l.paint(0.0, 40.0, 1.0);
    assert!(ink(&f, 20, 0, 40, 20) > 0, "bullet left of the content box");
    let none = layout(&format!("{PLAIN}<ul style='margin:0;list-style:none'><li>item</li></ul>"), 300.0);
    assert_eq!(ink(&none.paint(0.0, 40.0, 1.0), 0, 0, 40, 40), 0);
}

fn png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    let data: Vec<u8> = (0..w * h).flat_map(|_| rgba).collect();
    writer.write_image_data(&data).unwrap();
    drop(writer);
    out
}

#[test]
fn images_are_drawn_and_scaled() {
    let doc = Document::parse(&format!("{PLAIN}<img src='/red.png'><img src='/missing.png' alt='[gone]'>"), &base());
    let urls = doc.image_urls();
    assert_eq!(urls.len(), 2);
    let mut res = Resources::new();
    assert_eq!(res.insert_image(&urls[0], &png(400, 100, [0, 0, 255, 255])), Ok((400, 100)));
    assert!(res.insert_image(&urls[1], b"not an image").is_err());
    let l = Layout::new(&doc, &res, 200.0);
    let f = l.paint(0.0, 100.0, 1.0);
    // 400x100 scaled down to fit 200px wide.
    assert_eq!(pixel(&f, 100, 25), [0, 0, 255, 255]);
    assert_eq!(pixel(&f, 100, 55), [255, 255, 255, 255]);
    assert!(l.height() > 50.0, "alt text goes on the next line");
}

#[test]
fn rendering_is_deterministic() {
    let html = include_str!("../examples/sample.html");
    let render = || {
        let doc = Document::parse(html, &base());
        let l = Layout::new(&doc, &Resources::new(), 900.0);
        l.paint(0.0, l.height(), 1.5)
    };
    let a = render();
    let b = render();
    assert_eq!(a.rgba.len(), b.rgba.len());
    assert!(a.rgba == b.rgba, "two renders of the same page differ");
}

/// FNV-1a, so the golden value below does not depend on std's hasher.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

/// The same pixels on every machine: a fixed document must hash to a fixed
/// value everywhere. Update the constant only for intentional changes to
/// fonts, layout or rasterization.
#[test]
fn golden_frame_hash() {
    let html = "<body style='font-family:sans-serif'><h1>Hollow</h1><p>The <b>same</b> pixels on <i>every</i> \
                computer. <a href=/x>Link</a></p><pre>mono   text</pre><ul><li>one<li>two</ul></body>";
    let f = layout(html, 320.0).paint(0.0, 240.0, 1.25);
    assert_eq!(fnv(&f.rgba), GOLDEN, "frame hash changed: {:#x}", fnv(&f.rgba));
}
const GOLDEN: u64 = 0x764e6a6aef46586c;

#[test]
fn degenerate_parameters_do_not_panic() {
    let l = layout("<p>x</p>", f32::NAN);
    for (s, h, k) in [
        (f32::NAN, 100.0, 1.0),
        (0.0, f32::INFINITY, 1.0),
        (0.0, -5.0, 1.0),
        (1e30, 100.0, 1.0),
        (0.0, 100.0, 0.0),
        (0.0, 100.0, f32::NAN),
        (0.0, 1e9, 8.0),
    ] {
        let f = l.paint(s, h, k);
        assert_eq!(f.rgba.len(), (f.width * f.height * 4) as usize);
        assert!(u64::from(f.width) * u64::from(f.height) <= 64 * 1024 * 1024);
    }
    for w in [0.0, -10.0, 1.0, f32::INFINITY, 1e12] {
        let l = layout("<p>some text <img src=/a.png width=10 height=10></p><table><tr><td>x</td></tr></table>", w);
        assert!(l.height().is_finite());
        let _ = l.hit_test(f32::NAN, f32::NAN);
    }
}

/// Hostile documents: everything must finish, bounded, without panicking,
/// on a thread with a 1 MiB stack like a default Windows main thread.
#[test]
fn hostile_inputs() {
    let inputs: Vec<String> = vec![
        "<div>".repeat(100_000) + "deep" + &"</div>".repeat(100_000),
        "<span>".repeat(100_000) + "deep" + &"</span>".repeat(100_000),
        "<b><i>".repeat(20_000) + "formatting",
        "<table><tr><td>".repeat(2_000) + "cells",
        "<ul><li>".repeat(5_000) + "lists",
        "<p>".repeat(50_000),
        "<a href=/x>".repeat(20_000) + "links",
        "x".repeat(3_000_000),
        "word ".repeat(400_000),
        format!("<table><tr>{}</tr></table>", "<td>c</td>".repeat(5_000)),
        "<table><tr><td colspan=100000>x</td><td colspan=-5>y</td></tr></table>".to_string(),
        "<p style='font-size:1e30px;margin:-1e30px;padding:1e30px;width:1e30px'>big</p>".to_string(),
        format!("<style>{}</style><p class=a>x</p>", "p .a .b .c .d .e .f .g .h { color: red } ".repeat(20_000)),
        format!("<style>{}</style>", "*{".repeat(100_000)),
        "<ol start=9223372036854775807><li>a<li>b</ol><ol start=-9223372036854775808><li>c</ol>".to_string(),
        "<pre>\t\t\t\r\n\u{200B}\u{FEFF}\u{202E}rtl</pre><p>\u{0}\u{1}\u{FFFF}</p>".to_string(),
        "<img src=a width=999999999 height=999999999><img width=-1 height=nan>".to_string(),
        "<select><option>a<optgroup><option selected>b</optgroup></select><input><textarea>t</textarea>".to_string(),
        "\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064A}\u{0629} \u{05E2}\u{05D1}\u{05E8}\u{05D9}\u{05EA} 中文 😀"
            .to_string(),
    ];
    let handle = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            for html in &inputs {
                let doc = Document::parse(html, &base());
                let _ = doc.title();
                let _ = doc.image_urls();
                let l = Layout::new(&doc, &Resources::new(), 800.0);
                assert!(l.height().is_finite() && l.height() >= 0.0);
                let f = l.paint(0.0, 600.0, 1.0);
                assert_eq!(f.rgba.len(), (f.width * f.height * 4) as usize);
                let _ = l.hit_test(10.0, 10.0);
            }
        })
        .unwrap();
    handle.join().expect("hostile input panicked or overflowed the stack");
}

// ---------------------------------------------------------------------------
// Linked stylesheets and media queries

fn text_color_ink(f: &Frame, rgb: [u8; 3]) -> usize {
    f.rgba.chunks(4).filter(|p| p[..3] == rgb).count()
}

#[test]
fn linked_stylesheet_applies() {
    let html = format!(
        "<link rel=stylesheet href=/site.css>{PLAIN}<p class=big style='margin:0'>Styled</p><p class=gone>Gone</p>"
    );
    let doc = Document::parse(&html, &base());
    assert_eq!(doc.stylesheet_urls(), vec![Url::parse("https://example.org/site.css").unwrap()]);
    let plain = Layout::new(&doc, &Resources::new(), 400.0);
    let mut res = Resources::new();
    res.insert_stylesheet(
        &doc.stylesheet_urls()[0],
        "@import url(other.css); .big { color: #ff0000; font-size: 40px }\n.gone { display: none }",
    )
    .unwrap();
    let styled = Layout::new(&doc, &res, 400.0);
    assert!(text_color_ink(&styled.paint(0.0, 100.0, 1.0), [255, 0, 0]) > 50, "red text from the linked sheet");
    assert_eq!(text_color_ink(&plain.paint(0.0, 100.0, 1.0), [255, 0, 0]), 0);
    assert!(styled.height() < plain.height() + 30.0, "display:none removed the second paragraph");
    let big_only = layout(
        &format!("<style>.big {{ font-size: 40px }}</style>{PLAIN}<p class=big style='margin:0'>Styled</p>"),
        400.0,
    );
    assert_eq!(styled.height(), big_only.height());
}

#[test]
fn link_and_style_cascade_in_document_order() {
    let url = Url::parse("https://example.org/a.css").unwrap();
    let mut res = Resources::new();
    res.insert_stylesheet(&url, "p { color: #00ff00 }").unwrap();
    let red = |html: &str| {
        let l = Layout::new(&Document::parse(html, &base()), &res, 300.0);
        let f = l.paint(0.0, 60.0, 1.0);
        (text_color_ink(&f, [255, 0, 0]), text_color_ink(&f, [0, 255, 0]))
    };
    // The later sheet wins at equal specificity, whichever kind it is.
    let (r, g) = red("<link rel=stylesheet href=/a.css><style>p { color: #ff0000 }</style><p>Text</p>");
    assert!(r > 20 && g == 0, "{r} {g}");
    let (r, g) = red("<style>p { color: #ff0000 }</style><link rel=stylesheet href=/a.css><p>Text</p>");
    assert!(r == 0 && g > 20, "{r} {g}");
    // Specificity still beats order.
    let (r, g) = red("<style>body p { color: #ff0000 }</style><link rel=stylesheet href=/a.css><p>Text</p>");
    assert!(r > 20 && g == 0, "{r} {g}");
    // A link whose media doesn't match is skipped.
    let (r, g) = red("<style>p { color: #ff0000 }</style><link rel=stylesheet href=/a.css media=print><p>Text</p>");
    assert!(r > 20 && g == 0, "{r} {g}");
}

#[test]
fn media_min_width_depends_on_layout_width() {
    let html = format!(
        "<style>.wide {{ display: none }} @media screen and (min-width: 600px) {{ .wide {{ display: block }} .narrow {{ display: none }} }}</style>
         {PLAIN}<p class=wide style='margin:0'><a href=/wide>wide</a></p><p class=narrow style='margin:0'><a href=/narrow>narrow</a></p>"
    );
    let doc = Document::parse(&html, &base());
    let paths = |w: f32| -> Vec<String> {
        Layout::new(&doc, &Resources::new(), w).links().iter().map(|a| a.url.path().to_owned()).collect()
    };
    assert_eq!(paths(800.0), vec!["/wide"]);
    assert_eq!(paths(599.0), vec!["/narrow"]);
    // The fetch set never depends on the width.
    let linked = Document::parse("<link rel=stylesheet href=/p.css media='(max-width: 10px)'>", &base());
    assert_eq!(linked.stylesheet_urls().len(), 1);
}

#[test]
fn oversized_stylesheets_are_rejected() {
    use hollow_engine::StylesheetError;
    let mut res = Resources::new();
    let url = |i: usize| Url::parse(&format!("https://example.org/{i}.css")).unwrap();
    let huge = "p{color:red}".repeat(hollow_engine::MAX_STYLESHEET_BYTES / 12 + 1);
    assert_eq!(res.insert_stylesheet(&url(0), &huge), Err(StylesheetError::TooLarge));
    assert!(res.is_empty());
    let big = "p{color:red}".repeat((1 << 20) / 12);
    let mut accepted = 0;
    let mut last = Ok(());
    for i in 0..20 {
        last = res.insert_stylesheet(&url(i), &big);
        if last.is_ok() {
            accepted += 1;
        }
    }
    assert_eq!(last, Err(StylesheetError::BudgetExhausted));
    assert!((7..=8).contains(&accepted), "8 MiB total: {accepted}");
    // Replacing a sheet frees its share of the budget.
    assert_eq!(res.insert_stylesheet(&url(0), "p{}"), Ok(()));
    assert_eq!(res.insert_stylesheet(&url(19), "p{}"), Ok(()));
}

// ---------------------------------------------------------------------------
// Box model, flexbox, floats, positioning, clipping

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 128, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

fn frame(html: &str, width: f32, height: f32) -> Frame {
    layout(&format!("{PLAIN}{html}"), width).paint(0.0, height, 1.0)
}

#[test]
fn borders_and_heights() {
    let l = layout(
        &format!("{PLAIN}<div style='border:5px solid red; width:50px; height:20px; background:blue'></div>"),
        200.0,
    );
    assert_eq!(l.height(), 30.0, "height is the content box, borders add to it");
    let f = l.paint(0.0, 40.0, 1.0);
    assert_eq!(pixel(&f, 2, 2), RED);
    assert_eq!(pixel(&f, 58, 27), RED);
    assert_eq!(pixel(&f, 10, 10), BLUE);
    assert_eq!(pixel(&f, 60, 10), WHITE);
    // border-box sizing, per-side borders, and border-style none.
    let f = frame(
        "<div style='box-sizing:border-box; width:40px; height:40px; border-left:10px solid blue; border-top:3px none red'></div>",
        100.0,
        50.0,
    );
    assert_eq!(pixel(&f, 5, 20), BLUE);
    assert_eq!(pixel(&f, 15, 1), WHITE, "border-style none draws nothing");
    // min-height and max-height clamp; percentages need a definite parent.
    assert_eq!(layout(&format!("{PLAIN}<div style='min-height:70px'>x</div>"), 200.0).height(), 70.0);
    assert_eq!(layout(&format!("{PLAIN}<div style='height:500px; max-height:40px'></div>"), 200.0).height(), 40.0);
    let l = layout(
        &format!("{PLAIN}<div style='height:200px'><div style='height:25%; background:red'></div></div>"),
        200.0,
    );
    let f = l.paint(0.0, 200.0, 1.0);
    assert_eq!(pixel(&f, 10, 49), RED);
    assert_eq!(pixel(&f, 10, 50), WHITE);
}

#[test]
fn rounded_corners() {
    let f = frame("<div style='width:40px; height:40px; background:#0000ff; border-radius:20px'></div>", 60.0, 50.0);
    assert_eq!(pixel(&f, 20, 20), BLUE);
    assert_eq!(pixel(&f, 1, 1), WHITE, "the corner is cut off");
}

#[test]
fn calc_and_viewport_units() {
    let f = frame("<div style='width:calc(50% - 10px); height:10px; background:red'></div>", 200.0, 20.0);
    assert_eq!(pixel(&f, 89, 5), RED);
    assert_eq!(pixel(&f, 90, 5), WHITE);
    let f = frame("<div style='width:25vw; height:10px; background:red'></div>", 400.0, 20.0);
    assert_eq!(pixel(&f, 99, 5), RED);
    assert_eq!(pixel(&f, 100, 5), WHITE);
}

#[test]
fn flex_row_grows_and_aligns() {
    let item = |c: &str, extra: &str| format!("<div style='flex:1; height:20px; background:{c}; {extra}'></div>");
    let html = format!(
        "<div style='display:flex; width:300px'>{}{}{}</div>",
        item("red", ""),
        item("green", "flex-grow:2"),
        item("blue", "")
    );
    let f = frame(&html, 400.0, 30.0);
    assert_eq!(pixel(&f, 74, 10), RED);
    assert_eq!(pixel(&f, 76, 10), GREEN);
    assert_eq!(pixel(&f, 224, 10), GREEN);
    assert_eq!(pixel(&f, 226, 10), BLUE);
    assert_eq!(pixel(&f, 301, 10), WHITE);

    // justify-content and align-items; the container is as tall as its tallest item.
    let html = "<div style='display:flex; justify-content:space-between; align-items:center; width:200px'>\
                <div style='width:20px; height:60px; background:red'></div>\
                <div style='width:20px; height:20px; background:blue'></div></div>";
    let l = layout(&format!("{PLAIN}{html}"), 400.0);
    assert_eq!(l.height(), 60.0);
    let f = l.paint(0.0, 60.0, 1.0);
    assert_eq!(pixel(&f, 190, 30), BLUE, "pushed to the end and centered");
    assert_eq!(pixel(&f, 190, 15), WHITE);
    // Stretch is the default: an item without a height fills the line.
    let html = "<div style='display:flex'><div style='height:50px; width:10px'></div>\
                <div style='background:red; width:10px'></div></div>";
    assert_eq!(pixel(&frame(html, 100.0, 60.0), 15, 45), RED);
}

#[test]
fn flex_wraps_and_columns() {
    let item = "<div style='width:60px; height:10px; background:red; margin-right:10px'></div>";
    let l = layout(
        &format!("{PLAIN}<div style='display:flex; flex-wrap:wrap; width:150px'>{}</div>", item.repeat(4)),
        300.0,
    );
    assert_eq!(l.height(), 20.0, "two per line");
    let col = "<div style='display:flex; flex-direction:column; height:100px'>\
               <div style='height:10px; background:red'></div>\
               <div style='flex-grow:1; background:blue'></div></div>";
    let f = frame(col, 100.0, 120.0);
    assert_eq!(pixel(&f, 50, 5), RED);
    assert_eq!(pixel(&f, 50, 99), BLUE, "the second item fills the rest");
    assert_eq!(pixel(&f, 50, 101), WHITE);
    let html = "<div style='display:flex; flex-direction:row-reverse; width:200px'>\
                <div style='width:50px; height:10px; background:red'></div>\
                <div style='width:50px; height:10px; background:blue'></div></div>";
    let f = frame(html, 300.0, 20.0);
    assert_eq!(pixel(&f, 175, 5), RED);
    assert_eq!(pixel(&f, 125, 5), BLUE);
    // Text directly inside a flex container becomes an item of its own.
    let l = layout(&format!("{PLAIN}<div style='display:flex'>loose text<b>bold</b></div>"), 300.0);
    assert!(l.height() > 10.0 && l.height() < 30.0, "one line: {}", l.height());
}

#[test]
fn floats_and_clear() {
    let html = "<div style='float:left; width:100px; height:50px; background:red'></div>\
                <p style='margin:0'>Text flows beside the float</p>\
                <div style='clear:both; height:10px; background:blue'></div>";
    let f = frame(html, 400.0, 80.0);
    assert_eq!(pixel(&f, 50, 25), RED);
    assert!(ink(&f, 100, 0, 400, 20) > 30, "text starts right of the float");
    assert_eq!(pixel(&f, 50, 55), BLUE, "the cleared block starts below the float");
    let f = frame(
        "<div style='float:right; width:50px; height:20px; background:blue'></div>\
         <div style='float:right; width:50px; height:20px; background:red'></div>",
        200.0,
        30.0,
    );
    assert_eq!(pixel(&f, 175, 10), BLUE);
    assert_eq!(pixel(&f, 125, 10), RED, "right floats stack leftwards");
    // A formatting context root sits beside a float instead of under it.
    let f = frame(
        "<div style='float:left; width:50px; height:20px; background:red'></div>\
         <div style='overflow:hidden; height:20px; background:blue'></div>",
        200.0,
        30.0,
    );
    assert_eq!(pixel(&f, 25, 10), RED);
    assert_eq!(pixel(&f, 100, 10), BLUE);
    // A container that is its own formatting context grows to hold its floats.
    let html = "<div style='display:flow-root'><div style='float:left; height:40px; width:5px'></div></div>";
    assert_eq!(layout(&format!("{PLAIN}{html}"), 100.0).height(), 40.0);
}

#[test]
fn absolute_and_relative_positioning() {
    let html = "<div style='position:relative; width:200px; height:100px; margin-left:50px'>\
                <div style='position:absolute; top:10px; left:20px; width:30px; height:30px; background:red'></div>\
                <div style='position:absolute; right:0; bottom:0; width:10px; height:10px; background:blue'></div>\
                </div><p style='margin:0'>after</p>";
    let f = frame(html, 400.0, 130.0);
    assert_eq!(pixel(&f, 75, 15), RED);
    assert_eq!(pixel(&f, 69, 15), WHITE);
    assert_eq!(pixel(&f, 245, 95), BLUE);
    assert!(ink(&f, 0, 100, 100, 125) > 10, "absolute boxes take no room in the flow");
    let f = frame(
        "<div style='position:relative; left:30px; top:5px; width:10px; height:10px; background:red'></div>",
        100.0,
        20.0,
    );
    assert_eq!(pixel(&f, 35, 10), RED);
    assert_eq!(pixel(&f, 5, 2), WHITE);
    // Without offsets it stays where it would have been in flow.
    let f = frame(
        "<div style='height:30px'></div><div style='position:absolute; width:10px; height:10px; background:red'></div>",
        100.0,
        50.0,
    );
    assert_eq!(pixel(&f, 5, 35), RED);
    // left/right with auto margins centers.
    let f = frame(
        "<div style='position:absolute; left:0; right:0; margin:auto; width:20px; height:10px; background:red'></div>",
        100.0,
        20.0,
    );
    assert_eq!(pixel(&f, 50, 5), RED);
    assert_eq!(pixel(&f, 35, 5), WHITE);
}

#[test]
fn z_index_orders_painting() {
    let html = "<div style='position:relative'>\
                <div style='position:absolute; z-index:2; width:20px; height:20px; background:red'></div>\
                <div style='position:absolute; z-index:1; width:20px; height:20px; background:blue'></div></div>";
    assert_eq!(pixel(&frame(html, 100.0, 30.0), 10, 10), RED, "higher z-index paints on top");
}

#[test]
fn overflow_hidden_clips() {
    let html = "<div style='height:20px; overflow:hidden'><div style='height:100px; background:red'></div></div>";
    let f = frame(html, 100.0, 120.0);
    assert_eq!(pixel(&f, 50, 10), RED);
    assert_eq!(pixel(&f, 50, 30), WHITE, "clipped below the box");
    // A one-pixel "screen reader only" box shows nothing.
    let sr = "<p style='margin:0'>visible</p><span style='position:absolute; width:1px; height:1px; \
              overflow:hidden'>hidden screen reader text</span>";
    let f = frame(sr, 300.0, 60.0);
    assert_eq!(ink(&f, 0, 25, 300, 60), 0);
}

#[test]
fn inline_blocks_sit_on_the_line() {
    let html = "<div><span style='display:inline-block; width:40px; height:30px; background:red'></span> \
                <span style='display:inline-block; width:40px; height:30px; background:blue'></span></div>";
    let f = frame(html, 300.0, 50.0);
    assert_eq!(pixel(&f, 20, 15), RED);
    let blue_x = (0..300).find(|&x| pixel(&f, x, 15) == BLUE).expect("blue box");
    assert!((44..=50).contains(&blue_x), "a space apart: {blue_x}");
    // A block-level image without its resource keeps its explicit size.
    let l = layout(&format!("{PLAIN}<img src=/x.png style='display:block' width=40 height=30>"), 200.0);
    assert_eq!(l.height(), 30.0);
}

#[test]
fn nowrap_items_still_break_between_them() {
    let items = "<span style='white-space:nowrap'>one item</span> ".repeat(30);
    let l = layout(&format!("{PLAIN}<p style='margin:0'>{items}</p>"), 200.0);
    let f = l.paint(0.0, l.height(), 1.0);
    assert!(l.height() > 60.0, "wrapped onto several lines");
    assert_eq!(ink(&f, 200, 0, f.width, f.height), 0);
}

#[test]
fn hostile_layout_inputs() {
    let inputs: Vec<String> = vec![
        "<div style='display:flex'>".repeat(3_000) + "x",
        "<div style='float:left'>f</div>".repeat(5_000),
        "<div style='position:absolute'>".repeat(3_000) + "x",
        "<div style='position:relative'><span style='position:absolute'>a</span>".repeat(2_000),
        "<span style='display:inline-block'>".repeat(3_000) + "x",
        "<div style='display:flex;flex-wrap:wrap'>".to_string()
            + &"<div style='flex:1 1 1e9px;min-width:1e9px'></div>".repeat(2_000),
        "<div style='overflow:hidden;height:1px'>".repeat(2_000) + "x",
        "<div style='width:calc(100% - 1e30px);height:calc(1e30px * 1e30);margin:clamp(1px,1e40px,2px)'>x</div>"
            .to_string(),
        "<div style='display:flex;flex-direction:column;height:10px'>".to_string()
            + &"<p style='flex:1'>x</p>".repeat(5_000),
        "<div style='float:right;width:1e9px;clear:both'>x</div><p>y</p>".repeat(500),
    ];
    let handle = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            for html in &inputs {
                let l = layout(html, 800.0);
                assert!(l.height().is_finite() && l.height() >= 0.0);
                let f = l.paint(0.0, 600.0, 1.0);
                assert_eq!(f.rgba.len(), (f.width * f.height * 4) as usize);
            }
        })
        .unwrap();
    handle.join().expect("hostile layout input panicked or overflowed the stack");
}

// ---------------------------------------------------------------------------
// Generated content

const YELLOW: [u8; 4] = [255, 255, 0, 255];

#[test]
fn before_and_after_generate_boxes() {
    // Empty `content` boxes as decorations: block, absolute, floated.
    let html = "<style>
        .a::before { content: ''; display: block; height: 10px; background: red }
        .a::after { content: ''; display: block; height: 10px; background: blue }
        .p { position: relative; height: 30px }
        .p:before { content: ''; position: absolute; left: 50px; top: 5px; width: 10px; height: 10px; background: green }
        .f::before { content: ''; float: right; width: 20px; height: 20px; background: red }
        .n::before { content: none; display: block; height: 50px; background: red }
        </style>
        <div class=a></div><div class=p></div><div class=f></div><div class=n></div>";
    let f = frame(html, 200.0, 80.0);
    assert_eq!(pixel(&f, 5, 5), RED);
    assert_eq!(pixel(&f, 5, 15), BLUE);
    assert_eq!(pixel(&f, 55, 30), GREEN, "absolutely positioned against .p");
    assert_eq!(pixel(&f, 5, 30), WHITE);
    assert_eq!(pixel(&f, 190, 55), RED, "floated right");
    assert_eq!(ink(&f, 0, 70, 200, 80), 0, "content: none generates nothing");
}

#[test]
fn generated_text_counters_and_quotes() {
    let text = |css: &str, body: &str| {
        let l = layout(&format!("<style>{css}</style>{PLAIN}{body}"), 600.0);
        text_color_ink(&l.paint(0.0, 100.0, 1.0), [255, 0, 0])
    };
    let red = "p::before { color: #ff0000; font: bold 40px sans-serif }";
    assert!(text(&format!("{red} p::before {{ content: 'x' }}"), "<p></p>") > 10);
    assert!(text(&format!("{red} p::before {{ content: attr(data-x) }}"), "<p data-x=hello></p>") > 30);
    assert_eq!(text(&format!("{red} p::before {{ content: attr(data-y) }}"), "<p data-x=hello></p>"), 0);
    // Counters in generated text, with their own styles.
    let counted = text(
        &format!(
            "{red} body {{ counter-reset: n 1000 }} p {{ counter-increment: n }} p::before {{ content: counter(n, upper-roman) }}"
        ),
        "<p></p>",
    );
    let one = text(&format!("{red} p::before {{ content: 'M' }}"), "<p></p>");
    assert!(counted > 0 && counted >= one, "M (1001 is MI): {counted} {one}");
    // <q> gets its quotes from the UA sheet, in the element's color.
    let l =
        layout(&format!("{PLAIN}<p style='margin:0; color:#ff0000; font: bold 40px sans-serif'><q>q</q></p>"), 300.0);
    let with_quotes = text_color_ink(&l.paint(0.0, 40.0, 1.0), [255, 0, 0]);
    let l = layout(
        &format!(
            "<style>q {{ quotes: none }}</style>{PLAIN}<p style='margin:0; color:#ff0000; font: bold 40px sans-serif'><q>q</q></p>"
        ),
        300.0,
    );
    assert!(with_quotes > text_color_ink(&l.paint(0.0, 40.0, 1.0), [255, 0, 0]));
}

#[test]
fn css_triangles_have_diagonal_edges() {
    // The usual caret: one colored border between two transparent ones.
    let f = frame(
        "<div style='width:0; height:0; border-top:20px solid #0000ff; border-left:20px solid transparent; \
         border-right:20px solid transparent'></div>",
        100.0,
        30.0,
    );
    assert_eq!(pixel(&f, 20, 2), BLUE, "the wide end");
    assert_eq!(pixel(&f, 20, 17), BLUE, "the tip");
    assert_eq!(pixel(&f, 3, 17), WHITE, "cut off on the diagonal");
    assert_eq!(pixel(&f, 37, 17), WHITE);
}

// ---------------------------------------------------------------------------
// CSS tables

#[test]
fn display_table_cells_sit_side_by_side() {
    // Lone cells get an anonymous row and table around them.
    let html = "<div style='display:table-cell; width:40px; height:30px; background:red'></div>\
                <div style='display:table-cell; width:40px; background:blue'></div>";
    let f = frame(html, 200.0, 40.0);
    assert_eq!(pixel(&f, 20, 15), RED);
    assert_eq!(pixel(&f, 60, 25), BLUE, "the row's cells are as tall as its tallest");
    assert_eq!(pixel(&f, 100, 15), WHITE);
    // Vertical alignment in cells.
    let html = "<div style='display:table'><div style='display:table-row'>\
                <div style='display:table-cell; height:60px; width:10px'></div>\
                <div style='display:table-cell; vertical-align:middle'><div style='width:10px; height:10px; background:red'></div></div>\
                <div style='display:table-cell; vertical-align:bottom'><div style='width:10px; height:10px; background:blue'></div></div>\
                </div></div>";
    let f = frame(html, 100.0, 70.0);
    assert_eq!(pixel(&f, 15, 30), RED);
    assert_eq!(pixel(&f, 15, 5), WHITE);
    assert_eq!(pixel(&f, 25, 55), BLUE);
    // The clearfix: a table-like ::after clears the floats inside.
    let l = layout(
        &format!(
            "<style>.cf::after {{ content: ''; display: table; clear: both }}</style>{PLAIN}\
             <div class=cf><div style='float:left; height:50px; width:10px'></div></div>"
        ),
        100.0,
    );
    assert_eq!(l.height(), 50.0);
}

#[test]
fn table_features() {
    // border-spacing with two values, a caption below, and a table height
    // shared out among the rows.
    let html = "<div style='display:table; border-spacing:10px 4px; height:60px; background:#00ff00'>\
                <div style='display:table-caption; caption-side:bottom; height:10px; background:blue'></div>\
                <div style='display:table-row'><div style='display:table-cell; width:20px; background:red'></div></div>\
                </div>";
    let f = frame(html, 200.0, 100.0);
    assert_eq!(pixel(&f, 5, 30), [0, 255, 0, 255], "spacing shows the table background");
    assert_eq!(pixel(&f, 15, 30), RED, "the row fills the table's height");
    assert_eq!(pixel(&f, 15, 2), [0, 255, 0, 255]);
    assert_eq!(pixel(&f, 15, 65), BLUE, "the caption is below the 60px table");
    // Fixed layout: the first row's widths, whatever the content.
    let html = "<table style='table-layout:fixed; width:200px; border-spacing:0'><tr>\
                <td style='width:50px; padding:0; background:red'>xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx</td>\
                <td style='padding:0; background:blue'>y</td></tr></table>";
    let f = frame(html, 300.0, 30.0);
    assert_eq!(pixel(&f, 45, 3), RED);
    assert_eq!(pixel(&f, 55, 3), BLUE);
    // Collapsed borders: one line between cells instead of two.
    let cells = |collapse: &str| {
        let html = format!(
            "<table style='border-collapse:{collapse}; border-spacing:0'><tr>\
             <td style='border:2px solid #0000ff; padding:0; width:10px; height:10px'></td>\
             <td style='border:2px solid #0000ff; padding:0; width:10px; height:10px'></td></tr></table>"
        );
        let f = frame(&html, 100.0, 30.0);
        (0..100).filter(|&x| pixel(&f, x, 7) == BLUE).count()
    };
    assert_eq!(cells("separate"), 8);
    assert_eq!(cells("collapse"), 6);
}

// ---------------------------------------------------------------------------
// Backgrounds

/// A 1x1 yellow PNG (Acid2's), percent-encoded as Acid2 has it.
const YELLOW_PIXEL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4%2F58BAAT%2FAf9jgNErAAAAAElFTkSuQmCC";

#[test]
fn data_url_backgrounds_tile_and_position() {
    let html = format!(
        "<div style='width:40px; height:40px; background:red url({YELLOW_PIXEL})'></div>\
         <div style='width:40px; height:40px; background:url({YELLOW_PIXEL}) no-repeat right 10px bottom 5px / 10px 10px'></div>\
         <div style='width:40px; height:20px; background:url({YELLOW_PIXEL}) repeat-x 0 50% / 5px 5px'></div>"
    );
    let f = frame(&html, 100.0, 100.0);
    assert_eq!(pixel(&f, 20, 20), YELLOW, "repeats over the color");
    // 10x10 at right 10px, bottom 5px of the second box (y 40..80).
    assert_eq!(pixel(&f, 25, 70), YELLOW);
    assert_eq!(pixel(&f, 15, 70), WHITE);
    assert_eq!(pixel(&f, 25, 77), WHITE);
    // A 5px band through the middle of the third box (y 80..100).
    assert_eq!(pixel(&f, 35, 89), YELLOW);
    assert_eq!(pixel(&f, 35, 82), WHITE);
}

#[test]
fn background_images_come_from_resources() {
    let html = format!(
        "<style>.x {{ background-image: url(img/bg.png); background-size: cover; width: 50px; height: 20px }}
         @media (max-width: 10px) {{ .y {{ background: url(narrow.png) }} }}
         .unused {{ background: url(unused.png) }}</style>
         {PLAIN}<div class=x></div><div class=y></div><p style='background: url(\"/inline.png\")'>p</p>
         <link rel=stylesheet href=/s/site.css><div class=z style='width:20px; height:20px'></div>"
    );
    let doc = Document::parse(&html, &base());
    let urls: Vec<String> = doc.image_urls().iter().map(|u| u.to_string()).collect();
    // Media queries don't matter (the fetch set must not depend on the
    // viewport); rules for elements the page doesn't have do.
    assert_eq!(
        urls,
        ["https://example.org/dir/img/bg.png", "https://example.org/dir/narrow.png", "https://example.org/inline.png"]
    );
    let mut res = Resources::new();
    let sheet = Url::parse("https://example.org/s/site.css").unwrap();
    assert!(doc.stylesheet_image_urls(&res).is_empty(), "nothing before the sheet is in");
    res.insert_stylesheet(&sheet, ".z { background: url(z.png) } .gone { background: url(no.png) }").unwrap();
    let more = doc.stylesheet_image_urls(&res);
    assert_eq!(more, [Url::parse("https://example.org/s/z.png").unwrap()], "relative to the sheet");
    res.insert_image(&Url::parse(&urls[0]).unwrap(), &png(10, 10, [0, 0, 255, 255])).unwrap();
    res.insert_image(&more[0], &png(10, 10, [255, 0, 0, 255])).unwrap();
    let f = Layout::new(&doc, &res, 100.0).paint(0.0, 100.0, 1.0);
    assert_eq!(pixel(&f, 45, 15), BLUE, "covers the 50x20 box");
    let z = (0..100).find(|&y| pixel(&f, 5, y) == RED);
    assert!(z.is_some(), "the linked sheet's image is drawn");
}

#[test]
fn gradients_paint() {
    let html = "<div style='width:100px; height:10px; background:linear-gradient(to right, #ff0000, #0000ff)'></div>\
                <div style='width:100px; height:10px; background:linear-gradient(90deg, #ff0000 50%, #0000ff 50%)'></div>\
                <div style='width:40px; height:40px; background:radial-gradient(circle closest-side, #ff0000 50%, #0000ff 50%)'></div>\
                <div style='width:40px; height:10px; background:repeating-linear-gradient(to right, #ff0000 0 5px, #0000ff 5px 10px)'></div>";
    let f = frame(html, 120.0, 80.0);
    let [r0, _, b0, _] = pixel(&f, 1, 5);
    let [r1, _, b1, _] = pixel(&f, 98, 5);
    assert!(r0 > 240 && b0 < 15 && b1 > 240 && r1 < 15, "red to blue: {r0},{b0} {r1},{b1}");
    assert_eq!(pixel(&f, 45, 15), RED, "hard stop");
    assert_eq!(pixel(&f, 55, 15), BLUE);
    assert_eq!(pixel(&f, 20, 40), RED, "center of the circle");
    assert_eq!(pixel(&f, 2, 22), BLUE, "corner");
    assert_eq!(pixel(&f, 2, 65), RED, "stripes");
    assert_eq!(pixel(&f, 7, 65), BLUE);
    assert_eq!(pixel(&f, 12, 65), RED);
}

#[test]
fn background_clip_origin_and_layers() {
    let html = format!(
        "<div style='width:20px; height:20px; padding:10px; border:5px solid transparent; \
         background: url({YELLOW_PIXEL}) no-repeat content-box 0 0 / 5px 5px, #0000ff content-box'></div>"
    );
    let f = frame(&html, 100.0, 60.0);
    assert_eq!(pixel(&f, 2, 2), WHITE, "the color is clipped to the content box");
    assert_eq!(pixel(&f, 12, 12), WHITE);
    assert_eq!(pixel(&f, 17, 17), YELLOW, "the image layer is on top, at the content box");
    assert_eq!(pixel(&f, 30, 30), BLUE);
}

#[test]
fn data_stylesheets_and_object_images() {
    // A `data:` stylesheet is decoded in the engine, never fetched.
    let doc = Document::parse(
        "<link rel=stylesheet href='data:text/css,div%20%7Bbackground%3A%23ff0000%7D'>\
         <body style='margin:0'><div style='height:10px'></div>",
        &base(),
    );
    assert!(doc.stylesheet_urls().is_empty());
    let f = Layout::new(&doc, &Resources::new(), 50.0).paint(0.0, 20.0, 1.0);
    assert_eq!(pixel(&f, 5, 5), RED);
    // <object>: its image if it is one, otherwise its fallback content.
    let html = format!(
        "<object data='data:text/plain,nope'><object data='{YELLOW_PIXEL}' width=10 height=10>X</object></object>"
    );
    let f = frame(&html, 50.0, 20.0);
    assert!((0..20).any(|y| pixel(&f, 5, y) == YELLOW));
    assert_eq!(text_color_ink(&f, [0, 0, 0]), 0, "the fallback text isn't shown");
}

#[test]
fn floats_paint_over_block_backgrounds() {
    // CSS 2.1 Appendix E: floats paint after in-flow block backgrounds.
    let html = "<div style='float:left; width:20px; height:20px; background:#0000ff'></div>\
                <div style='height:20px; background:red'></div>";
    assert_eq!(pixel(&frame(html, 100.0, 30.0), 10, 10), BLUE);
}

#[test]
fn hostile_generated_content_tables_and_backgrounds() {
    let deep_counters = "<div style='counter-reset:c'>".repeat(400) + "x";
    let inputs: Vec<String> = vec![
        // Many pseudo-elements, each with a lot of content.
        format!(
            "<style>p::before {{ content: '{}' counters(c, '.') attr(id); counter-increment: c }} p::after {{ content: open-quote }}</style>{}",
            "x".repeat(10_000),
            "<p id=a>".repeat(5_000)
        ),
        format!("<style>div::before {{ content: counters(c, '----------'); counter-increment: c }}</style>{deep_counters}"),
        "<style>*::before, *::after { content: ''; display: table-cell }</style>".to_string() + &"<div>".repeat(3_000),
        "<div style='display:table-cell'>".repeat(3_000) + "x",
        "<div style='display:table-row'><span style='display:table-caption'>".repeat(1_000) + "x",
        format!("<body style='background:url({YELLOW_PIXEL}) 0 0 / 0.001px 0.001px; height: 100000px'>"),
        format!(
            "<div style='height:50000px; background:repeating-linear-gradient({}#f00 0 1px, #00f 1px 2px)'></div>",
            "#0f0 0px, ".repeat(60)
        ),
        "<div style='width:10px; height:10px; background-size: 1e30px 1e30px; background: radial-gradient(1e30px 1e-30px, red, blue)'></div>"
            .to_string(),
        format!("<div style='background:{}red'>x</div>", "url(data:,a) no-repeat, ".repeat(40)),
        "<table style='border-collapse:collapse; table-layout:fixed; width:1e9px; height:1e9px'><tr><td>x".to_string(),
    ];
    let handle = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            for html in &inputs {
                let doc = Document::parse(html, &base());
                let _ = doc.image_urls();
                let l = Layout::new(&doc, &Resources::new(), 800.0);
                assert!(l.height().is_finite() && l.height() >= 0.0);
                let f = l.paint(0.0, 600.0, 1.0);
                assert_eq!(f.rgba.len(), (f.width * f.height * 4) as usize);
            }
        })
        .unwrap();
    handle.join().expect("hostile input panicked or overflowed the stack");
}

#[test]
fn forms_are_clicked_typed_drawn_and_submitted() {
    use hollow_engine::{FormKey, FormMethod, Interaction};
    let html = format!(
        "{PLAIN}<form action='/find' method=post>\
         <input name=q placeholder='Search' style='display:block;width:200px;height:24px;margin:0'>\
         <input type=checkbox name=c style='display:block;width:16px;height:16px;margin:0'>\
         <select name=s style='display:block;margin:0'><option>first<option>second</select>\
         <button style='display:block;margin:0'>Go</button></form>"
    );
    let doc = Document::parse(&html, &base());
    let l = Layout::new(&doc, &Resources::new(), 400.0);
    let mut forms = doc.forms();
    // Placeholder text is drawn, gray, before anything is typed.
    let before = l.paint_forms(&forms, 0.0, 120.0, 1.0);
    assert!(ink(&before, 4, 4, 100, 22) > 20, "placeholder");

    let field = l.hit_test_forms(&forms, 50.0, 12.0);
    assert!(matches!(field, Some(HitTarget::Control(_))), "the text field is a control: {field:?}");
    assert_eq!(forms.click(&doc, field.as_ref()), Interaction::Repaint);
    assert!(forms.editing());
    forms.key(&doc, &FormKey::Text("WWWWWW".into()));
    let typed = l.paint_forms(&forms, 0.0, 120.0, 1.0);
    assert_ne!(typed, before, "typing changes the field");
    // The same layout painted without state still shows the page's own values.
    assert_eq!(l.paint(0.0, 120.0, 1.0), before);

    // Tick the checkbox: its inside fills in.
    let check = l.hit_test_forms(&forms, 8.0, 32.0);
    forms.click(&doc, check.as_ref());
    let ticked = l.paint_forms(&forms, 0.0, 120.0, 1.0);
    assert!(ink(&ticked, 4, 28, 12, 36) > ink(&typed, 4, 28, 12, 36));

    // Open the select, pick the second option from the floating list.
    let select = l.hit_test_forms(&forms, 10.0, 50.0);
    forms.click(&doc, select.as_ref());
    let open = l.paint_forms(&forms, 0.0, 120.0, 1.0);
    assert!(ink(&open, 0, 70, 120, 110) > ink(&ticked, 0, 70, 120, 110), "the list is drawn below the select");
    let mut picked = None;
    for y in 60..120 {
        if let Some(HitTarget::Option(_, 1)) = l.hit_test_forms(&forms, 10.0, y as f32) {
            picked = l.hit_test_forms(&forms, 10.0, y as f32);
            break;
        }
    }
    assert!(picked.is_some(), "the second option can be hit");
    forms.click(&doc, picked.as_ref());

    // Enter in the text field submits with every entry.
    forms.click(&doc, field.as_ref());
    match forms.key(&doc, &FormKey::Enter) {
        Interaction::Submit(s) => {
            assert_eq!(s.action.as_str(), "https://example.org/find");
            assert_eq!(s.method, FormMethod::Post);
            let pairs: Vec<(&str, &str)> = s.entries.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            assert_eq!(pairs, [("q", "WWWWWW"), ("c", "on"), ("s", "second")]);
        }
        other => panic!("expected a submission, got {other:?}"),
    }
}

#[test]
fn inline_buttons_and_submit_inputs_are_controls() {
    let html = format!("{PLAIN}<form><button>Go now</button> <input type=submit value=Send></form>");
    let doc = Document::parse(&html, &base());
    let l = Layout::new(&doc, &Resources::new(), 400.0);
    let forms = doc.forms();
    let hits: Vec<bool> =
        (0..200).map(|x| matches!(l.hit_test_forms(&forms, x as f32, 10.0), Some(HitTarget::Control(_)))).collect();
    assert!(hits[20], "the button's label is clickable");
    assert!(hits.iter().skip(80).any(|h| *h), "the submit input is clickable");
}
