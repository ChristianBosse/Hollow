//! Image decoding, with hard size limits checked before any pixel buffer is
//! allocated.
//!
//! Only pure-Rust decoders are used, and the JPEG decoder is built without
//! its SIMD paths: lossy decoders are where CPU-dependent code paths could
//! produce different pixels on different machines.

use std::io::Cursor;

use tiny_skia::{IntSize, Pixmap};

/// Largest image we will decode, in pixels (and per side).
pub(crate) const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_SIDE: u32 = 16_384;

/// Why an image was not accepted. The page renders without it either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageError {
    /// Not PNG, JPEG, GIF, WebP or SVG.
    Unsupported,
    /// Corrupt or truncated data.
    Decode,
    /// Over the per-image pixel limit.
    TooLarge,
    /// The document's total image budget is used up.
    BudgetExhausted,
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ImageError::Unsupported => "unsupported image format",
            ImageError::Decode => "image could not be decoded",
            ImageError::TooLarge => "image is too large",
            ImageError::BudgetExhausted => "image budget exhausted",
        })
    }
}

impl std::error::Error for ImageError {}

fn check_size(w: u32, h: u32) -> Result<(), ImageError> {
    if w == 0 || h == 0 {
        return Err(ImageError::Decode);
    }
    if w > MAX_IMAGE_SIDE || h > MAX_IMAGE_SIDE || u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        return Err(ImageError::TooLarge);
    }
    Ok(())
}

/// Decodes to a premultiplied RGBA pixmap, also saying whether the image
/// has only a shape and no size of its own (an SVG with just a `viewBox`).
pub(crate) fn decode_sized(bytes: &[u8]) -> Result<(Pixmap, bool), ImageError> {
    if crate::svg::looks_like_svg(bytes) {
        return crate::svg::render_sized(bytes);
    }
    let (w, h, rgba) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png(bytes)?
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        decode_jpeg(bytes)?
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        decode_gif(bytes)?
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        decode_webp(bytes)?
    } else {
        return Err(ImageError::Unsupported);
    };
    to_pixmap(w, h, rgba).map(|p| (p, false))
}

fn to_pixmap(w: u32, h: u32, mut rgba: Vec<u8>) -> Result<Pixmap, ImageError> {
    let len = (w as usize) * (h as usize) * 4;
    if rgba.len() < len {
        return Err(ImageError::Decode);
    }
    rgba.truncate(len);
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u16::from(px[3]);
        if a < 255 {
            for c in &mut px[..3] {
                *c = ((u16::from(*c) * a + 127) / 255) as u8;
            }
        }
    }
    let size = IntSize::from_wh(w, h).ok_or(ImageError::Decode)?;
    Pixmap::from_vec(rgba, size).ok_or(ImageError::Decode)
}

fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ImageError> {
    let limits = png::Limits { bytes: (MAX_IMAGE_PIXELS as usize) * 8 };
    let mut decoder = png::Decoder::new_with_limits(Cursor::new(bytes), limits);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|_| ImageError::Decode)?;
    // png 0.18.1 panics (slice out of range) expanding a PLTE whose length
    // isn't a whole number of RGB entries; such a palette is invalid anyway.
    // PLTE must come before IDAT, so read_info has seen it.
    if reader.info().palette.as_ref().is_some_and(|p| p.len() % 3 != 0) {
        return Err(ImageError::Decode);
    }
    let (w, h) = (reader.info().width, reader.info().height);
    check_size(w, h)?;
    let size = reader.output_buffer_size().ok_or(ImageError::TooLarge)?;
    let mut buf = vec![0; size];
    let frame = reader.next_frame(&mut buf).map_err(|_| ImageError::Decode)?;
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight {
        return Err(ImageError::Unsupported);
    }
    let stride = frame.line_size;
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for row in buf.chunks(stride).take(h as usize) {
        match color {
            png::ColorType::Rgba => out.extend_from_slice(&row[..(w as usize * 4).min(row.len())]),
            png::ColorType::Rgb => {
                for p in row.as_chunks::<3>().0.iter().take(w as usize) {
                    out.extend_from_slice(&[p[0], p[1], p[2], 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for p in row.as_chunks::<2>().0.iter().take(w as usize) {
                    out.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
                }
            }
            png::ColorType::Grayscale => {
                for &g in row.iter().take(w as usize) {
                    out.extend_from_slice(&[g, g, g, 255]);
                }
            }
            png::ColorType::Indexed => return Err(ImageError::Unsupported),
        }
    }
    Ok((w, h, out))
}

fn decode_jpeg(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ImageError> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let options = DecoderOptions::default()
        .set_max_width(MAX_IMAGE_SIDE as usize)
        .set_max_height(MAX_IMAGE_SIDE as usize)
        .set_use_unsafe(false)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(|_| ImageError::Decode)?;
    let (w, h) = decoder.dimensions().ok_or(ImageError::Decode)?;
    let (w, h) =
        (u32::try_from(w).map_err(|_| ImageError::TooLarge)?, u32::try_from(h).map_err(|_| ImageError::TooLarge)?);
    check_size(w, h)?;
    let pixels = decoder.decode().map_err(|_| ImageError::Decode)?;
    Ok((w, h, pixels))
}

fn decode_gif(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ImageError> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    if let Some(limit) = std::num::NonZeroU64::new(MAX_IMAGE_PIXELS * 4) {
        opts.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    }
    let mut decoder = opts.read_info(Cursor::new(bytes)).map_err(|_| ImageError::Decode)?;
    let (w, h) = (u32::from(decoder.width()), u32::from(decoder.height()));
    check_size(w, h)?;
    let mut canvas = vec![0u8; (w as usize) * (h as usize) * 4];
    // Animation is never played: the first frame is the image.
    if let Some(frame) = decoder.read_next_frame().map_err(|_| ImageError::Decode)? {
        let (fx, fy) = (usize::from(frame.left), usize::from(frame.top));
        let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
        for row in 0..fh {
            let y = fy + row;
            if y >= h as usize {
                break;
            }
            for col in 0..fw {
                let x = fx + col;
                if x >= w as usize {
                    break;
                }
                let src = (row * fw + col) * 4;
                let dst = (y * w as usize + x) * 4;
                if let (Some(s), Some(d)) = (frame.buffer.get(src..src + 4), canvas.get_mut(dst..dst + 4)) {
                    d.copy_from_slice(s);
                }
            }
        }
    }
    Ok((w, h, canvas))
}

fn decode_webp(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ImageError> {
    let mut decoder = image_webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|_| ImageError::Decode)?;
    let (w, h) = decoder.dimensions();
    check_size(w, h)?;
    let alpha = decoder.has_alpha();
    let size = decoder.output_buffer_size().ok_or(ImageError::TooLarge)?;
    let mut buf = vec![0; size];
    decoder.read_image(&mut buf).map_err(|_| ImageError::Decode)?;
    if alpha {
        return Ok((w, h, buf));
    }
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for p in buf.as_chunks::<3>().0 {
        out.extend_from_slice(&[p[0], p[1], p[2], 255]);
    }
    Ok((w, h, out))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Result<Pixmap, ImageError> {
        decode_sized(bytes).map(|(p, _)| p)
    }

    /// Encodes a solid-color RGBA PNG for tests.
    pub(crate) fn png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            let data: Vec<u8> = (0..w * h).flat_map(|_| rgba).collect();
            writer.write_image_data(&data).unwrap();
        }
        out
    }

    #[test]
    fn decodes_png_and_premultiplies() {
        let pm = decode(&png(3, 2, [255, 0, 0, 128])).unwrap();
        assert_eq!((pm.width(), pm.height()), (3, 2));
        assert_eq!(&pm.data()[..4], &[128, 0, 0, 128]);
    }

    #[test]
    fn rejects_garbage_and_oversized() {
        assert_eq!(decode(b"hello").unwrap_err(), ImageError::Unsupported);
        assert_eq!(decode(b"\x89PNG\r\n\x1a\nbroken").unwrap_err(), ImageError::Decode);
        assert_eq!(decode(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]).unwrap_err(), ImageError::Decode);
        assert_eq!(decode(b"GIF89a\x01").unwrap_err(), ImageError::Decode);
        // A valid header claiming 20000x20000 must be refused before allocating.
        let mut huge = png(1, 1, [0, 0, 0, 255]);
        huge[16..20].copy_from_slice(&20_000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&20_000u32.to_be_bytes());
        assert!(decode(&huge).is_err());
    }

    /// One PNG chunk: length, type, data, CRC.
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut crc = !0u32;
        for &b in kind.iter().chain(data) {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        [&(data.len() as u32).to_be_bytes()[..], kind, data, &(!crc).to_be_bytes()].concat()
    }

    /// A 1x1 8-bit indexed PNG with the given PLTE contents.
    fn indexed_png(palette: &[u8]) -> Vec<u8> {
        // zlib, one stored block: filter byte 0, palette index 0.
        let idat = [0x78, 0x01, 0x01, 0x02, 0x00, 0xFD, 0xFF, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01];
        [
            &b"\x89PNG\r\n\x1a\n"[..],
            &chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 3, 0, 0, 0]),
            &chunk(b"PLTE", palette),
            &chunk(b"IDAT", &idat),
            &chunk(b"IEND", &[]),
        ]
        .concat()
    }

    #[test]
    fn palette_with_a_partial_entry_is_rejected() {
        let pm = decode(&indexed_png(&[255, 0, 0])).unwrap();
        assert_eq!(pm.data(), &[255, 0, 0, 255]);
        // Found by fuzzing (SEC-07): png 0.18.1 panicked expanding a PLTE
        // whose length isn't a multiple of 3.
        assert_eq!(decode(&indexed_png(&[255, 0, 0, 7])).unwrap_err(), ImageError::Decode);
    }
}
