//! Post-quantum TLS key exchange (NET-17).
//!
//! X25519MLKEM768 (draft-ietf-tls-ecdhe-mlkem, group 0x11EC) combines
//! ML-KEM-768 with X25519, so a recorded handshake stays secret unless both
//! are broken. rustls only ships it with aws-lc-rs, which needs a C and
//! assembler toolchain, so this module builds it from RustCrypto's pure-Rust
//! `ml-kem` and ring's X25519. The layout follows the draft (and rustls's own
//! aws-lc-rs version): the ML-KEM part comes first everywhere.
//!
//! - client share: ML-KEM encapsulation key (1184) || X25519 public (32)
//! - server share: ML-KEM ciphertext (1088) || X25519 public (32)
//! - shared secret: ML-KEM secret (32) || X25519 secret (32)

use ml_kem::kem::{Decapsulate, KeyExport};
use ml_kem::{B32, DecapsulationKey, EncapsulationKey, MlKem768, Seed};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::crypto::{ActiveKeyExchange, CompletedKeyExchange, CryptoProvider, SharedSecret, SupportedKxGroup};
use rustls::ffdhe_groups::FfdheGroup;
use rustls::{Error, NamedGroup, PeerMisbehaved, ProtocolVersion};

/// The hybrid group. Offered first; its X25519 half is also offered on its
/// own (same key, see [`ActiveKeyExchange::hybrid_component`]), so servers
/// without ML-KEM still finish in one round trip.
pub static X25519MLKEM768: &dyn SupportedKxGroup = &X25519MlKem768;

const ENCAP_KEY_LEN: usize = 1184;
const CIPHERTEXT_LEN: usize = 1088;
const X25519_LEN: usize = 32;

const INVALID_KEY_SHARE: Error = Error::PeerMisbehaved(PeerMisbehaved::InvalidKeyShare);

/// ring's provider with X25519MLKEM768 in front of its classical groups.
pub fn provider() -> CryptoProvider {
    let mut provider = rustls::crypto::ring::default_provider();
    provider.kx_groups.insert(0, X25519MLKEM768);
    provider
}

fn classical() -> &'static dyn SupportedKxGroup {
    rustls::crypto::ring::kx_group::X25519
}

fn random<const N: usize>() -> Result<[u8; N], Error> {
    let mut buf = [0u8; N];
    SystemRandom::new().fill(&mut buf).map_err(|_| Error::FailedToGetRandomBytes)?;
    Ok(buf)
}

#[derive(Debug)]
struct X25519MlKem768;

impl SupportedKxGroup for X25519MlKem768 {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let x25519 = classical().start()?;
        let decaps = DecapsulationKey::<MlKem768>::from_seed(Seed::from(random::<64>()?));
        let mut share = decaps.encapsulation_key().to_bytes().to_vec();
        share.extend_from_slice(x25519.pub_key());
        Ok(Box::new(Active { decaps: Box::new(decaps), x25519, share }))
    }

    /// The server side: encapsulate to the client's ML-KEM key. Hollow never
    /// serves TLS; this is what the in-process test server runs.
    fn start_and_complete(&self, client_share: &[u8]) -> Result<CompletedKeyExchange, Error> {
        if client_share.len() != ENCAP_KEY_LEN + X25519_LEN {
            return Err(INVALID_KEY_SHARE);
        }
        let (pq_share, x25519_share) = client_share.split_at(ENCAP_KEY_LEN);
        let key = pq_share.try_into().map_err(|_| INVALID_KEY_SHARE)?;
        // Rejects keys that fail FIPS 203's modulus check.
        let encaps = EncapsulationKey::<MlKem768>::new(&key).map_err(|_| INVALID_KEY_SHARE)?;
        let (ciphertext, pq_secret) = encaps.encapsulate_deterministic(&B32::from(random::<32>()?));
        let x25519 = classical().start_and_complete(x25519_share)?;

        let mut pub_key = ciphertext.to_vec();
        pub_key.extend_from_slice(&x25519.pub_key);
        let mut secret = pq_secret.to_vec();
        secret.extend_from_slice(x25519.secret.secret_bytes());
        Ok(CompletedKeyExchange { group: self.name(), pub_key, secret: SharedSecret::from(secret) })
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        None
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::X25519MLKEM768
    }

    fn usable_for_version(&self, version: ProtocolVersion) -> bool {
        version == ProtocolVersion::TLSv1_3
    }
}

struct Active {
    decaps: Box<DecapsulationKey<MlKem768>>,
    x25519: Box<dyn ActiveKeyExchange>,
    share: Vec<u8>,
}

