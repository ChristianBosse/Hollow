//! `cargo xtask trace-test`: property P5 ("a session leaves nothing on disk"), host edition.
//!
//! Takes a snapshot of the host before and after one `hollow fetch`, and polls Hollow's
//! AppContainer state while it runs:
//!
//! - files (path, size, mtime) under %LOCALAPPDATA%, %APPDATA%, %TEMP% (recursive), the profile
//!   folder's top level, Documents/Downloads/Desktop (recursive), and the top level of %ProgramData%,
//!   %SystemRoot% and the binaries' folder;
//! - all of HKCU\Software, diffed in full for the AppContainer and Explorer subtrees and searched
//!   everywhere for Hollow's names and SIDs; plus HKLM's firewall policy (AppContainer firewall
//!   rules), which a normal user can read;
//! - the DACLs of hollow*.exe and their folder.
//!
//! The registry is read with `reg.exe export` and the ACLs with `icacls.exe /save`, both from
//! System32, instead of a registry/security crate: the workspace forbids `unsafe`, so xtask can't
//! call the Win32 APIs itself, and this adds no dependency (and no Cargo.lock change) for a
//! developer check. `reg export` writes all of HKCU\Software as UTF-16 in under a second (`reg
//! query /s` takes ~95 s and prints in the console code page), keeps binary data as hex for the
//! UTF-16 search, and `icacls /save` prints raw SIDs in SDDL, never resolved account names.

mod diff;
mod regfile;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, bail};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use self::diff::{FileMeta, FileSnapshot, Findings, Patterns, appcontainer_sid, first_component, reg_hit};
use self::regfile::{RegPath, RegSnapshot, string_value};
use crate::{no_more_args, take_flag, take_value, workspace_root};

const DEFAULT_URL: &str = "https://check.torproject.org/";
const POLL: Duration = Duration::from_millis(500);
/// Hollow's AppContainer names (crates/hollow-app/src/child.rs) and the moniker Windows stores
/// for them (the lower-cased name, also the `%LOCALAPPDATA%\Packages` folder name).
const CONTAINERS: [(&str, &str); 2] = [("Hollow.Fortress", "hollow.fortress"), ("Hollow.Net", "hollow.net")];
/// Where hollow-net points Arti (crates/hollow-net/src/tor.rs). It must never exist.
const NEVER_CREATED: &str = "hollow-ram-only-never-created";
const HKCU_SOFTWARE: &str = r"HKEY_CURRENT_USER\Software";
const APPCONTAINER_KEY: &str =
    r"HKEY_CURRENT_USER\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer";
const EXPLORER_KEY: &str = r"HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Explorer";
const FIREWALL_KEY: &str =
    r"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy";
/// Lines per review list before it is cut short.
const REVIEW_LINES: usize = 60;

pub fn run(args: &[String]) -> anyhow::Result<bool> {
    let mut args = args.to_vec();
    let url = take_value(&mut args, "--url")?.unwrap_or_else(|| DEFAULT_URL.to_string());
    let no_build = take_flag(&mut args, "--no-build");
    let locked = !take_flag(&mut args, "--unlocked");
    let bin_dir_arg = take_value(&mut args, "--bin-dir")?.map(PathBuf::from);
    let timeout = match take_value(&mut args, "--timeout")? {
        Some(s) => Duration::from_secs(s.parse().context("--timeout must be a number of seconds")?),
        None => Duration::from_secs(300),
    };
    no_more_args(&args)?;

    println!("== Trace test (P5, host edition) ==\n");
    let bin_dir = std::path::absolute(crate::dist::test_binaries(bin_dir_arg, no_build, locked)?)?;
    let hollow = bin_dir.join("hollow.exe");
    let host = Host::detect(&bin_dir)?;
    // xtask's own scratch files (registry exports, ACL dumps, hollow's working directory).
    if host.work.exists() {
        fs::remove_dir_all(&host.work).with_context(|| format!("removing {}", host.work.display()))?;
    }
    fs::create_dir_all(&host.work)?;

    let sids: Vec<(&str, String)> = CONTAINERS.iter().map(|(name, _)| (*name, appcontainer_sid(name))).collect();
    let mut patterns = Patterns::new(&sids.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>());
    println!("Binaries: {}", bin_dir.display());
    println!("Hollow's AppContainer SIDs, derived from the names like DeriveAppContainerSidFromAppContainerName:");
    for (name, sid) in &sids {
        println!("  {name:<16} {sid}");
    }

    let running = running_hollow();
    if !running.is_empty() {
        bail!(
            "Hollow is already running ({}). Close it first: both brokers would use the same AppContainer \
             profiles, and the first one to exit removes them for both.",
            running.join(", ")
        );
    }

    let before = Snapshot::take(&host, None)?;
    before.print_summary("BEFORE", &host);

    println!("\nRunning {} fetch {url}", hollow.display());
    let session = run_session(&hollow, &url, timeout, &host, &mut patterns)?;

    // Hollow cleans up before it exits; give Windows a moment to finish deleting the profiles.
    std::thread::sleep(Duration::from_secs(1));
    let after = Snapshot::take(&host, Some(&before.roots))?;
    after.print_summary("AFTER", &host);

    let ok = print_report(&host, &url, &sids, &patterns, &session, &before, &after);
    let _ = fs::remove_dir_all(&host.work);
    Ok(ok)
}

