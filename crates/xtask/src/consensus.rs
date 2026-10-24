//! The set of endpoints a Tor client may legitimately connect to: relay ORPorts from the current
//! microdesc consensus, plus Arti's compiled-in fallback directories and directory authorities.
//!
//! xtask fetches the consensus itself, over plain HTTP from a directory authority, before Hollow
//! starts. It does not check the consensus signatures: this is a test oracle, and a forged
//! consensus could only widen the allow-list, never hide a connection from the monitor.

use std::collections::{BTreeSet, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, bail};

/// Where an allowed endpoint came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Consensus,
    Fallback,
    Authority,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Consensus => "relay ORPort (consensus)",
            Source::Fallback => "Arti fallback dir",
            Source::Authority => "directory authority",
        }
    }
}

pub struct RelaySet {
    consensus: HashSet<SocketAddr>,
    fallbacks: HashSet<SocketAddr>,
    authorities: HashSet<SocketAddr>,
    pub valid_after: String,
    pub fetched_from: SocketAddr,
    pub arti_sources: Vec<PathBuf>,
}

impl RelaySet {
    pub fn classify(&self, addr: SocketAddr) -> Option<Source> {
        let addr = normalize(addr);
        if self.consensus.contains(&addr) {
            Some(Source::Consensus)
        } else if self.fallbacks.contains(&addr) {
            Some(Source::Fallback)
        } else if self.authorities.contains(&addr) {
            Some(Source::Authority)
        } else {
            None
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} relay ORPorts from the consensus valid-after {} (fetched from {}), {} Arti fallback \
             dir endpoints, {} directory authority endpoints",
            self.consensus.len(),
            self.valid_after,
            self.fetched_from,
            self.fallbacks.len(),
            self.authorities.len()
        )
    }
}

