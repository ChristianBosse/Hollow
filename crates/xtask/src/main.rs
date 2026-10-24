//! Hollow developer checks, run as `cargo xtask <command>` (alias in .cargo/config.toml).
//!
//! - `dist`: the reproducible release build (TRUST-02) into `target/dist`, with SHA-256 hashes.
//! - `repro-check`: two `dist` builds into different target directories; fails unless the
//!   binaries are bit-for-bit identical.
//! - `leak-test`: runs `hollow fetch` and watches the host's TCP/UDP tables; fails if any Hollow
//!   process talks to anything but Tor relays (property P1, host edition of TRUST-06).
//! - `trace-test`: snapshots files, registry and ACLs around one `hollow fetch`; fails if the
//!   session left a Hollow trace on the host (property P5, host edition).

mod consensus;
mod dist;
mod leak;
mod trace;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <command> [options]

commands:
    dist                       Reproducible release build into target/dist, prints SHA-256
    repro-check [--keep]       Build dist twice (target/xtask-a, target/xtask-b), compare hashes.
                               Cleans both dirs first unless --keep is given.
    leak-test [options]        Run `hollow fetch` and fail on any non-Tor socket
        --url <url>            Page to fetch (default https://check.torproject.org/)
        --no-build             Reuse target/dist/release instead of running `dist` first
        --bin-dir <dir>        Test the binaries in <dir> instead (implies --no-build)
        --timeout <secs>       Kill hollow after this long (default 300)
    trace-test [options]       Run `hollow fetch` between two host snapshots (files, HKCU registry,
                               ACLs) and fail on anything Hollow left behind
        (same options as leak-test)

`dist`, `repro-check`, `leak-test` and `trace-test` pass --locked to cargo; add --unlocked to allow
Cargo.lock updates.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        None => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match cmd {
        "dist" => dist::dist_command(rest),
        "repro-check" => dist::repro_check(rest),
        "leak-test" => leak::run(rest),
        "trace-test" => trace::run(rest),
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(true)
        }
        other => {
            eprintln!("unknown command `{other}`\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("xtask {cmd}: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The workspace root (this crate lives in `<root>/crates/xtask`).
pub fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().and_then(|p| p.parent()).map(PathBuf::from).unwrap_or(manifest)
}

/// Pulls `--name` (a flag without value) out of `args`.
pub fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != name);
    args.len() != before
}

/// Pulls `--name <value>` out of `args`.
pub fn take_value(args: &mut Vec<String>, name: &str) -> anyhow::Result<Option<String>> {
    let Some(i) = args.iter().position(|a| a == name) else { return Ok(None) };
    if i + 1 >= args.len() {
        anyhow::bail!("{name} needs a value");
    }
    let value = args.remove(i + 1);
    args.remove(i);
    Ok(Some(value))
}

/// Fails on arguments nobody consumed.
pub fn no_more_args(args: &[String]) -> anyhow::Result<()> {
    match args.first() {
        Some(a) => anyhow::bail!("unexpected argument `{a}`\n\n{USAGE}"),
        None => Ok(()),
    }
}
