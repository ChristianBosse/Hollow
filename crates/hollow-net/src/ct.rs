//! Certificate Transparency is required (NET-15).
//!
//! A certificate counts only if public CT logs saw it, so a CA that is
//! hacked or coerced into issuing a certificate for someone else's site
//! (say, for a malicious exit relay to use) can't do it in secret. The rules
//! are Chrome's CT policy:
//!
//! - at least 2 valid SCTs from distinct logs, 3 if the certificate is valid
//!   for more than 180 days; a retired log's SCT counts only if it was issued
//!   before the log retired;
//! - at least one of them from a log that is still qualified, usable or
//!   read-only;
//! - logs from at least 2 distinct operators.
//!
//! Only SCTs embedded in the certificate are checked: rustls 0.22+ no longer
//! requests the TLS `signed_certificate_timestamp` extension, and SCTs in
//! stapled OCSP responses are rare. Every major public CA embeds SCTs, as
//! Chrome and Safari require. `.onion` sites are exempt: the address is the
//! key, so there is no CA to keep honest.

mod asn1;
pub mod logs;

use std::fmt;
use std::sync::Arc;

use der::Tag;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, TrustAnchor, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, OtherError, SignatureScheme};

use self::asn1::Tlv;
use self::logs::{Log, LogKey, LogList, LogState};

/// Chrome's line between short- and long-lived certificates.
const SHORT_LIVED: u64 = 180 * 24 * 60 * 60;

/// 1.3.6.1.4.1.11129.2.4.2, the embedded SCT list.
const SCT_LIST_OID: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0xd6, 0x79, 0x02, 0x04, 0x02];

/// Why a certificate failed the CT check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtError {
    /// The certificate has no SCT list extension.
    NoScts,
    /// The certificate or its SCT list couldn't be parsed.
    Malformed,
    /// Too few SCTs verified against known logs.
    TooFewScts { valid: usize, required: usize },
    /// Every valid SCT is from a log that has since retired.
    NoCurrentLog,
    /// Every valid SCT comes from the same log operator.
    OneOperator,
}

impl fmt::Display for CtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("certificate is not in Certificate Transparency logs (")?;
        match self {
            CtError::NoScts => f.write_str("it carries no SCTs")?,
            CtError::Malformed => f.write_str("malformed certificate or SCT list")?,
            CtError::TooFewScts { valid, required } => {
                write!(f, "{valid} valid SCTs from known logs, {required} required")?
            }
            CtError::NoCurrentLog => f.write_str("all SCTs are from retired logs")?,
            CtError::OneOperator => f.write_str("all SCTs are from one log operator")?,
        }
        f.write_str(")")
    }
}

impl std::error::Error for CtError {}

/// Is CT checked for this host?
pub fn required_for(server_name: &ServerName<'_>) -> bool {
    match server_name {
        ServerName::DnsName(name) => {
            let name = name.as_ref().trim_end_matches('.');
            !(name.len() > 6 && name[name.len() - 6..].eq_ignore_ascii_case(".onion"))
        }
        _ => true,
    }
}

/// webpki's checks, then the CT policy.
#[derive(Debug)]
pub struct CtVerifier {
    inner: Arc<WebPkiServerVerifier>,
    roots: Vec<TrustAnchor<'static>>,
    logs: &'static LogList,
}

impl CtVerifier {
    pub fn new(inner: Arc<WebPkiServerVerifier>, roots: Vec<TrustAnchor<'static>>, logs: &'static LogList) -> Self {
        Self { inner, roots, logs }
    }
}

impl ServerCertVerifier for CtVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verified = self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;
        if required_for(server_name) {
            let issuers = issuer_keys(end_entity, intermediates, &self.roots);
            check(end_entity, &issuers, self.logs, now.as_secs())
                .map_err(|e| rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(e)))))?;
        }
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The parts of a certificate CT needs.
struct Cert<'a> {
    /// The fields of `TBSCertificate`, in order.
    tbs_fields: Vec<Tlv<'a>>,
    tbs_tag: Tag,
    issuer: Tlv<'a>,
    subject: Tlv<'a>,
    spki: Tlv<'a>,
    not_before: u64,
    not_after: u64,
}