/// Running hollow*.exe processes, as `name (pid N)`.
fn running_hollow() -> Vec<String> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let mut found: Vec<String> = sys
        .processes()
        .iter()
        .filter_map(|(pid, p)| {
            let name = p.name().to_string_lossy().to_lowercase();
            (name.starts_with("hollow") && name.ends_with(".exe")).then(|| format!("{name} (pid {pid})"))
        })
        .collect();
    found.sort();
    found
}

/// The host's folders, and how to abbreviate them in the report.
struct Host {
    local_appdata: PathBuf,
    appdata: PathBuf,
    temp: PathBuf,
    profile: PathBuf,
    program_data: PathBuf,
    system_root: PathBuf,
    packages: PathBuf,
    never_created: PathBuf,
    bin_dir: PathBuf,
    work: PathBuf,
}

impl Host {
    fn detect(bin_dir: &Path) -> anyhow::Result<Self> {
        let var = |name: &str| {
            std::env::var_os(name).map(PathBuf::from).with_context(|| format!("environment variable {name} is not set"))
        };
        let local_appdata = var("LOCALAPPDATA")?;
        let system_root = var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        Ok(Self {
            packages: local_appdata.join("Packages"),
            appdata: var("APPDATA")?,
            temp: std::path::absolute(std::env::temp_dir())?.components().collect(),
            profile: var("USERPROFILE")?,
            program_data: var("ProgramData")?,
            never_created: system_root.join(NEVER_CREATED),
            system_root,
            local_appdata,
            bin_dir: bin_dir.to_path_buf(),
            work: workspace_root().join("target").join("xtask-trace"),
        })
    }

    /// Scratch folders of xtask itself, left out of the file snapshots: this check's own
    /// (registry exports, ACL dumps, hollow's working directory) and `repro-check`'s source copies,
    /// so a developer check running alongside doesn't count as a Hollow trace.
    fn excluded(&self) -> Vec<PathBuf> {
        vec![self.work.clone(), self.temp.join("hollow-repro-check")]
    }

    fn system_tool(&self, name: &str) -> PathBuf {
        self.system_root.join("System32").join(name)
    }

    /// Replaces the host folders in `text` with %VARIABLES%, longest first.
    fn abbrev(&self, text: &str) -> String {
        let mut pairs: Vec<(String, &str)> = vec![
            (self.temp.display().to_string(), "%TEMP%"),
            (self.local_appdata.display().to_string(), "%LOCALAPPDATA%"),
            (self.appdata.display().to_string(), "%APPDATA%"),
            (self.profile.display().to_string(), "%USERPROFILE%"),
            (self.program_data.display().to_string(), "%ProgramData%"),
            (self.system_root.display().to_string(), "%SystemRoot%"),
        ];
        pairs.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        let mut out = text.to_string();
        for (path, var) in pairs {
            if !path.is_empty() {
                out = out.replace(&path, var);
            }
        }
        out
    }

