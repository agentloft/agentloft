//! Captured network exchanges as an agent gets them, the limits on their
//! bodies, and the redaction of a whole capture.
//!
//! Only the network capture's own files use this module.

use super::redaction::{MIN_IN_TEXT, Redactor};
use serde::Serialize;
use serde_json::{Value, json};

/// The most of one body kept as text.
pub(super) const BODY_LIMIT: u64 = 1_000_000;
/// The most of all of a capture's bodies kept as text.
pub(super) const CAPTURE_BODY_BUDGET: u64 = 25_000_000;
/// The most exchanges one capture keeps.
pub(super) const MAX_EXCHANGES: usize = 1000;

/// One request and what came of it.
#[derive(Serialize, Clone, Debug)]
pub(super) struct Exchange {
    /// `request-<n>`, n counting the capture's requests from 1.
    pub(super) id: String,
    pub(super) started: String,
    pub(super) duration_ms: u64,
    /// Bytes received over the network (headers included, compressed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) transfer_size: Option<u64>,
    pub(super) request: Request,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) response: Option<Response>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) failure: Option<String>,
    /// The origin of the page's frame of another site that made it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) frame: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub(super) struct Request {
    pub(super) method: String,
    pub(super) url: String,
    pub(super) headers: Vec<Header>,
    pub(super) resource_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) body: Option<Body>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) redirected_from: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub(super) struct Response {
    pub(super) status: i64,
    pub(super) status_text: String,
    pub(super) headers: Vec<Header>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) body: Option<Body>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub(super) struct Header {
    pub(super) name: String,
    pub(super) value: String,
}

/// A body: its text, or what is known of it and why it has no text.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub(super) struct Body {
    /// Decoded bytes, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) size: Option<u64>,
    pub(super) mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) text: Option<String>,
    pub(super) truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) omitted: Option<String>,
}

/// Why a body has no text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Omitted {
    /// Not a kind that is returned (only these can be checked for credentials).
    NotReturned,
    TooLarge,
    BudgetUsed,
    Unavailable,
    UnsupportedEncoding,
    Unredactable,
}

impl Omitted {
    fn reason(self) -> &'static str {
        match self {
            Omitted::NotReturned => {
                "not returned: only JSON, form and multipart bodies are, since others can't be checked for credentials"
            }
            Omitted::TooLarge => "larger than the 1 MB a body may be",
            Omitted::BudgetUsed => "the capture's 25 MB for bodies was used up",
            Omitted::Unavailable => "the browser no longer had it",
            Omitted::UnsupportedEncoding => "its content-encoding can't be decoded",
            Omitted::Unredactable => "it couldn't be parsed to check it for credentials",
        }
    }

    /// Kept out by a limit or a failure (counted), rather than by its kind.
    fn truncates(self) -> bool {
        self != Omitted::NotReturned
    }
}

impl Body {
    /// A body without its text.
    pub(super) fn described(size: Option<u64>, mime: Option<&str>, why: Omitted) -> Body {
        Body {
            size,
            mime_type: mime.unwrap_or("application/octet-stream").to_owned(),
            text: None,
            truncated: why.truncates(),
            omitted: Some(why.reason().to_owned()),
        }
    }
}

/// Whether a body of this MIME type is returned as text: only JSON, form
/// and multipart bodies can be checked for credentials.
pub(super) fn returnable(mime: Option<&str>) -> bool {
    mime.is_some_and(|mime| {
        let mime = mime.to_ascii_lowercase();
        mime.contains("json")
            || mime.contains("application/x-www-form-urlencoded")
            || mime.contains("multipart/form-data")
    })
}

/// A header's value, its name in any case.
pub(super) fn header<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// The `content-length` a message declares.
pub(super) fn declared_length(headers: &[Header]) -> Option<u64> {
    header(headers, "content-length")?.trim().parse().ok()
}

