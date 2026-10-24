//! Message types and framing between Hollow's broker and its sandboxed
//! processes (architecture §2).
//!
//! Each child talks to the broker over its stdin/stdout pipes. A frame is a
//! little-endian `u32` length followed by a `postcard` payload. Messages that
//! come *from* a child are untrusted: the broker must call
//! [`Validate::validate`] on every one before acting on it.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Largest frame either side will send or accept (a 4K viewport at 2x is ~33 MB).
pub const MAX_FRAME_LEN: usize = 96 * 1024 * 1024;
/// Largest response body the network process will hand to the broker.
pub const MAX_BODY_LEN: usize = 16 * 1024 * 1024;

const MAX_HEADERS: usize = 256;
const MAX_HEADER_LEN: usize = 16 * 1024;
const MAX_TITLE_LEN: usize = 1024;
const MAX_TEXT_LEN: usize = 1024 * 1024;
const MAX_LINKS: usize = 10_000;
const MAX_LINK_LEN: usize = 8 * 1024;
const MAX_CHECKS: usize = 32;
const MAX_MESSAGE_LEN: usize = 4 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("pipe error: {0}")]
    Io(#[from] io::Error),
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_LEN}-byte limit")]
    FrameTooLarge(usize),
    #[error("malformed message: {0}")]
    Decode(#[from] postcard::Error),
    #[error("invalid message: {0}")]
    Invalid(String),
}

/// Serializes `msg` and writes it as one frame.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), IpcError> {
    let bytes = postcard::to_stdvec(msg)?;
    if bytes.len() > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge(bytes.len()));
    }
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Reads one frame. Returns `Ok(None)` when the other side closed the pipe.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>, IpcError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge(len));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(postcard::from_bytes(&buf)?))
}

/// Checks applied by the broker to every message received from a child.
pub trait Validate {
    fn validate(&self) -> Result<(), IpcError>;
}

