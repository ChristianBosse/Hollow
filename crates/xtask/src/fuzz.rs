//! `cargo xtask fuzz`: the stable-toolchain fuzzer (SEC-07).
//!
//! It drives the same targets as the libFuzzer project in `fuzz/`
//! (`crates/hollow-fuzz-targets`), without coverage feedback: inputs are the
//! seed corpus (`fuzz/seeds/<target>`, `fuzz/regressions/<target>`) and
//! seeded mutations of it (see `fuzz/mutate.rs`). Every input runs on a
//! worker thread with the renderer's stack size, under `catch_unwind` and a
//! per-input time budget, while the process's memory is watched.
//!
//! The work happens in a child process (`xtask fuzz --child ...`) that tells
//! this one the number of each input before running it. A panic is caught
//! and minimized in the child; a hang or a memory blow-up makes the child
//! save the input and exit; anything that kills the child outright (stack
//! overflow, abort) is caught here: the input is rebuilt from its number,
//! saved, and a new child carries on after it. Findings go to
//! `target/fuzz-crashes/<target>/`, and the run fails if there are any.
//!
//! Fuzzing a debug build is slow, so `cargo xtask fuzz` first rebuilds
//! xtask with the `fuzz` profile (optimized, with debug assertions and
//! overflow checks) and runs that, unless `--debug` is given.

mod mutate;

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use hollow_fuzz_targets::{Kind, TARGETS, Target};
use sha2::{Digest, Sha256};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use self::mutate::{Material, parse_dict};
use crate::{no_more_args, take_flag, take_value, workspace_root};

/// Set in the optimized re-run, so it doesn't rebuild itself again.
const OPTIMIZED_ENV: &str = "HOLLOW_XTASK_FUZZ_OPTIMIZED";
/// Crash locations already reported this run, one per line, for new children.
const KNOWN_ENV: &str = "HOLLOW_XTASK_FUZZ_KNOWN";
/// The renderer runs the engine on its main thread: 1 MiB on Windows.
const WORKER_STACK: usize = 1 << 20;
/// Child exit codes: it saved an input that hung, or used too much memory.
const EXIT_TIMEOUT: i32 = 10;
const EXIT_MEMORY: i32 = 11;
/// How long a new panic may be minimized for.
const MINIMIZE_FOR: Duration = Duration::from_secs(20);

struct Options {
    targets: Vec<&'static Target>,
    time: Duration,
    seed: u64,
    timeout: Duration,
    rss_mb: u64,
    max_len: usize,
}

pub fn run(args: &[String]) -> anyhow::Result<bool> {
    let mut args = args.to_vec();
    if take_flag(&mut args, "--child") {
        return child(args);
    }
    let debug = take_flag(&mut args, "--debug");
    if !debug && std::env::var_os(OPTIMIZED_ENV).is_none() {
        return rerun_optimized(&args);
    }
    let mut files = Vec::new();
    while let Some(f) = take_value(&mut args, "--run")? {
        files.push(PathBuf::from(f));
    }
    let opts = options(&mut args)?;
    no_more_args(&args)?;
    if !files.is_empty() {
        return reproduce(&files, &opts);
    }

    println!(
        "fuzzing with seed {} ({}s per target, {}s per input)",
        opts.seed,
        opts.time.as_secs(),
        opts.timeout.as_secs()
    );
    let mut failed = Vec::new();
    for target in &opts.targets {
        let (execs, findings) = fuzz_target(target, &opts)?;
        println!("[{}] {execs} inputs, {findings} findings", target.name);
        if findings > 0 {
            failed.push(target.name);
        }
    }
    if failed.is_empty() {
        println!("no findings");
        Ok(true)
    } else {
        println!("FINDINGS in {} (see target/fuzz-crashes/<target>/; reproduce with --run <file>)", failed.join(", "));
        Ok(false)
    }
}

