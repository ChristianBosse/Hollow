//! The pure part of the trace test: what counts as a Hollow trace, snapshot diffs, the noise
//! filter, AppContainer SIDs and SDDL. No system access here, so all of it is unit-tested.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use super::regfile::{RegPath, RegSnapshot, searchable};

/// Names of the files Arti keeps in its state and cache directories, matched anywhere in a path.
/// Hollow's patched Arti must never create them; they don't contain "arti" themselves.
const ARTI_FILES: [&str; 5] = ["dir.sqlite3", "dir_blobs", "guards.json", "circuit_timeouts.json", "cached-microdesc"];

/// What marks a path, registry name or value as Hollow's.
pub struct Patterns {
    /// Lower-case AppContainer SIDs of Hollow's sandboxes.
    sids: Vec<String>,
}

impl Patterns {
    pub fn new<S: AsRef<str>>(sids: &[S]) -> Self {
        Self { sids: sids.iter().map(|s| s.as_ref().to_ascii_lowercase()).collect() }
    }

    pub fn add_sid(&mut self, sid: &str) {
        let sid = sid.to_ascii_lowercase();
        if !self.sids.contains(&sid) {
            self.sids.push(sid);
        }
    }

    pub fn is_hollow_sid(&self, sid: &str) -> bool {
        self.sids.iter().any(|s| s.eq_ignore_ascii_case(sid))
    }

    /// Returns the pattern `text` matches, if any. Case-insensitive.
    ///
    /// - substrings: `hollow`, `torproject`, the Hollow SIDs, Arti's file names;
    /// - tokens: `arti` and `tor-` only where not glued to other letters or digits (`arti`,
    ///   `arti-client`, `\tor-dirmgr`), so `partial`, `article`, `editor-` and `monitor-` don't count;
    /// - ROT13 `hollow` (`ubyybj`): Explorer's UserAssist stores program paths ROT13-encoded.
    pub fn hit(&self, text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        for needle in ["hollow", "torproject"].into_iter().chain(ARTI_FILES) {
            if lower.contains(needle) {
                return Some(needle.to_string());
            }
        }
        if let Some(sid) = self.sids.iter().find(|s| lower.contains(s.as_str())) {
            return Some(sid.to_uppercase());
        }
        if contains_token(&lower, "arti", true) {
            return Some("arti".to_string());
        }
        if contains_token(&lower, "tor-", false) {
            return Some("tor-".to_string());
        }
        if lower.contains("ubyybj") {
            return Some("hollow (ROT13, UserAssist)".to_string());
        }
        None
    }
}

/// `needle` occurs in `hay` with no letter/digit right before it (and, if `both_sides`, right
/// after it).
fn contains_token(hay: &str, needle: &str, both_sides: bool) -> bool {
    let glued = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        !glued(before) && !(both_sides && glued(after))
    })
}

/// The AppContainer SID Windows derives from a container name
/// (`DeriveAppContainerSidFromAppContainerName`): `S-1-15-2-` followed by the first 28 bytes of
/// SHA-256 over the lower-cased name in UTF-16LE, as seven little-endian u32s. `trace-test`
/// cross-checks this against the `Mappings` keys Windows writes during the session.
pub fn appcontainer_sid(name: &str) -> String {
    let utf16: Vec<u8> = name.to_lowercase().encode_utf16().flat_map(u16::to_le_bytes).collect();
    let hash = Sha256::digest(&utf16);
    let parts: Vec<String> = hash[..28].as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c).to_string()).collect();
    format!("S-1-15-2-{}", parts.join("-"))
}

/// Differences between two snapshots.
pub struct Diff<'a, K, V> {
    pub added: Vec<(&'a K, &'a V)>,
    pub removed: Vec<(&'a K, &'a V)>,
    pub changed: Vec<(&'a K, &'a V, &'a V)>,
}

pub fn diff<'a, K: Ord, V: PartialEq>(before: &'a BTreeMap<K, V>, after: &'a BTreeMap<K, V>) -> Diff<'a, K, V> {
    let mut d = Diff { added: Vec::new(), removed: Vec::new(), changed: Vec::new() };
    for (k, new) in after {
        match before.get(k) {
            None => d.added.push((k, new)),
            Some(old) if old != new => d.changed.push((k, old, new)),
            Some(_) => {}
        }
    }
    for (k, old) in before {
        if !after.contains_key(k) {
            d.removed.push((k, old));
        }
    }
    d
}

