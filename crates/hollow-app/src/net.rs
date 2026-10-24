//! The broker's side of the network process: start Tor, fetch pages with
//! redirects, fetch subresources.
//!
//! One network process serves every tab. [`Net`] is a cheap handle: requests
//! from any thread go out under a lock, and a reader thread hands each reply
//! to whoever is waiting for that request id, so a slow page in one tab never
//! holds up another.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use hollow_ipc::{BrokerToNet, FetchResponse, NetToBroker};
use url::Url;

use crate::child::{self, ChildSender};
use crate::policy::{self, Extras};
use crate::terminal::sanitize;

const MAX_REDIRECTS: usize = 10;

/// Something worth showing in the network ledger (UX-01).
pub enum Ledger<'a> {
    Request { url: &'a Url, circuit: &'a str },
    Response { status: u16, content_type: Option<&'a str>, bytes: usize },
    Failed { url: &'a Url, reason: &'a str },
}

type Reply = Result<FetchResponse, String>;

#[derive(Clone)]
pub struct Net {
    shared: Arc<Shared>,
}

struct Shared {
    tx: Mutex<Option<ChildSender>>,
    next_id: AtomicU64,
    /// Requests in flight, by id, and who is waiting for each.
    waiting: Mutex<Waiting>,
}

#[derive(Default)]
struct Waiting {
    replies: HashMap<u64, Sender<Reply>>,
    /// Set once the network process is gone; later requests fail at once.
    dead: Option<String>,
}

/// Where the page that led to a request came from: decides `Referer`.
pub struct Navigation<'a> {
    pub url: Url,
    pub referrer: Option<&'a Url>,
    /// A form submission: body and content type for `POST`.
    pub post: Option<(Vec<u8>, String)>,
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
        let (tx, mut rx) = child.split();
        let shared = Arc::new(Shared {
            tx: Mutex::new(Some(tx)),
            next_id: AtomicU64::new(1),
            waiting: Mutex::new(Waiting::default()),
        });
        let reader = Arc::clone(&shared);
        std::thread::spawn(move || {
            let reason = loop {
                let (id, reply) = match rx.recv::<NetToBroker>() {
                    Ok(NetToBroker::FetchOk(r)) => (r.id, Ok(r)),
                    Ok(NetToBroker::FetchErr { id, message }) => (id, Err(sanitize(&message))),
                    // Tor keeps reporting directory status after it's ready.
                    Ok(NetToBroker::Progress(p)) => {
                        on_progress(&sanitize(&p));
                        continue;
                    }
                    Ok(NetToBroker::Fatal(m)) => break format!("network process failed: {}", sanitize(&m)),
                    Ok(other) => break format!("unexpected message from network process: {other:?}"),
                    Err(e) => break format!("{e:#}"),
                };
                let waiter = reader.waiting.lock().unwrap_or_else(|e| e.into_inner()).replies.remove(&id);
                match waiter {
                    Some(w) => {
                        let _ = w.send(reply);
                    }
                    None => break format!("network process answered a request we didn't make ({id})"),
                }
            };
            // Everyone still waiting gets the reason instead of a hang.
            let mut waiting = reader.waiting.lock().unwrap_or_else(|e| e.into_inner());
            for (_, w) in waiting.replies.drain() {
                let _ = w.send(Err(reason.clone()));
            }
            waiting.dead = Some(reason);
        });
        Ok(Self { shared })
    }

    /// Sends one request; the reply arrives on the returned channel.
    fn request(&self, url: &Url, first_party: &str, extras: &Extras) -> Receiver<Reply> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut waiting = self.shared.waiting.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(reason) = &waiting.dead {
                let _ = reply_tx.send(Err(reason.clone()));
                return reply_rx;
            }
            waiting.replies.insert(id, reply_tx.clone());
        }
        let msg = BrokerToNet::Fetch(policy::to_request(url, id, first_party, extras));
        let sent = match self.shared.tx.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            Some(tx) => tx.send(&msg).map_err(|e| format!("{e:#}")),
            None => Err("the network process has shut down".into()),
        };
        if let Err(e) = sent {
            self.shared.waiting.lock().unwrap_or_else(|e| e.into_inner()).replies.remove(&id);
            let _ = reply_tx.send(Err(e));
        }
        reply_rx
    }

    fn wait(rx: &Receiver<Reply>) -> Reply {
        rx.recv().unwrap_or_else(|_| Err("the network process stopped answering".into()))
    }

    /// Fetches a page, following redirects through the URL policy. Returns
    /// the final URL and response. A redirect after `POST` turns it into a
    /// `GET`, except for 307 and 308, which keep the body.
    pub fn fetch_page(&self, nav: Navigation, ledger: &mut dyn FnMut(Ledger)) -> anyhow::Result<(Url, FetchResponse)> {
        let Navigation { mut url, referrer, mut post } = nav;
        let mut referrer = referrer.cloned();
        for _ in 0..=MAX_REDIRECTS {
            // A top-level navigation is its own first party.
            let key = policy::isolation_key(&url);
            let extras = Extras {
                referer: referrer.as_ref().and_then(|r| policy::referer(r, &url)),
                origin: post.as_ref().and(referrer.as_ref()).map(|r| r.origin().ascii_serialization()),
                post: post.clone(),
            };
            ledger(Ledger::Request { url: &url, circuit: &key });
            let resp = match Self::wait(&self.request(&url, &key, &extras)) {
                Ok(r) => r,
                Err(e) => {
                    ledger(Ledger::Failed { url: &url, reason: &e });
                    bail!("fetching {url} failed: {e}");
                }
            };
            ledger(Ledger::Response {
                status: resp.status,
                content_type: resp.header("content-type"),
                bytes: resp.body.len(),
            });
            match redirect_target(&url, &resp)? {
                Some(next) => {
                    if !matches!(resp.status, 307 | 308) {
                        post = None;
                    }
                    referrer = Some(url);
                    url = next;
                }
                None => return Ok((url, resp)),
            }
        }
        bail!("too many redirects")
    }

    /// Fetches subresources of a page, concurrently, on the page's circuit
    /// (first-party isolation: the page decides, not the resource's host).
    /// Redirects aren't followed. Results are in input order; failures and
    /// non-200 answers are `None`.
    pub fn fetch_subresources(
        &self,
        urls: &[Url],
        page: &Url,
        ledger: &mut dyn FnMut(Ledger),
    ) -> anyhow::Result<Vec<Option<FetchResponse>>> {
        let first_party = policy::isolation_key(page);
        let pending: Vec<Receiver<Reply>> = urls
            .iter()
            .map(|url| {
                ledger(Ledger::Request { url, circuit: &first_party });
                let extras = Extras { referer: policy::referer(page, url), ..Extras::default() };
                self.request(url, &first_party, &extras)
            })
            .collect();
        let mut results = Vec::with_capacity(urls.len());
        for (url, rx) in urls.iter().zip(pending) {
            match Self::wait(&rx) {
                Ok(r) => {
                    ledger(Ledger::Response {
                        status: r.status,
                        content_type: r.header("content-type"),
                        bytes: r.body.len(),
                    });
                    results.push(Some(r).filter(|r| r.status == 200));
                }
                Err(reason) => {
                    ledger(Ledger::Failed { url, reason: &reason });
                    if self.shared.waiting.lock().unwrap_or_else(|e| e.into_inner()).dead.is_some() {
                        return Err(anyhow!(reason)).context("network process failed");
                    }
                    results.push(None);
                }
            }
        }
        Ok(results)
    }

    /// Stops the network process. Other handles fail from then on.
    pub fn shutdown(&self) {
        if let Some(tx) = self.shared.tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            tx.shutdown(&BrokerToNet::Shutdown);
        }
    }
}

