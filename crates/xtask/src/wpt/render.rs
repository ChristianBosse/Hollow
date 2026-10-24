//! Rendering one WPT file with the engine and comparing two renders.

use std::path::Path;

use hollow_engine::{Document, Layout, Resources};

use super::{Fuzzy, base_url, local_path, read_source};

pub const WIDTH: u32 = 800;
pub const HEIGHT: u32 = 600;
/// Without a fuzzy annotation, a pixel counts as different when a channel differs by more than
/// this (the engine's antialiasing is not the browsers'), and a match allows this many of them.
const CHANNEL_TOLERANCE: u8 = 8;
const PIXEL_TOLERANCE: u32 = 40;

/// The top 800x600 CSS px of `rel` as RGBA (the canvas colour below the content), with its
/// stylesheets and images loaded from the checkout.
pub fn render(root: &Path, rel: &str) -> anyhow::Result<Vec<u8>> {
    let src = read_source(root, rel)?;
    let doc = Document::parse(&src, &base_url(rel));
    let mut resources = Resources::new();
    let read = |url: &url::Url| local_path(url).and_then(|p| std::fs::read(root.join(p)).ok());
    for url in doc.stylesheet_urls() {
        if let Some(bytes) = read(&url) {
            let css = String::from_utf8_lossy(&bytes);
            // Rejected sheets are simply not applied, as in the browser.
            let _ = resources.insert_stylesheet(&url, css.strip_prefix('\u{feff}').unwrap_or(&css));
        }
    }
    for url in doc.image_urls() {
        if let Some(bytes) = read(&url) {
            let _ = resources.insert_image(&url, &bytes);
        }
    }
    let layout = Layout::new(&doc, &resources, WIDTH as f32);
    let frame = layout.paint(0.0, HEIGHT as f32, 1.0);
    anyhow::ensure!(frame.width == WIDTH && frame.height == HEIGHT, "frame is {}x{}", frame.width, frame.height);
    Ok(frame.rgba)
}

/// Writes an RGBA render as a PNG.
pub fn save_png(path: &Path, rgba: &[u8]) -> anyhow::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, WIDTH, HEIGHT);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Diff {
    /// Largest difference in any channel.
    pub max: u8,
    /// Pixels that differ at all.
    pub differing: u32,
    /// Pixels with a channel differing by more than [`CHANNEL_TOLERANCE`].
    pub over_tolerance: u32,
}

pub fn diff(a: &[u8], b: &[u8]) -> Diff {
    let mut d = Diff::default();
    // Opaque frames: compare R, G and B.
    for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let m = (0..3).map(|i| p[i].abs_diff(q[i])).max().unwrap_or(0);
        if m > 0 {
            d.differing += 1;
            d.max = d.max.max(m);
            if m > CHANNEL_TOLERANCE {
                d.over_tolerance += 1;
            }
        }
    }
    d
}

/// Whether two renders count as equal: within the test's fuzzy ranges as wptrunner checks them
/// when it has some, else within Hollow's default tolerance.
pub fn equal(d: &Diff, fuzzy: Option<Fuzzy>) -> bool {
    match fuzzy {
        None => d.over_tolerance <= PIXEL_TOLERANCE,
        Some(f) => {
            let (max, total) = (u32::from(d.max), d.differing);
            (total == 0 && f.total_pixels.0 == 0)
                || (max == 0 && f.max_difference.0 == 0)
                || ((f.max_difference.0..=f.max_difference.1).contains(&max)
                    && (f.total_pixels.0..=f.total_pixels.1).contains(&total))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerance_and_fuzzy() {
        let d = Diff { max: 20, differing: 100, over_tolerance: 30 };
        assert!(equal(&d, None));
        assert!(!equal(&Diff { over_tolerance: 41, ..d }, None));
        assert!(equal(&d, Some(Fuzzy { max_difference: (0, 20), total_pixels: (0, 100) })));
        assert!(!equal(&d, Some(Fuzzy { max_difference: (0, 19), total_pixels: (0, 100) })));
        assert!(!equal(&d, Some(Fuzzy { max_difference: (0, 20), total_pixels: (0, 99) })));
        // An exact match passes any range starting at 0.
        assert!(equal(&Diff::default(), Some(Fuzzy { max_difference: (0, 2), total_pixels: (5, 9) })));
        let a = [10, 10, 10, 255, 0, 0, 0, 255];
        let b = [10, 19, 10, 255, 0, 0, 1, 255];
        let d = diff(&a, &b);
        assert_eq!((d.max, d.differing, d.over_tolerance), (9, 2, 1));
    }
}