/// Writers that are obviously not Hollow and change files all the time. Matched as
/// case-insensitive path substrings. Hollow-pattern matches are checked before this filter, so it
/// can't hide a Hollow trace. Keep this list short: everything else is shown for review.
pub const NOISE: [(&str, &str); 10] = [
    ("Chrome profile/cache", "\\google\\chrome\\"),
    ("Edge profile/cache", "\\microsoft\\edge\\"),
    ("Firefox profile/cache", "\\mozilla\\firefox\\"),
    ("Brave profile/cache", "\\bravesoftware\\"),
    ("OneDrive client", "\\microsoft\\onedrive\\"),
    ("VS Code", "\\appdata\\roaming\\code\\"),
    ("Discord", "\\appdata\\roaming\\discord\\"),
    ("Claude desktop/Claude Code", "\\claude-data\\"),
    ("Claude Code cache", "\\claude-cli-nodejs\\"),
    ("Claude Code temp", "\\appdata\\local\\temp\\claude\\"),
];

pub fn noise_rule(path: &str) -> Option<&'static str> {
    let lower = path.to_lowercase();
    NOISE.iter().find(|(_, pat)| lower.contains(pat)).map(|(label, _)| *label)
}

/// For a path under `base` (e.g. `%LOCALAPPDATA%\Packages` or the registry key
/// `...\AppContainer\Mappings`), the first component below it (the profile/container name).
/// `None` for `base` itself and for paths outside it. Case-insensitive.
pub fn first_component<'p>(path: &'p str, base: &str) -> Option<&'p str> {
    let head = path.get(..base.len())?;
    if !head.eq_ignore_ascii_case(base) {
        return None;
    }
    let rest = path[base.len()..].strip_prefix('\\')?;
    let name = rest.split('\\').next().unwrap_or(rest);
    (!name.is_empty()).then_some(name)
}

/// The ACEs of the DACL in an SDDL string, e.g. `(A;;0x1200a9;;;S-1-15-2-...)`.
pub fn sddl_dacl_aces(sddl: &str) -> Vec<String> {
    // Sections are `O:`, `G:`, `D:` and `S:` outside parentheses; conditional ACEs nest them.
    let chars: Vec<char> = sddl.chars().collect();
    let mut section = ' ';
    let mut depth = 0usize;
    let mut cur = String::new();
    let mut aces = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if depth == 0 && matches!(c, 'O' | 'G' | 'D' | 'S') && chars.get(i + 1) == Some(&':') {
            section = c;
            continue;
        }
        match c {
            '(' => {
                if depth == 0 {
                    cur.clear();
                }
                depth += 1;
                cur.push(c);
            }
            ')' if depth > 0 => {
                cur.push(c);
                depth -= 1;
                if depth == 0 && section == 'D' {
                    aces.push(cur.clone());
                }
            }
            _ if depth > 0 => cur.push(c),
            _ => {}
        }
    }
    aces
}

/// The trustee SID (sixth field) of an SDDL ACE string.
pub fn ace_trustee(ace: &str) -> &str {
    ace.trim_start_matches('(').trim_end_matches(')').split(';').nth(5).unwrap_or("")
}

/// One file system entry as snapshotted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileMeta {
    pub dir: bool,
    pub size: u64,
    /// Last write time, ns since the Unix epoch.
    pub mtime: Option<u128>,
}

pub type FileSnapshot = BTreeMap<String, FileMeta>;

/// The outcome of comparing one area (files, registry, ACLs) before and after the session.
#[derive(Default, Debug)]
pub struct Findings {
    /// Hollow traces this run left behind: each one fails the test.
    pub traces: Vec<String>,
    /// Other changes, for a human to review (not failures). `+` new, `~` changed, `-` removed.
    pub review: Vec<String>,
    /// Changes dropped by the noise filter, per rule.
    pub filtered: BTreeMap<&'static str, usize>,
    /// Hollow-pattern entries that were there before the run and are unchanged.
    pub preexisting: Vec<String>,
    /// Hollow-pattern entries that were there before the run and are gone now.
    pub cleaned: Vec<String>,
}

fn describe_file(path: &str, m: &FileMeta) -> String {
    if m.dir { format!("{path}\\ (folder)") } else { format!("{path} ({} bytes)", m.size) }
}

