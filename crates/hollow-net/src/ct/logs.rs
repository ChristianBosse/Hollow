//! The bundled list of Certificate Transparency logs.
//!
//! `crates/hollow-net/ct/log_list.json` is Google's official v3 list, byte
//! for byte, with Google's signature over it in `log_list.sig`. A unit test
//! checks that signature against the pinned key in `log_list_pubkey.pem`, so
//! a hand-edited list fails `cargo test`.
//!
//! To refresh it (every release: Chrome stops enforcing CT with a list older
//! than 70 days, and a stale list lacks new logs), from the repository root,
//! in PowerShell (`curl.exe`, not the `curl` alias) or Git Bash:
//!
//! ```text
//! curl.exe -o crates/hollow-net/ct/log_list.json https://www.gstatic.com/ct/log_list/v3/log_list.json
//! curl.exe -o crates/hollow-net/ct/log_list.sig https://www.gstatic.com/ct/log_list/v3/log_list.sig
//! cargo test -p hollow-net ct::
//! ```
//!
//! The tests use the fixture chains in `testdata/ct` at a fixed time, so they
//! keep passing until a fixture's log leaves the list; then fetch fresh
//! chains (`openssl s_client -showcerts`) and move the test time. Never
//! refresh `log_list_pubkey.pem` the same way: a changed key is a reason to
//! stop and check.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use super::asn1::{self, Tlv};

/// The list as compiled into this build.
pub fn bundled() -> &'static LogList {
    static LIST: LazyLock<LogList> = LazyLock::new(|| {
        LogList::parse(include_bytes!("../../ct/log_list.json")).expect("bundled CT log list is valid")
    });
    &LIST
}

pub struct LogList {
    /// When Google published the list (seconds since the Unix epoch).
    pub published: u64,
    pub logs: Vec<Log>,
}

pub struct Log {
    pub id: [u8; 32],
    pub description: String,
    /// Index into the list's operators; equal means "the same operator".
    pub operator: usize,
    pub key: LogKey,
    pub state: LogState,
}

/// A log's public key, ready for ring.
pub enum LogKey {
    /// Uncompressed P-256 point.
    EcdsaP256(Vec<u8>),
    /// PKCS#1 `RSAPublicKey`.
    Rsa(Vec<u8>),
}

/// Chrome's log lifecycle. `since` is when the log entered the state, in
/// seconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogState {
    Pending,
    Qualified { since: u64 },
    Usable { since: u64 },
    ReadOnly { since: u64 },
    Retired { since: u64 },
    Rejected { since: u64 },
}

impl fmt::Debug for LogList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogList").field("published", &self.published).field("logs", &self.logs.len()).finish()
    }
}

#[derive(Deserialize)]
struct JsonList {
    log_list_timestamp: String,
    operators: Vec<JsonOperator>,
}

#[derive(Deserialize)]
struct JsonOperator {
    #[serde(default)]
    logs: Vec<JsonLog>,
    /// Static CT API logs: same SCTs, different submission protocol.
    #[serde(default)]
    tiled_logs: Vec<JsonLog>,
}

#[derive(Deserialize)]
struct JsonLog {
    description: String,
    log_id: String,
    key: String,
    #[serde(default)]
    state: BTreeMap<String, JsonState>,
}

#[derive(Deserialize)]
struct JsonState {
    timestamp: String,
}

impl LogList {
    pub fn parse(json: &[u8]) -> Result<Self, String> {
        let list: JsonList = serde_json::from_slice(json).map_err(|e| e.to_string())?;
        let mut logs = Vec::new();
        for (operator, op) in list.operators.iter().enumerate() {
            for log in op.logs.iter().chain(&op.tiled_logs) {
                logs.push(parse_log(log, operator).map_err(|e| format!("{}: {e}", log.description))?);
            }
        }
        Ok(Self { published: parse_time(&list.log_list_timestamp)?, logs })
    }

    pub fn get(&self, id: &[u8; 32]) -> Option<&Log> {
        self.logs.iter().find(|log| &log.id == id)
    }
}

fn parse_log(log: &JsonLog, operator: usize) -> Result<Log, String> {
    let spki = BASE64.decode(&log.key).map_err(|e| format!("key: {e}"))?;
    let id: [u8; 32] = BASE64
        .decode(&log.log_id)
        .map_err(|e| format!("log_id: {e}"))?
        .try_into()
        .map_err(|_| "log_id is not 32 bytes")?;
    // RFC 6962 §3.2: the ID is the SHA-256 of the key.
    if ring::digest::digest(&ring::digest::SHA256, &spki).as_ref() != id {
        return Err("log_id doesn't match key".into());
    }
    let state = match log.state.iter().next() {
        None => LogState::Pending,
        Some((name, state)) => {
            let since = parse_time(&state.timestamp)?;
            match name.as_str() {
                "pending" => LogState::Pending,
                "qualified" => LogState::Qualified { since },
                "usable" => LogState::Usable { since },
                "readonly" => LogState::ReadOnly { since },
                "retired" => LogState::Retired { since },
                "rejected" => LogState::Rejected { since },
                other => return Err(format!("unknown state {other:?}")),
            }
        }
    };
    let key = parse_key(&spki).ok_or("unsupported key type")?;
    Ok(Log { id, description: log.description.clone(), operator, key, state })
}

