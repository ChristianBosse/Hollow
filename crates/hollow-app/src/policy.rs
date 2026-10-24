//! Which URLs Hollow will fetch, and how they map to network requests.

use std::net::{Ipv4Addr, Ipv6Addr};

use hollow_ipc::FetchRequest;
use url::{Host, Position, Url};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("not a valid address: {0}")]
    Invalid(String),
    #[error("{0}:// is not allowed; Hollow only loads https:// (and http:// for .onion sites)")]
    Scheme(String),
    #[error("plain http:// is only allowed for .onion sites (HTTPS-only, NET-15)")]
    InsecureHttp,
    #[error("addresses with a user name or password are not allowed")]
    Credentials,
    #[error("local and private addresses can't be reached over Tor")]
    LocalAddress,
}

/// Parses what the user typed. A bare `example.org` becomes `https://example.org/`.
pub fn parse_input(input: &str) -> Result<Url, PolicyError> {
    let input = input.trim();
    let url = match Url::parse(input) {
        Ok(url) if url.has_host() || url.scheme() != "http" && url.scheme() != "https" => url,
        _ => Url::parse(&format!("https://{input}")).map_err(|e| PolicyError::Invalid(e.to_string()))?,
    };
    check(&url)?;
    Ok(url)
}

/// Every URL the broker fetches, including redirect targets, goes through this.
pub fn check(url: &Url) -> Result<(), PolicyError> {
    let onion = is_onion(url);
    match url.scheme() {
        "https" => {}
        "http" if onion => {}
        "http" => return Err(PolicyError::InsecureHttp),
        other => return Err(PolicyError::Scheme(other.to_owned())),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PolicyError::Credentials);
    }
    match url.host() {
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.');
            if d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local") || d.ends_with(".internal") {
                return Err(PolicyError::LocalAddress);
            }
        }
        Some(Host::Ipv4(ip)) if is_local_v4(ip) => return Err(PolicyError::LocalAddress),
        Some(Host::Ipv6(ip)) if is_local_v6(ip) => return Err(PolicyError::LocalAddress),
        Some(_) => {}
        None => return Err(PolicyError::Invalid("missing host".into())),
    }
    Ok(())
}

pub fn is_onion(url: &Url) -> bool {
    matches!(url.host(), Some(Host::Domain(d)) if d.trim_end_matches('.').ends_with(".onion"))
}

/// The first-party key that decides which Tor circuit and connection state a
/// request uses. Phase 1 uses the host; Phase 2 switches to the registrable
/// domain (Public Suffix List) so `a.example.org` and `b.example.org` share.
pub fn isolation_key(url: &Url) -> String {
    url.host_str().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase()
}

/// Builds the request the network process will execute, on the circuit of
/// `first_party` (the site in the address bar). `url` must have passed
/// [`check`].
pub fn to_request(url: &Url, id: u64, first_party: &str) -> FetchRequest {
    FetchRequest {
        id,
        // The url crate has already converted internationalized names to
        // punycode, which is also what the user is shown (UX-04).
        host: url.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_owned(),
        port: url.port_or_known_default().unwrap_or(443),
        // Fragments are never sent.
        path_and_query: url[Position::BeforePath..Position::AfterQuery].to_owned(),
        tls: url.scheme() == "https",
        isolation_key: first_party.to_owned(),
    }
}

fn is_local_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64 // CGNAT 100.64/10
}

fn is_local_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback() || ip.is_unspecified() || ip.is_multicast()
        || (ip.segments()[0] & 0xfe00) == 0xfc00 // unique local
        || (ip.segments()[0] & 0xffc0) == 0xfe80 // link local
        || ip.to_ipv4_mapped().is_some_and(is_local_v4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https_and_onion_http() {
        assert!(parse_input("https://example.org/").is_ok());
        assert_eq!(parse_input("example.org").unwrap().as_str(), "https://example.org/");
        let onion = "http://2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion/";
        assert!(parse_input(onion).is_ok());
    }

    #[test]
    fn rejects_dangerous_urls() {
        assert_eq!(parse_input("http://example.org/"), Err(PolicyError::InsecureHttp));
        assert_eq!(parse_input("file:///C:/Windows/win.ini"), Err(PolicyError::Scheme("file".into())));
        assert_eq!(parse_input("ftp://example.org/"), Err(PolicyError::Scheme("ftp".into())));
        assert_eq!(parse_input("https://user:pw@example.org/"), Err(PolicyError::Credentials));
        assert_eq!(parse_input("https://localhost/"), Err(PolicyError::LocalAddress));
        assert_eq!(parse_input("https://192.168.1.1/"), Err(PolicyError::LocalAddress));
        assert_eq!(parse_input("https://[::1]/"), Err(PolicyError::LocalAddress));
        assert_eq!(parse_input("https://[::ffff:10.0.0.1]/"), Err(PolicyError::LocalAddress));
        assert_eq!(parse_input("https://100.64.0.1/"), Err(PolicyError::LocalAddress));
    }

    #[test]
    fn request_drops_fragment_and_punycodes_host() {
        let url = parse_input("https://bücher.example/a/b?q=1#secret").unwrap();
        let req = to_request(&url, 1, &isolation_key(&url));
        assert_eq!(req.host, "xn--bcher-kva.example");
        assert_eq!(req.path_and_query, "/a/b?q=1");
        assert_eq!(req.port, 443);
        assert!(req.tls);
        assert_eq!(req.isolation_key, "xn--bcher-kva.example");
    }
}