/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`) compares as IPv4.
pub fn normalize(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), addr.port()),
            None => addr,
        },
        IpAddr::V4(_) => addr,
    }
}

pub fn load(workspace: &Path) -> anyhow::Result<RelaySet> {
    let (fallbacks, authorities, arti_sources) = arti_builtin_endpoints(workspace)?;
    if fallbacks.is_empty() || authorities.is_empty() {
        bail!("found no fallback dirs or authorities in {arti_sources:?}");
    }

    // Authority DirPorts (port 80, IPv4) are where we download the consensus from; gabelmoo first.
    let gabelmoo: SocketAddr = "131.188.40.189:80".parse().expect("valid address");
    let mut mirrors: Vec<SocketAddr> = vec![gabelmoo];
    mirrors.extend(authorities.iter().copied().filter(|a| a.is_ipv4() && a.port() == 80 && *a != gabelmoo));

    let mut errors = Vec::new();
    for mirror in mirrors {
        match download_consensus(mirror) {
            Ok(text) => {
                let (consensus, valid_after) = parse_consensus(&text)?;
                return Ok(RelaySet {
                    consensus,
                    fallbacks: fallbacks.into_iter().collect(),
                    authorities: authorities.into_iter().collect(),
                    valid_after,
                    fetched_from: mirror,
                    arti_sources,
                });
            }
            Err(e) => {
                eprintln!("  consensus download from {mirror} failed: {e:#}");
                errors.push(format!("{mirror}: {e:#}"));
            }
        }
    }
    bail!("could not download the consensus from any authority: {}", errors.join("; "))
}

/// Plain HTTP/1.0 GET of the current microdesc consensus from a directory authority.
fn download_consensus(dirport: SocketAddr) -> anyhow::Result<String> {
    let mut stream = TcpStream::connect_timeout(&dirport, Duration::from_secs(15))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    write!(
        stream,
        "GET /tor/status-vote/current/consensus-microdesc HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        dirport.ip()
    )?;
    let mut raw = Vec::new();
    stream.take(32 * 1024 * 1024).read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").context("no HTTP header end")?;
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200") {
        bail!("HTTP status `{status}`");
    }
    Ok(body.to_string())
}

/// Returns (ORPort endpoints from `r` and `a` lines, valid-after).
fn parse_consensus(text: &str) -> anyhow::Result<(HashSet<SocketAddr>, String)> {
    if !text.starts_with("network-status-version 3 microdesc") {
        bail!("not a microdesc consensus (starts with {:?})", text.lines().next().unwrap_or_default());
    }
    let mut set = HashSet::new();
    let mut valid_after = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("valid-after ") {
            valid_after = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("r ") {
            // r <nickname> <identity> <YYYY-MM-DD> <HH:MM:SS> <IPv4> <ORPort> <DirPort>
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() >= 7
                && let (Ok(ip), Ok(port)) = (f[4].parse::<IpAddr>(), f[5].parse::<u16>())
            {
                set.insert(SocketAddr::new(ip, port));
            }
        } else if let Some(rest) = line.strip_prefix("a ") {
            // a [IPv6]:ORPort
            if let Ok(addr) = rest.trim().parse::<SocketAddr>() {
                set.insert(normalize(addr));
            }
        }
    }
    if set.len() < 1000 {
        bail!("consensus lists only {} relays; refusing to use it", set.len());
    }
    Ok((set, valid_after))
}

/// Fallback dirs and authorities compiled into the Arti that Hollow actually builds against: we
/// ask `cargo metadata` where `tor-dircommon` lives (registry or third_party patch) and read every
/// quoted socket address out of its fallback list and authority list.
fn arti_builtin_endpoints(
    workspace: &Path,
) -> anyhow::Result<(BTreeSet<SocketAddr>, BTreeSet<SocketAddr>, Vec<PathBuf>)> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .output()
        .context("running cargo metadata")?;
    if !out.status.success() {
        bail!("cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).context("parsing cargo metadata")?;
    let mut fallbacks = BTreeSet::new();
    let mut authorities = BTreeSet::new();
    let mut sources = Vec::new();
    for pkg in meta["packages"].as_array().into_iter().flatten() {
        if pkg["name"] != "tor-dircommon" {
            continue;
        }
        let manifest = PathBuf::from(pkg["manifest_path"].as_str().unwrap_or_default());
        let dir = manifest.parent().context("manifest without parent")?;
        let fallback_file = dir.join("data").join("fallback_dirs.rs");
        let authority_file = dir.join("src").join("authority.rs");
        fallbacks.extend(quoted_socket_addrs(
            &std::fs::read_to_string(&fallback_file).with_context(|| format!("{}", fallback_file.display()))?,
        ));
        authorities.extend(quoted_socket_addrs(
            &std::fs::read_to_string(&authority_file).with_context(|| format!("{}", authority_file.display()))?,
        ));
        sources.push(fallback_file);
        sources.push(authority_file);
    }
    if sources.is_empty() {
        bail!("tor-dircommon is not in the dependency graph");
    }
    Ok((fallbacks, authorities, sources))
}

fn quoted_socket_addrs(source: &str) -> impl Iterator<Item = SocketAddr> + '_ {
    source.split('"').skip(1).step_by(2).filter_map(|s| s.parse::<SocketAddr>().ok()).map(normalize)
}

#[cfg(test)]
impl RelaySet {
    /// A relay set with the given consensus ORPorts and nothing else.
    pub fn for_test(relays: &[&str]) -> RelaySet {
        RelaySet {
            consensus: relays.iter().map(|r| r.parse().expect("socket address")).collect(),
            fallbacks: HashSet::new(),
            authorities: HashSet::new(),
            valid_after: String::new(),
            fetched_from: "127.0.0.1:80".parse().expect("socket address"),
            arti_sources: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_addresses() {
        let src = r#"fallback("AB", "x", &["146.56.99.34:443", "[2620:7:6003::ffff:c759:e652]:82"]), sa("1.2.3.4:80")"#;
        let got: Vec<_> = quoted_socket_addrs(src).collect();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], "146.56.99.34:443".parse().unwrap());
    }

    #[test]
    fn consensus_lines() {
        let mut text = String::from("network-status-version 3 microdesc\nvalid-after 2026-10-24 12:00:00\n");
        for i in 0..1000u32 {
            text.push_str(&format!("r n{i} AAAA 2026-10-24 11:00:00 10.0.{}.{} 9001 0\n", i / 256, i % 256));
        }
        text.push_str("a [2001:db8::1]:443\n");
        let (set, va) = parse_consensus(&text).unwrap();
        assert_eq!(va, "2026-10-24 12:00:00");
        assert!(set.contains(&"10.0.0.5:9001".parse().unwrap()));
        assert!(set.contains(&"[2001:db8::1]:443".parse().unwrap()));
        assert_eq!(set.len(), 1001);
    }
}
