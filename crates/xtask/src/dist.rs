//! `cargo xtask dist` and `cargo xtask repro-check` (TRUST-02): the reproducible release build.
//!
//! The build is the normal `cargo build --release` of the three Hollow binaries plus a few
//! determinism levers (see `determinism_rustflags` and `build`). The hardening flags
//! (`+crt-static`, `-C control-flow-guard`, `/CETCOMPAT`) stay in `.cargo/config.toml` only: xtask
//! passes its extra flags with `--config target.<triple>.rustflags=[...]`, which cargo *appends*
//! to the config-file list. (Setting `RUSTFLAGS` instead would *replace* that list, so xtask
//! removes `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` from the build's environment.)

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

use crate::{no_more_args, take_flag};

const TRIPLE: &str = "x86_64-pc-windows-msvc";
/// The packages that make up a Hollow release, and the binaries they produce.
const PACKAGES: [&str; 3] = ["hollow-app", "hollow-net", "hollow-fortress"];
pub const BINARIES: [&str; 3] = ["hollow.exe", "hollow-net.exe", "hollow-fortress.exe"];

pub struct Build {
    pub release_dir: PathBuf,
    pub hashes: Vec<(&'static str, String)>,
}

pub fn dist_dir(root: &Path) -> PathBuf {
    root.join("target").join("dist")
}

/// The directory with the binaries a host check (`leak-test`, `trace-test`) runs: `bin_dir` if
/// given, else `target/dist/release`, built first unless `no_build`. Fails if a binary is missing.
pub fn test_binaries(bin_dir: Option<PathBuf>, no_build: bool, locked: bool) -> anyhow::Result<PathBuf> {
    let root = crate::workspace_root();
    let bin_dir = if let Some(dir) = bin_dir {
        dir
    } else if no_build {
        dist_dir(&root).join("release")
    } else {
        println!("Building the dist binaries (cargo xtask dist)...");
        build(&root, &dist_dir(&root), locked)?.release_dir
    };
    for name in BINARIES {
        if !bin_dir.join(name).is_file() {
            bail!("{} not found (run `cargo xtask dist` or drop --no-build)", bin_dir.join(name).display());
        }
    }
    Ok(bin_dir)
}

pub fn dist_command(args: &[String]) -> anyhow::Result<bool> {
    let mut args = args.to_vec();
    let locked = !take_flag(&mut args, "--unlocked");
    no_more_args(&args)?;
    let root = crate::workspace_root();
    let build = build(&root, &dist_dir(&root), locked)?;
    println!("\nHollow dist build: {}", build.release_dir.display());
    for (name, hash) in &build.hashes {
        println!("  {hash}  {name}");
    }
    println!("(also written to {})", build.release_dir.join("SHA256SUMS").display());
    Ok(true)
}

pub fn repro_check(args: &[String]) -> anyhow::Result<bool> {
    let mut args = args.to_vec();
    let keep = take_flag(&mut args, "--keep");
    let locked = !take_flag(&mut args, "--unlocked");
    no_more_args(&args)?;
    let root = crate::workspace_root();
    let dirs = [root.join("target").join("xtask-a"), root.join("target").join("xtask-b")];

    // Both builds use one frozen copy of the sources, so edits made in the workspace while the
    // check runs (~5 min) can't cause a difference. The copy is made twice, to two different
    // absolute paths, which also checks that the checkout path doesn't reach the binaries. The
    // snapshots live outside the workspace so cargo doesn't also read the workspace's
    // .cargo/config.toml as a parent config.
    let snap_base = std::env::temp_dir().join("hollow-repro-check");
    let snaps = [snap_base.join("src-a"), snap_base.join("src-b")];
    if snap_base.exists() {
        fs::remove_dir_all(&snap_base).with_context(|| format!("removing {}", snap_base.display()))?;
    }
    println!("Snapshotting the workspace sources (everything but target/) to {}", snaps[0].display());
    copy_tree(&root, &snaps[0], true)?;
    copy_tree(&snaps[0], &snaps[1], true)?;
    println!("Source snapshot SHA-256: {}", source_fingerprint(&snaps[0])?);

    let mut builds = Vec::new();
    for (i, (dir, snap)) in dirs.iter().zip(&snaps).enumerate() {
        if !keep && dir.exists() {
            println!("Removing {} for a from-scratch build (--keep to reuse it)", dir.display());
            fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
        }
        println!("\n== Build {} of 2: {} -> {} ==", i + 1, snap.display(), dir.display());
        builds.push(build(snap, dir, locked)?);
    }
    let _ = fs::remove_dir_all(&snap_base);

    println!("\n== Reproducibility check (TRUST-02) ==");
    println!("A = {}\nB = {}\n", builds[0].release_dir.display(), builds[1].release_dir.display());
    println!("{:<20} {:<6} SHA-256", "binary", "result");
    let mut all_same = true;
    for ((name, a), (_, b)) in builds[0].hashes.iter().zip(&builds[1].hashes) {
        let same = a == b;
        all_same &= same;
        println!("{name:<20} {:<6} A {a}", if same { "same" } else { "DIFF" });
        println!("{:<20} {:<6} B {b}", "", "");
    }
    println!();
    if all_same {
        println!(
            "RESULT: PASS - both builds are bit-for-bit identical.\n\n\
             This is same-machine reproducibility: same toolchain, Build Tools, Windows SDK and\n\
             user profile; different source paths and target directories. It is necessary but\n\
             not sufficient: another machine, user name and checkout still have to produce the\n\
             same hashes (cross-machine check: TODO)."
        );
    } else {
        println!(
            "RESULT: FAIL - the builds differ. Compare the files byte by byte, e.g. with\n\
             `fc /b` or dumpbin /headers /rawdata, and look for embedded paths or timestamps."
        );
    }
    Ok(all_same)
}

/// Copies `from` to `to`, skipping `target/` and `.git/` at the top level when `top` is set.
fn copy_tree(from: &Path, to: &Path, top: bool) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if ty.is_dir() {
            if top && matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                continue;
            }
            copy_tree(&entry.path(), &dest, false)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), &dest).with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// The determinism flags xtask adds on top of `.cargo/config.toml`. Each one is needed; see the