/// Files: anything new or changed whose path matches [`Patterns`], and anything new inside a
/// `packages` subfolder (AppContainer profile) that didn't exist before, is a trace.
pub fn classify_files(before: &FileSnapshot, after: &FileSnapshot, packages: &str, patterns: &Patterns) -> Findings {
    let mut f = Findings::default();
    let d = diff(before, after);
    let new_profile = |path: &str| {
        first_component(path, packages)
            .filter(|name| !before.contains_key(&format!("{packages}\\{name}")))
            .map(str::to_string)
    };
    let mut review: Vec<(char, &str, String)> = Vec::new();
    let entries = d.added.iter().map(|(p, m)| ('+', *p, *m)).chain(d.changed.iter().map(|(p, _, m)| ('~', *p, *m)));
    for (mark, path, meta) in entries {
        let what = describe_file(path, meta);
        let verb = if mark == '+' { "new" } else { "modified" };
        if let Some(pat) = patterns.hit(path) {
            f.traces.push(format!("{verb}: {what} (matches `{pat}`)"));
        } else if let Some(profile) = new_profile(path) {
            f.traces.push(format!("{verb}: {what} (new AppContainer profile folder `{profile}`)"));
        } else if let Some(rule) = noise_rule(path) {
            *f.filtered.entry(rule).or_default() += 1;
        } else {
            review.push((mark, path, what));
        }
    }
    for (path, meta) in d.removed {
        if patterns.hit(path).is_some() {
            f.cleaned.push(describe_file(path, meta));
        } else if let Some(rule) = noise_rule(path) {
            *f.filtered.entry(rule).or_default() += 1;
        } else {
            review.push(('-', path, describe_file(path, meta)));
        }
    }
    f.review = group_by_folder(review);
    for (path, meta) in after {
        if before.get(path) == Some(meta) && patterns.hit(path).is_some() && noise_rule(path).is_none() {
            f.preexisting.push(describe_file(path, meta));
        }
    }
    f
}

/// Folders with at least this many changed entries are shown as one line.
const GROUP_AT: usize = 5;

/// Review lines (`mark path`), sorted by path, with folders that have [`GROUP_AT`] or more changed
/// entries directly in them collapsed into one summary line.
fn group_by_folder(mut items: Vec<(char, &str, String)>) -> Vec<String> {
    items.sort_by(|a, b| a.1.cmp(b.1));
    let parent = |p: &str| p.rsplit_once('\\').map(|(dir, _)| dir.to_string()).unwrap_or_default();
    let mut counts: BTreeMap<String, [usize; 3]> = BTreeMap::new();
    for (mark, path, _) in &items {
        let c = counts.entry(parent(path)).or_default();
        c[match mark {
            '+' => 0,
            '~' => 1,
            _ => 2,
        }] += 1;
    }
    let mut out = Vec::new();
    let mut done = std::collections::BTreeSet::new();
    for (mark, path, line) in items {
        let dir = parent(path);
        let c = counts[&dir];
        if c.iter().sum::<usize>() < GROUP_AT {
            out.push(format!("{mark} {line}"));
        } else if done.insert(dir.clone()) {
            out.push(format!("* {dir}\\*: {} new, {} changed, {} removed", c[0], c[1], c[2]));
        }
    }
    out
}

/// What a registry value shows in the report: its data, shortened.
fn describe_reg(p: &RegPath, raw: &str) -> String {
    if p.value.is_none() {
        return format!("{} (key)", p.key);
    }
    let shown: String = raw.chars().take(72).collect();
    let more = if raw.chars().count() > 72 { "..." } else { "" };
    format!("{} = {shown}{more}", p.display())
}

/// The pattern a registry entry (key path, value name or decoded data) matches.
pub fn reg_hit(patterns: &Patterns, p: &RegPath, raw: &str) -> Option<String> {
    patterns
        .hit(&p.key)
        .or_else(|| p.value.as_deref().and_then(|v| patterns.hit(v)))
        .or_else(|| patterns.hit(&searchable(raw)))
}

