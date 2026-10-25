//! hollow-net: Hollow's network process (architecture §2.2).
//!
//! The only Hollow process with the `internetClient` capability. It takes
//! already-checked requests from the broker over stdin/stdout and sends every
//! byte through Tor. Because stdout is the IPC channel, all logging goes to
//! stderr.

mod fetch;
mod pq;
mod profile;
mod tls;
mod tor;

// CT parsing lives in the library target (src/lib.rs) so the fuzz targets can
// reach it; tls.rs keeps using it as `crate::ct`.
use hollow_net::ct;

use std::io;
use std::process::ExitCode;
use std::sync::Arc;

use hollow_ipc::{BrokerToNet, NetToBroker, read_frame, write_frame};
use tracing_subscriber::EnvFilter;

/// Locks allocations into RAM and zeroes them on free (WIN-02).
#[global_allocator]
static ALLOC: hollow_sandbox::LockedAlloc = hollow_sandbox::LockedAlloc;

fn main() -> ExitCode {
    if let Err(e) = hollow_sandbox::enter_sandbox() {
        eprintln!("hollow-net: {e}");
        return ExitCode::from(78);
    }
    if let Err(e) = hollow_sandbox::reserve_lockable_memory(768 << 20) {
        eprintln!("hollow-net: warning: memory may be paged to disk: {e}");
    }
    let log_filter = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hollow-net: {e}");
            return ExitCode::from(64);
        }
    };
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::try_new(&log_filter).unwrap_or_else(|_| EnvFilter::new("warn")))
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("hollow-net: starting async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run())
}

/// `[--log <tracing filter>]`
fn parse_args() -> Result<String, String> {
    const USAGE: &str = "usage: hollow-net [--log <filter>] (started by hollow.exe)";
    let mut args = std::env::args_os().skip(1);
    let mut log = String::from("warn");
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        match flag.to_str() {
            Some("--log") => log = value.to_string_lossy().into_owned(),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(log)
}

async fn run() -> ExitCode {
    let (out_tx, out_rx) = std::sync::mpsc::channel::<NetToBroker>();
    let writer = std::thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        for msg in out_rx {
            if let Err(e) = write_frame(&mut stdout, &msg) {
                eprintln!("hollow-net: writing to broker: {e}");
                break;
            }
        }
    });

    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<BrokerToNet>(16);
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            match read_frame(&mut stdin) {
                Ok(Some(msg)) => {
                    if in_tx.blocking_send(msg).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("hollow-net: bad message from broker: {e}");
                    break;
                }
            }
        }
    });

    // The broker checks our sandbox before letting Tor touch the network.
    let _ = out_tx.send(NetToBroker::Hello(hollow_sandbox::sandbox_report()));
    match in_rx.recv().await {
        Some(BrokerToNet::StartTor) => {}
        _ => return ExitCode::SUCCESS,
    }

    let client = match tor::start(out_tx.clone()).await {
        Ok(c) => c,
        Err(e) => {
            let _ = out_tx.send(NetToBroker::Fatal(format!("Tor failed to start: {e:#}")));
            drop(out_tx);
            let _ = writer.join();
            return ExitCode::FAILURE;
        }
    };
    let _ = out_tx.send(NetToBroker::Ready);
    let unlocked = hollow_sandbox::lock_stats().lock_failures;
    if unlocked > 0 {
        eprintln!("hollow-net: warning: {unlocked} allocations exceeded the locked-memory reserve and may be paged");
    }

    let fetcher = Arc::new(fetch::Fetcher::new(client));
    while let Some(msg) = in_rx.recv().await {
        match msg {
            BrokerToNet::Fetch(req) => {
                let fetcher = Arc::clone(&fetcher);
                let tx = out_tx.clone();
                tokio::spawn(async move {
                    let id = req.id;
                    let reply = match fetcher.fetch(req).await {
                        Ok(resp) => NetToBroker::FetchOk(resp),
                        Err(e) => NetToBroker::FetchErr { id, message: format!("{e:#}") },
                    };
                    let _ = tx.send(reply);
                });
            }
            BrokerToNet::Shutdown => break,
            BrokerToNet::StartTor => {}
        }
    }
    // In-flight fetches are dropped with the runtime.
    ExitCode::SUCCESS
}