fn options(args: &mut Vec<String>) -> anyhow::Result<Options> {
    let targets = match take_value(args, "--target")? {
        Some(names) => names
            .split(',')
            .map(|n| {
                hollow_fuzz_targets::find(n.trim())
                    .with_context(|| format!("no fuzz target `{n}`; targets: {}", names_list()))
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        None => TARGETS.iter().collect(),
    };
    let number = |args: &mut Vec<String>, name: &str, default: u64| -> anyhow::Result<u64> {
        match take_value(args, name)? {
            Some(v) => v.parse().with_context(|| format!("{name} must be a number")),
            None => Ok(default),
        }
    };
    let clock = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    Ok(Options {
        targets,
        time: Duration::from_secs(number(args, "--time", 60)?),
        seed: number(args, "--seed", clock)?,
        timeout: Duration::from_secs(number(args, "--timeout", 10)?.max(1)),
        rss_mb: number(args, "--rss-mb", 2048)?,
        max_len: number(args, "--max-len", 128 * 1024)? as usize,
    })
}

fn names_list() -> String {
    TARGETS.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
}

/// Runs this command again from an xtask built with the `fuzz` profile.
fn rerun_optimized(args: &[String]) -> anyhow::Result<bool> {
    println!("building xtask with the `fuzz` profile (optimized + debug assertions; --debug skips this)");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args(["run", "--profile", "fuzz", "--package", "xtask", "--", "fuzz"])
        .args(args)
        .env(OPTIMIZED_ENV, "1")
        .status()
        .context("running cargo")?;
    Ok(status.success())
}

fn crash_dir(target: &Target) -> PathBuf {
    workspace_root().join("target").join("fuzz-crashes").join(target.name)
}

/// The seed corpus and dictionaries of a target.
fn material(target: &Target, max_len: usize) -> anyhow::Result<Material> {
    let fuzz = workspace_root().join("fuzz");
    let mut files = Vec::new();
    for dir in [fuzz.join("seeds").join(target.name), fuzz.join("regressions").join(target.name)] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_file()) {
                files.push(e.path());
            }
        }
    }
    files.sort();
    let corpus = files.iter().map(std::fs::read).collect::<Result<Vec<_>, _>>().context("reading the seed corpus")?;
    let mut dicts: Vec<&str> = target.dict.into_iter().collect();
    // Pages carry CSS in style attributes and <style>.
    if target.kind == Kind::Html {
        dicts.push("css");
    }
    let mut dict = Vec::new();
    for name in dicts {
        let path = fuzz.join("dict").join(format!("{name}.dict"));
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        dict.extend(parse_dict(&text));
    }
    Ok(Material { kind: target.kind, corpus, dict, max_len })
}

/// Fuzzes one target for `opts.time`, restarting the child after each
/// fatal finding. Returns (inputs run, findings).
fn fuzz_target(target: &'static Target, opts: &Options) -> anyhow::Result<(u64, usize)> {
    let material = material(target, opts.max_len)?;
    println!(
        "[{}] {} seeds, {} dictionary tokens: {}",
        target.name,
        material.corpus.len(),
        material.dict.len(),
        target.about
    );
    let exe = std::env::current_exe().context("finding xtask's own path")?;
    let deadline = Instant::now() + opts.time;
    let mut known: BTreeSet<String> = BTreeSet::new();
    let (mut next, mut execs, mut findings) = (0u64, 0u64, 0usize);
    while let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        let mut child = Command::new(&exe)
            .args(["fuzz", "--child", target.name])
            .args(["--seed", &opts.seed.to_string(), "--from", &next.to_string()])
            .args(["--ms", &left.as_millis().to_string(), "--timeout-ms", &opts.timeout.as_millis().to_string()])
            .args(["--rss-mb", &opts.rss_mb.to_string(), "--max-len", &opts.max_len.to_string()])
            .env(OPTIMIZED_ENV, "1")
            .env(KNOWN_ENV, known.iter().cloned().collect::<Vec<_>>().join("\n"))
            .stdout(Stdio::piped())
            .spawn()
            .context("starting a fuzzing child process")?;
        let stdout = child.stdout.take().context("child stdout")?;
        let mut last = None;
        for line in BufReader::new(stdout).lines() {
            let line = line.context("reading the child's output")?;
            if let Some(i) = line.strip_prefix("@ ") {
                last = i.parse::<u64>().ok();
            } else if let Some(n) = line.strip_prefix("= ") {
                execs += n.parse::<u64>().unwrap_or(0);
            } else if let Some(found) = line.strip_prefix("! ") {
                let (key, path) = found.split_once('\t').unwrap_or((found, ""));
                known.insert(key.to_owned());
                findings += 1;
                println!("[{}] saved {path}", target.name);
            } else {
                println!("{line}");
            }
        }
        let status = child.wait().context("waiting for the child")?;
        match status.code() {
            Some(0) => break,
            Some(EXIT_TIMEOUT | EXIT_MEMORY) => {}
            _ => {
                let Some(i) = last else { bail!("fuzzing child exited with {status} before running any input") };
                // Killed outright (stack overflow, abort): rebuild its input.
                let input = material.input(opts.seed, i);
                let info = format!("the process died ({status}) on input {i} of seed {}", opts.seed);
                let path = save(&crash_dir(target), "abort", &input, target, &info)?;
                println!("[{}] input {i} killed the process ({status}); saved {}", target.name, path.display());
                findings += 1;
                execs += 1;
            }
        }
        next = last.map_or(next, |i| i + 1);
    }
    Ok((execs, findings))
}