/// Registry: anything new or changed that matches [`Patterns`] is a trace, and so is anything new
/// under `appcontainer` (the `...\CurrentVersion\AppContainer` key) that doesn't belong to a
/// container (a `Mappings\<SID>` or `Storage\<moniker>` key) which existed before. Review lines are
/// tagged with `area(key)`.
pub fn classify_registry(
    before: &RegSnapshot,
    after: &RegSnapshot,
    appcontainer: &str,
    area: &dyn Fn(&str) -> &'static str,
    patterns: &Patterns,
) -> Findings {
    let mut f = Findings::default();
    let d = diff(before, after);
    let mappings = format!("{appcontainer}\\Mappings");
    let storage = format!("{appcontainer}\\Storage");
    let existed = |key: &str| before.contains_key(&RegPath { key: key.to_string(), value: None });
    let entries = d.added.iter().map(|(p, v)| ('+', *p, *v)).chain(d.changed.iter().map(|(p, _, v)| ('~', *p, *v)));
    for (mark, p, raw) in entries {
        let what = describe_reg(p, raw);
        let verb = if mark == '+' { "new" } else { "modified" };
        if let Some(pat) = reg_hit(patterns, p, raw) {
            f.traces.push(format!("{verb}: {what} (matches `{pat}`)"));
            continue;
        }
        if first_component(&p.key, appcontainer).is_some() || p.key.eq_ignore_ascii_case(appcontainer) {
            let container = first_component(&p.key, &mappings)
                .map(|c| format!("{mappings}\\{c}"))
                .or_else(|| first_component(&p.key, &storage).map(|c| format!("{storage}\\{c}")));
            match container {
                Some(c) if existed(&c) => f.review.push(format!("{mark} [{}] {what}", area(&p.key))),
                Some(c) => f.traces.push(format!("{verb}: {what} (new AppContainer entry {c})")),
                None if mark == '+' => f.traces.push(format!("{verb}: {what} (new entry in the AppContainer subtree)")),
                None => f.review.push(format!("{mark} [{}] {what}", area(&p.key))),
            }
        } else if let Some(rule) = noise_rule(&p.key) {
            *f.filtered.entry(rule).or_default() += 1;
        } else {
            f.review.push(format!("{mark} [{}] {what}", area(&p.key)));
        }
    }
    for (p, raw) in d.removed {
        if reg_hit(patterns, p, raw).is_some() {
            f.cleaned.push(describe_reg(p, raw));
        } else if let Some(rule) = noise_rule(&p.key) {
            *f.filtered.entry(rule).or_default() += 1;
        } else {
            f.review.push(format!("- [{}] {}", area(&p.key), describe_reg(p, raw)));
        }
    }
    for (p, raw) in after {
        if before.get(p).map(String::as_str) == Some(raw.as_str()) && reg_hit(patterns, p, raw).is_some() {
            f.preexisting.push(describe_reg(p, raw));
        }
    }
    f
}

