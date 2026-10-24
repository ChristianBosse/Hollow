use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, anyhow};
use arti_client::{IsolationToken, StreamPrefs};
use bytes::Bytes;
use hollow_ipc::{FetchRequest, FetchResponse, MAX_BODY_LEN, Method};
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use hyper::header::{ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CONTENT_TYPE, HOST, ORIGIN, REFERER, USER_AGENT};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;

use crate::profile;
use crate::tls::{self, SiteConfigs};
use crate::tor::Client;

const FETCH_TIMEOUT: Duration = Duration::from_secs(90);

pub struct Fetcher {
    client: Client,
    /// TLS session state per first-party site (STATE-03).
    tls: SiteConfigs,
    /// One Tor circuit family per first-party site (NET-05).
    isolation: Mutex<HashMap<String, IsolationToken>>,
}

impl Fetcher {
    pub fn new(client: Client) -> Self {
        Self { client, tls: SiteConfigs::new(tls::client_config()), isolation: Mutex::new(HashMap::new()) }
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
            let connector = TlsConnector::from(self.tls.for_site(&req.isolation_key));
            let tls = connector.connect(name, stream).await.context("TLS handshake")?;
            http_request(tls, req).await
        } else {
            http_request(stream, req).await
        }
    }
}

async fn http_request<S>(io: S, req: &FetchRequest) -> anyhow::Result<FetchResponse>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await.context("HTTP handshake")?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let default_port = if req.tls { 443 } else { 80 };
    let host = if req.port == default_port { req.host.clone() } else { format!("{}:{}", req.host, req.port) };
    let builder = match req.method {
        Method::Get => Request::get(req.path_and_query.as_str()),
        Method::Post => Request::post(req.path_and_query.as_str()),
    };
    let mut builder = builder
        .header(HOST, host)
        .header(USER_AGENT, profile::USER_AGENT)
        .header(ACCEPT, profile::ACCEPT)
        .header(ACCEPT_LANGUAGE, profile::ACCEPT_LANGUAGE)
        .header(ACCEPT_ENCODING, profile::ACCEPT_ENCODING);
    if let Some(referer) = &req.referer {
        builder = builder.header(REFERER, referer);
    }
    if let Some(origin) = &req.origin {
        builder = builder.header(ORIGIN, origin);
    }
    if let Some(ct) = &req.content_type {
        builder = builder.header(CONTENT_TYPE, ct);
    }
    let request = builder.body(Full::new(Bytes::from(req.body.clone()))).context("building request")?;

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
