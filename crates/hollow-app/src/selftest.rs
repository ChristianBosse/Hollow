//! `hollow selftest`: proves P2 on this machine. A Fortress renderer is
//! started in its real sandbox and asked to escape; the broker watches its
//! own loopback listeners to catch anything the renderer can't see.
//!
//! It runs twice. Layer 1 is the real renderer sandbox, where Winsock can't
//! even initialize. Layer 2 drops LPAC so Winsock works, which is what an
//! attacker who talks to the network driver directly would have; the
//! AppContainer's network isolation must still block everything.
//!
//! If the sandbox failed, the external TCP and DNS probes would themselves
//! leave the machine outside Tor. That's the point of the test, and it only
//! ever contacts 1.1.1.1 and resolves example.com.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, bail};
use hollow_ipc::{BrokerToNet, BrokerToRenderer, Check, RendererToBroker, SelfTestPlan};

use crate::child::{self, Child, Kind, Variant, sibling_exe};
use crate::terminal::sanitize;

pub fn run() -> anyhow::Result<bool> {
    let sections = [
        ("Layer 1: real renderer sandbox (LPAC, no capabilities, all mitigations)", run_layer(Variant::Normal)?),
        (
            "Layer 2: same sandbox without LPAC (Winsock works; AppContainer must still block)",
            run_layer(Variant::WithoutLpac)?,
        ),
        ("Network process sandbox (checked before Tor may start; Tor is not started here)", net_sandbox()?),
        ("Startup", vec![refuses_unsandboxed_start(Kind::Fortress)?, refuses_unsandboxed_start(Kind::Net)?]),
    ];

    println!();
    println!("Hollow sandbox self-test (property P2)");
    let mut total = 0;
    let mut failed = 0;
    for (title, checks) in &sections {
        println!();
        println!("{title}");
        for c in checks {
            let mark = if c.passed { "PASS" } else { "FAIL" };
            println!("  [{mark}] {:<29} {}", sanitize(&c.name), sanitize(&c.detail));
            total += 1;
            failed += usize::from(!c.passed);
        }
    }
    println!();
    if failed == 0 {
        println!(
            "All {total} checks passed: every sandbox held, and the renderer could not reach the network, your files or other processes."
        );
    } else {
        println!("{failed} of {total} checks FAILED. Do not rely on this build.");
    }
    Ok(failed == 0)
}

fn run_layer(variant: Variant) -> anyhow::Result<Vec<Check>> {
    let tcp = TcpListener::bind("127.0.0.1:0").context("opening loopback TCP listener")?;
    let udp = UdpSocket::bind("127.0.0.1:0").context("opening loopback UDP socket")?;
    tcp.set_nonblocking(true)?;
    udp.set_nonblocking(true)?;
    let loopback_control = loopback_works(&tcp)?;

    let user_dir = std::env::temp_dir();
    let canary = user_dir.join(format!("hollow-canary-{}.txt", std::process::id()));
    fs::write(&canary, "if the renderer can read this, the sandbox is broken")?;

    let plan = SelfTestPlan {
        external_tcp: "1.1.1.1:443".into(),
        dns_name: "example.com".into(),
        loopback_tcp_port: tcp.local_addr()?.port(),
        loopback_udp_port: udp.local_addr()?.port(),
        canary_path: canary.to_string_lossy().into_owned(),
        write_dir: user_dir.to_string_lossy().into_owned(),
    };

    let result = (|| {
        let mut renderer = Child::spawn_with(Kind::Fortress, variant)?;
        renderer.send(&BrokerToRenderer::SelfTest(plan))?;
        let checks = match renderer.recv::<RendererToBroker>()? {
            RendererToBroker::SelfTestReport(c) => c,
            other => bail!("unexpected reply from renderer: {other:?}"),
        };
        renderer.shutdown(&BrokerToRenderer::Shutdown);
        Ok(checks)
    })();
    let _ = fs::remove_file(&canary);
    let mut checks = result?;

    // What the broker saw on its side overrides what the renderer reported.
    for c in &mut checks {
        if c.name == "tcp_loopback" && tcp.accept().is_ok() {
            c.passed = false;
            c.detail = "broker ACCEPTED a TCP connection from the renderer".into();
        }
        if c.name == "udp_loopback" {
            let mut buf = [0u8; 64];
            if udp.recv_from(&mut buf).is_ok() {
                c.passed = false;
                c.detail = "broker RECEIVED a UDP datagram from the renderer".into();
            }
        }
    }
    checks.push(Check {
        name: "loopback_control".into(),
        passed: loopback_control,
        detail: if loopback_control {
            "the broker itself can use loopback, so the loopback checks are meaningful".into()
        } else {
            "the broker can't use loopback either; loopback results are inconclusive".into()
        },
    });
    Ok(checks)
}

/// Sanity check: the broker can connect to its own listener. Without this, a
/// machine with loopback broken would make the loopback checks pass vacuously.
fn loopback_works(tcp: &TcpListener) -> anyhow::Result<bool> {
    let addr = tcp.local_addr()?;
    let Ok(mut client) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return Ok(false);
    };
    client.write_all(b"x")?;
    for _ in 0..50 {
        if let Ok((mut server, _)) = tcp.accept() {
            server.set_nonblocking(false)?;
            let mut b = [0u8; 1];
            return Ok(server.read_exact(&mut b).is_ok());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(false)
}

/// The network process's own report of its sandbox, without starting Tor.
fn net_sandbox() -> anyhow::Result<Vec<Check>> {
    let (net, report) = child::spawn_net_checked()?;
    net.shutdown(&BrokerToNet::Shutdown);
    Ok(report
        .checks()
        .iter()
        .map(|(name, on)| Check {
            name: (*name).into(),
            passed: *on,
            detail: if *on { "enforced".into() } else { "NOT enforced".into() },
        })
        .collect())
}

/// Each child must refuse to run when started outside its sandbox.
fn refuses_unsandboxed_start(kind: Kind) -> anyhow::Result<Check> {
    let exe = sibling_exe(kind.exe_name())?;
    let out = Command::new(&exe).output().with_context(|| format!("running {}", exe.display()))?;
    let code = out.status.code();
    let name = kind.exe_name();
    Ok(Check {
        name: format!("refuses_unsandboxed_{}", kind.label()),
        passed: code == Some(78),
        detail: match code {
            Some(78) => format!("{name} started outside the sandbox and refused to run"),
            other => format!("{name} started outside the sandbox and exited with {other:?}"),
        },
    })
}
