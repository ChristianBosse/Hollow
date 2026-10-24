//! `cargo xtask wpt`: the Web Platform Tests reftests the Fortress engine can run, scored against
//! a checked-in list of expected passes.
//!
//! A reftest renders a test page and its references at 800x600; it passes if it matches one of
//! its `rel=match` references and none of its `rel=mismatch` ones. Pages are loaded the way WPT's
//! server would serve them: from a sparse checkout of web-platform-tests at a pinned commit in
//! `target/wpt`, with `http://web-platform.test/` standing for its root, so relative and
//! root-relative (`/fonts/ahem.css`) stylesheets and images resolve to files in the checkout. XML
//! files go through a small pre-pass ([`xml`]) since the engine only has an HTML parser. The
//! engine has no scripts, so tests with `<script>` or `class=reftest-wait` are skipped and
//! counted.
//!
//! Tests run in-process on a pool of worker threads. A test still running after [`TIMEOUT`] is
//! recorded as a timeout and its worker is abandoned (a thread cannot be killed; it finishes or
//! dies with the process) and replaced. Reference renders are shared between the tests that use
//! them and dropped once the last one is done.
//!
//! The WPT `Ahem` font is registered through hollow-engine's `test-fonts` feature, which only
//! xtask enables.

mod discover;
mod render;
mod xml;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use url::Url;

use self::discover::Found;
use crate::{no_more_args, take_flag, take_value, workspace_root};

/// The web-platform-tests commit the expectations are for. Bump it together with an `--update`.
pub const WPT_COMMIT: &str = "ffe56d4420d270941329689aff42d925f615d0b4";
const WPT_REPO: &str = "https://github.com/web-platform-tests/wpt.git";
/// Directories whose reftests are run.
const TEST_DIRS: &[&str] = &[
    "css/CSS2",
    "css/css-backgrounds",
    "css/css-content",
    "css/css-flexbox",
    "css/css-grid",
    "css/css-position",
    "css/css-pseudo",
    "css/css-tables",
    "css/css-text",
    "css/css-writing-modes",
    "css/selectors",
];
/// Checked out besides [`TEST_DIRS`]: shared fonts, stylesheets, references and images that
/// tests load by root-relative URL.
const SUPPORT_DIRS: &[&str] = &["fonts", "css/support", "css/reference", "images"];
/// Stands in for the WPT server's origin.
const ORIGIN: &str = "http://web-platform.test/";
const TIMEOUT: Duration = Duration::from_secs(20);
const EXPECTED: &str = "tests/wpt/expected.txt";
const RESULTS: &str = "target/wpt-results.json";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fuzzy {
    /// Allowed range of the largest channel difference, inclusive.
    pub max_difference: (u32, u32),
    /// Allowed range of the number of differing pixels, inclusive.
    pub total_pixels: (u32, u32),
}

pub struct Reference {
    /// Relative to the checkout, `/`-separated.
    pub path: String,
    pub mismatch: bool,
    pub fuzzy: Option<Fuzzy>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Skip {
    ReftestWait,
    Script,
    Print,
    MissingReference,
}

impl Skip {
    fn label(self) -> &'static str {
        match self {
            Skip::ReftestWait => "reftest-wait",
            Skip::Script => "script",
            Skip::Print => "print",
            Skip::MissingReference => "missing-reference",
        }
    }
}

struct Test {
    path: String,
    refs: Vec<Reference>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Pass,
    Fail,
    Timeout,
    Crash,
    Error,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Timeout => "timeout",
            Status::Crash => "crash",
            Status::Error => "error",
        }
    }
}

struct Outcome {
    status: Status,
    /// The failing comparison (or the last one), if any.
    diff: Option<render::Diff>,
    message: Option<String>,
    /// Wall time of the test and the references it rendered.
    millis: u64,
}

impl Outcome {
    fn new(status: Status) -> Outcome {
        Outcome { status, diff: None, message: None, millis: 0 }
    }
}

