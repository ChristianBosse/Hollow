//! Input generation for `cargo xtask fuzz`: a seeded PRNG and mutations,
//! generic (bytes, integers, splices, dictionary tokens) and
//! structure-aware (HTML/CSS tokens, numbers, nesting, PNG checksums).
//!
//! Input `i` is a pure function of the seed, `i`, the corpus and the
//! dictionary, never of earlier results (there is no coverage feedback). So
//! the supervisor can rebuild the input a child process died on from its
//! number alone.

use hollow_fuzz_targets::Kind;

/// SplitMix64: tiny, fast, and the same sequence on every machine.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (0 if `n` is 0).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }

    /// True one time in `n`.
    pub fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// A length skewed towards small values: 1, 2, 4, ... up to `max`.
    fn len(&mut self, max: usize) -> usize {
        let max = max.max(1);
        let cap = 1usize << self.below(max.ilog2() as usize + 1);
        1 + self.below(cap.min(max))
    }
}

/// What inputs are made from.
pub struct Material {
    pub kind: Kind,
    pub corpus: Vec<Vec<u8>>,
    pub dict: Vec<Vec<u8>>,
    pub max_len: usize,
}

const INTERESTING_BYTES: &[u8] = b"\x00\x01\x7f\x80\xff0189-+.<>/=\"'&;#{}()[]:,!@%*\\ \t\n\r\x0c";

const INTERESTING_INTS: &[u64] = &[
    0,
    1,
    2,
    7,
    8,
    16,
    0x7f,
    0x80,
    0xff,
    0x100,
    0x3fff,
    0x4000,
    0x7fff,
    0x8000,
    0xffff,
    0x1_0000,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_ffff,
    0x1_0000_0000,
    u64::MAX / 2,
    u64::MAX,
];

const INTERESTING_NUMBERS: &[&str] = &[
    "0",
    "-0",
    "1",
    "-1",
    "2",
    "7",
    "100",
    "255",
    "256",
    "1000",
    "16384",
    "16385",
    "32767",
    "32768",
    "65535",
    "65536",
    "100000",
    "999999",
    "1000000",
    "2147483647",
    "2147483648",
    "-2147483649",
    "4294967296",
    "9999999999999999999999",
    "1e9",
    "1e38",
    "3.4e38",
    "1e39",
    "-1e38",
    "1e-9",
    "0.0001",
    "0.5",
    "1.5",
    "-0.5",
    ".5",
    "1.",
    "00000000000000000001",
    "NaN",
    "inf",
    "-inf",
    "infinity",
    "e",
    "1e",
    "+1",
    "--1",
];

const HTML_TAGS: &[&str] = &[
    "div",
    "span",
    "p",
    "a href=x",
    "b",
    "i",
    "table",
    "tr",
    "td colspan=1000",
    "th rowspan=0",
    "tbody",
    "ul",
    "ol start=-5",
    "li",
    "dl",
    "dt",
    "dd",
    "pre",
    "h1",
    "img src=x.png width=99999",
    "form",
    "select",
    "option",
    "button",
    "input",
    "textarea",
    "label",
    "fieldset disabled",
    "legend",
    "blockquote",
    "center",
    "font size=7",
    "sup",
    "sub",
    "q",
    "code",
    "svg",
    "math",
    "template",
    "iframe",
    "object",
    "noscript",
    "style",
    "script",
    "details open",
    "summary",
    "section",
    "nav",
    "br",
    "hr",
    "wbr",
];

const HTML_ATTRS: &[&str] = &[
    " style=\"display:flex\"",
    " style=\"float:left;width:50%\"",
    " style=\"position:absolute;top:-1px\"",
    " style=\"display:inline-block;width:1e9px\"",
    " style=\"margin:-99999px\"",
    " style=\"font-size:0\"",
    " style=\"white-space:pre\"",
    " style=\"overflow:hidden;height:0\"",
    " style=\"display:table-cell\"",
    " style=\"z-index:2147483647;position:relative\"",
    " style=\"--a:var(--a)\"",
    " style=\"width:calc(100% - 1e38px)\"",
    " class=a",
    " id=x",
    " dir=rtl",
    " hidden",
    " colspan=65535",
    " rowspan=65535",
    " width=-1",
    " height=1e9",
    " value=\"",
    " selected",
    " checked",
    " multiple",
    " disabled",
    " readonly",
    " maxlength=0",
    " name=n",
    " for=x",
    " form=x",
    " type=submit",
    " type=radio",
    " type=checkbox",
    " formaction=//x",
    " src=\"data:,x\"",
];