impl<'a> Cert<'a> {
    fn parse(der: &'a [u8]) -> Option<Self> {
        let cert = asn1::single(der).ok()?;
        let tbs = *asn1::elements(cert.contents).ok()?.first()?;
        let tbs_fields = asn1::elements(tbs.contents).ok()?;
        // An explicit [0] version comes first in every v3 certificate.
        let skip = usize::from(tbs_fields.first()?.tag.is_context_specific());
        let [_serial, _signature, issuer, validity, subject, spki] = *tbs_fields.get(skip..skip + 6)? else {
            return None;
        };
        let validity = asn1::elements(validity.contents).ok()?;
        let [not_before, not_after] = validity[..] else { return None };
        Some(Self {
            tbs_tag: tbs.tag,
            issuer,
            subject,
            spki,
            not_before: asn1::time(&not_before).ok()?,
            not_after: asn1::time(&not_after).ok()?,
            tbs_fields,
        })
    }

    /// The extensions inside the `[3]` wrapper, if any.
    fn extensions(&self) -> Option<(usize, Vec<Tlv<'a>>)> {
        let (index, wrapper) = self
            .tbs_fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.tag == Tag::ContextSpecific { constructed: true, number: der::TagNumber::N3 })?;
        let list = asn1::single(wrapper.contents).ok()?;
        Some((index, asn1::elements(list.contents).ok()?))
    }

    /// The TLS-encoded SCT list and the `TBSCertificate` without it: RFC
    /// 6962's precertificate TBS, which is what the logs signed.
    fn split_scts(&self) -> Result<(&'a [u8], Vec<u8>), CtError> {
        let (index, extensions) = self.extensions().ok_or(CtError::NoScts)?;
        let mut scts = None;
        let mut kept = Vec::new();
        for ext in &extensions {
            let parts = asn1::elements(ext.contents).map_err(|_| CtError::Malformed)?;
            if parts.first().is_some_and(|oid| oid.contents == SCT_LIST_OID) {
                // extnValue is an OCTET STRING holding an OCTET STRING.
                let value = parts.last().ok_or(CtError::Malformed)?;
                let inner = asn1::single(value.contents).map_err(|_| CtError::Malformed)?;
                if scts.is_some() || inner.tag != Tag::OctetString {
                    return Err(CtError::Malformed);
                }
                scts = Some(inner.contents);
            } else {
                kept.extend_from_slice(ext.whole);
            }
        }
        let scts = scts.ok_or(CtError::NoScts)?;

        let rebuild = || -> der::Result<Vec<u8>> {
            let wrapper = &self.tbs_fields[index];
            let wrapper = asn1::wrap(wrapper.tag, &asn1::wrap(Tag::Sequence, &kept)?)?;
            let mut fields = Vec::new();
            for (i, field) in self.tbs_fields.iter().enumerate() {
                fields.extend_from_slice(if i == index { &wrapper } else { field.whole });
            }
            asn1::wrap(self.tbs_tag, &fields)
        };
        Ok((scts, rebuild().map_err(|_| CtError::Malformed)?))
    }
}

/// SHA-256 hashes of the keys that may have issued `end_entity`: chain
/// certificates and roots whose subject is its issuer. Normally exactly one.
fn issuer_keys(
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    roots: &[TrustAnchor<'_>],
) -> Vec<[u8; 32]> {
    let Some(leaf) = Cert::parse(end_entity) else { return Vec::new() };
    let mut keys = Vec::new();
    for cert in intermediates.iter().filter_map(|c| Cert::parse(c)) {
        if cert.subject.whole == leaf.issuer.whole {
            keys.push(sha256(cert.spki.whole));
        }
    }
    // Trust anchors keep the contents of `subject` and `subjectPublicKeyInfo`
    // without their SEQUENCE headers.
    for root in roots.iter().filter(|r| r.subject.as_ref() == leaf.issuer.contents) {
        if let Ok(spki) = asn1::wrap(Tag::Sequence, root.subject_public_key_info.as_ref()) {
            keys.push(sha256(&spki));
        }
    }
    keys
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0; 32];
    out.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, data).as_ref());
    out
}

/// One parsed `SignedCertificateTimestamp` (RFC 6962 §3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Sct<'a> {
    log_id: [u8; 32],
    /// Milliseconds since the Unix epoch.
    timestamp: u64,
    extensions: &'a [u8],
    hash: u8,
    signature_alg: u8,
    signature: &'a [u8],
}

/// A TLS-style cursor.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<usize> {
        let b = self.take(2)?;
        Some(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    /// A `u16`-length-prefixed vector.
    fn vec16(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()?;
        self.take(n)
    }
}

