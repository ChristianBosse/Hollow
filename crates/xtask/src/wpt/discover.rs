//! Finding reftests in the checkout and reading their metadata, following the rules of WPT's
//! manifest (`tools/manifest/sourcefile.py`): files in `support`, `resources`, `tools` and
//! `reference` directories, and files named like references (`-ref`, `ref-`, `-notref`, ...), are
//! not tests; a test is a reftest if it links `rel=match` or `rel=mismatch` references.

use std::collections::BTreeMap;
use std::path::Path;

use scraper::{Html, Selector};
use url::Url;

use super::{Fuzzy, Reference, Skip, base_url, local_path, read_source};

/// One file's verdict.
pub enum Found {
    Reftest(Vec<Reference>),
    Skipped(Skip),
    /// Not a reftest (testharness test, reference, support file, ...).
    NotReftest,
}

/// Every candidate test file under `dir` (relative to `root`), as sorted relative paths.
pub fn candidates(root: &Path, dir: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else { return };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = format!("{dir}/{name}");
        if name.starts_with('.') {
            continue;
        }
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            if !matches!(name.as_str(), "support" | "resources" | "tools" | "reference") {
                candidates(root, &rel, out);
            }
            continue;
        }
        let Some((stem, ext)) = name.split_once('.') else { continue };
        let ext = ext.rsplit('.').next().unwrap_or(ext).to_ascii_lowercase();
        if !matches!(ext.as_str(), "html" | "htm" | "xht" | "xhtml" | "xml") || is_reference_name(stem) {
            continue;
        }
        let flag = stem.rsplit('-').next().unwrap_or("");
        if matches!(flag, "manual" | "visual" | "support" | "crash") {
            continue;
        }
        out.push(rel);
    }
}

/// WPT's `(^|[\-_])(not)?ref[0-9]*([\-_]|$)` on the file name before its first dot.
fn is_reference_name(stem: &str) -> bool {
    stem.split(['-', '_']).any(|part| {
        let part = part.strip_prefix("not").unwrap_or(part);
        part.strip_prefix("ref").is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Reads one candidate and decides whether (and how) it runs.
pub fn examine(root: &Path, rel: &str) -> Found {
    let Ok(src) = read_source(root, rel) else { return Found::NotReftest };
    // Cheap pre-filter: most files without the word are testharness tests.
    if !src.to_ascii_lowercase().contains("match") {
        return Found::NotReftest;
    }
    let doc = Html::parse_document(&src);
    let base = base_url(rel);
    let mut refs = Vec::new();
    for link in doc.select(&sel("link[rel][href]")) {
        let rel_attr = link.value().attr("rel").unwrap_or("").to_ascii_lowercase();
        let mismatch = match () {
            _ if rel_attr.split_ascii_whitespace().any(|r| r == "match") => false,
            _ if rel_attr.split_ascii_whitespace().any(|r| r == "mismatch") => true,
            _ => continue,
        };
        let Some(url) = link.value().attr("href").and_then(|h| base.join(h.trim()).ok()) else { continue };
        refs.push((url, mismatch));
    }
    if refs.is_empty() {
        return Found::NotReftest;
    }
    if rel.rsplit_once('.').is_some_and(|(s, _)| s.ends_with("-print") || s.contains("-print.")) {
        return Found::Skipped(Skip::Print);
    }
    let root_class = doc.root_element().value().attr("class").unwrap_or("");
    if root_class.split_ascii_whitespace().any(|c| c == "reftest-wait") {
        return Found::Skipped(Skip::ReftestWait);
    }
    if doc.select(&sel("script")).next().is_some() {
        return Found::Skipped(Skip::Script);
    }
    let fuzzy = fuzzy_metas(&doc, &base);
    let mut out = Vec::new();
    for (url, mismatch) in refs {
        let Some(path) = local_path(&url) else { return Found::Skipped(Skip::MissingReference) };
        if !root.join(&path).is_file() {
            return Found::Skipped(Skip::MissingReference);
        }
        // References run scripts in browsers too.
        let has_script = read_source(root, &path).is_ok_and(|s| {
            let lower = s.to_ascii_lowercase();
            lower.contains("<script") && Html::parse_document(&s).select(&sel("script")).next().is_some()
        });
        if has_script {
            return Found::Skipped(Skip::Script);
        }
        let fuzzy = fuzzy.get(&Some(path.clone())).or_else(|| fuzzy.get(&None)).copied();
        out.push(Reference { path, mismatch, fuzzy });
    }
    Found::Reftest(out)
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).unwrap_or_else(|_| unreachable!("static selector {s}"))
}

/// `<meta name=fuzzy content="[ref-url:]maxDifference=a-b;totalPixels=c-d">`, keyed by the
/// reference it is for (`None`: every reference without a more specific one).
fn fuzzy_metas(doc: &Html, base: &Url) -> BTreeMap<Option<String>, Fuzzy> {
    let mut out = BTreeMap::new();
    for meta in doc.select(&sel("meta[name][content]")) {
        if !meta.value().attr("name").is_some_and(|n| n.eq_ignore_ascii_case("fuzzy")) {
            continue;
        }
        let content = meta.value().attr("content").unwrap_or("");
        let (target, spec) = match content.split_once(':') {
            Some((url, spec)) => (base.join(url.trim()).ok().and_then(|u| local_path(&u)), spec),
            None => (None, content),
        };
        if let Some(f) = parse_fuzzy(spec) {
            out.insert(target, f);
        }
    }
    out
}

/// `maxDifference=a-b;totalPixels=c-d`, names optional, a single number meaning exactly that.
pub fn parse_fuzzy(spec: &str) -> Option<Fuzzy> {
    let mut max_difference = None;
    let mut total_pixels = None;
    for (i, part) in spec.split(';').map(str::trim).filter(|p| !p.is_empty()).enumerate() {
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (if i == 0 { "maxDifference" } else { "totalPixels" }, part),
        };
        let range = match value.split_once('-') {
            Some((a, b)) => (a.trim().parse().ok()?, b.trim().parse().ok()?),
            None => {
                let v = value.parse().ok()?;
                (v, v)
            }
        };
        match key {
            "maxDifference" => max_difference = Some(range),
            "totalPixels" => total_pixels = Some(range),
            _ => return None,
        }
    }
    Some(Fuzzy { max_difference: max_difference?, total_pixels: total_pixels? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_names() {
        for name in ["foo-ref", "ref-foo", "foo_notref", "a-ref2-b", "ref"] {
            assert!(is_reference_name(name), "{name}");
        }
        for name in ["prefix-refs", "reflow-001", "foo-preref", "abspos-001"] {
            assert!(!is_reference_name(name), "{name}");
        }
    }

    #[test]
    fn fuzzy_forms() {
        let f = parse_fuzzy("maxDifference=0-2; totalPixels=10-300").unwrap();
        assert_eq!((f.max_difference, f.total_pixels), ((0, 2), (10, 300)));
        let f = parse_fuzzy("15;300").unwrap();
        assert_eq!((f.max_difference, f.total_pixels), ((15, 15), (300, 300)));
        assert!(parse_fuzzy("maxDifference=1").is_none());
    }
}
