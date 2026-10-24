//! Reads `reg export` files (the "Windows Registry Editor Version 5.00" format, UTF-16LE).
//!
//! `reg export` is used instead of `reg query /s` because it is about 100x faster (under a second
//! for all of HKCU\Software, against ~95 s), writes real Unicode instead of the console code page,
//! and keeps binary values as hex, which lets the trace test look for UTF-16 text inside them.

use std::collections::BTreeMap;

/// One registry entry: a key (`value: None`) or one of its values (`Some("")` is the default
/// value, which `reg export` writes as `@`).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct RegPath {
    pub key: String,
    pub value: Option<String>,
}

impl RegPath {
    pub fn display(&self) -> String {
        match &self.value {
            None => self.key.clone(),
            Some(v) if v.is_empty() => format!("{} [(default)]", self.key),
            Some(v) => format!("{} [{v}]", self.key),
        }
    }
}

/// Entry -> the value exactly as exported (`"text"`, `dword:00000001`, `hex(7):41,00,...`).
/// Keys map to an empty string.
pub type RegSnapshot = BTreeMap<RegPath, String>;

pub fn decode_utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    let text = String::from_utf16_lossy(&units);
    match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    }
}

pub fn parse(text: &str) -> RegSnapshot {
    let mut out = RegSnapshot::new();
    let mut key: Option<String> = None;
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.starts_with('[') && line.ends_with(']') && line.len() >= 2 {
            let k = line[1..line.len() - 1].to_string();
            out.insert(RegPath { key: k.clone(), value: None }, String::new());
            key = Some(k);
            continue;
        }
        let Some(k) = &key else { continue };
        let (name, rest) = if let Some(rest) = line.strip_prefix("@=") {
            (String::new(), rest)
        } else if line.starts_with('"') {
            match parse_quoted(line) {
                Some((name, after)) => match after.strip_prefix('=') {
                    Some(rest) => (name, rest),
                    None => continue,
                },
                None => continue,
            }
        } else {
            continue;
        };
        let mut data = rest.to_string();
        if data.starts_with('"') {
            // A REG_SZ with an embedded line break continues on the next line.
            while parse_quoted(&data).is_none() {
                match lines.next() {
                    Some(l) => {
                        data.push('\n');
                        data.push_str(l);
                    }
                    None => break,
                }
            }
        } else {
            // Hex values are wrapped at ~80 columns with a trailing backslash.
            while data.ends_with('\\') {
                data.pop();
                match lines.next() {
                    Some(l) => data.push_str(l.trim_start()),
                    None => break,
                }
            }
        }
        out.insert(RegPath { key: k.clone(), value: Some(name) }, data);
    }
    out
}

/// Parses a `"..."` string with `\\` and `\"` escapes at the start of `s`; returns the unescaped
/// text and what follows the closing quote.
fn parse_quoted(s: &str) -> Option<(String, &str)> {
    let body = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                if let Some((_, next)) = chars.next() {
                    out.push(next);
                }
            }
            '"' => return Some((out, &body[i + 1..])),
            c => out.push(c),
        }
    }
    None
}

/// The value as text, if it is a plain string (REG_SZ).
pub fn string_value(raw: &str) -> Option<String> {
    if raw.starts_with('"') { parse_quoted(raw).map(|(s, _)| s) } else { None }
}

/// The value as text to search in. Strings are unescaped. Hex values (binary, REG_EXPAND_SZ,
/// REG_MULTI_SZ, ...) are decoded as UTF-16LE at both byte alignments and as ASCII, because shell
/// records (RecentDocs, ShellBags, jump lists) embed UTF-16 paths at arbitrary offsets.
pub fn searchable(raw: &str) -> String {
    if let Some(s) = string_value(raw) {
        return s;
    }
    let Some(hex) = hex_payload(raw) else { return raw.to_string() };
    let bytes: Vec<u8> = hex.split(',').filter_map(|b| u8::from_str_radix(b.trim(), 16).ok()).collect();
    let mut out = utf16_text(&bytes);
    out.push('\n');
    out.push_str(&utf16_text(bytes.get(1..).unwrap_or_default()));
    out.push('\n');
    out.extend(bytes.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '\n' }));
    out
}

/// `hex:..` / `hex(N):..` -> the comma-separated bytes.
fn hex_payload(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix("hex")?;
    if let Some(r) = rest.strip_prefix(':') {
        return Some(r);
    }
    let close = rest.find("):")?;
    rest.starts_with('(').then(|| &rest[close + 2..])
}

/// UTF-16LE text with NULs and unpaired surrogates turned into line breaks (string boundaries).
fn utf16_text(bytes: &[u8]) -> String {
    let units = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c));
    char::decode_utf16(units).map(|c| c.ok().filter(|&c| c != '\0').unwrap_or('\n')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Windows Registry Editor Version 5.00\r\n\
        \r\n\
        [HKEY_CURRENT_USER\\Software\\A]\r\n\
        @=\"default\"\r\n\
        \"Path\"=\"C:\\\\Users\\\\x \\\"q\\\"\"\r\n\
        \"Count\"=dword:00000002\r\n\
        \"Bin\"=hex:68,00,6f,00,6c,00,6c,00,6f,00,77,00,\\\r\n\
          2e,00,65,00,78,00,65,00,00,00\r\n\
        \"Two\"=\"line one\r\n\
        [not a key]\"\r\n\
        \r\n\
        [HKEY_CURRENT_USER\\Software\\A\\B]\r\n\
        \"Multi\"=hex(7):61,00,00,00,62,00,00,00,00,00\r\n";

    #[test]
    fn parses_reg_export() {
        let snap = parse(SAMPLE);
        let key = |k: &str| RegPath { key: k.to_string(), value: None };
        let val = |k: &str, v: &str| RegPath { key: k.to_string(), value: Some(v.to_string()) };
        let a = "HKEY_CURRENT_USER\\Software\\A";
        assert!(snap.contains_key(&key(a)));
        assert!(snap.contains_key(&key("HKEY_CURRENT_USER\\Software\\A\\B")));
        assert_eq!(snap[&val(a, "")], "\"default\"");
        assert_eq!(string_value(&snap[&val(a, "Path")]).unwrap(), "C:\\Users\\x \"q\"");
        assert_eq!(snap[&val(a, "Count")], "dword:00000002");
        // Continuation lines are joined, and a multi-line string doesn't start a new key.
        assert!(snap[&val(a, "Bin")].ends_with("2e,00,65,00,78,00,65,00,00,00"));
        assert_eq!(string_value(&snap[&val(a, "Two")]).unwrap(), "line one\n[not a key]");
        assert!(!snap.keys().any(|p| p.key == "not a key"));
        assert_eq!(snap.len(), 8);
    }

    #[test]
    fn searchable_decodes_hex() {
        let snap = parse(SAMPLE);
        let bin = &snap[&RegPath { key: "HKEY_CURRENT_USER\\Software\\A".into(), value: Some("Bin".into()) }];
        assert!(searchable(bin).contains("hollow.exe"));
        // UTF-16 at an odd offset inside a binary blob.
        assert!(searchable("hex:01,68,00,69,00").contains("hi"));
        assert!(searchable("hex(7):61,00,00,00,62,00").contains("a\nb"));
        assert!(searchable("hex:41,42,43").contains("ABC"));
        assert_eq!(searchable("dword:00000001"), "dword:00000001");
    }

    #[test]
    fn utf16_bom() {
        let bytes: Vec<u8> = "\u{feff}[K]".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_utf16(&bytes), "[K]");
    }
}