pub fn run(args: &[String]) -> anyhow::Result<bool> {
    let mut args = args.to_vec();
    let fetch = take_flag(&mut args, "--fetch");
    let update = take_flag(&mut args, "--update");
    let filter = take_value(&mut args, "--filter")?.map(|f| f.replace('\\', "/").trim_start_matches('/').to_owned());
    let jobs = match take_value(&mut args, "--jobs")? {
        Some(j) => j.parse::<usize>().ok().filter(|&j| j > 0).context("--jobs must be a positive number")?,
        None => std::thread::available_parallelism().map_or(4, |n| n.get()),
    };
    let compare = take_value(&mut args, "--compare")?.map(PathBuf::from);
    let save = take_value(&mut args, "--save")?.map(PathBuf::from);
    no_more_args(&args)?;
    let filter = filter.unwrap_or_default();

    let root = workspace_root();
    let wpt = root.join("target").join("wpt");
    if fetch {
        fetch_checkout(&wpt)?;
    }
    match git_head(&wpt) {
        Some(head) if head == WPT_COMMIT => {}
        Some(head) => {
            bail!("{} is at {head}, not the pinned {WPT_COMMIT}; run `cargo xtask wpt --fetch`", wpt.display())
        }
        None => bail!("no web-platform-tests checkout in {}; run `cargo xtask wpt --fetch`", wpt.display()),
    }
    register_ahem(&wpt);
    if let Some(dir) = &save {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let started = Instant::now();
    let dirs: Vec<&str> =
        TEST_DIRS.iter().copied().filter(|d| d.starts_with(&filter) || filter.starts_with(&format!("{d}/"))).collect();
    if dirs.is_empty() {
        bail!("--filter {filter} matches none of the test directories ({})", TEST_DIRS.join(", "));
    }
    let mut files = Vec::new();
    for d in dirs {
        discover::candidates(&wpt, d, &mut files);
    }
    files.retain(|f| f.starts_with(&filter));
    let found = par_map(&files, jobs, |f| discover::examine(&wpt, f));
    let mut tests = Vec::new();
    let mut skipped = BTreeMap::new();
    for (path, found) in files.into_iter().zip(found) {
        match found {
            Found::Reftest(refs) => tests.push(Test { path, refs }),
            Found::Skipped(why) => {
                skipped.insert(path, why);
            }
            Found::NotReftest => {}
        }
    }
    println!("{} reftests to run, {} skipped (discovered in {:.1?})", tests.len(), skipped.len(), started.elapsed());

    let results = run_all(&wpt, tests, jobs, save);
    let elapsed = started.elapsed();

    print_areas(&results, &skipped);
    println!("\nRan in {elapsed:.1?} on {jobs} threads.");

    let expected_file = root.join(EXPECTED);
    let (expected_commit, expected) = read_expected(&expected_file)?;
    if expected_commit.as_deref().is_some_and(|c| c != WPT_COMMIT) {
        println!(
            "\nnote: {EXPECTED} is for wpt commit {}, the checkout is {WPT_COMMIT}",
            expected_commit.unwrap_or_default()
        );
    }
    let in_scope: BTreeSet<&String> = expected.iter().filter(|t| t.starts_with(&filter)).collect();
    let passing: BTreeSet<&String> = results.iter().filter(|(_, o)| o.status == Status::Pass).map(|(t, _)| t).collect();
    let regressions: Vec<&&String> = in_scope.iter().filter(|t| !passing.contains(**t)).collect();
    let new_passes: Vec<&&String> = passing.iter().filter(|t| !expected.contains(**t)).collect();
    println!("\nREGRESSIONS (expected to pass, now failing): {}", regressions.len());
    for t in &regressions {
        let why = results.get(**t).map(|o| o.status.label()).or(skipped.get(**t).map(|s| s.label()));
        println!("  {t}  ({})", why.unwrap_or("not run"));
    }
    println!("NEW PASSES: {}", new_passes.len());
    const SHOWN: usize = 200;
    for t in new_passes.iter().take(SHOWN) {
        println!("  {t}");
    }
    if new_passes.len() > SHOWN {
        println!("  ... and {} more (see {RESULTS})", new_passes.len() - SHOWN);
    }

    write_results(&root.join(RESULTS), &results, &skipped, elapsed)?;
    println!("\nResults: {RESULTS}");
    if let Some(dir) = compare {
        compare_browsers(&dir, &results)?;
    }

    if update {
        let mut next: BTreeSet<String> = expected.iter().filter(|t| !t.starts_with(&filter)).cloned().collect();
        next.extend(passing.iter().map(|t| (*t).clone()));
        write_expected(&expected_file, &next)?;
        println!("Updated {EXPECTED}: {} expected passes", next.len());
        return Ok(true);
    }
    if !regressions.is_empty() {
        println!("\nFAIL: {} regression(s). If they are intended, run with --update.", regressions.len());
        return Ok(false);
    }
    if !new_passes.is_empty() {
        println!("\nPASS. Run with --update to record the new passes.");
    } else {
        println!("\nPASS.");
    }
    Ok(true)
}

/// The URL a file is served at.
pub fn base_url(rel: &str) -> Url {
    Url::parse(ORIGIN).and_then(|o| o.join(rel)).unwrap_or_else(|_| unreachable!("constant origin"))
}

/// The checkout path (relative, `/`-separated) a URL on [`ORIGIN`] is served from.
pub fn local_path(url: &Url) -> Option<String> {
    if url.scheme() != "http" || url.host_str() != Some("web-platform.test") {
        return None;
    }
    let mut parts = Vec::new();
    for seg in url.path_segments()? {
        let seg = percent_decode(seg);
        if seg.is_empty() || seg.contains(['\\', ':']) || seg == ".." {
            return None;
        }
        parts.push(seg);
    }
    Some(parts.join("/"))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c))) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A test file as the HTML parser should see it.