/// The SCTs of a `SignedCertificateTimestampList`. SCTs of an unknown
/// version are skipped, as RFC 6962 says.
fn parse_scts(list: &[u8]) -> Option<Vec<Sct<'_>>> {
    let mut outer = Bytes(list);
    let mut items = Bytes(outer.vec16()?);
    if !outer.0.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    while !items.0.is_empty() {
        let mut sct = Bytes(items.vec16()?);
        if sct.u8()? != 0 {
            continue;
        }
        let parsed = Sct {
            log_id: sct.take(32)?.try_into().ok()?,
            timestamp: sct.u64()?,
            extensions: sct.vec16()?,
            hash: sct.u8()?,
            signature_alg: sct.u8()?,
            signature: sct.vec16()?,
        };
        if !sct.0.is_empty() {
            return None;
        }
        out.push(parsed);
    }
    Some(out)
}

/// Checks an SCT's signature over a precertificate entry.
fn sct_signature_valid(sct: &Sct<'_>, log: &Log, issuer_key_hash: &[u8; 32], tbs: &[u8]) -> bool {
    let Ok(tbs_len) = u32::try_from(tbs.len()) else { return false };
    let Ok(ext_len) = u16::try_from(sct.extensions.len()) else { return false };
    let mut signed = Vec::with_capacity(tbs.len() + sct.extensions.len() + 50);
    signed.push(0); // sct_version v1
    signed.push(0); // signature_type certificate_timestamp
    signed.extend_from_slice(&sct.timestamp.to_be_bytes());
    signed.extend_from_slice(&1u16.to_be_bytes()); // entry_type precert_entry
    signed.extend_from_slice(issuer_key_hash);
    signed.extend_from_slice(&tbs_len.to_be_bytes()[1..]);
    signed.extend_from_slice(tbs);
    signed.extend_from_slice(&ext_len.to_be_bytes());
    signed.extend_from_slice(sct.extensions);

    use ring::signature::{ECDSA_P256_SHA256_ASN1, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    // TLS hash 4 = SHA-256; signature 3 = ECDSA, 1 = RSA.
    match (&log.key, sct.hash, sct.signature_alg) {
        (LogKey::EcdsaP256(key), 4, 3) => {
            UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, key).verify(&signed, sct.signature).is_ok()
        }
        (LogKey::Rsa(key), 4, 1) => {
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key).verify(&signed, sct.signature).is_ok()
        }
        _ => false,
    }
}

/// Does the log's SCT count at `now`, and is the log still current?
fn log_standing(log: &Log, sct_timestamp_ms: u64, now: u64) -> Option<bool> {
    match log.state {
        LogState::Qualified { .. } | LogState::Usable { .. } | LogState::ReadOnly { .. } => Some(true),
        LogState::Retired { since } if now < since => Some(true),
        LogState::Retired { since } => (sct_timestamp_ms / 1000 < since).then_some(false),
        LogState::Pending | LogState::Rejected { .. } => None,
    }
}

