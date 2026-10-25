//! Fuzz targets for every parser Hollow points at untrusted bytes (SEC-07).
//!
//! Each target is a plain `fn(&[u8])` that must return for every input
//! without panicking, hanging or allocating without bound. The same
//! functions are driven by two fuzzers:
//!
//! - `fuzz/` at the repository root: cargo-fuzz (libFuzzer, nightly,
//!   coverage-guided), one `fuzz_targets/<name>.rs` per entry of [`TARGETS`];
//! - `cargo xtask fuzz`: a coverage-less mutation fuzzer on the pinned
//!   stable toolchain, for machines without nightly.
//!
//! Besides "doesn't crash", some targets assert promises the code documents
//! (budgets, policy invariants), so a logic bug fails the same way a panic
//! does. Inputs that need several fields (a document and a list of clicks, a
//! form's action and entries, two URLs) split them on [`SEP`], a byte that
//! never occurs in UTF-8 text.
//!
//! This crate depends on the parsers, never the reverse: hollow-engine and
//! the others don't know they are being fuzzed.

mod ct;
mod engine;
mod ipc;
mod policy;

/// Separates the fields of a multi-part input. 0xFF never occurs in UTF-8,
/// so text fields can hold anything else.
pub const SEP: u8 = 0xFF;

/// What a target's input looks like, so the stable fuzzer can pick
/// mutations that keep it mostly well-formed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// HTML text (possibly with a binary prefix or [`SEP`]-separated tail).
    Html,
    /// CSS text.
    Css,
    /// URLs and other short text.
    Text,
    /// PNG, JPEG, GIF or WebP bytes.
    Image,
    /// Binary messages (IPC frames, DER).
    Binary,
}

/// One fuzz target.
pub struct Target {
    /// Also the cargo-fuzz target name and the directory under `fuzz/seeds`.
    pub name: &'static str,
    pub run: fn(&[u8]),
    pub kind: Kind,
    /// Dictionary in `fuzz/dict/<name>.dict`, if any.
    pub dict: Option<&'static str>,
    pub about: &'static str,
}

/// Every target, in the order `cargo xtask fuzz` runs them.
pub const TARGETS: &[Target] = &[
    Target {
        name: "html",
        run: engine::html,
        kind: Kind::Html,
        dict: Some("html"),
        about: "hollow-engine Document::parse (HTML, inline CSS, URLs) and its accessors",
    },
    Target {
        name: "css",
        run: engine::css,
        kind: Kind::Css,
        dict: Some("css"),
        about: "hollow-engine Resources::insert_stylesheet, then a fixed page styled by it",
    },
    Target {
        name: "image",
        run: engine::image,
        kind: Kind::Image,
        dict: None,
        about: "hollow-engine Resources::insert_image (PNG, JPEG, GIF, WebP), then drawn",
    },
    Target {
        name: "layout",
        run: engine::layout,
        kind: Kind::Html,
        dict: Some("html"),
        about: "hollow-engine Layout::new + paint + hit testing of a fuzzed page at a small viewport",
    },
    Target {
        name: "forms",
        run: engine::forms,
        kind: Kind::Html,
        dict: Some("html"),
        about: "hollow-engine FormState click/key sequences, submissions through the broker's encoder",
    },
    Target {
        name: "ipc_renderer",
        run: ipc::renderer_to_broker,
        kind: Kind::Binary,
        dict: None,
        about: "hollow-ipc read_frame + Validate of RendererToBroker, then the broker's URL handling",
    },
    Target {
        name: "ipc_net",
        run: ipc::net_to_broker,
        kind: Kind::Binary,
        dict: None,
        about: "hollow-ipc read_frame + Validate of NetToBroker, then redirect handling",
    },
    Target {
        name: "url_policy",
        run: policy::url_policy,
        kind: Kind::Text,
        dict: Some("url"),
        about: "hollow-app policy::parse_input, check, isolation_key, referer, to_request",
    },
    Target {
        name: "form_encode",
        run: policy::form_encode,
        kind: Kind::Text,
        dict: Some("url"),
        about: "hollow-app form::to_request (urlencoded, multipart, text/plain)",
    },
    Target {
        name: "ct",
        run: ct::ct,
        kind: Kind::Binary,
        dict: None,
        about: "hollow-net CT check: certificate DER and embedded SCT list parsing",
    },
];

/// The target called `name`.
pub fn find(name: &str) -> Option<&'static Target> {
    TARGETS.iter().find(|t| t.name == name)
}

/// Splits `data` into at most `n` fields on [`SEP`]; missing fields are empty.
fn fields(data: &[u8], n: usize) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = data.splitn(n, |&b| b == SEP).collect();
    out.resize(n, &[]);
    out
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Inputs to build seed corpora from, for `fuzz/seeds` (see the
/// `write_seeds` test).
pub mod seeds {
    pub use crate::ct::{SCT_MODE, sct_list_of};
    pub use crate::ipc::{net_seeds, renderer_seeds};
}
