//! hollow-net: Certificate Transparency checks on server certificates,
//! which parse DER and the embedded SCT list before anything vouches for
//! the certificate.

use std::ops::Range;
use std::sync::OnceLock;

use hollow_net::ct;

/// A real leaf certificate (www.torproject.org, from
/// crates/hollow-net/testdata/ct) to splice fuzzed SCT lists into.
const TEMPLATE: &[u8] = include_bytes!("../data/torproject-leaf.der");

/// The time the CT tests use, so results never depend on the clock.
const NOW: u64 = 1_792_843_200;

/// First byte of an input whose rest is an SCT list rather than a whole
/// certificate.
pub const SCT_MODE: u8 = 1;

/// 1.3.6.1.4.1.11129.2.4.2 as a DER OID, tag and length included.
const SCT_LIST_OID: &[u8] = &[0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0xd6, 0x79, 0x02, 0x04, 0x02];

/// Byte 0 picks the mode. Odd: the rest is a TLS-encoded SCT list, put into
/// the template certificate in place of its own (so the fuzzer works on
/// SCTs, not on getting DER right). Even: the rest is a whole certificate.
pub fn ct(data: &[u8]) {
    let Some((&mode, rest)) = data.split_first() else { return };
    let issuers = [[0u8; 32], [0x5a; 32]];
    if mode & 1 == SCT_MODE {
        let range = template_sct_range();
        if let Some(cert) = splice(TEMPLATE, range, rest) {
            let _ = ct::check(&cert, &issuers, ct::logs::bundled(), NOW);
        }
    } else {
        let _ = ct::check(rest, &issuers, ct::logs::bundled(), NOW);
    }
}

/// The SCT list embedded in a certificate (for seeds), if it has one.
pub fn sct_list_of(cert: &[u8]) -> Option<&[u8]> {
    sct_range(cert).map(|r| &cert[r])
}

fn template_sct_range() -> Range<usize> {
    static RANGE: OnceLock<Range<usize>> = OnceLock::new();
    RANGE.get_or_init(|| sct_range(TEMPLATE).expect("template has an SCT list")).clone()
}

/// Where the SCT list sits: after the extension's OID comes the extnValue
/// OCTET STRING, which holds another OCTET STRING, the list.
fn sct_range(cert: &[u8]) -> Option<Range<usize>> {
    let oid = cert.windows(SCT_LIST_OID.len()).position(|w| w == SCT_LIST_OID)?;
    let outer = oid + SCT_LIST_OID.len();
    let (h1, _) = header(cert.get(outer..)?)?;
    let inner = outer + h1;
    let (h2, len) = header(cert.get(inner..)?)?;
    let start = inner + h2;
    (start + len <= cert.len()).then_some(start..start + len)
}

/// A DER header with a one-byte tag (all X.509 uses): header length and
/// content length.
fn header(b: &[u8]) -> Option<(usize, usize)> {
    let first = *b.get(1)?;
    if first < 0x80 {
        return Some((2, usize::from(first)));
    }
    let n = usize::from(first & 0x7f);
    if n == 0 || n > 4 {
        return None;
    }
    let len = b.get(2..2 + n)?.iter().fold(0usize, |a, &x| a << 8 | usize::from(x));
    Some((2 + n, len))
}

fn push_len(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
}

/// `der` with the bytes at `range` replaced by `new`, every enclosing
/// length fixed up. `range` must be exactly the contents of a TLV nested in
/// `der` through TLVs whose contents are themselves DER.
fn splice(der: &[u8], range: Range<usize>, new: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(der.len() + new.len());
    let mut pos = 0;
    while pos < der.len() {
        let (h, len) = header(&der[pos..])?;
        let (start, end) = (pos + h, pos + h + len);
        if end > der.len() {
            return None;
        }
        if range.start >= start && range.end <= end {
            let contents = if range == (start..end) {
                new.to_vec()
            } else {
                splice(&der[start..end], range.start - start..range.end - start, new)?
            };
            out.push(der[pos]);
            push_len(contents.len(), &mut out);
            out.extend_from_slice(&contents);
        } else {
            out.extend_from_slice(&der[pos..end]);
        }
        pos = end;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splicing_the_same_list_back_is_the_identity() {
        let range = template_sct_range();
        let list = TEMPLATE[range.clone()].to_vec();
        assert_eq!(splice(TEMPLATE, range.clone(), &list).as_deref(), Some(TEMPLATE));
        // A different list still makes a certificate CT can parse: an empty
        // list (a zero u16 length) has no valid SCTs, which is not Malformed.
        let cert = splice(TEMPLATE, range, &[0, 0]).unwrap();
        assert_eq!(sct_list_of(&cert), Some(&[0u8, 0][..]));
        let result = ct::check(&cert, &[], ct::logs::bundled(), NOW);
        assert!(matches!(result, Err(ct::CtError::TooFewScts { valid: 0, .. })), "{result:?}");
    }
}