/// ACLs (path -> DACL ACEs in SDDL): an ACE for a Hollow SID after the session is a trace, and so
/// is any ACE that wasn't there before.
pub fn classify_acls(
    before: &BTreeMap<String, Vec<String>>,
    after: &BTreeMap<String, Vec<String>>,
    patterns: &Patterns,
) -> Findings {
    let mut f = Findings::default();
    let none = Vec::new();
    for (path, aces) in after {
        let old = before.get(path).unwrap_or(&none);
        for ace in aces {
            let hollow = patterns.is_hollow_sid(ace_trustee(ace));
            match (hollow, old.contains(ace)) {
                (true, true) => {
                    f.preexisting.push(format!("{path}: {ace}"));
                    f.traces.push(format!("{path}: {ace} is a Hollow AppContainer ACE (left from before this run)"));
                }
                (true, false) => f.traces.push(format!("{path}: {ace} is a Hollow AppContainer ACE")),
                (false, false) => f.traces.push(format!("{path}: ACE {ace} was added")),
                (false, true) => {}
            }
        }
        for ace in old.iter().filter(|a| !aces.contains(a)) {
            if patterns.is_hollow_sid(ace_trustee(ace)) {
                f.cleaned.push(format!("{path}: {ace}"));
            } else {
                f.review.push(format!("- {path}: ACE {ace} was removed"));
            }
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    const NET_SID: &str = "S-1-15-2-1-2-3-4-5-6-7";

    #[test]
    fn patterns() {
        let p = Patterns::new(&[NET_SID]);
        for yes in [
            r"C:\Users\x\AppData\Local\Packages\hollow.net\AC",
            r"C:\Users\x\AppData\Local\Temp\HOLLOW-crash.dmp",
            r"https://check.torproject.org/",
            r"C:\Windows\hollow-ram-only-never-created",
            r"C:\Users\x\AppData\Local\arti\cache",
            r"C:\Users\x\.local\share\arti-client\state",
            r"C:\x\tor-dirmgr.log",
            r"C:\x\cache\dir.sqlite3",
            r"C:\x\state\guards.json",
            r"HKCU\...\AppContainer\Mappings\s-1-15-2-1-2-3-4-5-6-7",
            // ROT13 "C:\...\hollow.exe" as UserAssist stores it.
            r"P:\_Cebwrpg\Oebjfre\gnetrg\qvfg\eryrnfr\ubyybj.rkr",
        ] {
            assert!(p.hit(yes).is_some(), "should match: {yes}");
        }
        for no in [
            r"C:\Users\x\Downloads\setup.exe.partial",
            r"C:\Users\x\AppData\Local\Microsoft\Edge\User Data\Default\Partitions",
            r"C:\Users\x\Documents\article.txt",
            r"C:\Users\x\AppData\Roaming\Code\editor-state.json",
            r"C:\x\monitor-1.log",
            r"C:\x\S-1-15-2-1-2-3-4-5-6-8",
            r"C:\Users\x\AppData\Local\Temp\martini",
        ] {
            assert_eq!(p.hit(no), None, "should not match: {no}");
        }
    }

    #[test]
    fn known_appcontainer_sid() {
        // A Windows inbox package, read from this key on a Windows 11 machine:
        // HKCU\...\AppContainer\Mappings\<SID> with Moniker = MicrosoftWindows.Client.CoreAI_cw5n1h2txyewy
        assert_eq!(
            appcontainer_sid("MicrosoftWindows.Client.CoreAI_cw5n1h2txyewy"),
            "S-1-15-2-1075972307-1477995677-3981890472-2965168153-2910496065-638303522-2449858333"
        );
        // The derivation lower-cases the name first.
        assert_eq!(appcontainer_sid("Hollow.Net"), appcontainer_sid("hollow.net"));
        assert_ne!(appcontainer_sid("Hollow.Net"), appcontainer_sid("Hollow.Fortress"));
    }

    #[test]
    fn diffs() {
        let before: BTreeMap<&str, u32> = [("a", 1), ("b", 2), ("c", 3)].into_iter().collect();
        let after: BTreeMap<&str, u32> = [("a", 1), ("b", 5), ("d", 4)].into_iter().collect();
        let d = diff(&before, &after);
        assert_eq!(d.added, vec![(&"d", &4)]);
        assert_eq!(d.removed, vec![(&"c", &3)]);
        assert_eq!(d.changed, vec![(&"b", &2, &5)]);
        let same = diff(&before, &before);
        assert!(same.added.is_empty() && same.removed.is_empty() && same.changed.is_empty());
    }

    #[test]
    fn noise() {
        assert_eq!(
            noise_rule(r"C:\Users\x\AppData\Local\Google\Chrome\User Data\Default\Cache\f_0001"),
            Some("Chrome profile/cache")
        );
        assert_eq!(noise_rule(r"C:\Users\x\AppData\Local\Microsoft\OneDrive\logs\a.odl"), Some("OneDrive client"));
        // Redirected Documents are not the OneDrive client's data.
        assert_eq!(noise_rule(r"C:\Users\x\OneDrive\Documents\a.txt"), None);
        assert_eq!(noise_rule(r"C:\Users\x\AppData\Local\Packages\hollow.net"), None);
        assert_eq!(noise_rule(r"C:\Users\x\AppData\Local\Temp\x.tmp"), None);
    }

    fn file(size: u64, mtime: u128) -> FileMeta {
        FileMeta { dir: false, size, mtime: Some(mtime) }
    }

    #[test]
    fn file_classification() {
        let pk = r"C:\U\AppData\Local\Packages";
        let before: FileSnapshot = [
            (format!("{pk}\\other.app"), FileMeta { dir: true, size: 0, mtime: Some(1) }),
            (format!("{pk}\\other.app\\LocalState\\a.db"), file(10, 1)),
            (r"C:\U\AppData\Local\Temp\old.tmp".to_string(), file(1, 1)),
            (r"C:\U\AppData\Local\Temp\hollow-old.log".to_string(), file(1, 1)),
            (r"C:\U\Downloads\hollow-notes.txt".to_string(), file(5, 1)),
        ]
        .into_iter()
        .collect();
        let mut after = before.clone();
        after.remove(r"C:\U\AppData\Local\Temp\old.tmp");
        after.remove(r"C:\U\AppData\Local\Temp\hollow-old.log");
        after.insert(format!("{pk}\\other.app\\LocalState\\a.db"), file(20, 2));
        after.insert(format!("{pk}\\new.profile"), FileMeta { dir: true, size: 0, mtime: Some(2) });
        after.insert(r"C:\U\AppData\Local\Temp\arti-cache\dir.sqlite3".to_string(), file(9, 2));
        after.insert(r"C:\U\AppData\Local\Google\Chrome\User Data\x".to_string(), file(1, 2));
        after.insert(r"C:\U\AppData\Local\Temp\unrelated.tmp".to_string(), file(3, 2));

        let f = classify_files(&before, &after, pk, &Patterns::new(&[NET_SID]));
        assert_eq!(f.traces.len(), 2, "{:?}", f.traces);
        assert!(f.traces.iter().any(|t| t.contains("dir.sqlite3")));
        assert!(f.traces.iter().any(|t| t.contains("new AppContainer profile folder `new.profile`")));
        // Writes into another app's existing profile, and unrelated files, are for review only.
        assert_eq!(f.review.len(), 3, "{:?}", f.review);
        assert!(f.review.iter().any(|r| r.starts_with("~ ") && r.contains("other.app")));
        assert!(f.review.iter().any(|r| r.starts_with("+ ") && r.contains("unrelated.tmp")));
        assert!(f.review.iter().any(|r| r.starts_with("- ") && r.contains("old.tmp")));
        assert_eq!(f.filtered.get("Chrome profile/cache"), Some(&1));
        assert_eq!(f.cleaned.len(), 1);
        assert_eq!(f.preexisting.len(), 1);
        assert!(f.preexisting[0].contains("hollow-notes.txt"));
    }

    #[test]
    fn review_grouping() {
        let paths: Vec<String> = (0..6).map(|i| format!(r"C:\L\D3DSCache\{i}")).collect();
        let mut items: Vec<(char, &str, String)> = paths.iter().map(|p| ('~', p.as_str(), p.clone())).collect();
        items[0].0 = '+';
        items.push(('-', r"C:\L\Temp\a.tmp", r"C:\L\Temp\a.tmp".to_string()));
        assert_eq!(
            group_by_folder(items),
            vec![r"* C:\L\D3DSCache\*: 1 new, 5 changed, 0 removed".to_string(), r"- C:\L\Temp\a.tmp".to_string()]
        );
    }

    #[test]
    fn registry_classification() {
        let ac =
            r"HKEY_CURRENT_USER\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer";
        let key = |k: String| RegPath { key: k, value: None };
        let val = |k: String, v: &str| RegPath { key: k, value: Some(v.to_string()) };
        let other = format!("{ac}\\Storage\\other.app");
        let before: RegSnapshot = [
            (key(ac.to_string()), String::new()),
            (key(format!("{ac}\\Mappings")), String::new()),
            (key(format!("{ac}\\Storage")), String::new()),
            (key(other.clone()), String::new()),
            (val(other.clone(), "Counter"), "dword:00000001".to_string()),
        ]
        .into_iter()
        .collect();
        let mut after = before.clone();
        after.insert(val(other.clone(), "Counter"), "dword:00000002".to_string());
        after.insert(key(format!("{ac}\\Storage\\some.new.app")), String::new());
        let hollow_sid_key = format!("{ac}\\Mappings\\{NET_SID}");
        after.insert(key(hollow_sid_key.clone()), String::new());
        after.insert(val(hollow_sid_key, "Moniker"), "\"hollow.net\"".to_string());
        let explorer = r"HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Explorer\RecentDocs".to_string();
        // UTF-16 "hollow" inside a binary RecentDocs record.
        after.insert(val(explorer.clone(), "7"), "hex:00,68,00,6f,00,6c,00,6c,00,6f,00,77,00,00,00".to_string());
        after.insert(val(explorer, "MRUListEx"), "hex:07,00,00,00".to_string());

        let area = |k: &str| if k.contains("Explorer") { "Explorer" } else { "AppContainer" };
        let f = classify_registry(&before, &after, ac, &area, &Patterns::new(&[NET_SID]));
        assert_eq!(f.traces.len(), 4, "{:?}", f.traces);
        assert!(f.traces.iter().any(|t| t.contains("new AppContainer entry") && t.contains("some.new.app")));
        assert!(f.traces.iter().any(|t| t.contains("[7]") && t.contains("matches `hollow`")));
        assert_eq!(f.review.len(), 2, "{:?}", f.review);
        assert!(f.review.iter().any(|r| r.starts_with("~ [AppContainer]") && r.contains("Counter")));
        assert!(f.review.iter().any(|r| r.starts_with("+ [Explorer]") && r.contains("MRUListEx")));
    }

    #[test]
    fn acl_classification() {
        let p = Patterns::new(&[NET_SID]);
        let base = vec!["(A;ID;FA;;;BA)".to_string(), "(A;ID;FA;;;SY)".to_string()];
        let hollow_ace = format!("(A;;0x1200a9;;;{NET_SID})");
        let before: BTreeMap<String, Vec<String>> = [("dir".to_string(), base.clone())].into_iter().collect();
        // Clean after the session: nothing to report.
        let f = classify_acls(&before, &before, &p);
        assert!(f.traces.is_empty() && f.review.is_empty());
        // A Hollow ACE left behind fails; so does any other new ACE.
        let mut left = base.clone();
        left.push(hollow_ace.clone());
        left.push("(A;;FA;;;S-1-15-2-9-9-9-9-9-9-9)".to_string());
        let after: BTreeMap<String, Vec<String>> = [("dir".to_string(), left.clone())].into_iter().collect();
        let f = classify_acls(&before, &after, &p);
        assert_eq!(f.traces.len(), 2, "{:?}", f.traces);
        // A leftover from an earlier crash that this run removed is reported as cleaned up.
        let before_left: BTreeMap<String, Vec<String>> = [("dir".to_string(), left)].into_iter().collect();
        let f = classify_acls(&before_left, &before, &p);
        assert!(f.traces.is_empty());
        assert_eq!(f.cleaned, vec![format!("dir: {hollow_ace}")]);
        assert_eq!(f.review.len(), 1);
    }

    #[test]
    fn components() {
        let base = r"C:\Users\x\AppData\Local\Packages";
        assert_eq!(first_component(r"C:\Users\x\AppData\Local\Packages\hollow.net\AC\Temp", base), Some("hollow.net"));
        assert_eq!(first_component(r"c:\users\x\appdata\local\packages\hollow.net", base), Some("hollow.net"));
        assert_eq!(first_component(base, base), None);
        assert_eq!(first_component(r"C:\Users\x\AppData\Local\PackagesX\y", base), None);
        assert_eq!(first_component(r"C:\Users\x\AppData\Local\Temp", base), None);
    }

    #[test]
    fn sddl() {
        let sddl = "D:AI(A;ID;FA;;;BA)(A;;0x1200a9;;;S-1-15-2-1-2-3-4-5-6-7)(A;ID;0x1301bf;;;AU)";
        let aces = sddl_dacl_aces(sddl);
        assert_eq!(aces.len(), 3);
        assert_eq!(aces[1], "(A;;0x1200a9;;;S-1-15-2-1-2-3-4-5-6-7)");
        assert_eq!(ace_trustee(&aces[1]), NET_SID);
        assert_eq!(ace_trustee(&aces[0]), "BA");
        assert!(Patterns::new(&[NET_SID]).is_hollow_sid(ace_trustee(&aces[1])));
        // Conditional ACEs nest parentheses; a SACL after the DACL is not part of it.
        let nested = "O:BAD:(XA;;FX;;;WD;(WIN://SYSAPPID Contains \"x\"))(A;;FA;;;SY)S:(ML;;NW;;;LW)";
        let aces = sddl_dacl_aces(nested);
        assert_eq!(aces, vec!["(XA;;FX;;;WD;(WIN://SYSAPPID Contains \"x\"))", "(A;;FA;;;SY)"]);
        assert_eq!(ace_trustee(&aces[0]), "WD");
        assert!(sddl_dacl_aces("O:BA").is_empty());
    }
}
