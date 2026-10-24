//! hollow-fortress: the Fortress renderer process (architecture §2.3).
//!
//! Runs in a Less Privileged AppContainer with no network capability. It
//! receives page bytes from the broker, lays them out with hollow-engine and
//! returns painted pixels. Images come from the broker too: the renderer can
//! only ask for them. One process per page; the broker kills it on navigation.

mod page;
mod render;
mod selftest;

use std::io;
use std::process::ExitCode;

use hollow_ipc::{BrokerToRenderer, RendererToBroker, read_frame, write_frame};

/// Locks allocations into RAM and zeroes them on free (WIN-02).
#[global_allocator]
static ALLOC: hollow_sandbox::LockedAlloc = hollow_sandbox::LockedAlloc;

fn main() -> ExitCode {
    if let Err(e) = hollow_sandbox::enter_sandbox() {
        eprintln!("hollow-fortress: {e}");
        return ExitCode::from(78);
    }
    if let Err(e) = hollow_sandbox::reserve_lockable_memory(512 << 20) {
        eprintln!("hollow-fortress: warning: memory may be paged to disk: {e}");
    }

    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut current: Option<page::Page> = None;
    loop {
        let msg = match read_frame::<_, BrokerToRenderer>(&mut stdin) {
            Ok(Some(msg)) => msg,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("hollow-fortress: bad message from broker: {e}");
                return ExitCode::FAILURE;
            }
        };
        let replies = match msg {
            BrokerToRenderer::Render { url, content_type, body } => {
                vec![RendererToBroker::Rendered(render::render(&url, content_type.as_deref(), &body))]
            }
            BrokerToRenderer::Load { url, content_type, body, viewport } => {
                let p = page::Page::load(&url, content_type.as_deref(), &body, viewport);
                let replies = vec![
                    RendererToBroker::Loaded { title: p.title() },
                    RendererToBroker::NeedResources { images: p.image_urls(), stylesheets: p.stylesheet_urls() },
                ];
                current = Some(p);
                replies
            }
            BrokerToRenderer::Resources { images, stylesheets } => match current.as_mut() {
                Some(p) => {
                    p.add_resources(images, stylesheets);
                    match p.stylesheet_images() {
                        // A second, last round for the stylesheets' images.
                        Some(images) => vec![RendererToBroker::NeedResources { images, stylesheets: vec![] }],
                        None => {
                            let viewport = p.viewport();
                            vec![RendererToBroker::Frame(p.paint(0.0, viewport))]
                        }
                    }
                }
                None => vec![RendererToBroker::Error("no page loaded".into())],
            },
            BrokerToRenderer::Paint { scroll_y, viewport } => match current.as_mut() {
                Some(p) => vec![RendererToBroker::Frame(p.paint(scroll_y, viewport))],
                None => vec![RendererToBroker::Error("no page loaded".into())],
            },
            BrokerToRenderer::Click { x, y } => match current.as_mut() {
                Some(p) => interacted(p, |p| p.click(x, y)),
                None => vec![RendererToBroker::Error("no page loaded".into())],
            },
            BrokerToRenderer::Key(key) => match current.as_mut() {
                Some(p) => interacted(p, |p| p.key(key)),
                None => vec![RendererToBroker::Error("no page loaded".into())],
            },
            BrokerToRenderer::SelfTest(plan) => vec![RendererToBroker::SelfTestReport(selftest::run(&plan))],
            BrokerToRenderer::Shutdown => return ExitCode::SUCCESS,
        };
        for reply in replies {
            if let Err(e) = write_frame(&mut stdout, &reply) {
                eprintln!("hollow-fortress: writing to broker: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
}

/// The answer to a click or key: what it did, then a fresh frame if
/// anything visible changed.
fn interacted(p: &mut page::Page, f: impl FnOnce(&mut page::Page) -> page::Outcome) -> Vec<RendererToBroker> {
    let o = f(p);
    let mut replies =
        vec![RendererToBroker::Interacted { link: o.link, submit: o.submit, editing: o.editing, repaint: o.repaint }];
    if o.repaint {
        let viewport = p.viewport();
        replies.push(RendererToBroker::Frame(p.paint(p.scroll_y(), viewport)));
    }
    replies
}
