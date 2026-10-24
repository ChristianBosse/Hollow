//! `cargo xtask leak-test`: property P1 ("no packet leaves the device except through Tor"),
//! host edition of TRUST-06.
//!
//! The real test runs Hollow in an isolated VM and fails on any non-Tor packet. Until that exists,
//! this runs `hollow fetch` on the host and polls the system TCP/UDP tables (with owning PIDs)
//! every ~10 ms while it runs:
//!
//! - `hollow.exe`, `hollow-fortress.exe` and any other process Hollow starts must own no socket.
//! - `hollow-net.exe` must own no UDP socket and no listening socket, and every remote TCP
//!   endpoint must be a relay ORPort from the current consensus, an Arti fallback directory or a
//!   directory authority.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::consensus::{self, RelaySet};
use crate::{no_more_args, take_flag, take_value, workspace_root};

const DEFAULT_URL: &str = "https://check.torproject.org/";
const POLL: Duration = Duration::from_millis(10);
/// Re-read the process list every this many polls (and whenever a socket has an unknown PID).
const PROCESS_REFRESH_EVERY: u64 = 5;

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

    println!("== Leak test (P1 / TRUST-06, host edition) ==\n");
    report_windows_sandbox();

    let root = workspace_root();
    let bin_dir = crate::dist::test_binaries(bin_dir_arg, no_build, locked)?;
    let hollow = bin_dir.join("hollow.exe");

    println!("\nLoading the Tor relay set (xtask downloads it itself, before Hollow starts)...");
    let relays = consensus::load(&root)?;
    println!("  {}", relays.summary());
    for source in &relays.arti_sources {
        println!("  Arti built-in list: {}", source.display());
    }

    println!("\nRunning {} fetch {url}", hollow.display());
    let outcome = monitor(&hollow, &url, timeout, &relays)?;
    Ok(print_report(&outcome, &url, &relays))
}

