//! hollow-ipc: what the broker reads from its sandboxed children. A
//! compromised renderer or network process controls every byte here.

use hollow_ipc::{
    Check, FetchResponse, FormEncoding, FormSubmit, Frame, Method, NetToBroker, RenderedPage, RendererToBroker,
    SandboxReport, Validate, read_frame, write_frame,
};
use url::Url;

/// Frames read from one input at most (a stream of messages, as on the pipe).
const MAX_MESSAGES: usize = 64;

/// The broker's read loop for a renderer: frames, validation, then what the
/// broker does with the URLs a valid message carries.
pub fn renderer_to_broker(data: &[u8]) {
    let mut pipe = data;
    for _ in 0..MAX_MESSAGES {
        let Ok(Some(msg)) = read_frame::<_, RendererToBroker>(&mut pipe) else { return };
        if msg.validate().is_err() {
            continue;
        }
        match msg {
            RendererToBroker::NeedResources { images, stylesheets } => {
                for u in images.iter().chain(&stylesheets) {
                    checked_url(u);
                }
            }
            RendererToBroker::Interacted { link, submit, .. } => {
                if let Some(link) = link {
                    checked_url(&link);
                }
                if let Some(submit) = submit {
                    let _ = hollow_app::form::to_request(submit);
                }
            }
            RendererToBroker::Rendered(page) => page.links.iter().for_each(|l| checked_url(l)),
            RendererToBroker::Frame(f) => {
                // Validation promises the buffer matches the size.
                assert_eq!(f.rgba.len() as u64, u64::from(f.width) * u64::from(f.height) * 4);
            }
            _ => {}
        }
    }
}

/// The broker's read loop for the network process, with redirect and
/// `Onion-Location` handling of valid responses.
pub fn net_to_broker(data: &[u8]) {
    let mut pipe = data;
    let page = Url::parse("https://fuzz.example/dir/page?q=1").expect("valid URL");
    for _ in 0..MAX_MESSAGES {
        let Ok(Some(msg)) = read_frame::<_, NetToBroker>(&mut pipe) else { return };
        if msg.validate().is_err() {
            continue;
        }
        if let NetToBroker::FetchOk(resp) = msg {
            assert!((100..=599).contains(&resp.status));
            for name in ["location", "onion-location", "content-type"] {
                if let Some(value) = resp.header(name)
                    && let Ok(next) = page.join(value.trim())
                {
                    let _ = hollow_app::policy::check(&next);
                    let _ = hollow_app::policy::is_onion(&next);
                }
            }
        }
    }
}

/// A URL from a renderer, as the broker treats it: parsed, then policy.
fn checked_url(s: &str) {
    if let Ok(u) = Url::parse(s)
        && hollow_app::policy::check(&u).is_ok()
    {
        let _ = hollow_app::policy::isolation_key(&u);
    }
}

/// Frames as a child would write them to its pipe (seed generation).
macro_rules! frames {
    ($msgs:expr) => {{
        let mut out = Vec::new();
        for m in $msgs {
            write_frame(&mut out, m).expect("seed message encodes");
        }
        out
    }};
}

/// Seed inputs: one stream per message kind, plus one with everything.
pub fn renderer_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let submit = |method, encoding| FormSubmit {
        action: "https://fuzz.example/s?x=1#f".into(),
        method,
        encoding,
        entries: vec![("q".into(), "a b\r\nc".into()), ("n\"x".into(), "\u{e9}".into())],
        new_tab: true,
    };
    let all = vec![
        RendererToBroker::Loaded { title: Some("Title".into()) },
        RendererToBroker::NeedResources {
            images: vec!["https://fuzz.example/a.png".into(), "http://x.onion/b.gif".into()],
            stylesheets: vec!["https://fuzz.example/s.css".into()],
        },
        RendererToBroker::Frame(Frame { scroll_y: 0.0, page_height: 10.0, width: 2, height: 2, rgba: vec![255; 16] }),
        RendererToBroker::Interacted {
            link: Some("https://fuzz.example/next".into()),
            submit: Some(submit(Method::Post, FormEncoding::Multipart)),
            editing: true,
            repaint: true,
        },
        RendererToBroker::Interacted {
            link: None,
            submit: Some(submit(Method::Get, FormEncoding::UrlEncoded)),
            editing: false,
            repaint: false,
        },
        RendererToBroker::Rendered(RenderedPage {
            title: None,
            text: "text".into(),
            links: vec!["/rel".into(), "https://127.0.0.1/".into()],
        }),
        RendererToBroker::SelfTestReport(vec![Check { name: "n".into(), passed: true, detail: "d".into() }]),
        RendererToBroker::Error("e".into()),
    ];
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();
    let names =
        ["loaded", "need-resources", "frame", "interacted-post", "interacted-get", "rendered", "selftest", "error"];
    for (name, msg) in names.iter().zip(&all) {
        out.push((name, frames!(std::slice::from_ref(msg))));
    }
    out.push(("all", frames!(&all)));
    out
}

/// Seed inputs for [`net_to_broker`].
pub fn net_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let resp = |status, headers: &[(&str, &str)]| FetchResponse {
        id: 7,
        status,
        headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        body: b"<p>body".to_vec(),
    };
    let report = SandboxReport {
        app_container: true,
        dynamic_code_prohibited: true,
        win32k_disabled: true,
        microsoft_signed_only: true,
        memory_locked: true,
    };
    let all = vec![
        NetToBroker::Hello(report),
        NetToBroker::Progress("50%".into()),
        NetToBroker::Ready,
        NetToBroker::FetchOk(resp(200, &[("Content-Type", "text/html"), ("Onion-Location", "http://x.onion/")])),
        NetToBroker::FetchOk(resp(302, &[("Location", "../other?x#y")])),
        NetToBroker::FetchErr { id: 7, message: "failed".into() },
        NetToBroker::Fatal("fatal".into()),
    ];
    let names = ["hello", "progress", "ready", "fetch-ok", "redirect", "fetch-err", "fatal"];
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();
    for (name, msg) in names.iter().zip(&all) {
        out.push((name, frames!(std::slice::from_ref(msg))));
    }
    out.push(("all", frames!(&all)));
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds_are_valid_messages() {
        for (_, bytes) in super::renderer_seeds() {
            super::renderer_to_broker(&bytes);
        }
        for (_, bytes) in super::net_seeds() {
            super::net_to_broker(&bytes);
        }
    }
}
