//! Request headers sent with every fetch.
//!
//! These approximate Tor Browser (Firefox ESR on Windows). Phase 5 (FP-04)
//! replaces them with an exact, CI-tested match of Tor Browser's "Safest"
//! level, including header order, TLS ClientHello and HTTP/2 settings.

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; rv:140.0) Gecko/20100101 Firefox/140.0";
pub const ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
pub const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.5";
/// Known difference: Tor Browser sends `gzip, deflate, br, zstd`, but
/// Phase 1 has no decoders yet.
pub const ACCEPT_ENCODING: &str = "identity";
