//! Dev tool: render an HTML file to a PNG.
//!
//! ```text
//! cargo run -p hollow-engine --example render -- in.html out.png [width] [scale] [height] [scroll]
//! ```
//!
//! Images and stylesheets the page asks for are looked up next to the HTML
//! file by the last segment of their URL (so `https://x/img/cat.png` loads
//! `./cat.png`), which stands in for the broker fetching them. A
//! `manifest.tsv` next to the HTML (`<absolute url> TAB <file>` lines, plus
//! `PAGE TAB <page url>`) maps exact URLs instead. `height` paints only that much
//! of the page, starting `scroll` CSS px down.

use std::path::Path;
use std::process::ExitCode;

use hollow_engine::{Document, Layout, Resources};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: render <in.html> <out.png> [width=1024] [scale=1] [height=all] [scroll=0]");
        return ExitCode::from(2);
    }
    let width: f32 = args.get(2).and_then(|w| w.parse().ok()).unwrap_or(1024.0);
    let scale: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let max_height: f32 = args.get(4).and_then(|h| h.parse().ok()).unwrap_or(f32::INFINITY);
    let scroll: f32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let input = Path::new(&args[0]);
    let html = match std::fs::read(input) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            eprintln!("render: {}: {e}", input.display());
            return ExitCode::FAILURE;
        }
    };
    let dir = input.parent().unwrap_or(Path::new("."));
    // A saved page: manifest.tsv maps absolute URLs to files next to it, and
    // `PAGE <url>` gives the page's own URL.
    let manifest: Vec<(String, String)> = std::fs::read_to_string(dir.join("manifest.tsv"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(u, f)| (u.to_owned(), f.to_owned())))
        .collect();
    let page = manifest.iter().find(|(k, _)| k == "PAGE").map_or("https://example.org/", |(_, u)| u.as_str());
    let base = url::Url::parse(page).expect("page URL");
    let started = std::time::Instant::now();
    let doc = Document::parse(&html, &base);
    let parsed = started.elapsed();
    let mut resources = Resources::new();
    let local = |url: &url::Url| {
        if let Some((_, file)) = manifest.iter().find(|(u, _)| u == url.as_str()) {
            return std::fs::read(dir.join(file)).ok();
        }
        let name = url.path_segments().and_then(|mut s| s.next_back()).filter(|n| !n.is_empty())?;
        std::fs::read(dir.join(name)).ok()
    };
    for url in doc.stylesheet_urls() {
        if let Some(bytes) = local(&url) {
            let result = resources.insert_stylesheet(&url, &String::from_utf8_lossy(&bytes));
            eprintln!("stylesheet {url}: {result:?}");
        }
    }
    // Images used by the linked stylesheets are only known once those are in.
    for url in doc.image_urls().into_iter().chain(doc.stylesheet_image_urls(&resources)) {
        if let Some(bytes) = local(&url) {
            match resources.insert_image(&url, &bytes) {
                Ok((w, h)) => eprintln!("image {url}: {w}x{h}"),
                Err(e) => eprintln!("image {url}: {e}"),
            }
        }
    }
    let laid = std::time::Instant::now();
    let layout = Layout::new(&doc, &resources, width);
    let laid_out = laid.elapsed();
    let painted = std::time::Instant::now();
    let frame = layout.paint(scroll, (layout.height() - scroll).max(1.0).min(max_height), scale);
    eprintln!(
        "title: {:?}, height: {} css px, frame {}x{}, parse {:?}, layout {:?}, paint {:?}, total {:?}",
        doc.title(),
        layout.height(),
        frame.width,
        frame.height,
        parsed,
        laid_out,
        painted.elapsed(),
        started.elapsed()
    );
    if let Err(e) = write_png(Path::new(&args[1]), &frame) {
        eprintln!("render: {}: {e}", args[1]);
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn write_png(path: &Path, frame: &hollow_engine::Frame) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, frame.width, frame.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&frame.rgba)?;
    Ok(())
}
