use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use arti_client::{IsolationToken, StreamPrefs};
use bytes::Bytes;
use hollow_ipc::{FetchRequest, FetchResponse, MAX_BODY_LEN};
use http_body_util::{BodyExt, Empty, Limited};
use hyper::Request;
use hyper::header::{ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, HOST, USER_AGENT};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;

use crate::profile;
use crate::tor::Client;

const FETCH_TIMEOUT: Duration = Duration::from_secs(90);

pub struct Fetcher {
    client: Client,
    tls: TlsConnector,
    /// One Tor circuit family per first-party site (NET-05).
    isolation: Mutex<HashMap<String, IsolationToken>>,
}

impl Fetcher {
    pub fn new(client: Client) -> Self {
        // Bundled roots, never the Windows store: every Hollow user trusts
        // exactly the same CAs.
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let mut config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self { client, tls: TlsConnector::from(Arc::new(config)), isolation: Mutex::new(HashMap::new()) }
    }

    fn isolation_for(&self, key: &str) -> IsolationToken {
        let mut map = self.isolation.lock().unwrap_or_else(|e| e.into_inner());
        *map.entry(key.to_owned()).or_insert_with(IsolationToken::new)
    }

    pub async fn fetch(&self, req: FetchRequest) -> anyhow::Result<FetchResponse> {
        tokio::time::timeout(FETCH_TIMEOUT, self.fetch_inner(&req))
            .await
            .map_err(|_| anyhow!("timed out after {}s", FETCH_TIMEOUT.as_secs()))?
    }

    async fn fetch_inner(&self, req: &FetchRequest) -> anyhow::Result<FetchResponse> {
        let mut prefs = StreamPrefs::new();
        prefs.set_isolation(self.isolation_for(&req.isolation_key));
        let stream = self
            .client
            .connect_with_prefs((req.host.as_str(), req.port), &prefs)
            .await
            .context("connecting through Tor")?;

        if req.tls {
            let name = ServerName::try_from(req.host.clone()).context("invalid server name")?;
            let tls = self.tls.connect(name, stream).await.context("TLS handshake")?;
            http_get(tls, req).await
        } else {
            http_get(stream, req).await
        }
    }
}

async fn http_get<S>(io: S, req: &FetchRequest) -> anyhow::Result<FetchResponse>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await.context("HTTP handshake")?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let default_port = if req.tls { 443 } else { 80 };
    let host = if req.port == default_port { req.host.clone() } else { format!("{}:{}", req.host, req.port) };
    let request = Request::get(req.path_and_query.as_str())
        .header(HOST, host)
        .header(USER_AGENT, profile::USER_AGENT)
        .header(ACCEPT, profile::ACCEPT)
        .header(ACCEPT_LANGUAGE, profile::ACCEPT_LANGUAGE)
        .header(ACCEPT_ENCODING, profile::ACCEPT_ENCODING)
        .body(Empty::<Bytes>::new())
        .context("building request")?;

    let resp = sender.send_request(request).await.context("sending request")?;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_owned(), v.to_owned())))
        .collect();
    let body = Limited::new(resp.into_body(), MAX_BODY_LEN)
        .collect()
        .await
        .map_err(|e| anyhow!("reading body: {e}"))?
        .to_bytes();

    Ok(FetchResponse { id: req.id, status, headers, body: body.to_vec() })
}
