//! Value types with structure: images and gradients, the background
//! longhands and shorthand, generated content, counters and quotes.
//!
//! These are parsed from the declaration as written, not lowercased,
//! because strings, URLs and counter names are case-sensitive.

use std::sync::Arc;

use url::Url;

use super::{
    Calc, ColorValue, Decl, Length, close_paren, escape, ident, is_ident_start, parse_color, parse_length,
    parse_list_style, split_top_level, string, words,
};
use crate::style::ListStyle;

/// Layers one `background` may have; more make the declaration invalid.
pub(crate) const MAX_LAYERS: usize = 16;
/// Color stops one gradient may have.
const MAX_STOPS: usize = 64;
/// Items one `content` value may have.
const MAX_CONTENT_ITEMS: usize = 64;
/// Counters one `counter-*` declaration may name.
const MAX_COUNTER_NAMES: usize = 32;
/// Quote pairs one `quotes` value may have.
const MAX_QUOTE_PAIRS: usize = 16;
/// Tokens one value may have.
const MAX_TOKENS: usize = 512;
/// Longest URL accepted in CSS; `data:` URLs may be longer, up to
/// [`crate::data_url::MAX_DATA_URL_BYTES`].
const MAX_URL_BYTES: usize = 8 * 1024;

/// The base URL relative `url()`s in a stylesheet resolve against.
pub(crate) type Base<'a> = Option<&'a Arc<Url>>;

/// An `<image>`: a URL or a gradient.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Image {
    Url(Arc<Url>),
    Gradient(Arc<Gradient>),
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Gradient {
    pub shape: Shape,
    pub repeating: bool,
    /// Colors with their positions along the gradient line or ray; `None`
    /// positions are spread evenly between their neighbours.
    pub stops: Vec<(ColorValue, Option<Length>)>,
}

// Only ever stored behind the `Arc` of a `Gradient`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Shape {
    /// A linear gradient's direction, in degrees clockwise from "to top".
    Linear(f32),
    /// `to left top` and the like, as signs of x and y (y down). The angle
    /// depends on the box's aspect ratio.
    Corner(i8, i8),
    Radial {
        circle: bool,
        size: RadialSize,
        /// The center, as for `background-position`.
        at: [Length; 2],
    },
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum RadialSize {
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
    /// Horizontal and vertical radii; equal for a circle.
    Explicit(Length, Length),
}

impl Gradient {
    /// The same gradient with every length passed through `f` (used to
    /// compute em-based lengths once the font size is known).
    pub fn map_lengths(&self, f: impl Fn(Length) -> Length) -> Gradient {
        let shape = match self.shape {
            Shape::Radial { circle, size, at } => Shape::Radial {
                circle,
                size: match size {
                    RadialSize::Explicit(a, b) => RadialSize::Explicit(f(a), f(b)),
                    s => s,
                },
                at: at.map(&f),
            },
            s => s,
        };
        Gradient { shape, repeating: self.repeating, stops: self.stops.iter().map(|&(c, p)| (c, p.map(&f))).collect() }
    }
}

/// The `background-repeat` keywords.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Repeat {
    Repeat,
    Space,
    Round,
    NoRepeat,
}

/// `background-origin` and `background-clip` boxes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BoxArea {
    Border,
    Padding,
    Content,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum BgSize {
    Cover,
    Contain,
    /// Width and height; `Length::Auto` for `auto`.
    Size(Length, Length),
}

/// One part of a `content` value.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum ContentItem {
    Text(Arc<str>),
    /// An attribute of the originating element, lowercase.
    Attr(Arc<str>),
    Counter(Arc<str>, ListStyle),
    /// Name, separator, style.
    Counters(Arc<str>, Arc<str>, ListStyle),
    OpenQuote,
    CloseQuote,
    NoOpenQuote,
    NoCloseQuote,
    Image(Arc<Url>),
}

/// Which `counter-*` property a counter list belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CounterOp {
    Reset,
    Increment,
    Set,
}

/// Counter names with their values.
pub(crate) type CounterList = Arc<[(Arc<str>, i32)]>;
/// Open and close quote strings, outermost first.
pub(crate) type QuoteList = Arc<[(Arc<str>, Arc<str>)]>;

// ---------------------------------------------------------------------------
// Tokens

/// A coarse CSS component value: enough structure for the properties here.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum Tok {
    Str(String),
    /// As written; keywords are compared case-insensitively.
    Ident(String),
    /// Lowercase name and the text between the parentheses.
    Func(String, String),
    /// The contents of `url()`, unquoted.
    Url(String),
    /// Anything else: numbers, dimensions, percentages, `#hash`.
    Other(String),
    Slash,
    Comma,
}

impl Tok {
    fn ident_is(&self, kw: &str) -> bool {
        matches!(self, Tok::Ident(w) if w.eq_ignore_ascii_case(kw))
    }

    fn keyword(&self) -> Option<String> {
        match self {
            Tok::Ident(w) => Some(w.to_ascii_lowercase()),
            _ => None,
        }
    }