fn check_len(field: &str, len: usize, max: usize) -> Result<(), IpcError> {
    if len > max {
        return Err(IpcError::Invalid(format!("{field} is {len} long, limit is {max}")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Broker <-> network process
// ---------------------------------------------------------------------------

/// A request the broker has already checked against policy. The network
/// process never parses URLs itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRequest {
    pub id: u64,
    pub host: String,
    pub port: u16,
    /// Path and query only; fragments are never sent.
    pub path_and_query: String,
    /// `false` only for plain-HTTP `.onion` sites.
    pub tls: bool,
    /// First-party site this request belongs to. Each key gets its own Tor
    /// circuit and connection state (NET-05, STATE-03).
    pub isolation_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum BrokerToNet {
    /// Sent once the broker has checked the [`SandboxReport`]; Tor doesn't
    /// start before this.
    StartTor,
    Fetch(FetchRequest),
    Shutdown,
}

/// What a child reports about its own sandbox, read back from Windows from
/// inside the process. The broker refuses to use a child whose report isn't
/// all `true`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SandboxReport {
    pub app_container: bool,
    pub dynamic_code_prohibited: bool,
    pub win32k_disabled: bool,
    pub microsoft_signed_only: bool,
    pub memory_locked: bool,
}

impl SandboxReport {
    pub fn checks(&self) -> [(&'static str, bool); 5] {
        [
            ("token_appcontainer", self.app_container),
            ("acg_no_dynamic_code", self.dynamic_code_prohibited),
            ("win32k_disabled", self.win32k_disabled),
            ("cig_microsoft_signed_only", self.microsoft_signed_only),
            ("memory_locked", self.memory_locked),
        ]
    }

    pub fn all_enforced(&self) -> bool {
        self.checks().iter().all(|(_, on)| *on)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResponse {
    pub id: u64,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl FetchResponse {
    /// First header named `name`, compared case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum NetToBroker {
    /// First message: the process's sandbox as Windows reports it.
    Hello(SandboxReport),
    /// Tor bootstrap progress, human-readable.
    Progress(String),
    /// Tor is bootstrapped; fetches can be sent.
    Ready,
    FetchOk(FetchResponse),
    FetchErr {
        id: u64,
        message: String,
    },
    /// The network process can't continue (e.g. Tor failed to start).
    Fatal(String),
}

impl Validate for NetToBroker {
    fn validate(&self) -> Result<(), IpcError> {
        match self {
            NetToBroker::Progress(m) | NetToBroker::Fatal(m) => check_len("message", m.len(), MAX_MESSAGE_LEN),
            NetToBroker::FetchErr { message, .. } => check_len("message", message.len(), MAX_MESSAGE_LEN),
            NetToBroker::Ready | NetToBroker::Hello(_) => Ok(()),
            NetToBroker::FetchOk(r) => {
                if !(100..=599).contains(&r.status) {
                    return Err(IpcError::Invalid(format!("status {} out of range", r.status)));
                }
                check_len("header count", r.headers.len(), MAX_HEADERS)?;
                for (k, v) in &r.headers {
                    check_len("header", k.len() + v.len(), MAX_HEADER_LEN)?;
                }
                check_len("body", r.body.len(), MAX_BODY_LEN)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Broker <-> renderer
// ---------------------------------------------------------------------------

/// Network and file-system escapes the renderer must fail to make (P2).
/// Loopback targets are listeners the broker opened, so the broker can see
/// for itself whether anything arrived.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfTestPlan {
    pub external_tcp: String,
    pub dns_name: String,
    pub loopback_tcp_port: u16,
    pub loopback_udp_port: u16,
    /// A file in the user's profile the renderer must not be able to read.
    pub canary_path: String,
    /// A user directory the renderer must not be able to write to.
    pub write_dir: String,
}

/// The page area in CSS px, and device pixels per CSS px.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    pub scale: f32,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum BrokerToRenderer {
    /// Text extraction only (`hollow fetch`).
    Render {
        url: String,
        content_type: Option<String>,
        body: Vec<u8>,
    },
    /// Lay out a page for display. The renderer answers `Loaded`, then
    /// `NeedResources` (possibly empty) and waits for `Resources` before painting.
    Load {
        url: String,
        content_type: Option<String>,
        body: Vec<u8>,
        viewport: Viewport,
    },
    /// What the broker fetched of `NeedResources`, keyed by URL. Missing
    /// entries failed or were refused.
    Resources {
        images: Vec<(String, Vec<u8>)>,
        stylesheets: Vec<(String, String)>,
    },
    /// Repaint at a new scroll position or viewport. Answer: `Frame`.
    Paint {
        scroll_y: f32,
        viewport: Viewport,
    },
    /// What's at this point (CSS px, page coordinates)? Answer: `Hit`.
    HitTest {
        x: f32,
        y: f32,
    },
    SelfTest(SelfTestPlan),
    Shutdown,
}

/// A painted viewport: straight (non-premultiplied) RGBA8, row-major.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub scroll_y: f32,
    pub page_height: f32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedPage {
    pub title: Option<String>,
    pub text: String,
    /// `href` values as written in the page, unresolved.
    pub links: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    /// `true` when the sandbox held: the attempt was blocked, or the token is
    /// what it should be.
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RendererToBroker {
    Rendered(RenderedPage),
    Loaded {
        title: Option<String>,
    },
    /// Absolute URLs of images and stylesheets the page wants. The broker
    /// decides which to fetch.
    NeedResources {
        images: Vec<String>,
        stylesheets: Vec<String>,
    },
    Frame(Frame),
    /// Link target at the hit-tested point, unvalidated.
    Hit(Option<String>),
    SelfTestReport(Vec<Check>),
    Error(String),
}

/// Largest painted frame accepted from a renderer: a 5K display's worth of
/// device pixels, which still fits in one IPC frame.
pub const MAX_FRAME_PIXELS: u64 = 5120 * 2880;
const MAX_IMAGE_REQUESTS: usize = 64;
const MAX_STYLESHEET_REQUESTS: usize = 32;
/// Largest stylesheet the broker hands to a renderer.
pub const MAX_STYLESHEET_LEN: usize = 2 * 1024 * 1024;

impl Validate for RendererToBroker {
    fn validate(&self) -> Result<(), IpcError> {
        match self {
            RendererToBroker::Loaded { title } => {
                check_len("title", title.as_ref().map_or(0, String::len), MAX_TITLE_LEN)
            }
            RendererToBroker::NeedResources { images, stylesheets } => {
                check_len("image count", images.len(), MAX_IMAGE_REQUESTS)?;
                check_len("stylesheet count", stylesheets.len(), MAX_STYLESHEET_REQUESTS)?;
                images.iter().chain(stylesheets).try_for_each(|u| check_len("resource url", u.len(), MAX_LINK_LEN))
            }
            RendererToBroker::Frame(f) => {
                let pixels = u64::from(f.width) * u64::from(f.height);
                if pixels > MAX_FRAME_PIXELS || f.rgba.len() as u64 != pixels * 4 {
                    return Err(IpcError::Invalid(format!(
                        "frame {}x{} with {} bytes of pixels",
                        f.width,
                        f.height,
                        f.rgba.len()
                    )));
                }
                if !f.scroll_y.is_finite() || !f.page_height.is_finite() {
                    return Err(IpcError::Invalid("non-finite frame geometry".into()));
                }
                Ok(())
            }
            RendererToBroker::Hit(link) => check_len("link", link.as_ref().map_or(0, String::len), MAX_LINK_LEN),
            RendererToBroker::Rendered(p) => {
                if let Some(t) = &p.title {
                    check_len("title", t.len(), MAX_TITLE_LEN)?;
                }
                check_len("text", p.text.len(), MAX_TEXT_LEN)?;
                check_len("link count", p.links.len(), MAX_LINKS)?;
                for l in &p.links {
                    check_len("link", l.len(), MAX_LINK_LEN)?;
                }
                Ok(())
            }
            RendererToBroker::SelfTestReport(checks) => {
                check_len("check count", checks.len(), MAX_CHECKS)?;
                for c in checks {
                    check_len("check name", c.name.len(), 64)?;
                    check_len("check detail", c.detail.len(), MAX_MESSAGE_LEN)?;
                }
                Ok(())
            }
            RendererToBroker::Error(m) => check_len("message", m.len(), MAX_MESSAGE_LEN),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let mut buf = Vec::new();
        let msg = BrokerToNet::Fetch(FetchRequest {
            id: 7,
            host: "example.org".into(),
            port: 443,
            path_and_query: "/a?b=c".into(),
            tls: true,
            isolation_key: "example.org".into(),
        });
        write_frame(&mut buf, &msg).unwrap();
        write_frame(&mut buf, &BrokerToNet::Shutdown).unwrap();

        let mut r = buf.as_slice();
        match read_frame::<_, BrokerToNet>(&mut r).unwrap() {
            Some(BrokerToNet::Fetch(f)) => assert_eq!(f.path_and_query, "/a?b=c"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(read_frame::<_, BrokerToNet>(&mut r).unwrap(), Some(BrokerToNet::Shutdown)));
        assert!(read_frame::<_, BrokerToNet>(&mut r).unwrap().is_none());
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let mut buf = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&[0; 16]);
        let err = read_frame::<_, BrokerToNet>(&mut buf.as_slice()).unwrap_err();
        assert!(matches!(err, IpcError::FrameTooLarge(_)));
    }

    #[test]
    fn garbage_is_a_decode_error() {
        let mut buf = 3u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0xff, 0xff, 0xff]);
        let err = read_frame::<_, BrokerToNet>(&mut buf.as_slice()).unwrap_err();
        assert!(matches!(err, IpcError::Decode(_)));
    }

    #[test]
    fn validation_rejects_oversized_renderer_output() {
        let msg = RendererToBroker::Rendered(RenderedPage {
            title: Some("x".repeat(MAX_TITLE_LEN + 1)),
            text: String::new(),
            links: vec![],
        });
        assert!(msg.validate().is_err());

        let msg = NetToBroker::FetchOk(FetchResponse { id: 1, status: 42, headers: vec![], body: vec![] });
        assert!(msg.validate().is_err());
    }
}
