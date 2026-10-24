//! `data:` URLs (RFC 2397), decoded inside the engine: they are part of the
//! page, so they never become requests.

/// Longest `data:` URL the engine accepts anywhere.
pub(crate) const MAX_DATA_URL_BYTES: usize = 1 << 20;

/// The media type (lowercase, without parameters; empty if none was given)
/// and the decoded bytes of a `data:` URL. `None` for anything else, for
/// malformed base64, and for URLs over [`MAX_DATA_URL_BYTES`].
pub(crate) fn decode(url: &str) -> Option<(String, Vec<u8>)> {
    if url.len() > MAX_DATA_URL_BYTES {
        return None;
    }
    let rest = url.get(..5).filter(|p| p.eq_ignore_ascii_case("data:")).map(|_| &url[5..])?;
    let (meta, data) = rest.split_once(',')?;
    let mut params = meta.split(';');
    let mime = params.next().unwrap_or("").trim().to_ascii_lowercase();
    let base64 = params.any(|p| p.trim().eq_ignore_ascii_case("base64"));
    let bytes = percent_decode(data);
    let bytes = if base64 { base64_decode(&bytes)? } else { bytes };
    Some((mime, bytes))
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c)))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Forgiving base64 (as browsers decode `data:` URLs): ASCII whitespace is
/// skipped and padding is optional, but any other stray byte fails.
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0;
    let mut padding = 0;
    for &c in input {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => {
                padding += 1;
                continue;
            }
            c if c.is_ascii_whitespace() => continue,
            _ => return None,
        };
        if padding > 0 {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (padding <= 2).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_and_percent_encoding() {
        assert_eq!(decode("data:text/plain;base64,SGVsbG8="), Some(("text/plain".into(), b"Hello".to_vec())));
        assert_eq!(decode("DATA:;BASE64,SGVsbG8"), Some(("".into(), b"Hello".to_vec())));
        // Acid2 percent-encodes its base64.
        assert_eq!(decode("data:image/png;base64,SGVs%62G8%3D").unwrap().1, b"Hello");
        assert_eq!(decode("data:text/css,p%20%7B%20color%3A%20red%20%7D").unwrap().1, b"p { color: red }");
        assert_eq!(decode("data:text/css;charset=utf-8,a").unwrap().0, "text/css");
        assert_eq!(decode("data:,").unwrap().1, b"");
        assert!(decode("data:;base64,SGV*sbG8=").is_none());
        assert!(decode("data:;base64,SG=Vs").is_none());
        assert!(decode("https://example.org/").is_none());
        assert!(decode("data:no-comma").is_none());
        let huge = format!("data:,{}", "a".repeat(MAX_DATA_URL_BYTES));
        assert!(decode(&huge).is_none());
    }
}
