//! TLS client setup: post-quantum key exchange (NET-17), required
//! Certificate Transparency (NET-15) and per-site session state (STATE-03).

use std::sync::{Arc, Mutex};

use rustls::client::{Resumption, WebPkiServerVerifier};
use rustls::{ClientConfig, RootCertStore};

use crate::ct::{self, CtVerifier};
use crate::pq;

/// First-party sites that keep TLS session state. The least recently used
/// site's state is dropped beyond this, so a page can't grow it forever.
const MAX_SITES: usize = 64;
/// Resumable sessions kept per site (rustls's default is 256).
const SESSIONS_PER_SITE: usize = 32;
/// Chrome stops enforcing CT with a list this old; Hollow keeps enforcing
/// but says so, because new logs will be missing.
const STALE_LOG_LIST: u64 = 70 * 24 * 60 * 60;

/// The configuration shared by every site. Its own resumption store is
/// disabled: only [`SiteConfigs::for_site`] copies ever connect.
pub fn client_config() -> ClientConfig {
    let provider = Arc::new(pq::provider());
    // Bundled roots, never the Windows store: every Hollow user trusts
    // exactly the same CAs.
    let anchors = webpki_roots::TLS_SERVER_ROOTS.to_vec();
    let roots = Arc::new(RootCertStore { roots: anchors.clone() });
    let webpki = WebPkiServerVerifier::builder_with_provider(roots, Arc::clone(&provider))
        .build()
        .expect("bundled roots are valid");

    let logs = ct::logs::bundled();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if now.saturating_sub(logs.published) > STALE_LOG_LIST {
        tracing::warn!("the bundled CT log list is over 70 days old; certificates from newer logs will be refused");
    }

    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(CtVerifier::new(webpki, anchors, logs)))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.resumption = Resumption::disabled();
    config
}

/// One client configuration per first-party site. rustls keeps resumption
/// tickets per `ClientConfig`, keyed only by server name; with a single
/// config, a tracker embedded on two sites could resume one TLS session from
/// both and link them. Everything except the session store is shared.
pub struct SiteConfigs {
    base: ClientConfig,
    /// Most recently used last.
    sites: Mutex<Vec<(String, Arc<ClientConfig>)>>,
}

impl SiteConfigs {
    pub fn new(base: ClientConfig) -> Self {
        Self { base, sites: Mutex::new(Vec::new()) }
    }

    pub fn for_site(&self, isolation_key: &str) -> Arc<ClientConfig> {
        let mut sites = self.sites.lock().unwrap_or_else(|e| e.into_inner());
        let entry = match sites.iter().position(|(key, _)| key == isolation_key) {
            Some(i) => sites.remove(i),
            None => {
                let mut config = self.base.clone();
                config.resumption = Resumption::in_memory_sessions(SESSIONS_PER_SITE);
                (isolation_key.to_owned(), Arc::new(config))
            }
        };
        let config = Arc::clone(&entry.1);
        sites.push(entry);
        if sites.len() > MAX_SITES {
            sites.remove(0);
        }
        config
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.sites.lock().unwrap().len()
    }
}

/// In-process TLS server and client for tests.
#[cfg(test)]
pub mod testing {
    use std::sync::Arc;

    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::server::ServerConfig;
    use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConnection};

    /// The test server's name; `testdata/tls` has its certificate.
    pub const SERVER_NAME: &str = "hollow.test";

    /// Trusts only the test CA. No CT: the test CA logs nothing.
    pub fn client_config(provider: CryptoProvider) -> ClientConfig {
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(&include_bytes!("../testdata/tls/ca.der")[..])).unwrap();
        ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth()
    }

    pub fn server_config(provider: CryptoProvider) -> Arc<ServerConfig> {
        let cert = CertificateDer::from(&include_bytes!("../testdata/tls/server.der")[..]).into_owned();
        let key = PrivatePkcs8KeyDer::from(&include_bytes!("../testdata/tls/server.key.der")[..]).clone_key();
        let config = ServerConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], PrivateKeyDer::Pkcs8(key))
            .unwrap();
        Arc::new(config)
    }

    /// Runs a full handshake in memory, including the session tickets the
    /// server sends afterwards, and returns the client side.
    pub fn handshake(client: impl Into<Arc<ClientConfig>>, server: Arc<ServerConfig>) -> ClientConnection {
        let name = SERVER_NAME.try_into().unwrap();
        let mut client = ClientConnection::new(client.into(), name).unwrap();
        let mut server = ServerConnection::new(server).unwrap();
        loop {
            let mut moved = false;
            while client.wants_write() {
                let mut buf = Vec::new();
                client.write_tls(&mut buf).unwrap();
                server.read_tls(&mut &buf[..]).unwrap();
                server.process_new_packets().unwrap();
                moved = true;
            }
            while server.wants_write() {
                let mut buf = Vec::new();
                server.write_tls(&mut buf).unwrap();
                client.read_tls(&mut &buf[..]).unwrap();
                client.process_new_packets().unwrap();
                moved = true;
            }
            if !moved {
                break;
            }
        }
        assert!(!client.is_handshaking() && !server.is_handshaking());
        client
    }
}

#[cfg(test)]
mod tests {
    use rustls::HandshakeKind;

    use super::*;

    fn sites() -> SiteConfigs {
        SiteConfigs::new(testing::client_config(pq::provider()))
    }

    fn server() -> Arc<rustls::server::ServerConfig> {
        testing::server_config(rustls::crypto::ring::default_provider())
    }

    #[test]
    fn sites_dont_share_sessions() {
        let (sites, server) = (sites(), server());
        let kind = |site: &str| testing::handshake(sites.for_site(site), Arc::clone(&server)).handshake_kind();

        assert_eq!(kind("a.example"), Some(HandshakeKind::Full));
        // The same site resumes its own session...
        assert_eq!(kind("a.example"), Some(HandshakeKind::Resumed));
        // ...but another first party embedding the same server can't.
        assert_eq!(kind("b.example"), Some(HandshakeKind::Full));
        assert_eq!(kind("b.example"), Some(HandshakeKind::Resumed));
        assert_eq!(kind("a.example"), Some(HandshakeKind::Resumed));
    }

    #[test]
    fn site_count_is_bounded() {
        let (sites, server) = (sites(), server());
        let first = sites.for_site("site0.example");
        testing::handshake(Arc::clone(&first), Arc::clone(&server));
        for i in 1..=MAX_SITES {
            sites.for_site(&format!("site{i}.example"));
        }
        assert_eq!(sites.len(), MAX_SITES);
        // The oldest site was dropped and starts over without its tickets.
        let again = sites.for_site("site0.example");
        assert!(!Arc::ptr_eq(&first, &again));
        assert_eq!(testing::handshake(again, server).handshake_kind(), Some(HandshakeKind::Full));
    }

    #[test]
    fn recently_used_sites_stay() {
        let sites = sites();
        let first = sites.for_site("keep.example");
        for i in 0..MAX_SITES * 2 {
            sites.for_site("keep.example");
            sites.for_site(&format!("site{i}.example"));
        }
        assert!(Arc::ptr_eq(&first, &sites.for_site("keep.example")));
    }

    #[test]
    fn real_config_offers_post_quantum_first() {
        let config = client_config();
        assert_eq!(config.crypto_provider().kx_groups[0].name(), rustls::NamedGroup::X25519MLKEM768);
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }
}