/// The Tor Browser behaviour for sites that advertise an onion service of
/// themselves (NET-09): an HTTPS page whose `Onion-Location` names a valid
/// `.onion` address of the same path is loaded from the onion instead.
pub fn onion_location(url: &Url, resp: &FetchResponse) -> Option<Url> {
    if url.scheme() != "https" || policy::is_onion(url) || resp.status != 200 {
        return None;
    }
    let target = Url::parse(resp.header("onion-location")?.trim()).ok()?;
    (policy::is_onion(&target) && policy::check(&target).is_ok()).then_some(target)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, headers: &[(&str, &str)]) -> FetchResponse {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        FetchResponse { id: 1, status, headers, body: vec![] }
    }

    #[test]
    fn onion_location_is_followed_only_from_https_to_onion() {
        let page = Url::parse("https://www.torproject.org/").unwrap();
        let onion = "http://2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion/";
        let r = resp(200, &[("Onion-Location", onion)]);
        assert_eq!(onion_location(&page, &r).unwrap().as_str(), onion);
        assert!(onion_location(&page, &resp(200, &[("Onion-Location", "https://evil.example/")])).is_none());
        assert!(onion_location(&page, &resp(200, &[("Onion-Location", "file:///C:/x.onion")])).is_none());
        assert!(onion_location(&page, &resp(404, &[("Onion-Location", onion)])).is_none());
        let already = Url::parse(onion).unwrap();
        assert!(onion_location(&already, &r).is_none());
    }
}