/// Applies the CT policy to `end_entity` at `now` (seconds since the Unix
/// epoch). `issuers` are candidate SHA-256 hashes of the issuer's key.
pub fn check(end_entity: &[u8], issuers: &[[u8; 32]], logs: &LogList, now: u64) -> Result<(), CtError> {
    let cert = Cert::parse(end_entity).ok_or(CtError::Malformed)?;
    let (list, tbs) = cert.split_scts()?;
    let scts = parse_scts(list).ok_or(CtError::Malformed)?;

    // (log, still current) for each distinct log with a valid SCT.
    let mut valid: Vec<(&Log, bool)> = Vec::new();
    for sct in &scts {
        let Some(log) = logs.get(&sct.log_id) else { continue };
        // Chrome ignores SCTs from the future.
        if valid.iter().any(|(l, _)| l.id == log.id) || sct.timestamp / 1000 > now {
            continue;
        }
        let Some(current) = log_standing(log, sct.timestamp, now) else { continue };
        if issuers.iter().any(|issuer| sct_signature_valid(sct, log, issuer, &tbs)) {
            tracing::trace!("valid SCT from {}", log.description);
            valid.push((log, current));
        }
    }

    let lifetime = cert.not_after.saturating_sub(cert.not_before);
    let required = if lifetime > SHORT_LIVED { 3 } else { 2 };
    if valid.len() < required {
        return Err(CtError::TooFewScts { valid: valid.len(), required });
    }
    if !valid.iter().any(|(_, current)| *current) {
        return Err(CtError::NoCurrentLog);
    }
    if valid.iter().all(|(log, _)| log.operator == valid[0].0.operator) {
        return Err(CtError::OneOperator);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rustls::pki_types::pem::PemObject;

    use super::*;

    /// 2026-10-24T12:00:00Z: all fixture chains are valid, all their logs usable.
    const NOW: u64 = 1_792_843_200;

    fn chain(pem: &[u8]) -> Vec<CertificateDer<'static>> {
        CertificateDer::pem_slice_iter(pem).collect::<Result<_, _>>().unwrap()
    }

    fn torproject() -> Vec<CertificateDer<'static>> {
        chain(include_bytes!("../testdata/ct/torproject.pem"))
    }

    fn wikipedia() -> Vec<CertificateDer<'static>> {
        chain(include_bytes!("../testdata/ct/wikipedia.pem"))
    }

    fn microsoft() -> Vec<CertificateDer<'static>> {
        chain(include_bytes!("../testdata/ct/microsoft.pem"))
    }

    fn roots() -> Vec<TrustAnchor<'static>> {
        webpki_roots::TLS_SERVER_ROOTS.to_vec()
    }

    fn check_chain(chain: &[CertificateDer<'_>], logs: &LogList, now: u64) -> Result<(), CtError> {
        check(&chain[0], &issuer_keys(&chain[0], &chain[1..], &roots()), logs, now)
    }

    fn verifier() -> CtVerifier {
        let roots = Arc::new(rustls::RootCertStore { roots: roots() });
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let inner = WebPkiServerVerifier::builder_with_provider(roots, provider).build().unwrap();
        CtVerifier::new(inner, self::roots(), logs::bundled())
    }

    /// The SCTs of a chain's leaf, as parsed.
    fn scts_of(cert: &[u8]) -> Vec<Sct<'_>> {
        let cert = Cert::parse(cert).unwrap();
        let (list, _) = cert.split_scts().unwrap();
        parse_scts(list).unwrap()
    }

    #[test]
    fn real_chains_pass() {
        for (name, chain) in [("torproject", torproject()), ("wikipedia", wikipedia()), ("microsoft", microsoft())] {
            assert_eq!(check_chain(&chain, logs::bundled(), NOW), Ok(()), "{name}");
        }
    }

    #[test]
    fn full_verifier_passes_real_chains() {
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(NOW));
        for (host, chain) in [("www.torproject.org", torproject()), ("en.wikipedia.org", wikipedia())] {
            let name = ServerName::try_from(host).unwrap();
            verifier().verify_server_cert(&chain[0], &chain[1..], &name, &[], now).expect(host);
        }
    }

    #[test]
    fn tampered_sct_signature_fails() {
        let chain = torproject();
        let mut leaf = chain[0].to_vec();
        // Flip a byte inside the first SCT's signature.
        let signature = scts_of(&chain[0])[0].signature;
        let offset = signature.as_ptr() as usize - chain[0].as_ptr() as usize + signature.len() / 2;
        leaf[offset] ^= 1;
        let issuers = issuer_keys(&chain[0], &chain[1..], &roots());
        let err = check(&leaf, &issuers, logs::bundled(), NOW).unwrap_err();
        assert_eq!(err, CtError::TooFewScts { valid: 1, required: 2 });
        assert!(err.to_string().starts_with("certificate is not in Certificate Transparency logs"));
    }

    #[test]
    fn wrong_issuer_key_fails() {
        let chain = torproject();
        let err = check(&chain[0], &[[0; 32]], logs::bundled(), NOW).unwrap_err();
        assert_eq!(err, CtError::TooFewScts { valid: 0, required: 2 });
    }

    #[test]
    fn long_lived_needs_three() {
        // microsoft.com: valid ~360 days, 3 SCTs. Without its first log only
        // 2 remain, which isn't enough.
        let chain = microsoft();
        let first = scts_of(&chain[0])[0].log_id;
        let mut list = LogList::parse(include_bytes!("../ct/log_list.json")).unwrap();
        list.logs.retain(|l| l.id != first);
        assert_eq!(check_chain(&chain, &list, NOW), Err(CtError::TooFewScts { valid: 2, required: 3 }));
    }

    #[test]
    fn unknown_logs_dont_count() {
        let mut list = LogList::parse(include_bytes!("../ct/log_list.json")).unwrap();
        list.logs.clear();
        assert_eq!(check_chain(&torproject(), &list, NOW), Err(CtError::TooFewScts { valid: 0, required: 2 }));
    }

    #[test]
    fn needs_two_operators() {
        let mut list = LogList::parse(include_bytes!("../ct/log_list.json")).unwrap();
        for log in &mut list.logs {
            log.operator = 0;
        }
        assert_eq!(check_chain(&torproject(), &list, NOW), Err(CtError::OneOperator));
    }

    #[test]
    fn retired_logs() {
        let chain = torproject();
        let scts = scts_of(&chain[0]);
        let sct_time = scts[0].timestamp / 1000;
        let retire = |since: u64| {
            let mut list = LogList::parse(include_bytes!("../ct/log_list.json")).unwrap();
            for log in list.logs.iter_mut().filter(|l| scts.iter().any(|s| s.log_id == l.id)) {
                log.state = LogState::Retired { since };
            }
            list
        };
        // Retired after the SCTs were issued: they count, but one log must
        // still be current.
        assert_eq!(check_chain(&chain, &retire(sct_time + 60), NOW), Err(CtError::NoCurrentLog));
        // Retired before: they don't count at all.
        assert_eq!(
            check_chain(&chain, &retire(sct_time - 60), NOW),
            Err(CtError::TooFewScts { valid: 0, required: 2 })
        );
        // Pending logs never count.
        let mut list = retire(0);
        list.logs.iter_mut().for_each(|l| l.state = LogState::Pending);
        assert_eq!(check_chain(&chain, &list, NOW), Err(CtError::TooFewScts { valid: 0, required: 2 }));
    }

    #[test]
    fn future_scts_dont_count() {
        let chain = torproject();
        let issued = scts_of(&chain[0])[0].timestamp / 1000;
        assert!(check_chain(&chain, logs::bundled(), issued - 3600).is_err());
    }

    #[test]
    fn certificate_without_scts_fails() {
        let ca = include_bytes!("../testdata/tls/ca.der");
        assert_eq!(check(ca, &[], logs::bundled(), NOW), Err(CtError::NoScts));
        assert_eq!(check(b"not a certificate", &[], logs::bundled(), NOW), Err(CtError::Malformed));
    }

    #[test]
    fn precert_tbs_drops_only_the_sct_extension() {
        let chain = wikipedia();
        let cert = Cert::parse(&chain[0]).unwrap();
        let (_, tbs) = cert.split_scts().unwrap();
        let fields = asn1::elements(asn1::single(&tbs).unwrap().contents).unwrap();
        assert_eq!(fields.len(), cert.tbs_fields.len());
        // Everything before the extensions is untouched.
        let last = fields.len() - 1;
        for (old, new) in cert.tbs_fields[..last].iter().zip(&fields[..last]) {
            assert_eq!(old.whole, new.whole);
        }
        let (_, before) = cert.extensions().unwrap();
        let after = asn1::elements(asn1::single(fields[last].contents).unwrap().contents).unwrap();
        let expected: Vec<_> = before.iter().filter(|e| !e.whole.windows(10).any(|w| w == SCT_LIST_OID)).collect();
        assert_eq!(after.len(), before.len() - 1);
        assert!(after.iter().zip(expected).all(|(a, b)| a.whole == b.whole));
    }

    #[test]
    fn issuer_found_among_roots() {
        // Microsoft's root cross-signed by DigiCert Global Root G2, which is
        // only in the bundled roots, not in the chain.
        let chain = microsoft();
        assert_eq!(issuer_keys(&chain[2], &[], &roots()).len(), 1);
        assert_eq!(
            issuer_keys(&chain[0], &chain[1..], &roots()),
            vec![sha256(Cert::parse(&chain[1]).unwrap().spki.whole)]
        );
    }

    #[test]
    fn onion_sites_exempt() {
        let name = |s: &'static str| ServerName::try_from(s).unwrap();
        assert!(!required_for(&name("duckduckgogg42xjoc72x3sjasowoarfbgcmvfimaftt6twagswzczad.onion")));
        assert!(!required_for(&name("www.example.ONION")));
        assert!(required_for(&name("onion.example.com")));
        assert!(required_for(&name("example.com")));
    }
}
