//! Starting sandboxed children and talking to them.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter};
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use hollow_ipc::{BrokerToNet, NetToBroker, SandboxReport, Validate, read_frame, write_frame};
use hollow_sandbox::{AppContainer, Capability, Mitigations, SandboxConfig, SandboxedProcess};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::terminal::sanitize;

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Fortress,
    Net,
}

impl Kind {
    pub fn exe_name(self) -> &'static str {
        match self {
            Kind::Fortress => "hollow-fortress.exe",
            Kind::Net => "hollow-net.exe",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Fortress => "fortress",
            Kind::Net => "net",
        }
    }

    fn container_name(self) -> &'static str {
        match self {
            Kind::Fortress => "Hollow.Fortress",
            Kind::Net => "Hollow.Net",
        }
    }

    const ALL: [Kind; 2] = [Kind::Fortress, Kind::Net];
}

/// Removes what sandboxing leaves behind on the host: file-access entries on
/// our executables and the session's AppContainer profiles (registry entries
/// and `Packages` folders). Runs at exit, and at startup in case the last run
/// crashed.
///
/// Known limit: a second Hollow instance exiting would pull these out from
/// under a running one, which is why `main` holds a single-instance lock.
pub fn clean_up_host_traces() {
    for kind in Kind::ALL {
        if let (Ok(container), Ok(exe)) = (AppContainer::derive(kind.container_name()), sibling_exe(kind.exe_name()))
            && let Err(e) = hollow_sandbox::revoke_image_access(&container, &exe)
        {
            eprintln!("hollow: couldn't revoke sandbox access to {}: {e}", exe.display());
        }
        if let Err(e) = AppContainer::delete_profile(kind.container_name()) {
            eprintln!("hollow: couldn't remove sandbox profile {}: {e}", kind.container_name());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Normal,
    /// Self-test only: the renderer sandbox minus LPAC, so Winsock works and
    /// the AppContainer's own network isolation is what gets tested.
    WithoutLpac,
}

pub struct Child {
    kind: Kind,
    process: SandboxedProcess,
    rx: BufReader<File>,
    tx: BufWriter<File>,
    stderr: Option<JoinHandle<()>>,
}

impl Child {
    pub fn spawn(kind: Kind) -> anyhow::Result<Self> {
        Self::spawn_with(kind, Variant::Normal)
    }

    pub fn spawn_with(kind: Kind, variant: Variant) -> anyhow::Result<Self> {
        let exe = sibling_exe(kind.exe_name())?;
        // Registered for this session only; clean_up_host_traces() removes it.
        let container = AppContainer::register(kind.container_name())?;
        // Neither child is given anywhere to write: the network process keeps
        // Tor in RAM and the renderer never touches disk.
        let (mut args, capabilities, less_privileged, mitigations, max_memory) = match kind {
            Kind::Fortress => (vec![], &[][..], variant == Variant::Normal, Mitigations::renderer(), 1 << 30),
            Kind::Net => (vec![], &[Capability::InternetClient][..], false, Mitigations::network(), 2 << 30),
        };
        // Developer switch: HOLLOW_LOG=<tracing filter> turns up child logging.
        if let Some(filter) = std::env::var_os("HOLLOW_LOG") {
            args.push("--log".into());
            args.push(filter);
        }
        let mut process = SandboxedProcess::spawn(&SandboxConfig {
            container: &container,
            exe: &exe,
            args,
            capabilities,
            less_privileged,
            mitigations,
            max_memory,
            extra_env: vec![],
        })
        .with_context(|| format!("starting {}", kind.exe_name()))?;

        let stderr = forward_stderr(kind.label(), process.stderr.take().expect("stderr pipe is set by spawn"));
        let rx = BufReader::new(process.stdout.take().expect("stdout pipe is set by spawn"));
        let tx = BufWriter::new(process.stdin.take().expect("stdin pipe is set by spawn"));
        Ok(Self { kind, process, rx, tx, stderr: Some(stderr) })
    }

    pub fn send<T: Serialize>(&mut self, msg: &T) -> anyhow::Result<()> {
        write_frame(&mut self.tx, msg).with_context(|| format!("sending to {}", self.kind.exe_name()))
    }

    /// Receives and validates the next message. A child that closes its pipe
    /// or sends something invalid is treated as failed.
    pub fn recv<T: DeserializeOwned + Validate>(&mut self) -> anyhow::Result<T> {
        let name = self.kind.exe_name();
        match read_frame::<_, T>(&mut self.rx) {
            Ok(Some(msg)) => {
                msg.validate().with_context(|| format!("{name} sent an invalid message"))?;
                Ok(msg)
            }
            Ok(None) => {
                let code = self.process.wait_timeout(Duration::from_secs(2)).ok().flatten();
                // Let the child's last log lines reach the terminal before the error.
                if let Some(t) = self.stderr.take() {
                    let _ = t.join();
                }
                match code {
                    Some(code) => bail!("{name} exited unexpectedly (exit code {code:#x})"),
                    None => bail!("{name} closed its pipe unexpectedly"),
                }
            }
            Err(e) => Err(anyhow!(e).context(format!("reading from {name}"))),
        }
    }

    /// Asks the child to exit, then kills it if it doesn't.
    pub fn shutdown<T: Serialize>(mut self, msg: &T) {
        let _ = self.send(msg);
        if !matches!(self.process.wait_timeout(Duration::from_secs(3)), Ok(Some(_))) {
            self.process.kill();
        }
    }
}

/// Starts the network process and reads its sandbox report.
pub fn spawn_net_checked() -> anyhow::Result<(Child, SandboxReport)> {
    let mut net = Child::spawn(Kind::Net)?;
    match net.recv::<NetToBroker>()? {
        NetToBroker::Hello(report) => Ok((net, report)),
        other => bail!("network process didn't report its sandbox first: {other:?}"),
    }
}

/// Starts the network process and lets Tor start only if every protection
/// is in force. Fails closed.
pub fn start_net() -> anyhow::Result<Child> {
    let (mut net, report) = spawn_net_checked()?;
    if !report.all_enforced() {
        let missing: Vec<_> = report.checks().iter().filter(|(_, on)| !on).map(|(name, _)| *name).collect();
        net.shutdown(&BrokerToNet::Shutdown);
        bail!("network process sandbox incomplete ({}); refusing to start Tor", missing.join(", "));
    }
    net.send(&BrokerToNet::StartTor)?;
    Ok(net)
}

pub fn sibling_exe(name: &str) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?.with_file_name(name);
    if !exe.is_file() {
        bail!("{} not found; build the whole workspace with `cargo build --workspace`", exe.display());
    }
    Ok(exe)
}

fn forward_stderr(label: &'static str, stderr: File) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = sanitize(String::from_utf8_lossy(&line).trim_end());
                    eprintln!("  [{label}] {text}");
                }
            }
        }
    })
}