/// Read-only check: is Windows Sandbox (a disposable Hyper-V VM) already installed? Enabling it
/// needs admin rights and is a system change, so xtask never does that.
fn report_windows_sandbox() {
    let system_root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
    let exe = system_root.join("System32").join("WindowsSandbox.exe");
    if exe.is_file() {
        println!(
            "Windows Sandbox: available ({}). A VM-isolated leak test can be built on it later;\n\
             this run uses the host monitor below.\n",
            exe.display()
        );
    } else {
        println!(
            "Windows Sandbox: not available ({} not found). Enabling it (the optional feature\n\
             Containers-DisposableClientVM) needs admin rights; xtask does not change the system.\n\
             Running the host-level monitor instead.\n",
            exe.display()
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Proto {
    Tcp,
    Udp,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Endpoint {
    proto: Proto,
    local: SocketAddr,
    /// `None` for UDP and for listening TCP sockets.
    remote: Option<SocketAddr>,
}

struct Seen {
    first: Duration,
    last: Duration,
    states: BTreeSet<String>,
}

struct Tracked {
    name: String,
    first_seen: Duration,
    endpoints: BTreeMap<Endpoint, Seen>,
}

struct Outcome {
    exit_ok: bool,
    exit_desc: String,
    timed_out: bool,
    stdout: String,
    elapsed: Duration,
    polls: u64,
    max_gap: Duration,
    processes: BTreeMap<u32, Tracked>,
    violations: Vec<String>,
    unattributed: BTreeSet<u32>,
}

fn monitor(hollow: &Path, url: &str, timeout: Duration, relays: &RelaySet) -> anyhow::Result<Outcome> {
    let mut sys = System::new();
    // PIDs that own sockets before Hollow starts can't be Hollow's (Windows doesn't reuse a PID
    // while sockets still reference the process), even if they don't show in the process list.
    let preexisting: HashSet<u32> = netstat2::get_sockets_info(
        AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
        ProtocolFlags::TCP | ProtocolFlags::UDP,
    )
    .context("reading the TCP/UDP tables")?
    .into_iter()
    .flat_map(|s| s.associated_pids)
    .collect();
    let mut child = Command::new(hollow)
        .args(["fetch", url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", hollow.display()))?;
    let start = Instant::now();
    let root = child.id();

    // Echo hollow's progress (stderr) live; keep stdout (the page text) for the report.
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

    let mut processes: BTreeMap<u32, Tracked> = BTreeMap::new();
    // PIDs we looked up and found not to belong to Hollow.
    let mut foreign: HashSet<u32> = preexisting;
    foreign.remove(&root);
    let mut violations: Vec<String> = Vec::new();
    let mut violation_keys: HashSet<String> = HashSet::new();
    let mut unattributed = BTreeSet::new();
    let mut polls = 0u64;
    let mut max_gap = Duration::ZERO;
    let mut last_poll = Instant::now();
    let mut timed_out = false;

    let status = loop {
        let exited = child.try_wait()?;
        let now = start.elapsed();
        if polls.is_multiple_of(PROCESS_REFRESH_EVERY) {
            refresh_family(&mut sys, root, &mut processes, &mut foreign, now);
        }
        let sockets = netstat2::get_sockets_info(
            AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
            ProtocolFlags::TCP | ProtocolFlags::UDP,
        )
        .context("reading the TCP/UDP tables")?;
        polls += 1;
        let gap = last_poll.elapsed();
        last_poll = Instant::now();
        max_gap = max_gap.max(gap);

        let mut refreshed = false;
        for s in &sockets {
            for &pid in &s.associated_pids {
                if pid == 0 || pid == 4 || foreign.contains(&pid) {
                    continue;
                }
                if !processes.contains_key(&pid) && !refreshed {
                    refresh_family(&mut sys, root, &mut processes, &mut foreign, now);
                    refreshed = true;
                }
                let Some(tracked) = processes.get_mut(&pid) else {
                    if !foreign.contains(&pid) {
                        // Gone before we could look it up; can't say whether it was Hollow's.
                        unattributed.insert(pid);
                    }
                    continue;
                };
                let (endpoint, state) = match &s.protocol_socket_info {
                    ProtocolSocketInfo::Tcp(t) => {
                        let listening = t.state == TcpState::Listen;
                        let remote = SocketAddr::new(t.remote_addr, t.remote_port);
                        let remote = (!listening && !is_unspecified(remote)).then(|| consensus::normalize(remote));
                        let local = consensus::normalize(SocketAddr::new(t.local_addr, t.local_port));
                        (Endpoint { proto: Proto::Tcp, local, remote }, format!("{:?}", t.state))
                    }
                    ProtocolSocketInfo::Udp(u) => {
                        let local = consensus::normalize(SocketAddr::new(u.local_addr, u.local_port));
                        (Endpoint { proto: Proto::Udp, local, remote: None }, "bound".to_string())
                    }
                };
                if let Some(v) = check_rule(&tracked.name, &endpoint, &state, relays) {
                    let key = format!("{pid} {v}");
                    if violation_keys.insert(key) {
                        violations.push(format!("+{:.2}s {} (pid {pid}): {v}", now.as_secs_f64(), tracked.name));
                    }
                }
                let seen = tracked.endpoints.entry(endpoint).or_insert_with(|| Seen {
                    first: now,
                    last: now,
                    states: BTreeSet::new(),
                });
                seen.last = now;
                seen.states.insert(state);
            }
        }

        if let Some(status) = exited {
            break status;
        }
        if start.elapsed() > timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(POLL.saturating_sub(last_poll.elapsed()));
    };
    let elapsed = start.elapsed();

    // Hollow's children should be gone with it; anything left over is reported, not killed
    // (they're sandboxed in a job object that dies with the broker).
    refresh_family(&mut sys, root, &mut processes, &mut foreign, elapsed);
    for pid in processes.keys() {
        if *pid != root && sys.process(Pid::from_u32(*pid)).is_some() {
            let name = &processes[pid].name;
            violations.push(format!("{name} (pid {pid}) still running after hollow.exe exited"));
        }
    }

    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let stdout = String::from_utf8_lossy(&stdout_buf.lock().map(|b| b.clone()).unwrap_or_default()).into_owned();
    Ok(Outcome {
        exit_ok: status.success() && !timed_out,
        exit_desc: if timed_out { format!("killed after {}s timeout", timeout.as_secs()) } else { status.to_string() },
        timed_out,
        stdout,
        elapsed,
        polls,
        max_gap,
        processes,
        violations,
        unattributed,
    })
}

fn is_unspecified(addr: SocketAddr) -> bool {
    addr.ip().is_unspecified() && addr.port() == 0
}

/// Re-reads the process list and adds every live descendant of `root` to `processes`.
fn refresh_family(
    sys: &mut System,
    root: u32,
    processes: &mut BTreeMap<u32, Tracked>,
    foreign: &mut HashSet<u32>,
    now: Duration,
) {
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let parent_of: HashMap<u32, u32> =
        sys.processes().iter().filter_map(|(pid, p)| p.parent().map(|pp| (pid.as_u32(), pp.as_u32()))).collect();
    for (pid, proc_) in sys.processes() {
        let pid = pid.as_u32();
        if processes.contains_key(&pid) {
            continue;
        }
        // Walk up the parent chain (bounded: PIDs can be reused and form cycles).
        let mut cur = pid;
        let mut ours = cur == root;
        for _ in 0..16 {
            if ours {
                break;
            }
            match parent_of.get(&cur) {
                Some(&p) if p != cur => {
                    cur = p;
                    ours = cur == root;
                }
                _ => break,
            }
        }
        if ours {
            let name = proc_.name().to_string_lossy().into_owned();
            processes.insert(pid, Tracked { name, first_seen: now, endpoints: BTreeMap::new() });
            foreign.remove(&pid);
        } else {
            foreign.insert(pid);
        }
    }
}

/// Returns a violation description, or `None` if the socket is allowed.
fn check_rule(process: &str, ep: &Endpoint, state: &str, relays: &RelaySet) -> Option<String> {
    let what = describe(ep, state);
    if !process.eq_ignore_ascii_case("hollow-net.exe") {
        return Some(format!("owns a socket, but only hollow-net.exe may: {what}"));
    }
    match (ep.proto, ep.remote) {
        (Proto::Udp, _) => Some(format!("UDP socket: {what}")),
        (Proto::Tcp, None) if state == "Listen" => Some(format!("listening socket: {what}")),
        (Proto::Tcp, None) => Some(format!("TCP socket without a remote endpoint: {what}")),
        (Proto::Tcp, Some(remote)) => match relays.classify(remote) {
            Some(_) => None,
            None => Some(format!("connection to a non-Tor endpoint: {what}")),
        },
    }
}

fn describe(ep: &Endpoint, state: &str) -> String {
    let proto = match ep.proto {
        Proto::Tcp => "TCP",
        Proto::Udp => "UDP",
    };
    match ep.remote {
        Some(r) => format!("{proto} {} -> {r} [{state}]", ep.local),
        None => format!("{proto} {} [{state}]", ep.local),
    }
}

fn print_report(o: &Outcome, url: &str, relays: &RelaySet) -> bool {
    println!("\n== Leak test report ==\n");
    println!(
        "Monitor: {} polls of the TCP+UDP tables (IPv4+IPv6) over {:.1}s, mean interval {:.1} ms, longest gap {} ms",
        o.polls,
        o.elapsed.as_secs_f64(),
        o.elapsed.as_secs_f64() * 1000.0 / o.polls.max(1) as f64,
        o.max_gap.as_millis()
    );
    println!("hollow fetch {url}: {}", o.exit_desc);

    let mut ok = o.exit_ok;
    if url == DEFAULT_URL {
        if o.stdout.contains("Congratulations") {
            println!("check.torproject.org says: Congratulations (the request arrived from a Tor exit)");
        } else if o.exit_ok {
            println!("check.torproject.org did NOT say \"Congratulations\": the request may not have come through Tor");
            ok = false;
        }
    }

    println!("\nProcesses started by Hollow and the sockets they owned:");
    let mut net_tcp = 0;
    for (pid, t) in &o.processes {
        println!(
            "  {} (pid {pid}, seen from +{:.2}s): {} endpoint(s)",
            t.name,
            t.first_seen.as_secs_f64(),
            t.endpoints.len()
        );
        for (ep, seen) in &t.endpoints {
            let states: Vec<&str> = seen.states.iter().map(String::as_str).collect();
            let class = match ep.remote {
                Some(r) => relays.classify(r).map(|s| s.label()).unwrap_or("NOT A TOR ENDPOINT"),
                None => "-",
            };
            println!(
                "      {:<84} {:<26} +{:.2}s..+{:.2}s",
                describe(ep, &states.join("/")),
                class,
                seen.first.as_secs_f64(),
                seen.last.as_secs_f64()
            );
            if t.name.eq_ignore_ascii_case("hollow-net.exe") && ep.proto == Proto::Tcp && ep.remote.is_some() {
                net_tcp += 1;
            }
        }
    }
    for name in ["hollow-net.exe", "hollow-fortress.exe"] {
        if !o.processes.values().any(|t| t.name.eq_ignore_ascii_case(name)) {
            println!("  {name}: never observed in the process list (short-lived, or not started)");
        }
    }
    if !o.unattributed.is_empty() {
        println!(
            "\nNote: {} PID(s) owned sockets but exited before they could be looked up: {:?}.\n      \
             They may or may not have been Hollow's.",
            o.unattributed.len(),
            o.unattributed
        );
    }

    println!();
    if o.violations.is_empty() {
        println!("Violations: none");
    } else {
        println!("Violations ({}):", o.violations.len());
        for v in &o.violations {
            println!("  - {v}");
        }
        ok = false;
    }
    if net_tcp == 0 && !o.timed_out {
        println!("Inconclusive: no hollow-net.exe connection to Tor was observed at all.");
        ok = false;
    }

    println!(
        "\nCaveats (host edition, not the VM test P1 asks for):\n\
         - The socket tables are polled, not traced: a socket that opens and closes between two\n\
           polls (~10 ms) is not seen. Longest gap this run: {} ms.\n\
         - DNS done through the Windows DNS Client service (svchost) is attributed to that\n\
           service, not to Hollow, so it is invisible here. (hollow-net resolves nothing itself in\n\
           Tor mode; the renderer sandbox blocks DNS, see `hollow selftest`.)\n\
         - Only Hollow's own process tree is checked; ICMP, raw sockets and traffic from other\n\
           processes acting for Hollow are out of scope.\n\
         - The relay allow-list comes from an unauthenticated consensus download; it can only\n\
           be too wide, never hide a connection.",
        o.max_gap.as_millis()
    );
    println!("\nRESULT: {}", if ok { "PASS" } else { "FAIL" });
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_remote() {
        assert!(is_unspecified("0.0.0.0:0".parse().unwrap()));
        assert!(is_unspecified("[::]:0".parse().unwrap()));
        assert!(!is_unspecified("1.2.3.4:0".parse().unwrap()));
    }

    #[test]
    fn rules() {
        let relays = RelaySet::for_test(&["1.2.3.4:9001"]);
        let local: SocketAddr = "10.0.0.2:50000".parse().unwrap();
        let tcp = |remote: Option<&str>| Endpoint {
            proto: Proto::Tcp,
            local,
            remote: remote.map(|r| consensus::normalize(r.parse().unwrap())),
        };
        let udp = Endpoint { proto: Proto::Udp, local, remote: None };
        let relay = tcp(Some("1.2.3.4:9001"));
        // hollow-net may talk to relays, as IPv4 or IPv4-mapped IPv6.
        assert_eq!(check_rule("hollow-net.exe", &relay, "Established", &relays), None);
        let mapped = tcp(Some("[::ffff:1.2.3.4]:9001"));
        assert_eq!(check_rule("HOLLOW-NET.EXE", &mapped, "SynSent", &relays), None);
        // ...and to nothing else.
        assert!(check_rule("hollow-net.exe", &tcp(Some("1.2.3.4:443")), "Established", &relays).is_some());
        assert!(check_rule("hollow-net.exe", &tcp(None), "Listen", &relays).is_some());
        assert!(check_rule("hollow-net.exe", &udp, "bound", &relays).is_some());
        // Broker and renderer may own no socket at all, not even to a relay.
        assert!(check_rule("hollow.exe", &relay, "Established", &relays).is_some());
        assert!(check_rule("hollow-fortress.exe", &relay, "Established", &relays).is_some());
    }
}
