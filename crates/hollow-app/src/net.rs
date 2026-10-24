//! The broker's side of the network process: start Tor, fetch pages with
//! redirects, fetch subresources.

use anyhow::{Context, bail};
use hollow_ipc::{BrokerToNet, FetchResponse, NetToBroker};
use url::Url;

use crate::child::{self, Child};
use crate::policy;
use crate::terminal::sanitize;

const MAX_REDIRECTS: usize = 10;

/// Something worth showing in the network ledger (UX-01).
pub enum Ledger<'a> {
    Request { url: &'a Url, circuit: &'a str },
    Response { status: u16, content_type: Option<&'a str>, bytes: usize },
    Failed { url: &'a Url, reason: &'a str },
}

pub struct Net {
    child: Child,
    next_id: u64,
    on_progress: Box<dyn FnMut(&str) + Send>,
}

impl Net {
    /// Starts the network process (after checking its sandbox) and waits for
    /// Tor to bootstrap. `on_progress` gets Tor's status lines, now and later.
    pub fn start(mut on_progress: Box<dyn FnMut(&str) + Send>) -> anyhow::Result<Self> {
        let mut child = child::start_net()?;
        loop {
            match child.recv::<NetToBroker>()? {
                NetToBroker::Progress(p) => on_progress(&sanitize(&p)),
                NetToBroker::Ready => break,
                NetToBroker::Fatal(m) => bail!("{}", sanitize(&m)),
                other => bail!("unexpected message while Tor was starting: {other:?}"),
            }
        }
        Ok(Self { child, next_id: 0, on_progress })
    }

    /// Fetches a page, following redirects through the URL policy. Returns
    /// the final URL and response.
    pub fn fetch_page(&mut self, mut url: Url, ledger: &mut dyn FnMut(Ledger)) -> anyhow::Result<(Url, FetchResponse)> {
        for _ in 0..=MAX_REDIRECTS {
            // A top-level navigation is its own first party.
            let key = policy::isolation_key(&url);
            let resp = self.fetch_one(&url, &key, ledger)?;
            match redirect_target(&url, &resp)? {
                Some(next) => url = next,
                None => return Ok((url, resp)),
            }
        }
        bail!("too many redirects")
    }

    /// Fetches subresources of a page, concurrently, on the page's circuit
    /// (first-party isolation: the page decides, not the resource's host).
    /// Redirects aren't followed. Results are in input order.
    pub fn fetch_subresources(
        &mut self,
        urls: &[Url],
        first_party: &str,
        ledger: &mut dyn FnMut(Ledger),
    ) -> anyhow::Result<Vec<Option<FetchResponse>>> {
        let first_id = self.next_id + 1;
        for url in urls {
            self.next_id += 1;
            ledger(Ledger::Request { url, circuit: first_party });
            self.child.send(&BrokerToNet::Fetch(policy::to_request(url, self.next_id, first_party)))?;
        }
        let mut results: Vec<Option<FetchResponse>> = vec![None; urls.len()];
        let mut pending = urls.len();
        while pending > 0 {
            let (id, resp, error) = match self.child.recv::<NetToBroker>()? {
                NetToBroker::FetchOk(r) => (r.id, Some(r), None),
                NetToBroker::FetchErr { id, message } => (id, None, Some(message)),
                NetToBroker::Progress(p) => {
                    (self.on_progress)(&sanitize(&p));
                    continue;
                }
                NetToBroker::Fatal(m) => bail!("network process failed: {}", sanitize(&m)),
                other => bail!("unexpected reply from network process: {other:?}"),
            };
            let Some(slot) = id.checked_sub(first_id).map(|i| i as usize).filter(|i| *i < urls.len()) else {
                bail!("network process answered a request we didn't make ({id})");
            };
            if let Some(r) = &resp {
                ledger(Ledger::Response {
                    status: r.status,
                    content_type: r.header("content-type"),
                    bytes: r.body.len(),
                });
            }
            if let Some(reason) = &error {
                ledger(Ledger::Failed { url: &urls[slot], reason: &sanitize(reason) });
            }
            results[slot] = resp.filter(|r| r.status == 200);
            pending -= 1;
        }
        Ok(results)
    }

    fn fetch_one(&mut self, url: &Url, key: &str, ledger: &mut dyn FnMut(Ledger)) -> anyhow::Result<FetchResponse> {
        self.next_id += 1;
        let id = self.next_id;
        ledger(Ledger::Request { url, circuit: key });
        self.child.send(&BrokerToNet::Fetch(policy::to_request(url, id, key)))?;
        loop {
            match self.child.recv::<NetToBroker>()? {
                NetToBroker::FetchOk(r) if r.id == id => {
                    ledger(Ledger::Response {
                        status: r.status,
                        content_type: r.header("content-type"),
                        bytes: r.body.len(),
                    });
                    return Ok(r);
                }
                NetToBroker::FetchErr { id: rid, message } if rid == id => {
                    bail!("fetching {url} failed: {}", sanitize(&message))
                }
                // Tor keeps reporting directory status after it's ready.
                NetToBroker::Progress(p) => (self.on_progress)(&sanitize(&p)),
                NetToBroker::Fatal(m) => bail!("network process failed: {}", sanitize(&m)),
                other => bail!("unexpected reply from network process: {other:?}"),
            }
        }
    }

    pub fn shutdown(self) {
        self.child.shutdown(&BrokerToNet::Shutdown);
    }
}

fn redirect_target(current: &Url, resp: &FetchResponse) -> anyhow::Result<Option<Url>> {
    if !matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let Some(location) = resp.header("location") else {
        return Ok(None);
    };
    let next = current.join(location).with_context(|| format!("bad redirect target {}", sanitize(location)))?;
    policy::check(&next).with_context(|| format!("redirect to {next} refused"))?;
    Ok(Some(next))
}
