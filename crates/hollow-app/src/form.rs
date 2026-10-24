//! Turning a form submission from a renderer into a request (MODE-02).
//!
//! The renderer only says what the user entered and where the page wants it
//! sent; the broker checks the target against policy and encodes the body
//! itself, so a compromised renderer can't shape the request beyond what any
//! page could.

use anyhow::{Context, bail};
use hollow_ipc::{FormEncoding, FormSubmit, MAX_REQUEST_BODY_LEN, Method};
use url::Url;

use crate::policy;

/// A checked submission, ready to fetch.
pub struct Request {
    pub url: Url,
    /// Body and content type for `POST`.
    pub post: Option<(Vec<u8>, String)>,
    pub new_tab: bool,
}

pub fn to_request(submit: FormSubmit) -> anyhow::Result<Request> {
    let mut url = Url::parse(&submit.action).context("the form's address is invalid")?;
    policy::check(&url).with_context(|| format!("the form sends to {url}, which Hollow won't load"))?;
    url.set_fragment(None);
    let post = match submit.method {
        Method::Get => {
            // GET replaces the query with the form data, as browsers do.
            let query = urlencoded(&submit.entries);
            url.set_query(Some(&query));
            if url.as_str().len() > MAX_REQUEST_BODY_LEN {
                bail!("the form data is too large to send");
            }
            None
        }
        Method::Post => {
            let (body, content_type) = match submit.encoding {
                FormEncoding::UrlEncoded => {
                    (urlencoded(&submit.entries).into_bytes(), "application/x-www-form-urlencoded".to_owned())
                }
                FormEncoding::Multipart => {
                    let boundary = boundary()?;
                    (multipart(&submit.entries, &boundary), format!("multipart/form-data; boundary={boundary}"))
                }
                FormEncoding::Plain => (plain(&submit.entries).into_bytes(), "text/plain".to_owned()),
            };
            if body.len() > MAX_REQUEST_BODY_LEN {
                bail!("the form data is too large to send");
            }
            Some((body, content_type))
        }
    };
    // Only a GET result can open in another tab; a POST stays here so it is
    // never silently sent twice.
    Ok(Request { url, new_tab: submit.new_tab && post.is_none(), post })
}

fn urlencoded(entries: &[(String, String)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in entries {
        s.append_pair(&normalize_newlines(k), &normalize_newlines(v));
    }
    s.finish()
}

/// Every line break becomes CRLF, as the HTML spec requires on submission.
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n")
}

/// A fresh random boundary per submission, in the style Firefox uses.
fn boundary() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness for the form boundary: {e}"))?;
    Ok(format!("----geckoformboundary{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

fn multipart(entries: &[(String, String)], boundary: &str) -> Vec<u8> {
    // Names are escaped the way the HTML spec says: quotes and line breaks
    // percent-encoded.
    let escape = |s: &str| normalize_newlines(s).replace('"', "%22").replace('\r', "%0D").replace('\n', "%0A");
    let mut out = Vec::new();
    for (k, v) in entries {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        out.extend_from_slice(format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", escape(k)).as_bytes());
        out.extend_from_slice(normalize_newlines(v).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

fn plain(entries: &[(String, String)]) -> String {
    entries.iter().map(|(k, v)| format!("{}={}\r\n", normalize_newlines(k), normalize_newlines(v))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit(action: &str, method: Method, encoding: FormEncoding) -> FormSubmit {
        FormSubmit {
            action: action.into(),
            method,
            encoding,
            entries: vec![("q".into(), "tor é&x".into()), ("t".into(), "a\nb".into())],
            new_tab: true,
        }
    }

    #[test]
    fn get_replaces_the_query_and_may_open_a_tab() {
        let r = to_request(submit("https://example.org/s?old=1#top", Method::Get, FormEncoding::UrlEncoded)).unwrap();
        assert_eq!(r.url.as_str(), "https://example.org/s?q=tor+%C3%A9%26x&t=a%0D%0Ab");
        assert!(r.post.is_none());
        assert!(r.new_tab);
    }

    #[test]
    fn post_bodies_are_encoded_by_the_broker() {
        let r = to_request(submit("https://example.org/login", Method::Post, FormEncoding::UrlEncoded)).unwrap();
        let (body, ct) = r.post.unwrap();
        assert_eq!(body, b"q=tor+%C3%A9%26x&t=a%0D%0Ab");
        assert_eq!(ct, "application/x-www-form-urlencoded");
        assert!(!r.new_tab);

        let r = to_request(submit("https://example.org/up", Method::Post, FormEncoding::Multipart)).unwrap();
        let (body, ct) = r.post.unwrap();
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(
            text.starts_with(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"q\"\r\n\r\ntor é&x\r\n"))
        );
        assert!(text.ends_with(&format!("a\r\nb\r\n--{boundary}--\r\n")));
    }

    #[test]
    fn policy_applies_to_form_targets() {
        assert!(to_request(submit("http://example.org/", Method::Get, FormEncoding::UrlEncoded)).is_err());
        assert!(to_request(submit("https://127.0.0.1/", Method::Post, FormEncoding::UrlEncoded)).is_err());
        assert!(to_request(submit("javascript:alert(1)", Method::Get, FormEncoding::UrlEncoded)).is_err());
    }
}