const CSS_WRAPPERS: &[(&str, &str)] = &[
    ("@media screen{", "}"),
    ("@media (min-width:1px) and (max-width:99999px){", "}"),
    ("@supports (display:flex){", "}"),
    ("@layer a{", "}"),
    ("{", "}"),
    (":is(", ")"),
    (":not(", ")"),
    (":where(", ")"),
    ("calc(", ")"),
    ("min(", ",1px)"),
    ("clamp(1px,", ",2px)"),
    ("var(--x,", ")"),
    ("rgb(", ")"),
    ("\"", "\""),
    ("/*", "*/"),
];

impl Material {
    /// Input number `i`: the corpus entries verbatim first, then mutants.
    pub fn input(&self, seed: u64, i: u64) -> Vec<u8> {
        if let Some(entry) = usize::try_from(i).ok().and_then(|i| self.corpus.get(i)) {
            return entry.clone();
        }
        // Each input gets its own generator, so input i never depends on i-1.
        let mut rng = Rng::new(seed ^ i.wrapping_mul(0xD6E8_FEB8_6659_FD93));
        rng.next();
        let mut data = if self.corpus.is_empty() { Vec::new() } else { rng.pick(&self.corpus).clone() };
        let shift = rng.below(4);
        let rounds = 1 + rng.below(1 << shift);
        for _ in 0..rounds {
            self.mutate(&mut rng, &mut data);
            data.truncate(self.max_len);
        }
        if self.kind == Kind::Image {
            fix_png_checksums(&mut data);
        }
        data
    }

    fn mutate(&self, rng: &mut Rng, data: &mut Vec<u8>) {
        let textual = matches!(self.kind, Kind::Html | Kind::Css | Kind::Text);
        // Text kinds favour structure-aware mutations, binary ones bytes.
        let choice = if textual && rng.one_in(2) { 100 + rng.below(6) } else { rng.below(14) };
        match choice {
            0 => {
                if !data.is_empty() {
                    let i = rng.below(data.len());
                    data[i] ^= 1 << rng.below(8);
                }
            }
            1 => {
                if !data.is_empty() {
                    let i = rng.below(data.len());
                    data[i] = rng.next() as u8;
                }
            }
            2 => {
                if !data.is_empty() {
                    let i = rng.below(data.len());
                    data[i] = *rng.pick(INTERESTING_BYTES);
                }
            }
            3 => self.write_int(rng, data),
            4 => {
                // Small arithmetic on one byte.
                if !data.is_empty() {
                    let i = rng.below(data.len());
                    let delta = 1 + rng.below(16) as u8;
                    data[i] = if rng.one_in(2) { data[i].wrapping_add(delta) } else { data[i].wrapping_sub(delta) };
                }
            }
            5 => {
                // Delete a range.
                if !data.is_empty() {
                    let start = rng.below(data.len());
                    let n = rng.len(data.len() - start);
                    data.drain(start..start + n);
                }
            }
            6 => {
                let at = rng.below(data.len() + 1);
                let n = rng.len(32);
                let bytes: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
                data.splice(at..at, bytes);
            }
            7 => {
                // Copy a range somewhere else.
                if !data.is_empty() {
                    let start = rng.below(data.len());
                    let chunk = data[start..start + rng.len(data.len() - start)].to_vec();
                    let at = rng.below(data.len() + 1);
                    data.splice(at..at, chunk);
                }
            }
            8 => self.repeat(rng, data),
            9 => {
                // Splice in part of another corpus entry.
                if !self.corpus.is_empty() {
                    let other = rng.pick(&self.corpus);
                    if !other.is_empty() {
                        let start = rng.below(other.len());
                        let chunk = &other[start..start + rng.len(other.len() - start)];
                        let at = rng.below(data.len() + 1);
                        if rng.one_in(2) {
                            data.truncate(at);
                        }
                        data.splice(at..at, chunk.iter().copied());
                    }
                }
            }
            10 | 11 => {
                if !self.dict.is_empty() {
                    let token = rng.pick(&self.dict).clone();
                    let at = rng.below(data.len() + 1);
                    if choice == 11 && at + token.len() <= data.len() {
                        data[at..at + token.len()].copy_from_slice(&token);
                    } else {
                        data.splice(at..at, token);
                    }
                }
            }
            12 => {
                let keep = rng.below(data.len() + 1);
                data.truncate(keep);
            }
            13 => {
                // Swap two ranges of the same length.
                if data.len() >= 2 {
                    let n = rng.len(data.len() / 2);
                    let a = rng.below(data.len() - n + 1);
                    let b = rng.below(data.len() - n + 1);
                    if a + n <= b || b + n <= a {
                        for k in 0..n {
                            data.swap(a + k, b + k);
                        }
                    }
                }
            }
            100 => self.number(rng, data),
            101 | 102 => self.tokens(rng, data),
            103 => self.wrap(rng, data),
            104 => self.markup(rng, data),
            _ => {
                // Dictionary tokens are worth more in text.
                if !self.dict.is_empty() {
                    let token = rng.pick(&self.dict).clone();
                    let bounds = boundaries(self.kind, data);
                    let at = *rng.pick(&bounds);
                    data.splice(at..at, token);
                }
            }
        }
    }

