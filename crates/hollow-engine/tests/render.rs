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