impl ActiveKeyExchange for Active {
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        if peer_pub_key.len() != CIPHERTEXT_LEN + X25519_LEN {
            return Err(INVALID_KEY_SHARE);
        }
        let (ciphertext, x25519_share) = peer_pub_key.split_at(CIPHERTEXT_LEN);
        let ciphertext = ciphertext.try_into().map_err(|_| INVALID_KEY_SHARE)?;
        // ML-KEM decapsulation never fails: a bad ciphertext gives a random
        // secret (implicit rejection), and the handshake's Finished check
        // catches it.
        let pq_secret = self.decaps.decapsulate(&ciphertext);
        let x25519_secret = self.x25519.complete(x25519_share)?;

        let mut secret = pq_secret.to_vec();
        secret.extend_from_slice(x25519_secret.secret_bytes());
        Ok(SharedSecret::from(secret))
    }

    fn hybrid_component(&self) -> Option<(NamedGroup, &[u8])> {
        Some((self.x25519.group(), self.x25519.pub_key()))
    }

    fn complete_hybrid_component(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        self.x25519.complete(peer_pub_key)
    }

    fn pub_key(&self) -> &[u8] {
        &self.share
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        None
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::X25519MLKEM768
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustls::server::ServerConfig;
    use rustls::{ClientConfig, HandshakeKind};

    use super::*;
    use crate::tls::testing;

    fn client() -> ClientConfig {
        testing::client_config(provider())
    }

    fn server(groups: Vec<&'static dyn SupportedKxGroup>) -> Arc<ServerConfig> {
        let mut provider = rustls::crypto::ring::default_provider();
        provider.kx_groups = groups;
        testing::server_config(provider)
    }

    #[test]
    fn offers_hybrid_first() {
        let groups: Vec<_> = provider().kx_groups.iter().map(|g| g.name()).collect();
        assert_eq!(groups[0], NamedGroup::X25519MLKEM768);
        assert_eq!(u16::from(groups[0]), 0x11ec);
        assert!(groups.contains(&NamedGroup::X25519));
    }

    #[test]
    fn shares_have_draft_lengths() {
        let active = X25519MLKEM768.start().unwrap();
        assert_eq!(active.pub_key().len(), ENCAP_KEY_LEN + X25519_LEN);
        let done = X25519MLKEM768.start_and_complete(active.pub_key()).unwrap();
        assert_eq!(done.pub_key.len(), CIPHERTEXT_LEN + X25519_LEN);
        let client_secret = active.complete(&done.pub_key).unwrap();
        assert_eq!(client_secret.secret_bytes(), done.secret.secret_bytes());
        assert_eq!(client_secret.secret_bytes().len(), 64);
    }

    #[test]
    fn rejects_malformed_shares() {
        assert!(X25519MLKEM768.start_and_complete(&[0; 32]).is_err());
        let active = X25519MLKEM768.start().unwrap();
        assert!(active.complete(&[0; CIPHERTEXT_LEN]).is_err());
    }

    #[test]
    fn negotiates_x25519mlkem768() {
        let conn = testing::handshake(client(), server(vec![X25519MLKEM768]));
        let group = conn.negotiated_key_exchange_group().unwrap().name();
        assert_eq!(group, NamedGroup::X25519MLKEM768);
    }

    #[test]
    fn falls_back_to_x25519_key_share() {
        // The X25519 share sent next to the hybrid one: no extra round trip.
        let conn = testing::handshake(client(), server(vec![rustls::crypto::ring::kx_group::X25519]));
        assert_eq!(conn.negotiated_key_exchange_group().unwrap().name(), NamedGroup::X25519);
        assert_eq!(conn.handshake_kind(), Some(HandshakeKind::Full));
    }

    #[test]
    fn falls_back_through_hello_retry_request() {
        // A server with neither offered share asks again for P-256.
        let conn = testing::handshake(client(), server(vec![rustls::crypto::ring::kx_group::SECP256R1]));
        assert_eq!(conn.negotiated_key_exchange_group().unwrap().name(), NamedGroup::secp256r1);
        assert_eq!(conn.handshake_kind(), Some(HandshakeKind::FullWithHelloRetryRequest));
    }

    #[test]
    fn server_only_hybrid_after_retry() {
        // A client that guessed X25519 first is asked for the hybrid share.
        let mut provider = provider();
        provider.kx_groups.rotate_left(1);
        let conn = testing::handshake(testing::client_config(provider), server(vec![X25519MLKEM768]));
        assert_eq!(conn.negotiated_key_exchange_group().unwrap().name(), NamedGroup::X25519MLKEM768);
        assert_eq!(conn.handshake_kind(), Some(HandshakeKind::FullWithHelloRetryRequest));
    }
}