/// A body's MIME type: its `content-type`, parameters and all (multipart
/// needs its boundary).
pub(super) fn content_type(headers: &[Header]) -> Option<String> {
    header(headers, "content-type")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// What a capture has kept of bodies so far.
#[derive(Default, Debug)]
pub(super) struct Budget {
    /// Bytes kept as text.
    pub(super) used: u64,
    /// Bodies kept out by a limit or a failure.
    pub(super) truncated: u64,
}

impl Budget {
    pub(super) fn remaining(&self) -> u64 {
        CAPTURE_BODY_BUDGET.saturating_sub(self.used)
    }

    /// Whether a body of `size` (or its declared length) may be fetched.
    pub(super) fn fits(&self, headers: &[Header], size: Option<u64>) -> Result<(), Omitted> {
        if let Some(encoding) = header(headers, "content-encoding") {
            let encoding = encoding.trim().to_ascii_lowercase();
            if !matches!(
                encoding.as_str(),
                "identity" | "gzip" | "br" | "deflate" | "zstd"
            ) {
                return Err(Omitted::UnsupportedEncoding);
            }
        }
        let size = size.or_else(|| declared_length(headers));
        let remaining = self.remaining();
        match size {
            Some(size) if size > BODY_LIMIT => Err(Omitted::TooLarge),
            Some(size) if size > remaining => Err(Omitted::BudgetUsed),
            _ if remaining == 0 => Err(Omitted::BudgetUsed),
            _ => Ok(()),
        }
    }

    /// A fetched body, kept as text if the limits allow.
    pub(super) fn keep(&mut self, bytes: &[u8], mime: Option<&str>) -> Body {
        let size = bytes.len() as u64;
        if size > BODY_LIMIT {
            return self.described(Some(size), mime, Omitted::TooLarge);
        }
        if size > self.remaining() {
            return self.described(Some(size), mime, Omitted::BudgetUsed);
        }
        self.used += size;
        Body {
            size: Some(size),
            mime_type: mime.unwrap_or("application/octet-stream").to_owned(),
            text: Some(String::from_utf8_lossy(bytes).into_owned()),
            truncated: false,
            omitted: None,
        }
    }

    /// A body without text, counted if a limit or a failure kept it out.
    pub(super) fn described(
        &mut self,
        size: Option<u64>,
        mime: Option<&str>,
        why: Omitted,
    ) -> Body {
        if why.truncates() {
            self.truncated += 1;
        }
        Body::described(size, mime, why)
    }
}

/// Take credentials out of a whole capture, in place. The first pass takes
/// them out by name and by shape and learns each value; the second takes
/// every learned value out wherever else it shows up, earlier exchanges
/// included. Returns how many bodies lost their text (they couldn't be
/// checked).
pub(super) fn redact(exchanges: &mut [Exchange]) -> u64 {
    let mut redactor = Redactor::default();
    let mut dropped = 0;
    for exchange in exchanges.iter_mut() {
        let request = &mut exchange.request;
        redact_headers(&mut redactor, &mut request.headers);
        if let Some(response) = &mut exchange.response {
            redact_headers(&mut redactor, &mut response.headers);
        }
        dropped += redact_body(&mut redactor, &mut exchange.request.body);
        if let Some(response) = &mut exchange.response {
            dropped += redact_body(&mut redactor, &mut response.body);
        }
        let request = &mut exchange.request;
        request.url = redactor.url(&request.url);
        if let Some(from) = &mut request.redirected_from {
            *from = redactor.url(from);
        }
    }
    for exchange in exchanges.iter_mut() {
        let request = &mut exchange.request;
        request.url = redactor.known_in_text(&request.url, MIN_IN_TEXT);
        if let Some(from) = &mut request.redirected_from {
            *from = redactor.known_in_text(from, MIN_IN_TEXT);
        }
        let response = exchange.response.as_mut();
        let headers = request
            .headers
            .iter_mut()
            .chain(response.into_iter().flat_map(|r| r.headers.iter_mut()));
        for header in headers {
            header.value = redactor.known_in_text(&header.value, MIN_IN_TEXT);
        }
        let bodies = [
            exchange.request.body.as_mut(),
            exchange.response.as_mut().and_then(|r| r.body.as_mut()),
        ];
        for body in bodies.into_iter().flatten() {
            let Some(text) = &body.text else {
                continue;
            };
            let mime = body.mime_type.to_ascii_lowercase();
            let redacted = if mime.contains("json") {
                redactor.known_in_json(text)
            } else {
                Some(redactor.known_in_text(text, MIN_IN_TEXT))
            };
            match redacted {
                Some(text) => body.text = Some(text),
                None => {
                    unredactable(body);
                    dropped += 1;
                }
            }
        }
    }
    dropped
}

fn redact_headers(redactor: &mut Redactor, headers: &mut [Header]) {
    for header in headers {
        header.value = redactor.header(&header.name, &header.value);
    }
}

/// 1 if the body lost its text.
fn redact_body(redactor: &mut Redactor, body: &mut Option<Body>) -> u64 {
    let Some(body) = body else {
        return 0;
    };
    let Some(text) = &body.text else {
        return 0;
    };
    match redactor.body(&body.mime_type, text) {
        Some(text) => {
            body.text = Some(text);
            0
        }
        None => {
            unredactable(body);
            1
        }
    }
}

fn unredactable(body: &mut Body) {
    body.text = None;
    body.truncated = true;
    body.omitted = Some(Omitted::Unredactable.reason().to_owned());
}

/// An exchange in brief, for a capture too big to return whole.
pub(super) fn summary(exchange: &Exchange) -> Value {
    let request = &exchange.request;
    let mut brief = json!({
        "id": exchange.id,
        "method": request.method,
        "url": cut(&request.url, 500),
        "resource_type": request.resource_type,
        "duration_ms": exchange.duration_ms,
    });
    if let Some(response) = &exchange.response {
        brief["status"] = json!(response.status);
        if let Some(body) = &response.body {
            brief["mime_type"] = json!(body.mime_type);
            brief["size"] = json!(body.size);
        }
    }
    if let Some(size) = exchange.transfer_size {
        brief["transfer_size"] = json!(size);
    }
    if let Some(failure) = &exchange.failure {
        brief["failure"] = json!(failure);
    }
    if let Some(frame) = &exchange.frame {
        brief["frame"] = json!(frame);
    }
    brief
}

/// At most `max` bytes of `text` (at a character's boundary), with `…` when
/// cut.
pub(super) fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, value: &str) -> Header {
        Header {
            name: name.into(),
            value: value.into(),
        }
    }

    fn text_body(mime: &str, text: &str) -> Option<Body> {
        Some(Body {
            size: Some(text.len() as u64),
            mime_type: mime.into(),
            text: Some(text.into()),
            truncated: false,
            omitted: None,
        })
    }

    fn exchange(n: u32, method: &str, url: &str) -> Exchange {
        Exchange {
            id: format!("request-{n}"),
            started: "2026-01-01T00:00:00.000Z".into(),
            duration_ms: 3,
            transfer_size: None,
            request: Request {
                method: method.into(),
                url: url.into(),
                headers: Vec::new(),
                resource_type: "Fetch".into(),
                body: None,
                redirected_from: None,
            },
            response: None,
            failure: None,
            frame: None,
        }
    }

    #[test]
    fn a_whole_capture_is_redacted_together() {
        let mut login = exchange(
            1,
            "POST",
            "https://api.test/login?api_key=k1234567890&page=1",
        );
        login.request.headers = vec![
            h("Authorization", "Bearer tok_live_1234567890abcdef"),
            h("Cookie", "sid=abcdefghijklmnop"),
        ];
        login.request.body = text_body(
            "application/json",
            r#"{"username":"dax","password":"hunter2hunter2"}"#,
        );
        login.response = Some(Response {
            status: 200,
            status_text: "OK".into(),
            headers: vec![
                h("Set-Cookie", "sid=abcdefghijklmnop; Path=/; HttpOnly"),
                h("Content-Type", "application/json"),
            ],
            body: text_body(
                "application/json",
                r#"{"access_token":"tok_live_1234567890abcdef","echo":"your token is tok_live_1234567890abcdef","id":7}"#,
            ),
        });
        let next = exchange(
            2,
            "GET",
            "https://api.test/next?cursor=tok_live_1234567890abcdef",
        );
        let mut all = vec![login, next];
        assert_eq!(redact(&mut all), 0);
        let text = serde_json::to_string(&all).unwrap();
        for secret in [
            "tok_live_1234567890abcdef",
            "abcdefghijklmnop",
            "hunter2hunter2",
            "k1234567890",
        ] {
            assert!(!text.contains(secret), "{secret}: {text}");
        }
        let login = &all[0];
        assert_eq!(
            login.request.url,
            "https://api.test/login?api_key=[redacted]&page=1"
        );
        assert_eq!(login.request.headers[0].value, "Bearer [redacted]");
        assert_eq!(
            login.request.body.as_ref().unwrap().text.as_deref(),
            Some(r#"{"username":"dax","password":"[redacted]"}"#)
        );
        let response = login.response.as_ref().unwrap();
        assert_eq!(
            response.body.as_ref().unwrap().text.as_deref(),
            Some(r#"{"access_token":"[redacted]","echo":"your token is [redacted]","id":7}"#)
        );
        assert_eq!(
            response.headers[0].value,
            "sid=[redacted]; Path=/; HttpOnly"
        );
        assert_eq!(response.headers[1].value, "application/json");
        assert_eq!(
            all[1].request.url,
            "https://api.test/next?cursor=[redacted]"
        );
    }

    #[test]
    fn a_body_that_cant_be_checked_loses_its_text() {
        let mut broken = exchange(1, "POST", "https://api.test/x");
        broken.request.body = text_body("application/json", "{not json");
        let mut all = vec![broken];
        assert_eq!(redact(&mut all), 1);
        let body = all[0].request.body.as_ref().unwrap();
        assert_eq!(body.text, None);
        assert!(body.truncated);
        assert_eq!(
            body.omitted.as_deref(),
            Some(Omitted::Unredactable.reason())
        );
        // A described body is left as it was.
        let mut described = exchange(1, "GET", "https://api.test/x");
        described.request.body = Some(Body::described(Some(5), None, Omitted::NotReturned));
        let mut all = vec![described];
        assert_eq!(redact(&mut all), 0);
        assert!(!all[0].request.body.as_ref().unwrap().truncated);
    }

    #[test]
    fn the_json_shape() {
        let mut e = exchange(1, "GET", "https://x.test/");
        e.request.body = Some(Body::described(
            None,
            Some("text/html"),
            Omitted::NotReturned,
        ));
        let value = serde_json::to_value(&e).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["id", "started", "duration_ms", "request"]);
        assert_eq!(
            value["request"]["body"],
            json!({
                "mime_type": "text/html",
                "truncated": false,
                "omitted": Omitted::NotReturned.reason(),
            })
        );
        e.transfer_size = Some(10);
        e.frame = Some("http://localhost:1".into());
        e.response = Some(Response {
            status: 200,
            status_text: String::new(),
            headers: Vec::new(),
            body: Some(Body::described(None, None, Omitted::Unavailable)),
        });
        let brief = summary(&e);
        assert_eq!(
            brief,
            json!({
                "id": "request-1", "method": "GET", "url": "https://x.test/",
                "resource_type": "Fetch", "duration_ms": 3, "status": 200,
                "mime_type": "application/octet-stream", "size": null,
                "transfer_size": 10, "frame": "http://localhost:1",
            })
        );
        assert_eq!(cut("ab€cd", 3), "ab…");
        assert_eq!(cut("abc", 3), "abc");
    }

    #[test]
    fn body_limits() {
        let mut budget = Budget::default();
        let kept = budget.keep(b"{}", Some("application/json"));
        assert_eq!(kept.text.as_deref(), Some("{}"));
        assert_eq!(budget.used, 2);
        let big = vec![b'a'; 1_000_001];
        let too_big = budget.keep(&big, Some("application/json"));
        assert_eq!(too_big.text, None);
        assert!(too_big.truncated);
        assert_eq!(too_big.omitted.as_deref(), Some(Omitted::TooLarge.reason()));
        assert_eq!(too_big.size, Some(1_000_001));
        assert_eq!((budget.used, budget.truncated), (2, 1));

        assert!(returnable(Some("application/problem+json")));
        assert!(returnable(Some("Application/JSON; charset=utf-8")));
        assert!(returnable(Some("multipart/form-data; boundary=x")));
        assert!(!returnable(Some("text/html")));
        assert!(!returnable(None));

        let length = |n: &str| vec![h("Content-Length", n)];
        assert_eq!(
            Budget::default().fits(&length("2000000"), None),
            Err(Omitted::TooLarge)
        );
        let nearly_used = Budget {
            used: CAPTURE_BODY_BUDGET - 10,
            truncated: 0,
        };
        assert_eq!(
            nearly_used.fits(&length(" 20 "), None),
            Err(Omitted::BudgetUsed)
        );
        assert_eq!(nearly_used.fits(&length("5"), None), Ok(()));
        assert_eq!(nearly_used.fits(&[], Some(11)), Err(Omitted::BudgetUsed));
        assert_eq!(
            Budget::default().fits(&[h("content-encoding", "compress")], None),
            Err(Omitted::UnsupportedEncoding)
        );
        assert_eq!(
            Budget::default().fits(&[h("Content-Encoding", " GZIP ")], None),
            Ok(())
        );
        let used_up = Budget {
            used: CAPTURE_BODY_BUDGET,
            truncated: 0,
        };
        assert_eq!(used_up.fits(&[], None), Err(Omitted::BudgetUsed));
        let mut used_up = used_up;
        let kept = used_up.keep(b"x", None);
        assert_eq!(kept.omitted.as_deref(), Some(Omitted::BudgetUsed.reason()));
        assert_eq!(kept.mime_type, "application/octet-stream");
    }
}