/// Saves a finding as `<kind>-<hash>` with a `.txt` note next to it.
fn save(dir: &Path, kind: &str, input: &[u8], target: &Target, info: &str) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let hash: String = Sha256::digest(input).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("{kind}-{hash}"));
    std::fs::write(&path, input).with_context(|| format!("writing {}", path.display()))?;
    let note = format!(
        "target: {}\n{info}\n\nreproduce:\n  cargo xtask fuzz --run {}\n  cargo +nightly fuzz run {} {}\n",
        target.name,
        path.display(),
        target.name,
        path.display()
    );
    std::fs::write(dir.join(format!("{kind}-{hash}.txt")), note)?;
    Ok(path)
}

/// `--run`: each file once through its target (`--target`, or the name of
/// the folder it is in), with the normal panic message and backtrace.
fn reproduce(files: &[PathBuf], opts: &Options) -> anyhow::Result<bool> {
    let mut ok = true;
    for file in files {
        let target = match &opts.targets[..] {
            [one] => *one,
            _ => {
                let dir = file.parent().and_then(|d| d.file_name()).and_then(|d| d.to_str()).unwrap_or("");
                hollow_fuzz_targets::find(dir).with_context(|| {
                    format!("which target is {} for? pass --target ({})", file.display(), names_list())
                })?
            }
        };
        let input = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        let started = Instant::now();
        let result = catch_unwind(AssertUnwindSafe(|| (target.run)(&input)));
        let took = started.elapsed();
        let slow = if took > opts.timeout { " (over the time budget)" } else { "" };
        match result {
            Ok(()) => println!("ok     [{}] {} in {took:.2?}{slow}", target.name, file.display()),
            Err(_) => println!("PANIC  [{}] {} in {took:.2?}", target.name, file.display()),
        }
        ok &= result.is_ok() && slow.is_empty();
    }
    Ok(ok)
}

/// What the panic hook saw last: (location, message).
static LAST_PANIC: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Runs one input; a panic comes back as (location, message).
fn run_one(target: &Target, input: &[u8]) -> Result<(), (String, String)> {
    LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner()).take();
    match catch_unwind(AssertUnwindSafe(|| (target.run)(input))) {
        Ok(()) => Ok(()),
        Err(_) => Err(LAST_PANIC
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_else(|| ("unknown location".into(), "unknown panic".into()))),
    }
}

/// Where a panic is filed, for telling bugs apart: its location if that is
/// in the workspace, else (a panic inside std or a dependency, such as a
/// `clamp` with min > max) the first workspace frame that led to it.
fn panic_key(location: &str) -> String {
    let in_workspace = |s: &str| s.trim_start_matches(".\\").trim_start_matches("./").starts_with("crates");
    if in_workspace(location) {
        return location.to_owned();
    }
    let trace = std::backtrace::Backtrace::force_capture().to_string();
    trace
        .lines()
        .filter_map(|l| l.trim().strip_prefix("at "))
        .find(|at| in_workspace(at))
        .map_or_else(|| location.to_owned(), |at| format!("{at} (in {location})"))
}

/// The input being run: its number, since when, and the bytes.
type Current = Option<(u64, Instant, Arc<Vec<u8>>)>;