pub fn read_source(root: &Path, rel: &str) -> anyhow::Result<String> {
    let bytes = std::fs::read(root.join(rel)).with_context(|| format!("reading {rel}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let ext = rel.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    Ok(if matches!(ext.as_str(), "xht" | "xhtml" | "xml") { xml::to_html(text) } else { text.to_owned() })
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("git").current_dir(dir).args(args).status().context("running git")?;
    anyhow::ensure!(status.success(), "git {} failed ({status})", args.join(" "));
    Ok(())
}

fn git_head(dir: &Path) -> Option<String> {
    if !dir.join(".git").exists() {
        return None;
    }
    let out = Command::new("git").current_dir(dir).args(["rev-parse", "HEAD"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Makes `dir` a shallow, blobless, sparse checkout of [`WPT_COMMIT`].
fn fetch_checkout(dir: &Path) -> anyhow::Result<()> {
    if !dir.join(".git").exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        git(dir, &["init", "-q"])?;
        git(dir, &["remote", "add", "origin", WPT_REPO])?;
    }
    let mut sparse = vec!["sparse-checkout", "set"];
    sparse.extend(TEST_DIRS.iter().chain(SUPPORT_DIRS));
    git(dir, &sparse)?;
    if git_head(dir).as_deref() == Some(WPT_COMMIT) {
        println!("{} is at {WPT_COMMIT}", dir.display());
        return Ok(());
    }
    println!("Fetching web-platform-tests {WPT_COMMIT} into {}", dir.display());
    git(dir, &["fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", WPT_COMMIT])?;
    git(dir, &["-c", "advice.detachedHead=false", "checkout", "-q", "--detach", WPT_COMMIT])?;
    Ok(())
}

fn register_ahem(wpt: &Path) {
    let path = wpt.join("fonts").join("Ahem.ttf");
    match std::fs::read(&path) {
        Ok(data) => {
            if !hollow_engine::register_test_fonts(vec![("Ahem".to_owned(), data)]) {
                println!("warning: could not register {}", path.display());
            }
        }
        Err(e) => println!("warning: {}: {e}; tests using Ahem will fail", path.display()),
    }
}

/// `f` over `items` on `jobs` threads, results in order.
fn par_map<T: Sync, R: Send>(items: &[T], jobs: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let mut out: Vec<(usize, R)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..jobs)
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break done };
                        done.push((i, f(item)));
                    }
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect()
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

type Rendered = Result<Arc<Vec<u8>>, String>;
/// A reference's render (once made) and how many tests still need it.
type RefEntry = (Arc<OnceLock<Rendered>>, usize);

/// Reference renders, each kept until the last test using it is done.
struct RefCache {
    entries: Mutex<HashMap<String, RefEntry>>,
}

impl RefCache {
    fn new(tests: &[Test]) -> RefCache {
        let mut entries: HashMap<String, RefEntry> = HashMap::new();
        for r in tests.iter().flat_map(|t| &t.refs) {
            entries.entry(r.path.clone()).or_default().1 += 1;
        }
        RefCache { entries: Mutex::new(entries) }
    }

    fn get(&self, root: &Path, path: &str) -> Rendered {
        let cell = {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries.entry(path.to_owned()).or_insert_with(|| (Arc::default(), 1)).0.clone()
        };
        cell.get_or_init(|| render::render(root, path).map(Arc::new).map_err(|e| format!("{e:#}"))).clone()
    }

    fn release(&self, path: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = entries.get_mut(path) {
            e.1 = e.1.saturating_sub(1);
            if e.1 == 0 {
                entries.remove(path);
            }
        }
    }
}

fn run_test(root: &Path, test: &Test, cache: &RefCache, save: Option<&Path>) -> Outcome {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| compare_refs(root, test, cache, save)));
    for r in &test.refs {
        cache.release(&r.path);
    }
    match result {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(e)) => Outcome { message: Some(e), ..Outcome::new(Status::Error) },
        Err(_) => Outcome::new(Status::Crash),
    }
}

