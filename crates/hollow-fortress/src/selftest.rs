//! Tries every escape the sandbox must block (P2). Each attempt is real: if
//! one succeeds, the self-test fails.

use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::Path;
use std::time::Duration;

use hollow_ipc::{Check, SelfTestPlan};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub fn run(plan: &SelfTestPlan) -> Vec<Check> {
    // In the real (LPAC) sandbox Winsock can't even initialize, and the
    // standard library panics instead of returning an error. That counts as
    // blocked; the panic message becomes the detail.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut checks = vec![token_check()];
    checks.extend(mitigation_checks());
    checks.push(memory_locked());
    checks.push(guarded("tcp_external", || tcp("tcp_external", &plan.external_tcp)));
    checks.push(guarded("tcp_loopback", || tcp("tcp_loopback", &format!("127.0.0.1:{}", plan.loopback_tcp_port))));
    checks.push(guarded("udp_loopback", || udp_loopback(plan.loopback_udp_port)));
    checks.push(guarded("dns_lookup", || dns(&plan.dns_name)));
    checks.push(read_file(&plan.canary_path));
    checks.push(write_file(Path::new(&plan.write_dir)));
    checks.push(spawn_process());

    std::panic::set_hook(previous_hook);
    checks
}

fn guarded(name: &str, probe: impl FnOnce() -> Check) -> Check {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(probe)) {
        Ok(c) => c,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("panic");
            // std asserts that WSAStartup returned 0; the left side is the error.
            let detail = if msg.contains("10107") {
                "blocked: Winsock can't initialize in LPAC (WSAStartup error 10107)".to_owned()
            } else {
                format!("blocked: networking unavailable ({})", msg.replace('\n', " "))
            };
            check(name, true, detail)
        }
    }
}

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    let mut detail = detail.into();
    detail.truncate(400);
    Check { name: name.into(), passed, detail }
}

fn token_check() -> Check {
    match hollow_sandbox::is_app_container() {
        Ok(true) => check("token_appcontainer", true, "running inside an AppContainer"),
        Ok(false) => check("token_appcontainer", false, "NOT inside an AppContainer"),
        Err(e) => check("token_appcontainer", false, e.to_string()),
    }
}

fn mitigation_checks() -> Vec<Check> {
    match hollow_sandbox::active_mitigations() {
        Ok(m) => vec![
            check("acg_no_dynamic_code", m.dynamic_code_prohibited, flag(m.dynamic_code_prohibited)),
            check("win32k_disabled", m.win32k_disabled, flag(m.win32k_disabled)),
            check("cig_microsoft_signed_only", m.microsoft_signed_only, flag(m.microsoft_signed_only)),
        ],
        Err(e) => vec![check("mitigations", false, e.to_string())],
    }
}

/// A fresh allocation must sit on a page Windows reports as locked in RAM.
fn memory_locked() -> Check {
    let buf = vec![0x5au8; 64 * 1024];
    let locked = hollow_sandbox::is_page_locked(buf.as_ptr());
    let failures = hollow_sandbox::lock_stats().lock_failures;
    match locked {
        Ok(true) if failures == 0 => check("memory_locked", true, "allocations are locked out of the pagefile"),
        Ok(true) => check("memory_locked", false, format!("{failures} allocations could not be locked")),
        Ok(false) => check("memory_locked", false, "allocation is NOT locked; it may be paged to disk"),
        Err(e) => check("memory_locked", false, e.to_string()),
    }
}

fn flag(on: bool) -> &'static str {
    if on { "enforced" } else { "NOT enforced" }
}

fn tcp(name: &str, addr: &str) -> Check {
    let addr: SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => return check(name, false, format!("bad test address {addr}: {e}")),
    };
    match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(_) => check(name, false, format!("CONNECTED to {addr}")),
        Err(e) => check(name, true, format!("blocked: {e}")),
    }
}

/// UDP is connectionless, so a successful send proves nothing by itself. The
/// broker listens on this port and overrides the result if a datagram arrives.
fn udp_loopback(port: u16) -> Check {
    let result = UdpSocket::bind("127.0.0.1:0").and_then(|s| s.send_to(b"hollow-selftest", ("127.0.0.1", port)));
    match result {
        Ok(_) => check("udp_loopback", true, "send returned ok; broker verifies delivery"),
        Err(e) => check("udp_loopback", true, format!("blocked: {e}")),
    }
}

fn dns(name: &str) -> Check {
    match (name, 443).to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            if addrs.is_empty() {
                check("dns_lookup", true, "lookup returned no addresses")
            } else {
                check("dns_lookup", false, format!("RESOLVED {name} to {addrs:?}"))
            }
        }
        Err(e) => check("dns_lookup", true, format!("blocked: {e}")),
    }
}

fn read_file(path: &str) -> Check {
    match fs::File::open(path).and_then(|mut f| {
        let mut s = String::new();
        f.read_to_string(&mut s).map(|_| s)
    }) {
        Ok(_) => check("fs_read_user_file", false, format!("READ {path}")),
        Err(e) => check("fs_read_user_file", true, format!("blocked: {e}")),
    }
}

fn write_file(dir: &Path) -> Check {
    let path = dir.join("hollow-fortress-escape.txt");
    match fs::write(&path, b"escaped") {
        Ok(()) => {
            let _ = fs::remove_file(&path);
            check("fs_write_user_dir", false, format!("WROTE {}", path.display()))
        }
        Err(e) => check("fs_write_user_dir", true, format!("blocked: {e}")),
    }
}

fn spawn_process() -> Check {
    match std::process::Command::new(r"C:\Windows\System32\cmd.exe").arg("/c").arg("exit").spawn() {
        Ok(mut child) => {
            let _ = child.kill();
            check("spawn_child_process", false, "STARTED cmd.exe")
        }
        Err(e) => check("spawn_child_process", true, format!("blocked: {e}")),
    }
}