const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]; // 1.2.840.10045.2.1
const PRIME256V1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]; // 1.2.840.10045.3.1.7
const RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01]; // 1.2.840.113549.1.1.1

/// Reads a `SubjectPublicKeyInfo`. RFC 6962 allows only P-256 and RSA logs.
pub(super) fn parse_key(spki: &[u8]) -> Option<LogKey> {
    let spki = asn1::single(spki).ok()?;
    let [algorithm, key] = asn1::elements(spki.contents).ok()?[..] else { return None };
    let algorithm = asn1::elements(algorithm.contents).ok()?;
    let key = bit_string(&key)?.to_vec();
    match &algorithm[..] {
        [oid, curve] if oid.contents == EC_PUBLIC_KEY && curve.contents == PRIME256V1 => Some(LogKey::EcdsaP256(key)),
        [oid, ..] if oid.contents == RSA_ENCRYPTION => Some(LogKey::Rsa(key)),
        _ => None,
    }
}

fn bit_string<'a>(tlv: &Tlv<'a>) -> Option<&'a [u8]> {
    match tlv.contents {
        [0, key @ ..] if tlv.tag == der::Tag::BitString => Some(key),
        _ => None,
    }
}

/// `2024-09-30T22:19:27Z`, optionally with fractional seconds (dropped).
fn parse_time(s: &str) -> Result<u64, String> {
    let bad = || format!("bad timestamp {s:?}");
    let s = s.strip_suffix('Z').ok_or_else(bad)?;
    let s = s.split('.').next().ok_or_else(bad)?;
    let b = s.as_bytes();
    if b.len() != 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return Err(bad());
    }
    let num = |range: std::ops::Range<usize>| s.get(range).and_then(|n| n.parse::<u16>().ok()).ok_or_else(bad);
    let t = der::DateTime::new(
        num(0..4)?,
        num(5..7)? as u8,
        num(8..10)? as u8,
        num(11..13)? as u8,
        num(14..16)? as u8,
        num(17..19)? as u8,
    )
    .map_err(|_| bad())?;
    Ok(t.unix_duration().as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_list_is_googles() {
        // log_list.sig is Google's RSA-SHA256 signature over the exact bytes.
        let pem = include_str!("../../ct/log_list_pubkey.pem");
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        let Some(LogKey::Rsa(key)) = parse_key(&BASE64.decode(body).unwrap()) else { panic!("not an RSA key") };
        let key = ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PKCS1_2048_8192_SHA256, key);
        key.verify(include_bytes!("../../ct/log_list.json"), include_bytes!("../../ct/log_list.sig"))
            .expect("log_list.sig doesn't match log_list.json");
    }

    #[test]
    fn parses_bundled_list() {
        let list = bundled();
        assert!(list.logs.len() > 20, "only {} logs", list.logs.len());
        assert!(list.published > parse_time("2026-01-01T00:00:00Z").unwrap());
        // Several operators, including tiled (static CT API) logs.
        let operators: std::collections::BTreeSet<_> = list.logs.iter().map(|l| l.operator).collect();
        assert!(operators.len() >= 5);
        assert!(list.logs.iter().any(|l| l.description.contains("Sycamore")));
        assert!(list.logs.iter().any(|l| matches!(l.state, LogState::Usable { .. })));
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_time("2024-09-30T22:19:27Z"), Ok(1_727_734_767));
        assert_eq!(parse_time("2024-09-30T22:19:27.5Z"), Ok(1_727_734_767));
        assert!(parse_time("2024-09-30 22:19:27Z").is_err());
        assert!(parse_time("2024-09-30T22:19:27").is_err());
    }

    #[test]
    fn rejects_mismatched_log_id() {
        let json = br#"{"log_list_timestamp": "2026-01-01T00:00:00Z", "operators": [{"logs": [{
            "description": "x", "log_id": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "key": "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEKjpni/66DIYrSlGK6Rf+e6F2c/28ZUvDJ79N81+gyimAESAyeNZ++TRgjHWg9TVQnKHTSU0T1TtqDupFnSQTIg==",
            "state": {"usable": {"timestamp": "2025-01-01T00:00:00Z"}}}]}]}"#;
        assert!(LogList::parse(json).unwrap_err().contains("doesn't match"));
    }
}