/// Passes if any `match` reference (if there are some) and no `mismatch` reference renders equal.
/// With `save`, a failing test's render and that of the reference it was last compared with are
/// written there as PNGs.
fn compare_refs(root: &Path, test: &Test, cache: &RefCache, save: Option<&Path>) -> Result<Outcome, String> {
    let frame = render::render(root, &test.path).map_err(|e| format!("{e:#}"))?;
    let mut matched = !test.refs.iter().any(|r| !r.mismatch);
    let mut last = None;
    let mut failed = false;
    // Mismatches first: one equal mismatch fails the test without rendering the rest.
    let mut order: Vec<&Reference> = test.refs.iter().collect();
    order.sort_by_key(|r| !r.mismatch);
    for r in order {
        if !r.mismatch && matched {
            break;
        }
        let reference = cache.get(root, &r.path).map_err(|e| format!("{}: {e}", r.path))?;
        let d = render::diff(&frame, &reference);
        let equal = render::equal(&d, r.fuzzy);
        last = Some((d, reference));
        if r.mismatch && equal {
            failed = true;
            break;
        }
        if !r.mismatch && equal {
            matched = true;
        }
    }
    let status = if matched && !failed { Status::Pass } else { Status::Fail };
    if let (Some(dir), Status::Fail, Some((_, reference))) = (save, status, &last) {
        let name = test.path.replace('/', "__");
        let saved = render::save_png(&dir.join(format!("{name}.png")), &frame)
            .and_then(|()| render::save_png(&dir.join(format!("{name}.ref.png")), reference));
        if let Err(e) = saved {
            println!("  could not save {name}: {e:#}");
        }
    }
    Ok(Outcome { diff: last.map(|l| l.0), ..Outcome::new(status) })
}

/// Per worker: the test it is on and since when.
#[derive(Default)]
struct Slot {
    current: Mutex<Option<(usize, Instant)>>,
    abandoned: AtomicBool,
}

/// Runs every test on `jobs` workers, with [`TIMEOUT`] per test.
fn run_all(root: &Path, mut tests: Vec<Test>, jobs: usize, save: Option<PathBuf>) -> BTreeMap<String, Outcome> {
    // Tests sharing a reference run close together, so its render is not kept for long.
    tests.sort_by(|a, b| a.refs.first().map(|r| &r.path).cmp(&b.refs.first().map(|r| &r.path)));
    let cache = Arc::new(RefCache::new(&tests));
    let tests = Arc::new(tests);
    let root = Arc::new(root.to_path_buf());
    let save = Arc::new(save);
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel::<(usize, Outcome)>();
    let spawn = |slot: Arc<Slot>| {
        let (tests, cache, root, save) = (tests.clone(), cache.clone(), root.clone(), save.clone());
        let (next, tx) = (next.clone(), tx.clone());
        // Big stacks: hostile documents nest deeply.
        std::thread::Builder::new().stack_size(16 << 20).spawn(move || {
            while !slot.abandoned.load(Ordering::Relaxed) {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(test) = tests.get(i) else { break };
                *slot.current.lock().unwrap_or_else(|e| e.into_inner()) = Some((i, Instant::now()));
                let started = Instant::now();
                let mut outcome = run_test(&root, test, &cache, save.as_deref());
                outcome.millis = started.elapsed().as_millis() as u64;
                *slot.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
                if tx.send((i, outcome)).is_err() {
                    break;
                }
            }
        })
    };
    let mut slots: Vec<Arc<Slot>> = Vec::new();
    for _ in 0..jobs {
        let slot = Arc::new(Slot::default());
        if spawn(slot.clone()).is_ok() {
            slots.push(slot);
        }
    }
    let n = tests.len();
    let mut results: Vec<Option<Outcome>> = (0..n).map(|_| None).collect();
    let mut done = 0;
    let mut next_report = 1000;
    while done < n {
        if let Ok((i, outcome)) = rx.recv_timeout(Duration::from_millis(200))
            && results[i].is_none()
        {
            results[i] = Some(outcome);
            done += 1;
        }
        let mut replacements = 0;
        for slot in slots.iter().filter(|s| !s.abandoned.load(Ordering::Relaxed)) {
            let current = *slot.current.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((i, since)) = current
                && since.elapsed() > TIMEOUT
            {
                slot.abandoned.store(true, Ordering::Relaxed);
                if results[i].is_none() {
                    println!("  timeout: {}", tests[i].path);
                    results[i] = Some(Outcome::new(Status::Timeout));
                    done += 1;
                }
                replacements += 1;
            }
        }
        for _ in 0..replacements {
            let slot = Arc::new(Slot::default());
            if spawn(slot.clone()).is_ok() {
                slots.push(slot);
            }
        }
        if slots.iter().all(|s| s.abandoned.load(Ordering::Relaxed)) && done < n {
            // Could not start any worker; nothing more will finish.
            break;
        }
        if done >= next_report {
            println!("  {done}/{n}");
            next_report += 1000;
        }
    }
    tests.iter().zip(results).map(|(t, o)| (t.path.clone(), o.unwrap_or_else(|| Outcome::new(Status::Error)))).collect()
}

