//! Just enough DER to take certificates apart and put one back together
//! byte for byte. The `der` crate does the strict decoding.

use der::{Decode, Encode, Header, Length, Reader, SliceReader, Tag};

/// One DER element: its tag, the whole encoding and the contents.
#[derive(Clone, Copy, Debug)]
pub struct Tlv<'a> {
    pub tag: Tag,
    pub whole: &'a [u8],
    pub contents: &'a [u8],
}

fn read<'a>(reader: &mut SliceReader<'a>) -> der::Result<Tlv<'a>> {
    let whole = reader.tlv_bytes()?;
    let mut inner = SliceReader::new(whole)?;
    let header = Header::decode(&mut inner)?;
    let contents = inner.read_slice(header.length)?;
    Ok(Tlv { tag: header.tag, whole, contents })
}

/// Exactly one element, nothing after it.
pub fn single(bytes: &[u8]) -> der::Result<Tlv<'_>> {
    let mut reader = SliceReader::new(bytes)?;
    let tlv = read(&mut reader)?;
    reader.finish(tlv)
}

/// The elements one after another, e.g. the contents of a SEQUENCE.
pub fn elements(bytes: &[u8]) -> der::Result<Vec<Tlv<'_>>> {
    let mut reader = SliceReader::new(bytes)?;
    let mut out = Vec::new();
    while !reader.is_finished() {
        out.push(read(&mut reader)?);
    }
    Ok(out)
}

/// Encodes `tag` around `contents`.
pub fn wrap(tag: Tag, contents: &[u8]) -> der::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(contents.len() + 6);
    Header::new(tag, Length::try_from(contents.len())?)?.encode_to_vec(&mut out)?;
    out.extend_from_slice(contents);
    Ok(out)
}

/// Seconds since the Unix epoch of a `UTCTime` or `GeneralizedTime`.
pub fn time(tlv: &Tlv<'_>) -> der::Result<u64> {
    let since_epoch = match tlv.tag {
        Tag::UtcTime => der::asn1::UtcTime::from_der(tlv.whole)?.to_unix_duration(),
        _ => der::asn1::GeneralizedTime::from_der(tlv.whole)?.to_unix_duration(),
    };
    Ok(since_epoch.as_secs())
}