    /// The token as lowercase CSS text, for the older value parsers.
    fn text(&self) -> Option<String> {
        match self {
            Tok::Ident(w) | Tok::Other(w) => Some(w.to_ascii_lowercase()),
            Tok::Func(n, a) => Some(format!("{n}({})", a.to_ascii_lowercase())),
            _ => None,
        }
    }

    fn length(&self) -> Option<Length> {
        parse_length(&self.text()?).filter(|l| *l != Length::Auto)
    }

    fn color(&self) -> Option<ColorValue> {
        parse_color(&self.text()?)
    }
}

pub(super) fn tokenize(v: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = v.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if out.len() >= MAX_TOKENS {
            return None;
        }
        let numeric_minus = c == '-' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit() || *n == '.');
        match c {
            '"' | '\'' => {
                i += 1;
                out.push(Tok::Str(string(&chars, &mut i, c)?));
            }
            '/' => {
                i += 1;
                out.push(Tok::Slash);
            }
            ',' => {
                i += 1;
                out.push(Tok::Comma);
            }
            c if (c == '\\' || is_ident_start(c)) && !numeric_minus => {
                let name = ident(&chars, &mut i)?;
                if chars.get(i) == Some(&'(') {
                    let close = close_paren(&chars, i)?;
                    let args: String = chars[i + 1..close].iter().collect();
                    i = close + 1;
                    let lower = name.to_ascii_lowercase();
                    out.push(if lower == "url" { Tok::Url(url_arg(&args)?) } else { Tok::Func(lower, args) });
                } else {
                    out.push(Tok::Ident(name));
                }
            }
            _ => {
                let start = i;
                while i < chars.len() && !chars[i].is_whitespace() && !matches!(chars[i], ',' | '/' | '"' | '\'') {
                    if chars[i] == '(' {
                        i = close_paren(&chars, i)? + 1;
                    } else {
                        i += 1;
                    }
                }
                if i == start {
                    return None;
                }
                out.push(Tok::Other(chars[start..i].iter().collect()));
            }
        }
    }
    Some(out)
}

/// The URL inside `url(...)`: a quoted string, or raw text with escapes.
fn url_arg(args: &str) -> Option<String> {
    let chars: Vec<char> = args.trim().chars().collect();
    match chars.first() {
        Some(&q @ ('"' | '\'')) => {
            let mut i = 1;
            let s = string(&chars, &mut i, q)?;
            chars[i..].iter().all(|c| c.is_whitespace()).then_some(s)
        }
        _ => {
            let mut out = String::new();
            let mut i = 0;
            while i < chars.len() {
                let c = chars[i];
                i += 1;
                match c {
                    '\\' => out.push(escape(&chars, &mut i)?),
                    '"' | '\'' | '(' => return None,
                    c if c.is_whitespace() => return None,
                    c => out.push(c),
                }
            }
            Some(out)
        }
    }
}

/// Resolves a URL from CSS. Only http(s) and `data:` URLs can ever be
/// shown; anything else (and anything unresolvable) is no image.
pub(crate) fn resolve_url(raw: &str, base: Base<'_>) -> Option<Arc<Url>> {
    let raw = raw.trim();
    let is_data = raw.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("data:"));
    let max = if is_data { crate::data_url::MAX_DATA_URL_BYTES } else { MAX_URL_BYTES };
    if raw.is_empty() || raw.len() > max {
        return None;
    }
    let url = match base {
        Some(b) => b.join(raw).ok()?,
        None => Url::parse(raw).ok()?,
    };
    matches!(url.scheme(), "http" | "https" | "data").then(|| Arc::new(url))
}

// ---------------------------------------------------------------------------
// Images and gradients

/// `none`, `url()` or a gradient. `Some(None)` is `none` (or an image that
/// can't be resolved, which renders the same); `None` is not an image.
fn image(t: &Tok, base: Base<'_>) -> Option<Option<Image>> {
    match t {
        Tok::Ident(w) if w.eq_ignore_ascii_case("none") => Some(None),
        Tok::Url(u) => Some(resolve_url(u, base).map(Image::Url)),
        Tok::Func(name, args) if name.ends_with("gradient") => {
            Some(Some(Image::Gradient(Arc::new(parse_gradient(name, args)?))))
        }
        // `image-set()`: its first image, whatever the resolution.
        Tok::Func(name, args) if name == "image-set" || name == "-webkit-image-set" => {
            let first = split_top_level(args, b',').into_iter().next()?;
            let toks = tokenize(first)?;
            match toks.first()? {
                Tok::Str(s) => Some(resolve_url(s, base).map(Image::Url)),
                t => image(t, base),
            }
        }
        _ => None,
    }
}

