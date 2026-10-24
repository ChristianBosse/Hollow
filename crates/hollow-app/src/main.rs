//! hollow: the broker (architecture §2.1).
//!
//! It owns policy and the window, starts each child in its sandbox,
//! validates everything the children send back, and is the only place where
//! pieces are put together.

mod child;
mod fetch;
mod gui;
mod net;
mod policy;
mod selftest;
mod terminal;

use std::process::ExitCode;

const USAGE: &str = "\
Hollow (preview)

USAGE:
    hollow                Open the browser window
    hollow open <url>     Open the browser window at <url>
    hollow fetch <url>    Fetch a page over Tor and print it as Fortress extracts it
    hollow selftest       Try to break out of the renderer sandbox; fails if anything gets through
    hollow --version";

/// Locks allocations into RAM and zeroes them on free (WIN-02).
#[global_allocator]
static ALLOC: hollow_sandbox::LockedAlloc = hollow_sandbox::LockedAlloc;

fn main() -> ExitCode {
    hollow_sandbox::harden_crash_handling();
    if let Err(e) = hollow_sandbox::reserve_lockable_memory(256 << 20) {
        eprintln!("hollow: warning: memory may be paged to disk: {e}");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!("hollow {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    // Exit cleanup removes the sandbox profiles every child needs, so two
    // brokers must never run at once.
    let _instance = match hollow_sandbox::single_instance() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("hollow: Hollow is already running");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("hollow: {e}");
            return ExitCode::FAILURE;
        }
    };

    child::clean_up_host_traces();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => gui::run(None),
        ["open", url] => policy::parse_input(url).map_err(Into::into).and_then(|u| gui::run(Some(u))),
        ["fetch", url] => fetch::run(url),
        ["selftest"] => selftest::run(),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(64);
        }
    };
    child::clean_up_host_traces();
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("hollow: {e:#}");
            ExitCode::FAILURE
        }
    }
}