    /// An interesting integer, 1 to 8 bytes, either endianness, at a random place.
    fn write_int(&self, rng: &mut Rng, data: &mut [u8]) {
        let width = *rng.pick(&[1usize, 2, 4, 8]);
        if data.len() < width {
            return;
        }
        let mut v = *rng.pick(INTERESTING_INTS);
        if rng.one_in(4) {
            v = v.wrapping_add(rng.below(3) as u64).wrapping_sub(1);
        }
        // The low `width` bytes of the value.
        let bytes =
            if rng.one_in(2) { v.to_le_bytes()[..width].to_vec() } else { v.to_be_bytes()[8 - width..].to_vec() };
        let at = rng.below(data.len() - width + 1);
        data[at..at + width].copy_from_slice(&bytes);
    }

    /// One chunk repeated many times: deep nesting, long runs, big tables.
    fn repeat(&self, rng: &mut Rng, data: &mut Vec<u8>) {
        if data.is_empty() {
            return;
        }
        let bounds = boundaries(self.kind, data);
        let a = *rng.pick(&bounds);
        let b = *rng.pick(&bounds);
        let (start, end) = (a.min(b), a.max(b).min(a.min(b) + 256));
        if start == end {
            return;
        }
        let chunk = data[start..end].to_vec();
        let room = self.max_len.saturating_sub(data.len()) / chunk.len();
        let times = rng.len(room.min(4096));
        let repeated: Vec<u8> = chunk.iter().copied().cycle().take(chunk.len() * times).collect();
        data.splice(end..end, repeated);
    }

    /// Replaces a number in the text, or puts one somewhere.
    fn number(&self, rng: &mut Rng, data: &mut Vec<u8>) {
        let number = rng.pick(INTERESTING_NUMBERS).as_bytes();
        let runs: Vec<(usize, usize)> = digit_runs(data);
        if runs.is_empty() || rng.one_in(8) {
            let at = rng.below(data.len() + 1);
            data.splice(at..at, number.iter().copied());
        } else {
            let (start, end) = *rng.pick(&runs);
            data.splice(start..end, number.iter().copied());
        }
    }