/// "Developer checks" section of README.md for what breaks without it.
fn determinism_rustflags(root: &Path, cargo_home: &Path) -> Vec<String> {
    vec![
        // Panic locations of registry crates are absolute paths like
        // C:\Users\<name>\.cargo\registry\src\index.crates.io-...\anyhow-1.0.104\src\fmt.rs
        // (over a thousand in hollow-net.exe). Same machine, same bytes; but another builder
        // gets other bytes, and the binary would carry the builder's user name.
        format!("--remap-path-prefix={}=/cargo", cargo_home.display()),
        // Workspace crates are passed to rustc with relative paths already; this covers
        // anything that still sees the absolute checkout path (e.g. path dependencies).
        // No remap for the target dir: nothing from OUT_DIR ends up in the binaries
        // (`check_no_build_paths` verifies that), and a per-build flag would differ between
        // the two repro builds by construction.
        format!("--remap-path-prefix={}=/hollow", root.display()),
        // MSVC link.exe writes the link time into the PE header and into every debug directory
        // entry, and a random GUID into the CodeView (PDB) record. /Brepro replaces both with
        // values derived from a hash of the output.
        "-Clink-arg=/Brepro".to_string(),
    ]
}

/// Fails if a binary contains an absolute path of this build machine (checkout, cargo home,
/// user profile, target dir), as UTF-8 or UTF-16: it would differ on every other machine.
fn check_no_build_paths(release_dir: &Path, needles: &[PathBuf]) -> anyhow::Result<()> {
    let mut found = Vec::new();
    for name in BINARIES {
        let data = fs::read(release_dir.join(name))?.to_ascii_lowercase();
        for needle in needles {
            let text = needle.to_string_lossy().to_ascii_lowercase();
            let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
            for (enc, pat) in [("UTF-8", text.as_bytes()), ("UTF-16", &utf16[..])] {
                let hits = data.windows(pat.len()).filter(|w| *w == pat).count();
                if hits > 0 {
                    found.push(format!("{name}: {hits}x {} ({enc})", needle.display()));
                }
            }
        }
    }
    if !found.is_empty() {
        bail!("build-machine paths are embedded in the binaries:\n  {}", found.join("\n  "));
    }
    Ok(())
}

pub fn build(root: &Path, target_dir: &Path, locked: bool) -> anyhow::Result<Build> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let cargo_home = cargo_home()?;
    let flags = determinism_rustflags(root, &cargo_home);
    // TOML literal strings ('...') keep Windows backslashes as they are.
    let list = flags.iter().map(|f| format!("'{f}'")).collect::<Vec<_>>().join(", ");
    let config = format!("target.{TRIPLE}.rustflags=[{list}]");

    let mut cmd = Command::new(&cargo);
    cmd.current_dir(root).args(["build", "--release"]);
    if locked {
        cmd.arg("--locked");
    }
    for p in PACKAGES {
        cmd.args(["--package", p]);
    }
    cmd.arg("--config").arg(&config).arg("--target-dir").arg(target_dir);
    // RUSTFLAGS would replace the hardening flags from .cargo/config.toml (see module docs).
    for var in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "CARGO_TARGET_DIR"] {
        if std::env::var_os(var).is_some() {
            eprintln!("note: ignoring {var} from the environment for the dist build");
        }
        cmd.env_remove(var);
    }
    cmd.env("CARGO_INCREMENTAL", "0");

    println!(
        "cargo build --release{} -p {} --config \"{config}\"",
        if locked { " --locked" } else { "" },
        PACKAGES.join(" -p ")
    );
    let status = cmd.status().context("running cargo build")?;
    if !status.success() {
        bail!("cargo build failed ({status})");
    }

    let release_dir = target_dir.join("release");
    let mut hashes = Vec::new();
    let mut sums = String::new();
    for name in BINARIES {
        let hash = sha256_file(&release_dir.join(name))?;
        sums.push_str(&format!("{hash}  {name}\n"));
        hashes.push((name, hash));
    }
    fs::write(release_dir.join("SHA256SUMS"), sums)?;

    let mut needles = vec![crate::workspace_root(), root.to_path_buf(), cargo_home, target_dir.to_path_buf()];
    needles.dedup();
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        needles.push(PathBuf::from(profile));
    }
    check_no_build_paths(&release_dir, &needles)?;
    println!("No build-machine paths in the binaries (checked: {needles:?})");
    Ok(Build { release_dir, hashes })
}

fn cargo_home() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return Ok(PathBuf::from(home));
    }
    let profile = std::env::var_os("USERPROFILE").context("neither CARGO_HOME nor USERPROFILE is set")?;
    Ok(PathBuf::from(profile).join(".cargo"))
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A hash over every file of a source tree except `target/` and `.git/`.
fn source_fingerprint(root: &Path) -> anyhow::Result<String> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for rel in files {
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0]);
        // A file can vanish mid-walk while others edit; that's a change too.
        hasher.update(fs::read(root.join(&rel)).unwrap_or_default());
    }
    Ok(hex(&hasher.finalize()))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            if dir == root && matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                continue;
            }
            collect_files(root, &path, out)?;
        } else if ty.is_file() {
            out.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}