/// `--child <target>`: the fuzzing loop, from input `--from` until `--ms`
/// have passed.
fn child(mut args: Vec<String>) -> anyhow::Result<bool> {
    let name = if args.is_empty() { bail!("--child needs a target") } else { args.remove(0) };
    let target = hollow_fuzz_targets::find(&name).with_context(|| format!("no fuzz target `{name}`"))?;
    let mut number = |flag: &str| -> anyhow::Result<u64> {
        take_value(&mut args, flag)?.with_context(|| format!("{flag} missing"))?.parse().context("bad number")
    };
    let (seed, from, ms, timeout_ms) = (number("--seed")?, number("--from")?, number("--ms")?, number("--timeout-ms")?);
    let (rss_mb, max_len) = (number("--rss-mb")?, number("--max-len")? as usize);
    no_more_args(&args)?;
    let material = Arc::new(material(target, max_len)?);
    let known: BTreeSet<String> =
        std::env::var(KNOWN_ENV).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
    let dir = crash_dir(target);

    std::panic::set_hook(Box::new(|info| {
        let location = panic_key(&info.location().map_or_else(|| "unknown location".into(), |l| l.to_string()));
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        *LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner()) = Some((location, message));
    }));

    let current: Arc<Mutex<Current>> = Arc::new(Mutex::new(None));
    let execs = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (current, execs, stop, material, dir) =
            (current.clone(), execs.clone(), stop.clone(), material.clone(), dir.clone());
        std::thread::Builder::new().stack_size(WORKER_STACK).spawn(move || -> anyhow::Result<()> {
            let mut known = known;
            let set = |c: Current| *current.lock().unwrap_or_else(|e| e.into_inner()) = c;
            for i in from.. {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let input = Arc::new(material.input(seed, i));
                println!("@ {i}");
                set(Some((i, Instant::now(), input.clone())));
                let result = run_one(target, &input);
                set(None);
                execs.fetch_add(1, Ordering::Relaxed);
                let Err((location, message)) = result else { continue };
                if !known.insert(location.clone()) {
                    continue;
                }
                eprintln!("[{}] panic at {location}: {message}", target.name);
                let small = minimize(target, &input, &location, |c| set(c.map(|c| (i, Instant::now(), c))));
                let info = format!(
                    "panicked at {location}:\n{message}\n\ninput {i} of seed {seed}, minimized from {} to {} bytes",
                    input.len(),
                    small.len()
                );
                let path = save(&dir, "crash", &small, target, &info)?;
                println!("! {location}\t{}", path.display());
            }
            Ok(())
        })?
    };

    // This thread is the watchdog.
    let started = Instant::now();
    let deadline = started + Duration::from_millis(ms);
    let timeout = Duration::from_millis(timeout_ms);
    let pid = Pid::from_u32(std::process::id());
    let mut sys = System::new();
    let (mut last_report, mut last_memory) = (started, started);
    loop {
        std::thread::sleep(Duration::from_millis(25));
        let now = Instant::now();
        if worker.is_finished() || now >= deadline {
            stop.store(true, Ordering::Relaxed);
        }
        let running = current.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((i, since, input)) = &running
            && now.duration_since(*since) > timeout
        {
            let info = format!("input {i} of seed {seed} ran for over {timeout:?}");
            let path = save(&dir, "timeout", input, target, &info)?;
            eprintln!("[{}] input {i} ran for over {timeout:?}", target.name);
            println!("! timeout:{i}\t{}", path.display());
            println!("= {}", execs.load(Ordering::Relaxed));
            std::io::stdout().flush()?;
            std::process::exit(EXIT_TIMEOUT);
        }
        if now.duration_since(last_memory) >= Duration::from_millis(250) {
            last_memory = now;
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing().with_memory(),
            );
            let rss = sys.process(pid).map_or(0, |p| p.memory());
            // Blamed on the input being run; between inputs there is none.
            if rss > rss_mb << 20
                && let Some((i, _, input)) = running
            {
                let info = format!("input {i} of seed {seed} took the process to {} MiB (limit {rss_mb})", rss >> 20);
                let path = save(&dir, "memory", &input, target, &info)?;
                eprintln!("[{}] input {i} used {} MiB", target.name, rss >> 20);
                println!("! memory:{i}\t{}", path.display());
                println!("= {}", execs.load(Ordering::Relaxed));
                std::io::stdout().flush()?;
                std::process::exit(EXIT_MEMORY);
            }
        }
        if now.duration_since(last_report) >= Duration::from_secs(10) {
            last_report = now;
            let n = execs.load(Ordering::Relaxed);
            let secs = now.duration_since(started).as_secs_f64();
            println!("[{}] {secs:.0}s: {n} inputs ({:.0}/s)", target.name, n as f64 / secs);
        }
        // Once stopped, the worker ends after its current input, which the
        // checks above still watch.
        if worker.is_finished() {
            break;
        }
    }
    let result = worker.join().map_err(|_| anyhow::anyhow!("fuzzing worker panicked"))?;
    println!("= {}", execs.load(Ordering::Relaxed));
    result.map(|()| true)
}

/// Shrinks a panicking input while it still panics at `location`: drops
/// ever smaller chunks (delta debugging), for at most [`MINIMIZE_FOR`].
fn minimize(target: &Target, input: &[u8], location: &str, mut running: impl FnMut(Option<Arc<Vec<u8>>>)) -> Vec<u8> {
    let started = Instant::now();
    let mut best = input.to_vec();
    let mut chunk = best.len() / 2;
    while chunk > 0 && started.elapsed() < MINIMIZE_FOR {
        let mut at = 0;
        while at < best.len() && started.elapsed() < MINIMIZE_FOR {
            let mut candidate = best[..at].to_vec();
            candidate.extend_from_slice(&best[(at + chunk).min(best.len())..]);
            let candidate = Arc::new(candidate);
            running(Some(candidate.clone()));
            let same = matches!(run_one(target, &candidate), Err((l, _)) if l == location);
            running(None);
            if same {
                best = candidate.to_vec();
            } else {
                at += chunk;
            }
        }
        chunk /= 2;
    }
    best
}