    /// The folders whose file trees are snapshotted. `hkcu` resolves Documents/Downloads/Desktop
    /// through Explorer's `Shell Folders` key, since they can be redirected (e.g. to OneDrive).
    fn file_roots(&self, hkcu: &RegSnapshot) -> Vec<Root> {
        let mut roots = vec![
            Root::new(&self.local_appdata, true),
            Root::new(&self.appdata, true),
            Root::new(&self.temp, true),
            Root::new(&self.profile, false),
        ];
        let shell_folders = format!(r"{EXPLORER_KEY}\Shell Folders");
        let mut known: Vec<PathBuf> =
            ["Documents", "Downloads", "Desktop"].iter().map(|d| self.profile.join(d)).collect();
        for value in ["Personal", "{374DE290-123F-4565-9164-39C4925E467B}", "Desktop"] {
            let entry = RegPath { key: shell_folders.clone(), value: Some(value.to_string()) };
            if let Some(path) = hkcu.get(&entry).and_then(|raw| string_value(raw)) {
                known.push(PathBuf::from(path));
            }
        }
        roots.extend(known.iter().map(|p| Root::new(p, true)));
        roots.push(Root::new(&self.program_data, false));
        roots.push(Root::new(&self.system_root, false));
        roots.push(Root::new(&self.bin_dir, false));

        // Drop missing folders, duplicates and folders a recursive root already covers.
        let mut out: Vec<Root> = Vec::new();
        for root in roots {
            if !root.path.is_dir() {
                continue;
            }
            let lower = root.path.to_string_lossy().to_lowercase();
            let covered = out.iter().any(|r| {
                let rl = r.path.to_string_lossy().to_lowercase();
                rl == lower || (r.recursive && lower.starts_with(&format!("{rl}\\")))
            });
            if !covered {
                out.push(root);
            }
        }
        out
    }
}

#[derive(Clone)]
struct Root {
    path: PathBuf,
    recursive: bool,
}

impl Root {
    fn new(path: &Path, recursive: bool) -> Self {
        Self { path: path.to_path_buf(), recursive }
    }
}

struct Snapshot {
    roots: Vec<Root>,
    files: FileSnapshot,
    unreadable_dirs: usize,
    hkcu: RegSnapshot,
    /// `Err` holds why HKLM's firewall policy couldn't be read.
    firewall: Result<RegSnapshot, String>,
    acls: BTreeMap<String, Vec<String>>,
    never_created: bool,
    took: Duration,
}

impl Snapshot {
    /// `roots`: reuse the BEFORE roots, so both snapshots cover the same folders.
    fn take(host: &Host, roots: Option<&[Root]>) -> anyhow::Result<Self> {
        let start = Instant::now();
        let never_created = host.never_created.exists();
        let hkcu = reg_export(host, HKCU_SOFTWARE, "hkcu.reg")?;
        let firewall = reg_export(host, FIREWALL_KEY, "firewall.reg").map_err(|e| format!("{e:#}"));
        let roots = roots.map(<[Root]>::to_vec).unwrap_or_else(|| host.file_roots(&hkcu));
        let (files, unreadable_dirs) = walk(&roots, &host.excluded());
        let mut acls = BTreeMap::new();
        for path in acl_paths(&host.bin_dir)? {
            acls.insert(path.display().to_string(), read_acl(host, &path)?);
        }
        Ok(Self { roots, files, unreadable_dirs, hkcu, firewall, acls, never_created, took: start.elapsed() })
    }

    fn print_summary(&self, label: &str, host: &Host) {
        println!("\nSnapshot {label} ({:.1}s):", self.took.as_secs_f64());
        let roots: Vec<String> = self
            .roots
            .iter()
            .map(|r| {
                format!("{}{}", host.abbrev(&r.path.display().to_string()), if r.recursive { "" } else { " (top)" })
            })
            .collect();
        println!(
            "  files:    {} entries ({} folders unreadable) under {}",
            self.files.len(),
            self.unreadable_dirs,
            roots.join(", ")
        );
        let excluded: Vec<String> = host.excluded().iter().map(|p| host.abbrev(&p.display().to_string())).collect();
        println!("            excluded (xtask's own scratch folders): {}", excluded.join(", "));
        println!("  registry: {} keys+values in HKCU\\Software", self.hkcu.len());
        match &self.firewall {
            Ok(fw) => println!("            {} keys+values in HKLM firewall policy", fw.len()),
            Err(e) => println!("            HKLM firewall policy not readable: {e}"),
        }
        println!("  ACLs:     {}", self.acls.keys().cloned().collect::<Vec<_>>().join(", "));
        if self.never_created {
            println!("  {} EXISTS", host.never_created.display());
        }
    }
}