fn parse_gradient(name: &str, args: &str) -> Option<Gradient> {
    let (prefixed, name) = match name.strip_prefix('-') {
        Some(rest) => (true, rest.split_once('-')?.1),
        None => (false, name),
    };
    let (repeating, kind) = match name.strip_prefix("repeating-") {
        Some(k) => (true, k),
        None => (false, name),
    };
    let parts = split_top_level(args, b',');
    if parts.len() > MAX_STOPS + 1 {
        return None;
    }
    let first = parts.first()?.trim().to_ascii_lowercase();
    let (shape, from) = match kind {
        "linear-gradient" => match linear_direction(&first, prefixed) {
            Some(s) => (s, 1),
            None => (Shape::Linear(180.0), 0),
        },
        "radial-gradient" => match radial_shape(&first) {
            Some(s) => (s, 1),
            None => (radial_default(), 0),
        },
        _ => return None,
    };
    let mut stops = Vec::new();
    for p in &parts[from..] {
        let p = p.trim().to_ascii_lowercase();
        let w = words(&p);
        // Color hints (a lone position between two stops) are skipped:
        // the transition stays linear.
        if let [h] = w.as_slice()
            && parse_color(h).is_none()
            && parse_length(h).is_some()
        {
            continue;
        }
        let (color, positions) = match parse_color(w.first()?) {
            Some(c) => (c, &w[1..]),
            // `10% red` is the same as `red 10%`.
            None => {
                let (c, rest) = w.split_last()?;
                (parse_color(c)?, rest)
            }
        };
        let pos: Option<Vec<Length>> = positions.iter().map(|t| parse_length(t)).collect();
        match pos?.as_slice() {
            [] => stops.push((color, None)),
            [a] => stops.push((color, Some(*a))),
            [a, b] => stops.extend([(color, Some(*a)), (color, Some(*b))]),
            _ => return None,
        }
    }
    (!stops.is_empty()).then_some(Gradient { shape, repeating, stops })
}