/// `css/CSS2/<subdir>` for CSS 2.1 tests, else `css/<spec>`.
fn area(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let n = if parts.get(1) == Some(&"CSS2") && parts.len() > 3 { 3 } else { 2 };
    parts[..n.min(parts.len())].join("/")
}

fn print_areas(results: &BTreeMap<String, Outcome>, skipped: &BTreeMap<String, Skip>) {
    let mut areas: BTreeMap<String, BTreeMap<Status, usize>> = BTreeMap::new();
    for (path, o) in results {
        *areas.entry(area(path)).or_default().entry(o.status).or_default() += 1;
    }
    println!("\n{:<36}{:>14}{:>8}{:>7}{:>9}{:>7}", "area", "pass/total", "%", "fail", "timeout", "error");
    let mut all: BTreeMap<Status, usize> = BTreeMap::new();
    let row = |name: &str, c: &BTreeMap<Status, usize>| {
        let get = |s| c.get(&s).copied().unwrap_or(0);
        let total: usize = c.values().sum();
        println!(
            "{name:<36}{:>14}{:>7.1}%{:>7}{:>9}{:>7}",
            format!("{}/{total}", get(Status::Pass)),
            100.0 * get(Status::Pass) as f64 / total.max(1) as f64,
            get(Status::Fail),
            get(Status::Timeout),
            get(Status::Crash) + get(Status::Error)
        );
    };
    for (name, c) in &areas {
        row(name, c);
        for (s, n) in c {
            *all.entry(*s).or_default() += n;
        }
    }
    row("TOTAL", &all);
    let mut skips: BTreeMap<Skip, usize> = BTreeMap::new();
    for s in skipped.values() {
        *skips.entry(*s).or_default() += 1;
    }
    let list: Vec<String> = skips.iter().map(|(s, n)| format!("{n} {}", s.label())).collect();
    println!("Skipped: {} ({})", skipped.len(), if list.is_empty() { "none".to_owned() } else { list.join(", ") });
}

fn read_expected(path: &Path) -> anyhow::Result<(Option<String>, BTreeSet<String>)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, BTreeSet::new())),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut commit = None;
    let mut tests = BTreeSet::new();
    for line in text.lines().map(str::trim) {
        if let Some(c) = line.strip_prefix("# wpt commit:") {
            commit = Some(c.trim().to_owned());
        } else if !line.is_empty() && !line.starts_with('#') {
            tests.insert(line.to_owned());
        }
    }
    Ok((commit, tests))
}

