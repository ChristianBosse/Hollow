//! `hollow fetch <url>`: network process fetches over Tor, a fresh Fortress
//! renderer renders, the broker checks and shows the result.

use anyhow::bail;
use hollow_ipc::{BrokerToRenderer, RendererToBroker};
use url::Url;

use crate::child::{Child, Kind};
use crate::net::{Ledger, Navigation, Net};
use crate::policy;
use crate::terminal::sanitize;

const PREVIEW_CHARS: usize = 3000;

pub fn run(input: &str) -> anyhow::Result<bool> {
    let url = policy::parse_input(input)?;

    eprintln!("Starting Tor (every session starts fresh, in RAM; this takes a few seconds)...");
    let net = Net::start(Box::new(|p| eprintln!("   Tor: {p}")))?;
    eprintln!("   Tor: ready");
    let (url, response) = net.fetch_page(Navigation { url, referrer: None, post: None }, &mut print_ledger)?;
    net.shutdown();

    // A fresh renderer for this page, discarded afterwards (SEC-03).
    let mut renderer = Child::spawn(Kind::Fortress)?;
    renderer.send(&BrokerToRenderer::Render {
        url: url.to_string(),
        content_type: response.header("content-type").map(str::to_owned),
        body: response.body,
    })?;
    let page = match renderer.recv::<RendererToBroker>()? {
        RendererToBroker::Rendered(p) => p,
        RendererToBroker::Error(e) => bail!("renderer failed: {e}"),
        other => bail!("unexpected reply from renderer: {other:?}"),
    };
    renderer.shutdown(&BrokerToRenderer::Shutdown);

    println!();
    println!("== {} ==", sanitize(page.title.as_deref().unwrap_or("(untitled)")));
    println!("{url}");
    println!();
    let text = sanitize(&page.text);
    let preview: String = text.chars().take(PREVIEW_CHARS).collect();
    println!("{preview}");
    if text.chars().count() > PREVIEW_CHARS {
        println!("... ({} more characters)", text.chars().count() - PREVIEW_CHARS);
    }
    let links: Vec<Url> = page.links.iter().filter_map(|l| url.join(l).ok()).collect();
    println!();
    println!("{} links. First few:", links.len());
    for link in links.iter().take(10) {
        let note = if policy::check(link).is_ok() { "" } else { "  (blocked by policy)" };
        println!("  {}{note}", sanitize(link.as_str()));
    }
    Ok(true)
}

/// Network ledger (UX-01), terminal edition.
fn print_ledger(event: Ledger) {
    match event {
        Ledger::Request { url, circuit } => eprintln!("-> GET {url}  [circuit: {circuit}]"),
        Ledger::Response { status, content_type, bytes } => eprintln!(
            "<- {status} {}, {bytes} bytes",
            content_type.map(sanitize).unwrap_or_else(|| "(no content type)".into())
        ),
        Ledger::Failed { url, reason } => eprintln!("<- {url} failed: {reason}"),
    }
}