    /// Token-level edits: duplicate, drop, swap or borrow tokens.
    fn tokens(&self, rng: &mut Rng, data: &mut Vec<u8>) {
        let bounds = boundaries(self.kind, data);
        if bounds.len() < 2 {
            return;
        }
        let i = rng.below(bounds.len() - 1);
        let j = (i + 1 + rng.len(8)).min(bounds.len() - 1);
        let (start, end) = (bounds[i], bounds[j]);
        match rng.below(4) {
            0 => {
                let chunk = data[start..end].to_vec();
                data.splice(end..end, chunk);
            }
            1 => {
                data.drain(start..end);
            }
            2 => {
                // Move the span elsewhere.
                let chunk: Vec<u8> = data.drain(start..end).collect();
                let bounds = boundaries(self.kind, data);
                let at = *rng.pick(&bounds);
                data.splice(at..at, chunk);
            }
            _ => {
                // A span of another corpus entry.
                if !self.corpus.is_empty() {
                    let other = rng.pick(&self.corpus);
                    let ob = boundaries(self.kind, other);
                    if ob.len() >= 2 {
                        let k = rng.below(ob.len() - 1);
                        let l = (k + 1 + rng.len(8)).min(ob.len() - 1);
                        data.splice(start..start, other[ob[k]..ob[l]].iter().copied());
                    }
                }
            }
        }
    }

    /// Wraps a span in an element (HTML) or a block/function (CSS).
    fn wrap(&self, rng: &mut Rng, data: &mut Vec<u8>) {
        let bounds = boundaries(self.kind, data);
        let a = *rng.pick(&bounds);
        let b = *rng.pick(&bounds);
        let (start, end) = (a.min(b), a.max(b));
        let (open, close) = match self.kind {
            Kind::Css => {
                let (o, c) = rng.pick(CSS_WRAPPERS);
                (o.to_string(), c.to_string())
            }
            _ => {
                let tag = rng.pick(HTML_TAGS);
                let name = tag.split(' ').next().unwrap_or(tag);
                let attr = if rng.one_in(2) { *rng.pick(HTML_ATTRS) } else { "" };
                // Sometimes the end tag is missing or wrong, as on real pages.
                let close = match rng.below(4) {
                    0 => String::new(),
                    1 => format!("</{}>", rng.pick(HTML_TAGS).split(' ').next().unwrap_or("p")),
                    _ => format!("</{name}>"),
                };
                (format!("<{tag}{attr}>"), close)
            }
        };
        data.splice(end..end, close.into_bytes());
        data.splice(start..start, open.into_bytes());
    }

    /// HTML: an attribute or a run of open tags; CSS: a declaration.
    fn markup(&self, rng: &mut Rng, data: &mut Vec<u8>) {
        match self.kind {
            Kind::Html => {
                let tags: Vec<usize> = data
                    .windows(2)
                    .enumerate()
                    .filter(|(_, w)| w[0] == b'<' && w[1].is_ascii_alphabetic())
                    .map(|(i, _)| i)
                    .collect();
                if !tags.is_empty() && rng.one_in(2) {
                    // An attribute right after a tag name.
                    let t = *rng.pick(&tags);
                    let end =
                        data[t + 1..].iter().position(|c| !c.is_ascii_alphanumeric()).map_or(data.len(), |p| t + 1 + p);
                    data.splice(end..end, rng.pick(HTML_ATTRS).bytes());
                } else {
                    let tag = rng.pick(HTML_TAGS);
                    let open = format!("<{tag}>").repeat(rng.len(512));
                    let at = rng.below(data.len() + 1);
                    data.splice(at..at, open.into_bytes());
                }
            }
            Kind::Css => {
                let props: Vec<&Vec<u8>> = self.dict.iter().filter(|t| t.ends_with(b":")).collect();
                if props.is_empty() {
                    return;
                }
                let mut decl = (*rng.pick(&props)).clone();
                let values: Vec<&Vec<u8>> = self.dict.iter().filter(|t| !t.ends_with(b":")).collect();
                for _ in 0..1 + rng.below(3) {
                    if values.is_empty() {
                        break;
                    }
                    decl.extend_from_slice(rng.pick(&values));
                    decl.push(b' ');
                }
                decl.push(b';');
                let at = *rng.pick(&boundaries(self.kind, data));
                data.splice(at..at, decl);
            }
            _ => self.number(rng, data),
        }
    }
}