fn walk(roots: &[Root], exclude: &[PathBuf]) -> (FileSnapshot, usize) {
    let mut snap = FileSnapshot::new();
    let mut unreadable = 0;
    for root in roots {
        let Ok(meta) = fs::symlink_metadata(&root.path) else { continue };
        snap.insert(root.path.display().to_string(), file_meta(&meta));
        let mut stack = vec![root.path.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                unreadable += 1;
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if exclude.iter().any(|e| path.starts_with(e)) {
                    continue;
                }
                // DirEntry::metadata doesn't follow links, so junctions (e.g. the "Application
                // Data" loop in %LOCALAPPDATA%) are recorded but not entered.
                let Ok(meta) = entry.metadata() else { continue };
                if root.recursive && meta.file_type().is_dir() {
                    stack.push(path.clone());
                }
                snap.insert(path.display().to_string(), file_meta(&meta));
            }
        }
    }
    (snap, unreadable)
}

fn file_meta(meta: &fs::Metadata) -> FileMeta {
    let dir = meta.file_type().is_dir();
    FileMeta {
        dir,
        size: if dir { 0 } else { meta.len() },
        mtime: meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()),
    }
}

/// `reg export <key>` into the scratch folder, parsed; the file is deleted right away.
fn reg_export(host: &Host, key: &str, file: &str) -> anyhow::Result<RegSnapshot> {
    let path = host.work.join(file);
    let _ = fs::remove_file(&path);
    let out = Command::new(host.system_tool("reg.exe"))
        .args(["export", key])
        .arg(&path)
        .arg("/y")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("running reg.exe")?;
    if !out.status.success() {
        let _ = fs::remove_file(&path);
        bail!("reg export {key}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let _ = fs::remove_file(&path);
    Ok(regfile::parse(&regfile::decode_utf16(&bytes)))
}

/// The binaries' folder and every hollow*.exe in it.
fn acl_paths(bin_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = vec![bin_dir.to_path_buf()];
    for entry in fs::read_dir(bin_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.starts_with("hollow") && name.ends_with(".exe") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

/// The DACL of `path` as SDDL ACEs (`icacls /save` writes raw SIDs, never resolved names).
fn read_acl(host: &Host, path: &Path) -> anyhow::Result<Vec<String>> {
    let file = host.work.join("acl.txt");
    let _ = fs::remove_file(&file);
    let out = Command::new(host.system_tool("icacls.exe"))
        .arg(path)
        .arg("/save")
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("running icacls.exe")?;
    if !out.status.success() {
        bail!("icacls {} /save: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = regfile::decode_utf16(&fs::read(&file)?);
    let _ = fs::remove_file(&file);
    // Line 1 is the file name, line 2 its SDDL.
    let sddl = text.lines().map(str::trim).filter(|l| !l.is_empty()).nth(1).unwrap_or("");
    Ok(diff::sddl_dacl_aces(sddl))
}

struct Session {
    exit_ok: bool,
    exit_desc: String,
    stdout: String,
    elapsed: Duration,
    polls: u32,
    /// What the polls saw of Hollow -> (first, last) seen.
    seen: BTreeMap<String, (Duration, Duration)>,
    /// Moniker -> SID, from `Mappings\<SID>` keys seen during the session.
    discovered: BTreeMap<String, String>,
    poll_errors: BTreeSet<String>,
}

fn run_session(
    hollow: &Path,
    url: &str,
    timeout: Duration,
    host: &Host,
    patterns: &mut Patterns,
) -> anyhow::Result<Session> {
    // An empty working directory, checked afterwards: relative-path writes would land here.
    let cwd = host.work.join("cwd");
    fs::create_dir_all(&cwd)?;
    let acl_paths = acl_paths(&host.bin_dir)?;
    let mut child = Command::new(hollow)
        .args(["fetch", url])
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", hollow.display()))?;
    let start = Instant::now();

    let stderr = child.stderr.take().context("no stderr pipe")?;
    let stderr_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            println!("  hollow | {line}");
        }
    });
    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let mut stdout = child.stdout.take().context("no stdout pipe")?;
    let stdout_sink = Arc::clone(&stdout_buf);
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        if let Ok(mut b) = stdout_sink.lock() {
            *b = buf;
        }
    });

    let mut seen: BTreeMap<String, (Duration, Duration)> = BTreeMap::new();
    let mut discovered = BTreeMap::new();
    let mut poll_errors = BTreeSet::new();
    let mut polls = 0;
    let mut timed_out = false;
    let status = loop {
        // Checked before the poll, so the last poll runs after hollow.exe has exited.
        let exited = child.try_wait()?;
        let poll_start = Instant::now();
        match observe(host, patterns, &acl_paths, &mut discovered) {
            Ok(items) => {
                let now = start.elapsed();
                for item in items {
                    seen.entry(item).and_modify(|e| e.1 = now).or_insert((now, now));
                }
            }
            Err(e) => {
                poll_errors.insert(format!("{e:#}"));
            }
        }
        polls += 1;
        if let Some(status) = exited {
            break status;
        }
        if start.elapsed() > timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(POLL.saturating_sub(poll_start.elapsed()));
    };
    let elapsed = start.elapsed();
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let stdout = String::from_utf8_lossy(&stdout_buf.lock().map(|b| b.clone()).unwrap_or_default()).into_owned();

    let leftovers: Vec<String> = fs::read_dir(&cwd)?.flatten().map(|e| e.path().display().to_string()).collect();
    for path in leftovers {
        seen.insert(format!("CWD-WRITE: {path}"), (elapsed, elapsed));
    }
    Ok(Session {
        exit_ok: status.success() && !timed_out,
        exit_desc: if timed_out { format!("killed after {}s timeout", timeout.as_secs()) } else { status.to_string() },
        stdout,
        elapsed,
        polls,
        seen,
        discovered,
        poll_errors,
    })
}

/// One look at Hollow's sandbox state: AppContainer registry entries, profile folders, ACEs on
/// the binaries, and the never-created folder.
fn observe(
    host: &Host,
    patterns: &mut Patterns,
    acl_paths: &[PathBuf],
    discovered: &mut BTreeMap<String, String>,
) -> anyhow::Result<Vec<String>> {
    let mut items = Vec::new();
    let ac = reg_export(host, APPCONTAINER_KEY, "during.reg")?;
    let mappings = format!(r"{APPCONTAINER_KEY}\Mappings");
    for (p, raw) in &ac {
        if p.value.as_deref() != Some("Moniker") {
            continue;
        }
        let (Some(sid), Some(moniker)) = (first_component(&p.key, &mappings), string_value(raw)) else { continue };
        let moniker = moniker.to_lowercase();
        if CONTAINERS.iter().any(|(_, m)| *m == moniker) {
            discovered.insert(moniker, sid.to_string());
            patterns.add_sid(sid);
        }
    }
    for (p, raw) in &ac {
        if reg_hit(patterns, p, raw).is_some() {
            let shown = match &p.value {
                None => format!("registry: {}", p.key),
                Some(_) => format!("registry: {} = {}", p.display(), raw),
            };
            items.push(host.abbrev(&shown.replace(APPCONTAINER_KEY, r"HKCU\...\AppContainer")));
        }
    }
    // Registering a profile also adds AppIso firewall rules (machine-wide, in HKLM).
    if let Ok(fw) = reg_export(host, FIREWALL_KEY, "during-fw.reg") {
        for (p, raw) in &fw {
            if let Some(name) = p.value.as_deref().filter(|_| reg_hit(patterns, p, raw).is_some()) {
                items.push(format!("HKLM firewall rule: {name}"));
            }
        }
    }
    if let Ok(entries) = fs::read_dir(&host.packages) {
        for entry in entries.flatten() {
            if patterns.hit(&entry.file_name().to_string_lossy()).is_some() {
                let root = [Root::new(&entry.path(), true)];
                let (files, _) = walk(&root, &host.excluded());
                for (path, meta) in files {
                    let suffix = if meta.dir { "\\" } else { "" };
                    items.push(format!("profile: {}{suffix}", host.abbrev(&path)));
                }
            }
        }
    }
    for path in acl_paths {
        for ace in read_acl(host, path)? {
            if patterns.is_hollow_sid(diff::ace_trustee(&ace)) {
                items.push(format!("ACE: {} {ace}", path.display()));
            }
        }
    }
    if host.never_created.exists() {
        items.push(format!("NEVER-CREATED FOLDER EXISTS: {}", host.never_created.display()));
    }
    Ok(items)
}

fn reg_area(key: &str) -> &'static str {
    let starts = |prefix: &str| key.get(..prefix.len()).is_some_and(|k| k.eq_ignore_ascii_case(prefix));
    if starts(APPCONTAINER_KEY) {
        "AppContainer"
    } else if starts(EXPLORER_KEY) {
        "Explorer"
    } else if starts(FIREWALL_KEY) {
        "HKLM firewall"
    } else {
        "HKCU\\Software"
    }
}