/// The first argument of a linear gradient, if it is a direction.
fn linear_direction(s: &str, legacy: bool) -> Option<Shape> {
    if let Some(a) = angle(s) {
        // Prefixed gradients measured counterclockwise from "to right".
        return Some(Shape::Linear(if legacy { 90.0 - a } else { a }));
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    let sides = match words.split_first() {
        Some((&"to", rest)) if !legacy => rest,
        _ if legacy => &words[..],
        _ => return None,
    };
    let (mut x, mut y) = (0i8, 0i8);
    for w in sides {
        match *w {
            "left" if x == 0 => x = -1,
            "right" if x == 0 => x = 1,
            "top" if y == 0 => y = -1,
            "bottom" if y == 0 => y = 1,
            "center" if legacy => {}
            _ => return None,
        }
    }
    if legacy {
        // `-webkit-linear-gradient(top, ...)` names the starting side.
        (x, y) = (-x, -y);
    }
    Some(match (x, y) {
        (0, 0) if legacy => Shape::Linear(180.0),
        (0, 0) => return None,
        (0, -1) => Shape::Linear(0.0),
        (1, 0) => Shape::Linear(90.0),
        (0, 1) => Shape::Linear(180.0),
        (-1, 0) => Shape::Linear(270.0),
        (x, y) => Shape::Corner(x, y),
    })
}

/// An angle in degrees: `45deg`, `0.25turn`, `1rad`, `50grad`, or `0`.
fn angle(s: &str) -> Option<f32> {
    let (n, unit) = super::split_number(s.trim())?;
    let deg = match unit {
        "deg" => n,
        "turn" => n * 360.0,
        "grad" => n * 0.9,
        "rad" => n * (180.0 / std::f32::consts::PI),
        "" if n == 0.0 => 0.0,
        _ => return None,
    };
    deg.is_finite().then(|| deg.clamp(-1e6, 1e6))
}

fn radial_default() -> Shape {
    let center = [Length::Pct(50.0), Length::Pct(50.0)];
    Shape::Radial { circle: false, size: RadialSize::FarthestCorner, at: center }
}

/// The first argument of a radial gradient, if it is a shape, size and/or
/// position.
fn radial_shape(s: &str) -> Option<Shape> {
    let (head, at) = match s.split_once(" at ").or_else(|| s.strip_prefix("at ").map(|r| ("", r))) {
        Some((h, a)) => (h.trim(), Some(a)),
        None => (s.trim(), None),
    };
    let mut circle = None;
    let mut size = None;
    let mut lengths = Vec::new();
    for w in words(head) {
        match w {
            "circle" if circle.is_none() => circle = Some(true),
            "ellipse" if circle.is_none() => circle = Some(false),
            "closest-side" | "contain" if size.is_none() => size = Some(RadialSize::ClosestSide),
            "farthest-side" if size.is_none() => size = Some(RadialSize::FarthestSide),
            "closest-corner" if size.is_none() => size = Some(RadialSize::ClosestCorner),
            "farthest-corner" | "cover" if size.is_none() => size = Some(RadialSize::FarthestCorner),
            _ => lengths.push(parse_length(w).filter(|l| *l != Length::Auto)?),
        }
    }
    let explicit = match lengths.as_slice() {
        [] => None,
        [r] if circle != Some(false) && !matches!(r, Length::Pct(_)) => {
            circle = Some(true);
            Some(RadialSize::Explicit(*r, *r))
        }
        [a, b] if circle != Some(true) => {
            circle = Some(false);
            Some(RadialSize::Explicit(*a, *b))
        }
        _ => return None,
    };
    if explicit.is_some() && size.is_some() {
        return None;
    }
    let at = match at {
        Some(a) => position(&tokenize(a)?)?,
        None if circle.is_none() && size.is_none() && explicit.is_none() => return None,
        None => [Length::Pct(50.0), Length::Pct(50.0)],
    };
    Some(Shape::Radial {
        circle: circle.unwrap_or(false),
        size: explicit.or(size).unwrap_or(RadialSize::FarthestCorner),
        at,
    })
}

// ---------------------------------------------------------------------------
// Background

/// `background-position` values: one to four keywords and lengths, as
/// (x, y) lengths where percentages refer to the free space.
pub(super) fn position(toks: &[Tok]) -> Option<[Length; 2]> {
    #[derive(Clone, Copy, PartialEq)]
    enum P {
        Left,
        Right,
        Top,
        Bottom,
        Center,
        Len(Length),
    }
    let parts: Option<Vec<P>> = toks
        .iter()
        .map(|t| match t.keyword().as_deref() {
            Some("left") => Some(P::Left),
            Some("right") => Some(P::Right),
            Some("top") => Some(P::Top),
            Some("bottom") => Some(P::Bottom),
            Some("center") => Some(P::Center),
            Some(_) => None,
            None => t.length().map(P::Len),
        })
        .collect();
    let pct = Length::Pct;
    let edge = |p: P| match p {
        P::Left | P::Top => Some(pct(0.0)),
        P::Right | P::Bottom => Some(pct(100.0)),
        P::Center => Some(pct(50.0)),
        P::Len(l) => Some(l),
    };
    let vertical = |p: P| matches!(p, P::Top | P::Bottom);
    let horizontal = |p: P| matches!(p, P::Left | P::Right);
    match parts?.as_slice() {
        [a] if vertical(*a) => Some([pct(50.0), edge(*a)?]),
        [a] => Some([edge(*a)?, pct(50.0)]),
        [a, b] => {
            let swap = vertical(*a) || horizontal(*b);
            let (x, y) = if swap { (*b, *a) } else { (*a, *b) };
            let lengths = matches!(a, P::Len(_)) || matches!(b, P::Len(_));
            if vertical(x) || horizontal(y) || (swap && lengths) {
                return None;
            }
            Some([edge(x)?, edge(y)?])
        }
        parts @ ([_, _, _] | [_, _, _, _]) => {
            // Keyword-offset pairs: `right 10px top`, `left 5% bottom 2em`.
            let mut x = None;
            let mut y = None;
            let mut i = 0;
            while i < parts.len() {
                let kw = parts[i];
                let offset = match parts.get(i + 1) {
                    Some(P::Len(l)) => {
                        i += 1;
                        Some(*l)
                    }
                    _ => None,
                };
                i += 1;
                let from_end = |l: Length| -> Option<Length> {
                    Some(Length::Calc(Calc::of(l)?.scaled(-1.0).plus(Calc { pct: 100.0, ..Calc::default() })))
                };
                let v = match (kw, offset) {
                    (P::Len(_), _) => return None,
                    (P::Center, Some(_)) => return None,
                    (P::Left | P::Top, Some(l)) => l,
                    (P::Right | P::Bottom, Some(l)) => from_end(l)?,
                    (k, None) => edge(k)?,
                };
                let slot = match kw {
                    P::Left | P::Right => &mut x,
                    P::Top | P::Bottom => &mut y,
                    _ if x.is_none() && !parts[i..].iter().any(|p| horizontal(*p)) => &mut x,
                    _ => &mut y,
                };
                if slot.is_some() {
                    return None;
                }
                *slot = Some(v);
            }
            Some([x.unwrap_or(pct(50.0)), y.unwrap_or(pct(50.0))])
        }
        _ => None,
    }
}

fn repeat_keyword(k: &str) -> Option<Repeat> {
    Some(match k {
        "repeat" => Repeat::Repeat,
        "space" => Repeat::Space,
        "round" => Repeat::Round,
        "no-repeat" => Repeat::NoRepeat,
        _ => return None,
    })
}

/// One or two repeat keywords at the start of `toks`, and how many were used.
fn repeat(toks: &[Tok]) -> Option<([Repeat; 2], usize)> {
    let first = toks.first()?.keyword()?;
    match first.as_str() {
        "repeat-x" => return Some(([Repeat::Repeat, Repeat::NoRepeat], 1)),
        "repeat-y" => return Some(([Repeat::NoRepeat, Repeat::Repeat], 1)),
        _ => {}
    }
    let a = repeat_keyword(&first)?;
    match toks.get(1).and_then(Tok::keyword).as_deref().and_then(repeat_keyword) {
        Some(b) => Some(([a, b], 2)),
        None => Some(([a, a], 1)),
    }
}

fn box_area(k: &str) -> Option<BoxArea> {
    Some(match k {
        "border-box" => BoxArea::Border,
        "padding-box" => BoxArea::Padding,
        "content-box" => BoxArea::Content,
        _ => return None,
    })
}

fn size_value(t: &Tok) -> Option<Length> {
    if t.ident_is("auto") {
        return Some(Length::Auto);
    }
    t.length().filter(|l| super::non_negative(*l))
}

/// `cover`, `contain`, or one or two sizes, at the start of `toks`.
fn bg_size(toks: &[Tok]) -> Option<(BgSize, usize)> {
    let first = toks.first()?;
    if first.ident_is("cover") {
        return Some((BgSize::Cover, 1));
    }
    if first.ident_is("contain") {
        return Some((BgSize::Contain, 1));
    }
    let w = size_value(first)?;
    match toks.get(1).and_then(size_value) {
        Some(h) => Some((BgSize::Size(w, h), 2)),
        None => Some((BgSize::Size(w, Length::Auto), 1)),
    }
}

fn is_position_part(t: &Tok) -> bool {
    match t.keyword().as_deref() {
        Some("left" | "right" | "top" | "bottom" | "center") => true,
        Some(_) => false,
        None => t.length().is_some(),
    }
}

/// Splits a comma-separated list into its items' tokens.
fn layers(value: &str) -> Option<Vec<Vec<Tok>>> {
    let parts = split_top_level(value, b',');
    if parts.len() > MAX_LAYERS {
        return None;
    }
    parts.into_iter().map(|p| tokenize(p).filter(|t| !t.is_empty())).collect()
}

/// The background longhands that take a comma-separated list.
pub(super) fn background_longhand(name: &str, value: &str, base: Base<'_>) -> Option<Decl> {
    let layers = layers(value)?;
    let each = |f: &dyn Fn(&[Tok]) -> Option<usize>| layers.iter().all(|l| f(l) == Some(l.len()));
    Some(match name {
        "background-image" => {
            let images: Option<Vec<Option<Image>>> =
                layers.iter().map(|l| if l.len() == 1 { image(&l[0], base) } else { None }).collect();
            Decl::BgImage(images?.into())
        }
        "background-repeat" => {
            if !each(&|l| repeat(l).map(|r| r.1)) {
                return None;
            }
            Decl::BgRepeat(layers.iter().filter_map(|l| repeat(l).map(|r| r.0)).collect())
        }
        "background-position" => {
            let p: Option<Vec<[Length; 2]>> = layers.iter().map(|l| position(l)).collect();
            Decl::BgPosition(p?.into())
        }
        "background-size" | "-webkit-background-size" => {
            if !each(&|l| bg_size(l).map(|s| s.1)) {
                return None;
            }
            Decl::BgSize(layers.iter().filter_map(|l| bg_size(l).map(|s| s.0)).collect())
        }
        "background-origin" | "background-clip" | "-webkit-background-clip" | "-webkit-background-origin" => {
            let b: Option<Vec<BoxArea>> = layers
                .iter()
                .map(|l| match l.as_slice() {
                    // `text` clips to the glyphs, which can't be drawn; the
                    // content box is the closest box.
                    [t] if t.ident_is("text") => Some(BoxArea::Content),
                    [t] => box_area(&t.keyword()?),
                    _ => None,
                })
                .collect();
            if name.ends_with("origin") { Decl::BgOrigin(b?.into()) } else { Decl::BgClip(b?.into()) }
        }
        "background-attachment" => {
            let fixed: Option<Vec<bool>> = layers
                .iter()
                .map(|l| match l.as_slice() {
                    [t] => match t.keyword()?.as_str() {
                        "fixed" => Some(true),
                        "scroll" | "local" => Some(false),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            Decl::BgFixed(fixed?.into())
        }
        _ => return None,
    })
}

/// The `background` shorthand. Every longhand is set, to its initial value
/// where the shorthand doesn't say; only the last layer may have a color.
pub(super) fn background_shorthand(value: &str, base: Base<'_>, out: &mut Vec<Decl>) {
    let Some(layers) = layers(value) else { return };
    let n = layers.len();
    let mut images = Vec::with_capacity(n);
    let mut repeats = Vec::with_capacity(n);
    let mut positions = Vec::with_capacity(n);
    let mut sizes = Vec::with_capacity(n);
    let mut origins = Vec::with_capacity(n);
    let mut clips = Vec::with_capacity(n);
    let mut fixed = Vec::with_capacity(n);
    let mut color = None;
    for (li, toks) in layers.iter().enumerate() {
        let mut img: Option<Option<Image>> = None;
        let mut rep = None;
        let mut pos = None;
        let mut size = None;
        let mut boxes = Vec::new();
        let mut attachment = None;
        let mut k = 0;
        while k < toks.len() {
            let t = &toks[k];
            if img.is_none()
                && let Some(i) = image(t, base)
            {
                img = Some(i);
                k += 1;
                continue;
            }
            if rep.is_none()
                && let Some((r, used)) = repeat(&toks[k..])
            {
                rep = Some(r);
                k += used;
                continue;
            }
            let kw = t.keyword();
            if attachment.is_none() && matches!(kw.as_deref(), Some("scroll" | "fixed" | "local")) {
                attachment = Some(kw.as_deref() == Some("fixed"));
                k += 1;
                continue;
            }
            if let Some(b) = kw.as_deref().and_then(box_area)
                && boxes.len() < 2
            {
                boxes.push(b);
                k += 1;
                continue;
            }
            if pos.is_none() && is_position_part(t) {
                let end =
                    (k..toks.len().min(k + 4)).take_while(|&j| is_position_part(&toks[j])).last().unwrap_or(k) + 1;
                // The longest run of up to four parts that parses.
                let Some((p, e)) = (k + 1..=end).rev().find_map(|e| position(&toks[k..e]).map(|p| (p, e))) else {
                    return;
                };
                pos = Some(p);
                k = e;
                if toks.get(k) == Some(&Tok::Slash) {
                    let Some((s, used)) = bg_size(&toks[k + 1..]) else { return };
                    size = Some(s);
                    k += 1 + used;
                }
                continue;
            }
            if li == n - 1
                && color.is_none()
                && let Some(c) = t.color()
            {
                color = Some(c);
                k += 1;
                continue;
            }
            return;
        }
        images.push(img.flatten());
        repeats.push(rep.unwrap_or([Repeat::Repeat; 2]));
        positions.push(pos.unwrap_or([Length::Pct(0.0); 2]));
        sizes.push(size.unwrap_or(BgSize::Size(Length::Auto, Length::Auto)));
        let (origin, clip) = match boxes.as_slice() {
            [b] => (*b, *b),
            [o, c] => (*o, *c),
            _ => (BoxArea::Padding, BoxArea::Border),
        };
        origins.push(origin);
        clips.push(clip);
        fixed.push(attachment.unwrap_or(false));
    }
    let transparent = ColorValue::Rgba(super::Color { r: 0, g: 0, b: 0, a: 0 });
    out.extend([
        Decl::Background(color.unwrap_or(transparent)),
        Decl::BgImage(images.into()),
        Decl::BgRepeat(repeats.into()),
        Decl::BgPosition(positions.into()),
        Decl::BgSize(sizes.into()),
        Decl::BgOrigin(origins.into()),
        Decl::BgClip(clips.into()),
        Decl::BgFixed(fixed.into()),
    ]);
}

// ---------------------------------------------------------------------------
// Generated content

pub(super) fn content(value: &str, base: Base<'_>) -> Option<Decl> {
    let toks = tokenize(value)?;
    if let [t] = toks.as_slice()
        && (t.ident_is("normal") || t.ident_is("none"))
    {
        return Some(Decl::Content(None));
    }
    let mut items = Vec::new();
    for t in &toks {
        if items.len() >= MAX_CONTENT_ITEMS {
            return None;
        }
        let item = match t {
            // Alternative text for speech follows a slash.
            Tok::Slash => break,
            Tok::Str(s) => ContentItem::Text(s.as_str().into()),
            Tok::Url(u) => match resolve_url(u, base) {
                Some(u) => ContentItem::Image(u),
                None => continue,
            },
            Tok::Func(name, args) => match name.as_str() {
                "attr" => {
                    let a = args.trim();
                    let name = a.split(|c: char| c.is_whitespace() || c == ',').next()?;
                    if name.is_empty() || !name.chars().all(|c| is_ident_start(c) || c.is_ascii_digit()) {
                        return None;
                    }
                    ContentItem::Attr(name.to_ascii_lowercase().into())
                }
                "counter" | "counters" => {
                    let parts = split_top_level(args, b',');
                    let counter_name = counter_ident(parts.first()?.trim())?;
                    let (sep, style_at) = if name == "counters" {
                        let sep = tokenize(parts.get(1)?)?;
                        let [Tok::Str(s)] = sep.as_slice() else { return None };
                        (Some(s.clone()), 2)
                    } else {
                        (None, 1)
                    };
                    let style = match parts.get(style_at) {
                        Some(s) => counter_style(s.trim())?,
                        None => ListStyle::Decimal,
                    };
                    if parts.len() > style_at + 1 {
                        return None;
                    }
                    match sep {
                        Some(s) => ContentItem::Counters(counter_name, s.into(), style),
                        None => ContentItem::Counter(counter_name, style),
                    }
                }
                _ => return None,
            },
            Tok::Ident(w) => match w.to_ascii_lowercase().as_str() {
                "open-quote" => ContentItem::OpenQuote,
                "close-quote" => ContentItem::CloseQuote,
                "no-open-quote" => ContentItem::NoOpenQuote,
                "no-close-quote" => ContentItem::NoCloseQuote,
                _ => return None,
            },
            _ => return None,
        };
        items.push(item);
    }
    Some(Decl::Content(Some(items.into())))
}

/// A counter style name; unknown ones fall back to decimal, as CSS says.
fn counter_style(s: &str) -> Option<ListStyle> {
    let s = s.to_ascii_lowercase();
    if s.is_empty() || !s.chars().all(|c| is_ident_start(c) || c.is_ascii_digit()) {
        return None;
    }
    Some(parse_list_style(&s).unwrap_or(ListStyle::Decimal))
}

fn counter_ident(s: &str) -> Option<Arc<str>> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let name = ident(&chars, &mut i)?;
    let reserved = ["none", "inherit", "initial", "unset", "default", "revert"];
    (i == chars.len() && !reserved.iter().any(|r| name.eq_ignore_ascii_case(r))).then(|| name.into())
}

/// `counter-reset`, `counter-increment` and `counter-set`: names, each
/// optionally followed by an integer.
pub(super) fn counters(op: CounterOp, value: &str) -> Option<Decl> {
    let toks = tokenize(value)?;
    if let [t] = toks.as_slice()
        && t.ident_is("none")
    {
        return Some(Decl::Counters(op, Arc::from([])));
    }
    let default = if op == CounterOp::Increment { 1 } else { 0 };
    let mut list: Vec<(Arc<str>, i32)> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let Tok::Ident(name) = &toks[i] else { return None };
        let name = counter_ident(name)?;
        i += 1;
        let mut value = default;
        if let Some(Tok::Other(n)) = toks.get(i)
            && let Ok(v) = n.parse::<i64>()
        {
            value = v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
            i += 1;
        }
        if list.len() >= MAX_COUNTER_NAMES {
            return None;
        }
        list.push((name, value));
    }
    Some(Decl::Counters(op, list.into()))
}

pub(super) fn quotes(value: &str) -> Option<Decl> {
    let toks = tokenize(value)?;
    match toks.as_slice() {
        [t] if t.ident_is("none") => return Some(Decl::Quotes(Some(Arc::from([])))),
        [t] if t.ident_is("auto") => return Some(Decl::Quotes(None)),
        _ => {}
    }
    if toks.is_empty() || toks.len() % 2 != 0 || toks.len() > MAX_QUOTE_PAIRS * 2 {
        return None;
    }
    let pairs: Option<Vec<(Arc<str>, Arc<str>)>> = toks
        .chunks(2)
        .map(|p| match p {
            [Tok::Str(a), Tok::Str(b)] => Some((a.as_str().into(), b.as_str().into())),
            _ => None,
        })
        .collect();
    Some(Decl::Quotes(Some(pairs?.into())))
}

/// Every http(s) URL a declaration refers to, for fetching.
pub(crate) fn decl_urls(d: &Decl, out: &mut Vec<Arc<Url>>) {
    let mut push = |u: &Arc<Url>| {
        if matches!(u.scheme(), "http" | "https") && !out.contains(u) {
            out.push(u.clone());
        }
    };
    match d {
        Decl::BgImage(list) => list.iter().flatten().for_each(|i| {
            if let Image::Url(u) = i {
                push(u);
            }
        }),
        Decl::Content(Some(items)) => items.iter().for_each(|i| {
            if let ContentItem::Image(u) = i {
                push(u);
            }
        }),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::{Color, parse_declarations};

    fn base() -> Arc<Url> {
        Arc::new(Url::parse("https://example.org/css/site.css").unwrap())
    }

    fn decls(css: &str) -> Vec<Decl> {
        parse_declarations(css, Some(&base())).into_iter().map(|d| d.decl).collect()
    }

    fn url(s: &str) -> Arc<Url> {
        Arc::new(Url::parse(s).unwrap())
    }

    #[test]
    fn background_shorthand_sets_every_longhand() {
        let d = decls("background: #fff URL('../img/A.png') no-repeat right 10px top / 20px auto padding-box");
        assert_eq!(d[0], Decl::Background(ColorValue::Rgba(Color::WHITE)));
        assert_eq!(d[1], Decl::BgImage(vec![Some(Image::Url(url("https://example.org/img/A.png")))].into()));
        assert_eq!(d[2], Decl::BgRepeat(vec![[Repeat::NoRepeat; 2]].into()));
        let right = Length::Calc(Calc { px: -10.0, pct: 100.0, ..Calc::default() });
        assert_eq!(d[3], Decl::BgPosition(vec![[right, Length::Pct(0.0)]].into()));
        assert_eq!(d[4], Decl::BgSize(vec![BgSize::Size(Length::Px(20.0), Length::Auto)].into()));
        assert_eq!(d[5], Decl::BgOrigin(vec![BoxArea::Padding].into()));
        assert_eq!(d[6], Decl::BgClip(vec![BoxArea::Padding].into()));
        // Two layers; only the last may have a color.
        let d = decls("background: url(a.png) center, linear-gradient(red, blue) red");
        assert_eq!(d.len(), 8);
        assert!(decls("background: red, url(a.png)").is_empty());
        assert!(decls("background: red pink").is_empty());
        assert!(decls("background: url(a.png) url(b.png)").is_empty());
        // Unresolvable or non-web URLs are no image, but still valid.
        let d = decls("background: url('javascript:alert(1)') red");
        assert_eq!(d[1], Decl::BgImage(vec![None].into()));
    }

    #[test]
    fn background_longhands() {
        assert_eq!(
            decls("background-repeat: repeat-x, space round"),
            vec![Decl::BgRepeat(vec![[Repeat::Repeat, Repeat::NoRepeat], [Repeat::Space, Repeat::Round]].into())]
        );
        assert_eq!(
            decls("background-position: bottom; background-size: cover, 50%"),
            vec![
                Decl::BgPosition(vec![[Length::Pct(50.0), Length::Pct(100.0)]].into()),
                Decl::BgSize(vec![BgSize::Cover, BgSize::Size(Length::Pct(50.0), Length::Auto)].into()),
            ]
        );
        assert_eq!(
            decls("background-position: 10px 2em"),
            vec![Decl::BgPosition(vec![[Length::Px(10.0), Length::Em(2.0)]].into())]
        );
        assert!(decls("background-position: left left; background-size: -1px; background-repeat: x").is_empty());
        let data = "background-image: url(data:image/png;base64,iVBOR%2F==), none";
        assert!(matches!(&decls(data)[0], Decl::BgImage(l) if l.len() == 2 && l[1].is_none()));
    }

    #[test]
    fn gradients() {
        let g = |css: &str| match &decls(&format!("background-image: {css}"))[..] {
            [Decl::BgImage(l)] => match &l[0] {
                Some(Image::Gradient(g)) => Some((**g).clone()),
                _ => None,
            },
            _ => None,
        };
        let red = ColorValue::Rgba(Color::rgb(255, 0, 0));
        let lin = g("linear-gradient(to right, red 10%, blue)").unwrap();
        assert_eq!(lin.shape, Shape::Linear(90.0));
        assert_eq!(lin.stops[0], (red, Some(Length::Pct(10.0))));
        assert_eq!(g("linear-gradient(45deg, red, blue)").unwrap().shape, Shape::Linear(45.0));
        assert_eq!(g("linear-gradient(red, blue)").unwrap().shape, Shape::Linear(180.0));
        assert_eq!(g("linear-gradient(to top left, red, blue)").unwrap().shape, Shape::Corner(-1, -1));
        assert_eq!(g("-webkit-linear-gradient(top, red, blue)").unwrap().shape, Shape::Linear(180.0));
        assert_eq!(g("-moz-linear-gradient(0deg, red, blue)").unwrap().shape, Shape::Linear(90.0));
        assert!(g("repeating-linear-gradient(red 0 10px, blue 10px 20px)").unwrap().repeating);
        let r = g("radial-gradient(circle at top left, red, blue)").unwrap();
        assert_eq!(
            r.shape,
            Shape::Radial { circle: true, size: RadialSize::FarthestCorner, at: [Length::Pct(0.0), Length::Pct(0.0)] }
        );
        let r = g("radial-gradient(10px 20px, red, blue)").unwrap();
        assert_eq!(
            r.shape,
            Shape::Radial {
                circle: false,
                size: RadialSize::Explicit(Length::Px(10.0), Length::Px(20.0)),
                at: [Length::Pct(50.0); 2]
            }
        );
        assert_eq!(g("radial-gradient(red, blue)").unwrap().stops.len(), 2);
        assert!(g("linear-gradient(to nowhere, red)").is_none());
        assert!(g("conic-gradient(red, blue)").is_none());
    }

    #[test]
    fn content_values() {
        let c = |v: &str| match &decls(&format!("content: {v}"))[..] {
            [Decl::Content(c)] => Some(c.clone()),
            _ => None,
        };
        assert_eq!(c("none"), Some(None));
        assert_eq!(c("NORMAL"), Some(None));
        let items = c(r#""\201C" attr(Title) counter(Item, upper-roman) counters(x, ".") open-quote url(i.png)"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            &items[..],
            &[
                ContentItem::Text("\u{201C}".into()),
                ContentItem::Attr("title".into()),
                ContentItem::Counter("Item".into(), ListStyle::UpperRoman),
                ContentItem::Counters("x".into(), ".".into(), ListStyle::Decimal),
                ContentItem::OpenQuote,
                ContentItem::Image(url("https://example.org/css/i.png")),
            ]
        );
        assert_eq!(c("''").unwrap().unwrap().len(), 1);
        assert_eq!(c("'a' / 'alt'").unwrap().unwrap().len(), 1);
        assert_eq!(c("foo"), None);
        assert_eq!(c("counter()"), None);
    }

    #[test]
    fn counters_and_quotes() {
        assert_eq!(
            decls("counter-reset: a b -3; counter-increment: c; counter-set: none"),
            vec![
                Decl::Counters(CounterOp::Reset, vec![("a".into(), 0), ("b".into(), -3)].into()),
                Decl::Counters(CounterOp::Increment, vec![("c".into(), 1)].into()),
                Decl::Counters(CounterOp::Set, Arc::from([])),
            ]
        );
        assert!(decls("counter-reset: 3 a; counter-increment: none a").is_empty());
        assert_eq!(
            decls("quotes: '<' '>' \"[\" ']'"),
            vec![Decl::Quotes(Some(vec![("<".into(), ">".into()), ("[".into(), "]".into())].into()))]
        );
        assert!(decls("quotes: 'a'").is_empty());
    }
}