/// Offsets where a token starts or ends (always including 0 and the end).
fn boundaries(kind: Kind, data: &[u8]) -> Vec<usize> {
    let special: &[u8] = match kind {
        Kind::Html => b"<>\"'= \n\t/&;",
        Kind::Css => b"{};:(),\" \n\t>+~[]*/!@",
        _ => b"/?#&=:.@[]% \n",
    };
    let mut out = vec![0];
    for (i, c) in data.iter().enumerate() {
        if special.contains(c) {
            out.push(i);
            out.push(i + 1);
        }
    }
    out.push(data.len());
    out.dedup();
    out
}

fn digit_runs(data: &[u8]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data[i].is_ascii_digit() {
            let start = i;
            while i < data.len() && (data[i].is_ascii_digit() || data[i] == b'.') {
                i += 1;
            }
            runs.push((start, i));
        } else {
            i += 1;
        }
    }
    runs
}

/// Recomputes every PNG chunk CRC, so mutations reach the decoder proper
/// instead of failing its checksum test.
fn fix_png_checksums(data: &mut [u8]) {
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return;
    }
    let mut pos = 8;
    while pos + 12 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let Some(end) = (pos + 8).checked_add(len).filter(|&e| e + 4 <= data.len()) else { return };
        let crc = crc32(&data[pos + 4..end]);
        data[end..end + 4].copy_from_slice(&crc.to_be_bytes());
        pos = end + 4;
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Parses a libFuzzer/AFL dictionary: one `name="value"` or `"value"` per
/// line, with `\\`, `\"` and `\xNN` escapes; `#` starts a comment.
pub fn parse_dict(text: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(open) = line.find('"') else { continue };
        let Some(body) = line[open + 1..].strip_suffix('"') else { continue };
        let mut value = Vec::new();
        let bytes = body.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if bytes.get(i + 1) == Some(&b'x') && i + 4 <= bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[i + 2..i + 4]).unwrap_or("");
                    match u8::from_str_radix(hex, 16) {
                        Ok(b) => value.push(b),
                        Err(_) => value.extend_from_slice(&bytes[i..i + 4]),
                    }
                    i += 4;
                }
                b'\\' if i + 1 < bytes.len() => {
                    value.push(bytes[i + 1]);
                    i += 2;
                }
                b => {
                    value.push(b);
                    i += 1;
                }
            }
        }
        if !value.is_empty() {
            out.push(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn material(kind: Kind) -> Material {
        Material {
            kind,
            corpus: vec![b"<p class=a style=\"width:10px\">hi 12</p>".to_vec(), b"a{color:red;width:3em}".to_vec()],
            dict: parse_dict("# c\nkw=\"div\"\n\"width:\"\n\"\\x41\\\"b\"\n"),
            max_len: 4096,
        }
    }

    #[test]
    fn inputs_are_deterministic_and_bounded() {
        for kind in [Kind::Html, Kind::Css, Kind::Text, Kind::Binary, Kind::Image] {
            let m = material(kind);
            assert_eq!(m.input(7, 0), m.corpus[0]);
            for i in 0..2000 {
                let a = m.input(7, i);
                assert_eq!(a, m.input(7, i));
                assert!(a.len() <= m.max_len);
            }
            assert_ne!(m.input(7, 100), m.input(8, 100));
        }
    }

    #[test]
    fn dictionaries_and_checksums() {
        assert_eq!(material(Kind::Html).dict, [b"div".to_vec(), b"width:".to_vec(), b"A\"b".to_vec()]);
        // The CRC of IEND is fixed.
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\0IEND\0\0\0\0".to_vec();
        fix_png_checksums(&mut png);
        assert_eq!(&png[16..], &[0xAE, 0x42, 0x60, 0x82]);
    }
}