fn print_list(host: &Host, title: &str, lines: &[String], limit: usize) {
    if lines.is_empty() {
        return;
    }
    println!("{title} ({}):", lines.len());
    for line in lines.iter().take(limit) {
        println!("    {}", host.abbrev(line));
    }
    if lines.len() > limit {
        println!("    ... and {} more", lines.len() - limit);
    }
}

fn print_report(
    host: &Host,
    url: &str,
    sids: &[(&str, String)],
    patterns: &Patterns,
    s: &Session,
    before: &Snapshot,
    after: &Snapshot,
) -> bool {
    println!("\n== Trace test report ==\n");
    let mut failures: Vec<String> = Vec::new();

    println!("hollow fetch {url}: {} after {:.1}s", s.exit_desc, s.elapsed.as_secs_f64());
    if !s.exit_ok {
        failures.push(format!("hollow fetch failed: {}", s.exit_desc));
    }
    if url == DEFAULT_URL {
        if s.stdout.contains("Congratulations") {
            println!("check.torproject.org says: Congratulations (the request arrived from a Tor exit)");
        } else {
            failures.push("check.torproject.org did not say \"Congratulations\"".to_string());
        }
    }

    // During the session.
    println!("\nDuring the session ({} polls, about every {} ms):", s.polls, POLL.as_millis());
    for (name, sid) in sids {
        let moniker = name.to_lowercase();
        match s.discovered.get(&moniker) {
            Some(found) if found.eq_ignore_ascii_case(sid) => {
                println!("  {name}: Mappings\\{found} (Moniker {moniker}) = the derived SID")
            }
            Some(found) => println!(
                "  {name}: Mappings\\{found} (Moniker {moniker}) DIFFERS from the derived {sid}; both are checked"
            ),
            None => println!("  {name}: no Mappings key with Moniker {moniker} was seen"),
        }
    }
    for (item, (first, last)) in &s.seen {
        let gone = if *last >= s.elapsed { "  <- still there after hollow.exe exited" } else { "" };
        println!("  +{:>6.2}s..+{:>6.2}s  {item}{gone}", first.as_secs_f64(), last.as_secs_f64());
        if item.starts_with("NEVER-CREATED") || item.starts_with("CWD-WRITE") {
            failures.push(item.clone());
        }
    }
    for e in &s.poll_errors {
        println!("  poll error: {e}");
    }
    if s.discovered.is_empty() && !s.seen.keys().any(|i| i.starts_with("profile:")) {
        failures.push(
            "inconclusive: no Hollow AppContainer profile was seen during the session, so the \
             checks below can't show they would catch one"
                .to_string(),
        );
    }
    if before.never_created || after.never_created {
        failures.push(format!("{} exists", host.never_created.display()));
    }

    // After the session.
    let packages = host.packages.display().to_string();
    let files = diff::classify_files(&before.files, &after.files, &packages, patterns);
    let hkcu = diff::classify_registry(&before.hkcu, &after.hkcu, APPCONTAINER_KEY, &reg_area, patterns);
    let firewall = match (&before.firewall, &after.firewall) {
        (Ok(b), Ok(a)) => Some(diff::classify_registry(b, a, APPCONTAINER_KEY, &reg_area, patterns)),
        _ => None,
    };
    let acls = diff::classify_acls(&before.acls, &after.acls, patterns);
    let areas: Vec<(&str, &Findings)> = [
        ("Files", Some(&files)),
        ("Registry", Some(&hkcu)),
        ("HKLM firewall", firewall.as_ref()),
        ("ACLs", Some(&acls)),
    ]
    .into_iter()
    .filter_map(|(n, f)| f.map(|f| (n, f)))
    .collect();

    println!("\nAfter the session, compared with BEFORE:");
    let leftover = |f: &Findings| {
        f.cleaned.iter().chain(&f.preexisting).any(|e| e.contains(APPCONTAINER_KEY) || e.contains("AppIso"))
    };
    if leftover(&hkcu) || firewall.as_ref().is_some_and(leftover) {
        println!(
            "  Note: BEFORE already held Hollow AppContainer state, left by an earlier session that crashed\n\
             \x20 or was killed (Hollow's startup cleanup is meant to remove it; see the lists below)."
        );
    }
    for (name, f) in &areas {
        for t in &f.traces {
            failures.push(format!("{name}: {t}"));
        }
        // A Hollow profile from an earlier run that is still there was not cleaned up either.
        for p in &f.preexisting {
            if p.contains(&packages) || p.contains(APPCONTAINER_KEY) {
                failures.push(format!("{name}: still there from before this run: {p}"));
            }
        }
        print_list(host, &format!("  {name}: Hollow traces left by this run"), &f.traces, usize::MAX);
        if f.traces.is_empty() {
            println!("  {name}: no Hollow traces");
        }
    }

    println!("\nOther changes since BEFORE, for review (not failures):");
    for (name, f) in &areas {
        if f.review.is_empty() {
            println!("  {name}: none");
        } else {
            print_list(host, &format!("  {name}"), &f.review, REVIEW_LINES);
        }
    }
    if files.review.iter().any(|r| r.starts_with(&format!("~ {packages}\\ "))) {
        println!(
            "  Note: the Packages folder's timestamp changes whenever a profile folder is created or deleted\n\
             \x20 in it, so it still shows that some AppContainer profile existed (metadata, no content)."
        );
    }
    let prefetch = host.system_root.join("Prefetch").display().to_string();
    if files.review.iter().any(|r| r.starts_with(&format!("~ {prefetch}\\ "))) {
        println!(
            "  Note: %SystemRoot%\\Prefetch changed, so Windows wrote execution records during the run (for\n\
             \x20 hollow*.exe, and for the reg.exe/icacls.exe this check starts). Reading them needs admin rights."
        );
    }
    let mut filtered: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, f) in &areas {
        for (rule, n) in &f.filtered {
            *filtered.entry(rule).or_default() += n;
        }
    }
    if filtered.is_empty() {
        println!("  Noise filter: nothing filtered");
    } else {
        let list: Vec<String> = filtered.iter().map(|(r, n)| format!("{r}: {n}")).collect();
        println!("  Noise filter (unrelated writers, not listed): {}", list.join(", "));
    }

    for (name, f) in &areas {
        print_list(
            host,
            &format!("\n{name}: Hollow-pattern entries already there before this run, unchanged"),
            &f.preexisting,
            10,
        );
        print_list(
            host,
            &format!("\n{name}: Hollow-pattern entries there before this run and gone now"),
            &f.cleaned,
            10,
        );
    }

    println!(
        "\nLimits (printed on every run):\n\
         - Windows' own execution records are NOT checked: Prefetch (%SystemRoot%\\Prefetch), Amcache.hve,\n\
           ShimCache/AppCompatCache and BAM (HKLM\\SYSTEM), SRUM (SRUDB.dat) and the PCA launch logs\n\
           (%SystemRoot%\\appcompat\\pca) can't be read without admin rights. They do record hollow.exe.\n\
         - The NTFS journal ($UsnJrnl, $LogFile) and $MFT can keep the names of files and folders that\n\
           were created and deleted (the Packages\\hollow.* profiles); registry hive slack space and the\n\
           NTUSER.DAT/UsrClass.dat transaction logs can keep deleted keys. Neither is inspected.\n\
         - hiberfil.sys, pagefile.sys, swapfile.sys and crash dumps are not inspected.\n\
         - Only the folders listed above (and below them where recursive), HKCU\\Software and HKLM's\n\
           firewall policy are compared. A changed file is noticed by size or timestamp, not content.\n\
         - Mid-session state is polled every ~{} ms, not traced; a short-lived write elsewhere is only\n\
           seen if it is still in the AFTER snapshot.\n\
         - Other programs write while the test runs; their changes are listed for review. A Hollow\n\
           write with an unrelated-looking name inside a noise-filtered folder would not be noticed.",
        POLL.as_millis()
    );

    println!();
    if failures.is_empty() {
        println!("Failures: none");
    } else {
        println!("Failures ({}):", failures.len());
        for f in &failures {
            println!("  - {}", host.abbrev(f));
        }
    }
    let ok = failures.is_empty();
    println!("\nRESULT: {}", if ok { "PASS" } else { "FAIL" });
    ok
}
