use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use anyhow::Context;
use arti_client::config::{BoolOrAuto, TorClientConfigBuilder};
use arti_client::{TorClient, TorClientConfig};
use futures::StreamExt;
use hollow_ipc::NetToBroker;
use tor_rtcompat::PreferredRuntime;

pub type Client = Arc<TorClient<PreferredRuntime>>;

/// Builds and bootstraps the embedded Tor client, reporting progress to the
/// broker as it goes.
///
/// Tor runs entirely in RAM: our patched Arti (third_party/) keeps its state
/// and directory cache in memory, so every session starts with fresh guards
/// and leaves nothing behind (DEV-01). Arti still insists on directory paths;
/// they point somewhere the sandbox can't write, so any disk use we missed
/// fails loudly instead of quietly leaving a trace.
pub async fn start(progress: Sender<NetToBroker>) -> anyhow::Result<Client> {
    let unwritable = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("hollow-ram-only-never-created");
    let mut builder = TorClientConfigBuilder::from_directories(unwritable.join("state"), unwritable.join("cache"));
    // Nothing is read from those paths, so there is nothing to check.
    builder.storage().permissions().dangerously_trust_everyone();
    // The keystore only holds onion-service client-auth keys, which Hollow
    // doesn't use yet; Arti's only persistent keystore is on disk.
    builder.storage().keystore().enabled(BoolOrAuto::Explicit(false));
    let config: TorClientConfig = builder.build().context("building Tor configuration")?;

    let client =
        TorClient::builder().config(config).create_unbootstrapped_async().await.context("creating Tor client")?;

    let mut events = client.bootstrap_events();
    let reporter = tokio::spawn(async move {
        let mut last = String::new();
        while let Some(status) = events.next().await {
            let text = status.to_string();
            if text != last {
                let _ = progress.send(NetToBroker::Progress(text.clone()));
                last = text;
            }
            if status.ready_for_traffic() {
                break;
            }
        }
    });
    client.bootstrap().await.context("bootstrapping Tor")?;
    reporter.abort();
    Ok(client)
}
