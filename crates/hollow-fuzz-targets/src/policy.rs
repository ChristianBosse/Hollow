//! hollow-app: the broker's URL policy and form encoder.

use hollow_app::form;
use hollow_app::policy::{self, Extras};
use hollow_ipc::{FormEncoding, FormSubmit, MAX_REQUEST_BODY_LEN, Method};
use url::Url;

use crate::{SEP, fields, text};

/// `input SEP from SEP to`: what the user typed, and a link from one page
/// to another. Asserts the promises the policy makes: whatever
/// `parse_input` accepts also passes `check`; requests never carry a
/// fragment; referers never carry a fragment and never go from HTTPS to HTTP.
pub fn url_policy(data: &[u8]) {
    let [input, from, to] = fields(data, 3)[..] else { unreachable!() };
    let input = text(input);
    if let Ok(url) = policy::parse_input(&input) {
        assert_eq!(policy::check(&url), Ok(()), "parse_input accepted {url}");
        request(&url);
    }
    if let Ok(url) = Url::parse(&input)
        && policy::check(&url).is_ok()
    {
        assert!(matches!(url.scheme(), "https" | "http"), "check accepted {url}");
        assert!(url.scheme() == "https" || policy::is_onion(&url), "check accepted {url}");
        assert!(url.username().is_empty() && url.password().is_none(), "check accepted {url}");
        request(&url);
    }

    let Ok(from) = Url::parse(&text(from)) else { return };
    let Ok(to) = from.join(&text(to)) else { return };
    if let Some(r) = policy::referer(&from, &to) {
        assert!(!r.contains('#'), "referer {r} from {from} to {to}");
        assert!(!(from.scheme() == "https" && to.scheme() == "http"), "referer {r} from {from} to {to}");
        if from.origin() != to.origin() {
            assert!(r.ends_with('/') && !r.contains('?'), "cross-origin referer {r} from {from} to {to}");
        }
    }
}

fn request(url: &Url) {
    let key = policy::isolation_key(url);
    let req = policy::to_request(url, 1, &key, &Extras::default());
    assert!(!req.path_and_query.contains('#'), "request for {url}: {}", req.path_and_query);
    assert!(req.path_and_query.starts_with('/'), "request for {url}: {}", req.path_and_query);
    assert!(!req.host.is_empty(), "request for {url} has no host");
}

/// `flags action SEP name SEP value SEP name ...`: a submission as a
/// renderer could send it. Bit 0 of `flags` picks POST, bits 1-2 the
/// encoding, bit 3 `target=_blank`. Accepted requests must pass policy,
/// carry no fragment and fit the body limit; a multipart body must have
/// exactly one part per entry, whatever the names and values contain.
pub fn form_encode(data: &[u8]) {
    let Some((&flags, rest)) = data.split_first() else { return };
    let mut parts = rest.split(|&b| b == SEP);
    let action = text(parts.next().unwrap_or_default());
    let mut entries = Vec::new();
    while let Some(k) = parts.next() {
        entries.push((text(k), text(parts.next().unwrap_or_default())));
    }
    let method = if flags & 1 == 0 { Method::Get } else { Method::Post };
    let encoding = match (flags >> 1) % 4 {
        0 | 3 => FormEncoding::UrlEncoded,
        1 => FormEncoding::Multipart,
        _ => FormEncoding::Plain,
    };
    let n = entries.len();
    let submit = FormSubmit { action, method, encoding, entries, new_tab: flags & 8 != 0 };
    let Ok(req) = form::to_request(submit) else { return };
    assert_eq!(policy::check(&req.url), Ok(()), "form sent to {}", req.url);
    assert!(req.url.fragment().is_none(), "form sent to {}", req.url);
    match (&req.post, method) {
        (None, Method::Get) => assert!(req.url.as_str().len() <= MAX_REQUEST_BODY_LEN),
        (Some((body, content_type)), Method::Post) => {
            assert!(body.len() <= MAX_REQUEST_BODY_LEN);
            assert!(!req.new_tab, "a POST result opened in a new tab");
            if let Some(boundary) = content_type.strip_prefix("multipart/form-data; boundary=") {
                let delimiter = format!("--{boundary}\r\n");
                let count = body.windows(delimiter.len()).filter(|w| *w == delimiter.as_bytes()).count();
                assert_eq!(count, n, "multipart body has {count} parts for {n} entries");
            }
        }
        (post, method) => panic!("{method:?} submission with body {:?}", post.is_some()),
    }
}