fn write_expected(path: &Path, tests: &BTreeSet<String>) -> anyhow::Result<()> {
    let mut text = format!(
        "# Web Platform Tests reftests the Fortress engine passes; `cargo xtask wpt` fails if one of\n\
         # them stops passing. Regenerate with `cargo xtask wpt --update` (see README).\n\
         # wpt commit: {WPT_COMMIT}\n"
    );
    for t in tests {
        text.push_str(t);
        text.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

fn write_results(
    path: &Path,
    results: &BTreeMap<String, Outcome>,
    skipped: &BTreeMap<String, Skip>,
    elapsed: Duration,
) -> anyhow::Result<()> {
    let tests: serde_json::Map<String, serde_json::Value> = results
        .iter()
        .map(|(t, o)| {
            let mut v = serde_json::json!({ "status": o.status.label(), "ms": o.millis });
            if let Some(d) = o.diff {
                v["max_difference"] = d.max.into();
                v["differing_pixels"] = d.differing.into();
            }
            if let Some(m) = &o.message {
                v["message"] = m.as_str().into();
            }
            (t.clone(), v)
        })
        .collect();
    let skipped: serde_json::Map<String, serde_json::Value> =
        skipped.iter().map(|(t, s)| (t.clone(), s.label().into())).collect();
    let json = serde_json::json!({
        "wpt_commit": WPT_COMMIT,
        "elapsed_seconds": elapsed.as_secs_f64(),
        "tests": tests,
        "skipped": skipped,
    });
    std::fs::write(path, serde_json::to_string_pretty(&json)?).with_context(|| format!("writing {}", path.display()))
}

/// Per-area pass rates next to the browsers' wpt.fyi results saved in `dir` (one
/// `<browser>.json[.gz]` per browser: wpt.fyi's summary format, `{"/path": {"s": status, "c":
/// [passed, total]}}`, uncompressed), on the tests Hollow ran that every browser ran too.
fn compare_browsers(dir: &Path, results: &BTreeMap<String, Outcome>) -> anyhow::Result<()> {
    let mut runs = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".json") || p.to_string_lossy().ends_with(".json.gz"))
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let name = name.split('.').next().unwrap_or_default().to_owned();
        let bytes = std::fs::read(&path)?;
        let run: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&bytes).with_context(|| {
            format!("{}: not wpt.fyi summary JSON (gzipped files must be unpacked)", path.display())
        })?;
        runs.push((name, run));
    }
    anyhow::ensure!(!runs.is_empty(), "no *.json summaries in {}", dir.display());
    let passed = |r: &serde_json::Value| {
        let c = |i: usize| r["c"].get(i).and_then(|v| v.as_u64()).unwrap_or(0);
        r["s"] == "P" || (c(1) > 0 && c(0) == c(1))
    };
    let mut rows: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (path, o) in results {
        let key = format!("/{path}");
        if runs.iter().any(|(_, run)| !run.contains_key(&key)) {
            continue;
        }
        let row = rows.entry(area(path)).or_insert_with(|| vec![0; runs.len() + 2]);
        row[0] += 1;
        row[1] += usize::from(o.status == Status::Pass);
        for (i, (_, run)) in runs.iter().enumerate() {
            row[i + 2] += usize::from(passed(&run[&key]));
        }
    }
    let mut header = format!("\n{:<36}{:>6}{:>9}", "area", "tests", "hollow");
    for (name, _) in &runs {
        header.push_str(&format!("{name:>9}"));
    }
    println!("{header}");
    let mut total = vec![0; runs.len() + 2];
    let line = |name: &str, r: &[usize], decimals: usize| {
        let mut s = format!("{name:<36}{:>6}", r[0]);
        for v in &r[1..] {
            s.push_str(&format!("{:>8.*}%", decimals, 100.0 * *v as f64 / r[0].max(1) as f64));
        }
        println!("{s}");
    };
    for (name, r) in &rows {
        line(name, r, 0);
        for (t, v) in total.iter_mut().zip(r) {
            *t += v;
        }
    }
    line("ALL", &total, 1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_map_into_the_checkout() {
        let base = base_url("css/CSS2/abspos/test.xht");
        let path = |u: &str| base.join(u).ok().and_then(|u| local_path(&u));
        assert_eq!(path("ref.xht").as_deref(), Some("css/CSS2/abspos/ref.xht"));
        assert_eq!(path("../support/a%20b.css").as_deref(), Some("css/CSS2/support/a b.css"));
        assert_eq!(path("/fonts/ahem.css").as_deref(), Some("fonts/ahem.css"));
        assert_eq!(path("../../../../../x.css").as_deref(), Some("x.css"));
        assert_eq!(path("https://example.org/x.css"), None);
    }

    #[test]
    fn areas() {
        assert_eq!(area("css/CSS2/abspos/a.xht"), "css/CSS2/abspos");
        assert_eq!(area("css/CSS2/a.xht"), "css/CSS2");
        assert_eq!(area("css/css-grid/sub/a.html"), "css/css-grid");
    }
}
