//! A tab's network capture over the DevTools `Network` domain (ADR-0017 §8).
//!
//! One capture at a time per tab. The domain is on only while one runs (or
//! emulated network conditions need it). Each request becomes an exchange
//! when it begins; when it ends (finished, failed, redirected, or the
//! capture stopped) its bodies are fetched in a task of its own, a few at a
//! time, and it joins the capture's exchanges. At the stop the whole
//! capture is redacted together (`network_model::redact`), so a credential
//! seen anywhere is taken out everywhere.
//!
//! Request ids are the browser's, shared by the page's session and its
//! frames' (`cross_site.rs`): a frame's own document is requested over the
//! page's session and answered over the frame's, and is one exchange.

use super::super::automation::Agent;
use super::super::cdp::{Cdp, Event};
use super::super::lock;
use super::cross_site::{self, Unfollow, Wanted};
use super::network_model::{
    self as model, Body, Budget, Exchange, Header, MAX_EXCHANGES, Omitted, Request, Response,
};
use super::{NetworkUse, PageCapture, network_off, save};
use base64::Engine as _;
use loft_page_script::{BrowserError, NetworkOptions, NetworkStopOptions};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};
use tokio::task::AbortHandle;

/// How long a capture runs when not told (`max_duration_ms`).
const DEFAULT_LONGEST_MS: u64 = 30 * 60 * 1000;
const LONGEST_ALLOWED_MS: std::ops::RangeInclusive<u64> = 1000..=86_400_000;
/// Exchanges having their bodies fetched at once.
const FINISHING_AT_ONCE: usize = 4;
/// A body fetch is given up after this long.
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a stop waits for exchanges still having their bodies fetched.
const DRAIN_WAIT: Duration = Duration::from_secs(5);
/// A stop result longer than this (as pretty JSON) is saved and returned in
/// brief.
const RETURNED_AT_MOST: usize = 1_000_000;
/// Raw headers kept for requests not seen yet (or no longer open).
const UNCLAIMED_HEADERS: usize = 4000;
const STOPPED_EARLY: &str = "the capture stopped before the request completed";

/// The network capture's say in whether the page's `Network` domain is on.
fn capture_flag(using: &mut NetworkUse) -> &mut bool {
    &mut using.capture
}

/// A page's network capture: the running one, and why the last one ended
/// if it ended by itself.
#[derive(Default)]
pub(super) struct NetworkState {
    running: Option<Arc<Capture>>,
    ended: Option<String>,
}

/// One capture.
struct Capture {
    started: String,
    ends: String,
    filter: Filter,
    bodies: bool,
    /// Where bodies are asked for (`None` only in tests: none can be).
    cdp: Option<Arc<Cdp>>,
    /// The page's own session, for events that name none.
    page_session: String,
    timer: Mutex<Option<AbortHandle>>,
    /// Exchanges having their bodies fetched take a slot each.
    slots: Semaphore,
    /// An exchange finished (a stop may be waiting for the last).
    finished_one: Notify,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// A stop is draining it: events are ignored.
    draining: bool,
    /// A stop has taken its exchanges: late ones are dropped.
    closed: bool,
    /// The last exchange's number.
    numbered: u64,
    /// Begun and not ended, by request id.
    open: HashMap<String, Open>,
    /// Ended, having their bodies fetched.
    finishing: usize,
    /// Ended and complete, with their numbers.
    finished: Vec<(u64, Exchange)>,
    /// Raw (on the wire) headers that came before their exchange began, or
    /// for a request id whose exchange already has its own (a redirect's
    /// next hop), by request id.
    raw_requests: HashMap<String, Value>,
    raw_responses: HashMap<String, Value>,
    dropped: u64,
    budget: Budget,
}

/// What a capture keeps.
struct Filter {
    url: Option<String>,
    types: Vec<String>,
    methods: Vec<String>,
}

impl Filter {
    fn keeps(&self, url: &str, method: &str, resource_type: &str) -> bool {
        self.url
            .as_ref()
            .is_none_or(|part| url.contains(part.as_str()))
            && (self.types.is_empty()
                || self
                    .types
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(resource_type)))
            && (self.methods.is_empty()
                || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
    }
}

/// A request begun and not yet ended.
struct Open {
    number: u64,
    seen: Instant,
    started: String,
    request_id: String,
    method: String,
    url: String,
    headers: Value,
    resource_type: String,
    post_data: Option<String>,
    has_post_data: bool,
    redirected_from: Option<String>,
    response: Option<Seen>,
    failure: Option<String>,
    frame: Option<String>,
    /// The session that saw it begin, and the last one that saw it.
    request_session: String,
    last_session: String,
    /// Decoded body bytes received so far.
    decoded: u64,
    transfer_size: Option<u64>,
    raw_request: Option<Value>,
    raw_response: Option<Value>,
}

impl Open {
    /// An event about it came over `session`, from `frame`.
    fn seen(&mut self, session: &str, frame: Option<&str>) {
        if self.frame.is_none() {
            self.frame = frame.map(str::to_owned);
        }
        session.clone_into(&mut self.last_session);
    }
}

/// A response as DevTools describes it.
struct Seen {
    status: i64,
    status_text: String,
    headers: Value,
    mime_type: Option<String>,
}

impl Seen {
    fn of(response: &Value) -> Seen {
        Seen {
            status: response["status"]
                .as_i64()
                .or_else(|| response["status"].as_f64().map(|s| s as i64))
                .unwrap_or(0),
            status_text: response["statusText"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            headers: response["headers"].clone(),
            mime_type: response["mimeType"]
                .as_str()
                .filter(|m| !m.is_empty())
                .map(str::to_owned),
        }
    }
}

/// A DevTools headers object, in its order.
fn headers_of(headers: &Value) -> Vec<Header> {
    headers
        .as_object()
        .map(|headers| {
            headers
                .iter()
                .map(|(name, value)| Header {
                    name: name.clone(),
                    value: match value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    },
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Raw headers if there are any, else the event's.
fn chosen_headers(raw: Option<&Value>, event: &Value) -> Vec<Header> {
    match raw.map(headers_of).filter(|h| !h.is_empty()) {
        Some(raw) => raw,
        None => headers_of(event),
    }
}

/// Raw headers for a request id: to its open exchange if that has none yet,
/// else kept until one begins.
fn raw_headers(
    store: &mut HashMap<String, Value>,
    slot: Option<&mut Option<Value>>,
    id: &str,
    headers: &Value,
) {
    match slot {
        Some(slot @ None) => *slot = Some(headers.clone()),
        _ => {
            if store.len() >= UNCLAIMED_HEADERS {
                store.clear();
            }
            store.insert(id.to_owned(), headers.clone());
        }
    }
}

impl Capture {
    fn new(
        options: &NetworkOptions,
        longest_ms: u64,
        cdp: Option<Arc<Cdp>>,
        page_session: String,
    ) -> Capture {
        let now = chrono::Utc::now();
        let iso = |t: chrono::DateTime<chrono::Utc>| {
            t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        let ends = now + chrono::Duration::milliseconds(longest_ms as i64);
        Capture {
            started: iso(now),
            ends: iso(ends),
            filter: Filter {
                url: options.url_filter.clone().filter(|u| !u.is_empty()),
                types: options.resource_types.clone(),
                methods: options.methods.clone(),
            },
            bodies: options.bodies.unwrap_or(true),
            cdp,
            page_session,
            timer: Mutex::new(None),
            slots: Semaphore::new(FINISHING_AT_ONCE),
            finished_one: Notify::new(),
            inner: Mutex::new(Inner::default()),
        }
    }

    fn status(&self) -> Value {
        let inner = lock(&self.inner);
        let exchanges = || inner.finished.iter().map(|(_, e)| e);
        json!({
            "active": true,
            "started": self.started,
            "ends": self.ends,
            "entry_count": inner.finished.len(),
            "pending_count": inner.open.len() + inner.finishing,
            "response_count": exchanges().filter(|e| e.response.is_some()).count(),
            "failure_count": exchanges().filter(|e| e.failure.is_some()).count(),
            "captured_body_bytes": inner.budget.used,
            "truncated_body_count": inner.budget.truncated,
            "dropped_entry_count": inner.dropped,
        })
    }

    fn stop_timer(&self) {
        if let Some(timer) = lock(&self.timer).take() {
            timer.abort();
        }
    }

    /// Ended without a stop (cancelled, or failed to start): nothing more
    /// is kept.
    fn discard(&self) {
        self.stop_timer();
        self.close();
    }

    /// Nothing more is kept, and what was is let go.
    fn close(&self) {
        let mut inner = lock(&self.inner);
        inner.draining = true;
        inner.closed = true;
        inner.open.clear();
        inner.finished.clear();
    }

    /// A `Network.*` event, over `session` (`None`: the page's), from
    /// `frame`.
    fn observe(self: &Arc<Self>, event: &Event, frame: Option<&str>) {
        let params = &event.params;
        let id = params["requestId"].as_str().unwrap_or_default();
        let session = event.session.as_deref().unwrap_or(&self.page_session);
        let mut inner = lock(&self.inner);
        if inner.draining {
            return;
        }
        let inner = &mut *inner;
        match event.method.as_str() {
            "Network.requestWillBeSent" => self.begin(inner, params, id, session, frame),
            "Network.requestWillBeSentExtraInfo" => raw_headers(
                &mut inner.raw_requests,
                inner.open.get_mut(id).map(|o| &mut o.raw_request),
                id,
                &params["headers"],
            ),
            "Network.responseReceivedExtraInfo" => raw_headers(
                &mut inner.raw_responses,
                inner.open.get_mut(id).map(|o| &mut o.raw_response),
                id,
                &params["headers"],
            ),
            "Network.responseReceived" => {
                if let Some(open) = inner.open.get_mut(id) {
                    open.seen(session, frame);
                    open.response = Some(Seen::of(&params["response"]));
                }
            }
            "Network.dataReceived" => {
                if let Some(open) = inner.open.get_mut(id) {
                    open.seen(session, frame);
                    open.decoded += params["dataLength"].as_f64().unwrap_or(0.0).max(0.0) as u64;
                }
            }
            "Network.loadingFinished" => {
                if let Some(mut open) = inner.open.remove(id) {
                    open.seen(session, frame);
                    open.transfer_size = params["encodedDataLength"]
                        .as_f64()
                        .map(|n| n.max(0.0) as u64);
                    self.finish(inner, open);
                }
            }
            "Network.loadingFailed" => {
                if let Some(mut open) = inner.open.remove(id) {
                    open.seen(session, frame);
                    open.failure = Some(
                        params["errorText"]
                            .as_str()
                            .filter(|t| !t.is_empty())
                            .unwrap_or("Request failed")
                            .to_owned(),
                    );
                    self.finish(inner, open);
                }
            }
            _ => {}
        }
    }

    /// `Network.requestWillBeSent`: a request begins (or a redirect's next
    /// hop: the hop before ends with its redirect as its response).
    fn begin(
        self: &Arc<Self>,
        inner: &mut Inner,
        params: &Value,
        id: &str,
        session: &str,
        frame: Option<&str>,
    ) {
        let mut redirected_from = None;
        if let Some(redirect) = params.get("redirectResponse").filter(|r| r.is_object()) {
            match inner.open.remove(id) {
                Some(mut previous) => {
                    previous.seen(session, frame);
                    previous.response = Some(Seen::of(redirect));
                    redirected_from = Some(previous.url.clone());
                    self.finish(inner, previous);
                }
                None => redirected_from = redirect["url"].as_str().map(str::to_owned),
            }
        }
        let request = &params["request"];
        let url = request["url"].as_str().unwrap_or_default();
        let method = request["method"].as_str().unwrap_or("GET");
        let resource_type = params["type"].as_str().unwrap_or("Other");
        if !self.filter.keeps(url, method, resource_type) {
            inner.raw_requests.remove(id);
            inner.raw_responses.remove(id);
            return;
        }
        if inner.finished.len() + inner.finishing + inner.open.len() >= MAX_EXCHANGES {
            inner.dropped += 1;
            inner.raw_requests.remove(id);
            inner.raw_responses.remove(id);
            return;
        }
        inner.numbered += 1;
        let open = Open {
            number: inner.numbered,
            seen: Instant::now(),
            started: super::now_iso(),
            request_id: id.to_owned(),
            method: method.to_owned(),
            url: url.to_owned(),
            headers: request["headers"].clone(),
            resource_type: resource_type.to_owned(),
            post_data: post_data(request),
            has_post_data: request["hasPostData"].as_bool().unwrap_or(false),
            redirected_from,
            response: None,
            failure: None,
            frame: frame.map(str::to_owned),
            request_session: session.to_owned(),
            last_session: session.to_owned(),
            decoded: 0,
            transfer_size: None,
            raw_request: inner.raw_requests.remove(id),
            raw_response: inner.raw_responses.remove(id),
        };
        inner.open.insert(id.to_owned(), open);
    }

    /// An exchange ended: its bodies are fetched in a task of its own, then
    /// it joins the finished ones.
    fn finish(self: &Arc<Self>, inner: &mut Inner, open: Open) {
        inner.finishing += 1;
        let capture = self.clone();
        tokio::spawn(async move {
            let exchange = capture.complete(open).await;
            let mut inner = lock(&capture.inner);
            if inner.closed {
                // Too late: the stop counted it as dropped.
                return;
            }
            inner.finishing -= 1;
            inner.finished.push(exchange);
            drop(inner);
            capture.finished_one.notify_waiters();
        });
    }

    /// The exchange, with its bodies.
    async fn complete(&self, open: Open) -> (u64, Exchange) {
        let _slot = self.slots.acquire().await;
        let request_headers = chosen_headers(open.raw_request.as_ref(), &open.headers);
        let response_headers = open
            .response
            .as_ref()
            .map(|seen| chosen_headers(open.raw_response.as_ref(), &seen.headers));
        let (request_body, response_body) = if self.bodies {
            let request_body = self.request_body(&open, &request_headers).await;
            let response_body = match (&open.response, &response_headers) {
                (Some(seen), Some(headers)) => self.response_body(&open, seen, headers).await,
                _ => None,
            };
            (request_body, response_body)
        } else {
            (None, None)
        };
        let response = open.response.map(|seen| Response {
            status: seen.status,
            status_text: seen.status_text,
            headers: response_headers.unwrap_or_default(),
            body: response_body,
        });
        let exchange = Exchange {
            id: format!("request-{}", open.number),
            started: open.started,
            duration_ms: open.seen.elapsed().as_millis() as u64,
            transfer_size: open.transfer_size,
            request: Request {
                method: open.method,
                url: open.url,
                headers: request_headers,
                resource_type: open.resource_type,
                body: request_body,
                redirected_from: open.redirected_from,
            },
            response,
            failure: open.failure,
            frame: open.frame,
        };
        (open.number, exchange)
    }

    async fn request_body(&self, open: &Open, headers: &[Header]) -> Option<Body> {
        let mime = model::content_type(headers);
        let mime = mime.as_deref();
        let size = open
            .post_data
            .as_ref()
            .map(|d| d.len() as u64)
            .or_else(|| model::declared_length(headers));
        if size == Some(0) || (size.is_none() && !open.has_post_data) {
            return None;
        }
        if !model::returnable(mime) {
            return Some(Body::described(size, mime, Omitted::NotReturned));
        }
        if let Some(text) = &open.post_data {
            return Some(lock(&self.inner).budget.keep(text.as_bytes(), mime));
        }
        {
            let mut inner = lock(&self.inner);
            if let Err(why) = inner.budget.fits(headers, size) {
                return Some(inner.budget.described(size, mime, why));
            }
        }
        let answer = self
            .fetch(
                &open.request_session,
                "Network.getRequestPostData",
                &open.request_id,
            )
            .await;
        let bytes = answer.and_then(|a| decoded(&a, "postData"));
        let mut inner = lock(&self.inner);
        Some(match bytes {
            Some(bytes) => inner.budget.keep(&bytes, mime),
            None => inner.budget.described(size, mime, Omitted::Unavailable),
        })
    }

    async fn response_body(&self, open: &Open, seen: &Seen, headers: &[Header]) -> Option<Body> {
        let bodiless = matches!(seen.status, 101 | 204 | 205 | 304)
            || ((300..400).contains(&seen.status) && model::header(headers, "location").is_some());
        if open.failure.is_some() || bodiless {
            return None;
        }
        let mime = model::content_type(headers).or_else(|| seen.mime_type.clone());
        let mime = mime.as_deref();
        let size = Some(open.decoded)
            .filter(|&d| d > 0)
            .or_else(|| model::declared_length(headers));
        if size == Some(0) {
            return None;
        }
        if !model::returnable(mime) {
            return Some(Body::described(size, mime, Omitted::NotReturned));
        }
        {
            let mut inner = lock(&self.inner);
            if let Err(why) = inner.budget.fits(headers, size) {
                return Some(inner.budget.described(size, mime, why));
            }
        }
        let mut answer = self
            .fetch(
                &open.last_session,
                "Network.getResponseBody",
                &open.request_id,
            )
            .await;
        if answer.is_none() && open.request_session != open.last_session {
            answer = self
                .fetch(
                    &open.request_session,
                    "Network.getResponseBody",
                    &open.request_id,
                )
                .await;
        }
        let bytes = answer.and_then(|a| decoded(&a, "body"));
        let mut inner = lock(&self.inner);
        match bytes {
            Some(bytes) if bytes.is_empty() => None,
            Some(bytes) => Some(inner.budget.keep(&bytes, mime)),
            None => Some(inner.budget.described(size, mime, Omitted::Unavailable)),
        }
    }

    /// A body-fetching command's answer, given up on after a while.
    async fn fetch(&self, session: &str, method: &str, request_id: &str) -> Option<Value> {
        let cdp = self.cdp.as_ref()?;
        let asked = cdp.send_within(
            method,
            json!({ "requestId": request_id }),
            Some(session),
            FETCH_TIMEOUT,
        );
        tokio::time::timeout(FETCH_TIMEOUT, asked).await.ok()?.ok()
    }

    /// The stop: every exchange still open ends as stopped, then the
    /// capture waits a while for the ones having their bodies fetched.
    /// Returns its exchanges in the order their requests began.
    async fn drain(self: &Arc<Self>) -> Vec<Exchange> {
        {
            let mut inner = lock(&self.inner);
            inner.draining = true;
            let open: Vec<Open> = inner.open.drain().map(|(_, open)| open).collect();
            for mut open in open {
                open.failure.get_or_insert_with(|| STOPPED_EARLY.to_owned());
                open.response = None;
                self.finish(&mut inner, open);
            }
        }
        let deadline = tokio::time::Instant::now() + DRAIN_WAIT;
        loop {
            let finished_one = self.finished_one.notified();
            tokio::pin!(finished_one);
            finished_one.as_mut().enable();
            if lock(&self.inner).finishing == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, finished_one)
                .await
                .is_err()
            {
                break;
            }
        }
        let mut inner = lock(&self.inner);
        inner.closed = true;
        inner.dropped += inner.finishing as u64;
        let mut finished = std::mem::take(&mut inner.finished);
        finished.sort_by_key(|(number, _)| *number);
        finished.into_iter().map(|(_, exchange)| exchange).collect()
    }
}

/// A request's body as the event carries it (up to 64 KiB, `mod.rs`).
fn post_data(request: &Value) -> Option<String> {
    if let Some(text) = request["postData"].as_str() {
        return Some(text.to_owned());
    }
    // Newer browsers may give only the entries, base64.
    let entries = request["postDataEntries"].as_array()?;
    let mut bytes = Vec::new();
    for entry in entries {
        let piece = base64::engine::general_purpose::STANDARD
            .decode(entry["bytes"].as_str()?)
            .ok()?;
        bytes.extend(piece);
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A body from a DevTools answer's `field`, base64-decoded if it says so.
fn decoded(answer: &Value, field: &str) -> Option<Vec<u8>> {
    let text = answer[field].as_str()?;
    if answer["base64Encoded"].as_bool().unwrap_or(false) {
        base64::engine::general_purpose::STANDARD.decode(text).ok()
    } else {
        Some(text.as_bytes().to_vec())
    }
}

/// An event of the page's (or of its frame of another site, `frame`), for
/// its capture if one runs.
pub(super) fn observe(state: &PageCapture, event: &Event, frame: Option<&str>) {
    let Some(capture) = lock(&state.network).running.clone() else {
        return;
    };
    capture.observe(event, frame);
}

/// Whether a capture runs.
pub(super) fn running(state: &PageCapture) -> bool {
    lock(&state.network).running.is_some()
}

/// `network_status`: counts only.
pub(super) fn status(state: &PageCapture) -> Value {
    let network = lock(&state.network);
    match &network.running {
        Some(capture) => capture.status(),
        None => match &network.ended {
            Some(reason) => json!({ "active": false, "ended": reason }),
            None => json!({ "active": false }),
        },
    }
}

/// A capture, running from now (none must be).
fn install(
    state: &PageCapture,
    options: &NetworkOptions,
    cdp: Option<Arc<Cdp>>,
    page_session: &str,
) -> Result<(Arc<Capture>, u64), BrowserError> {
    let longest = options.max_duration_ms.unwrap_or(DEFAULT_LONGEST_MS);
    if !LONGEST_ALLOWED_MS.contains(&longest) {
        return Err(BrowserError::Action(
            "max_duration_ms must be from 1000 to 86400000".into(),
        ));
    }
    let mut network = lock(&state.network);
    if network.running.is_some() {
        return Err(BrowserError::Action(
            "a network capture already runs in this tab: stop or cancel it first".into(),
        ));
    }
    let capture = Arc::new(Capture::new(options, longest, cdp, page_session.to_owned()));
    network.ended = None;
    network.running = Some(capture.clone());
    Ok((capture, longest))
}

/// Take the capture away if it is still `capture` (saying why, if it ended
/// by itself).
fn take_if(
    state: &PageCapture,
    capture: &Weak<Capture>,
    ended: Option<String>,
) -> Option<Arc<Capture>> {
    let mut network = lock(&state.network);
    let ours = network
        .running
        .as_ref()
        .is_some_and(|running| Arc::as_ptr(running) == capture.as_ptr());
    if !ours {
        return None;
    }
    network.ended = ended;
    network.running.take()
}

/// `network_start`.
pub(super) async fn start(
    agent: &Agent<'_>,
    state: &Arc<PageCapture>,
    options: NetworkOptions,
) -> Result<Value, BrowserError> {
    let (capture, longest) = install(
        state,
        &options,
        Some(agent.browser.cdp.clone()),
        &agent.page.session,
    )?;
    // Running before the domain is on, so its first events are seen.
    if let Err(error) = agent.network_on(state, capture_flag).await {
        if let Some(capture) = take_if(state, &Arc::downgrade(&capture), None) {
            capture.discard();
        }
        return Err(error);
    }
    cross_site::follow(agent, Wanted::NETWORK).await;
    let result = status(state);
    let timer = {
        let (page_state, ours) = (Arc::downgrade(state), Arc::downgrade(&capture));
        let (cdp, session) = (agent.browser.cdp.clone(), agent.page.session.clone());
        let unfollow = Unfollow::of(agent);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(longest)).await;
            let Some(state) = page_state.upgrade() else {
                return;
            };
            let reason = format!(
                "it reached its longest ({} s, max_duration_ms) and what it captured was discarded",
                longest as f64 / 1000.0
            );
            let Some(capture) = take_if(&state, &ours, Some(reason)) else {
                return;
            };
            // Not `discard`: that would abort this very task.
            capture.close();
            network_off(&cdp, &session, &state, capture_flag).await;
            unfollow.run(&state, Wanted::NETWORK).await;
        })
        .abort_handle()
    };
    *lock(&capture.timer) = Some(timer);
    Ok(result)
}

/// `network_stop`: every exchange, redacted.
pub(super) async fn stop(
    agent: &Agent<'_>,
    state: &PageCapture,
    options: NetworkStopOptions,
) -> Result<Value, BrowserError> {
    let capture = {
        let mut network = lock(&state.network);
        match network.running.take() {
            Some(capture) => capture,
            None => {
                return Err(BrowserError::Action(match network.ended.take() {
                    Some(reason) => format!(
                        "no network capture is running in this tab: the last one ended because {reason}; \
                         start a new one"
                    ),
                    None => "no network capture is running in this tab; start one first".into(),
                }));
            }
        }
    };
    capture.stop_timer();
    let mut exchanges = capture.drain().await;
    network_off(&agent.browser.cdp, &agent.page.session, state, capture_flag).await;
    Unfollow::of(agent).run(state, Wanted::NETWORK).await;
    let result = stopped(&capture, &mut exchanges);
    respond(result, &exchanges, options.save, |text| {
        save(agent.page, "network", "json", text.as_bytes())
    })
}

/// A stopped capture's result, its exchanges redacted.
fn stopped(capture: &Capture, exchanges: &mut [Exchange]) -> Value {
    let lost_bodies = model::redact(exchanges);
    let inner = lock(&capture.inner);
    json!({
        "active": false,
        "started": capture.started,
        "stopped": super::now_iso(),
        "entry_count": exchanges.len(),
        "response_count": exchanges.iter().filter(|e| e.response.is_some()).count(),
        "failure_count": exchanges.iter().filter(|e| e.failure.is_some()).count(),
        "captured_body_bytes": inner.budget.used,
        "truncated_body_count": inner.budget.truncated + lost_bodies,
        "dropped_entry_count": inner.dropped,
        "exchanges": exchanges,
    })
}

/// The result as returned: saved too if asked or if too big to return, and
/// then, if too big, in brief.
fn respond(
    mut result: Value,
    exchanges: &[Exchange],
    save: bool,
    save_text: impl FnOnce(&str) -> Result<std::path::PathBuf, BrowserError>,
) -> Result<Value, BrowserError> {
    let text = serde_json::to_string_pretty(&result)
        .map_err(|e| BrowserError::Action(format!("couldn't write the capture: {e}")))?;
    let too_big = text.len() > RETURNED_AT_MOST;
    if !too_big && !save {
        return Ok(result);
    }
    let path = save_text(&text)?;
    result["path"] = json!(path);
    if too_big {
        result["note"] = json!(format!(
            "the whole capture ({} KB) is in the file at path; here each exchange is given without \
             its headers and bodies",
            text.len() / 1000
        ));
        let briefs: Vec<Value> = exchanges.iter().map(model::summary).collect();
        result["exchanges"] = json!(briefs);
    }
    Ok(result)
}

/// `network_cancel`: everything discarded.
pub(super) async fn cancel(agent: &Agent<'_>, state: &PageCapture) -> Result<Value, BrowserError> {
    let capture = {
        let mut network = lock(&state.network);
        network.ended = None;
        network.running.take()
    };
    let Some(capture) = capture else {
        return Ok(json!({ "cancelled": false }));
    };
    capture.discard();
    network_off(&agent.browser.cdp, &agent.page.session, state, capture_flag).await;
    Unfollow::of(agent).run(state, Wanted::NETWORK).await;
    Ok(json!({ "cancelled": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(method: &str, params: Value, session: Option<&str>) -> Event {
        Event {
            method: method.into(),
            params,
            session: session.map(str::to_owned),
        }
    }

    fn request(id: &str, url: &str, kind: &str, extra: Value) -> Event {
        let mut params = json!({
            "requestId": id,
            "type": kind,
            "request": { "url": url, "method": "GET", "headers": { "Accept": "*/*" } },
        });
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                params.insert(key.clone(), value.clone());
            }
        }
        event("Network.requestWillBeSent", params, None)
    }

    fn response(id: &str, status: u32, headers: Value) -> Event {
        event(
            "Network.responseReceived",
            json!({ "requestId": id, "response": {
                "status": status, "statusText": "", "headers": headers, "mimeType": "text/html",
            } }),
            None,
        )
    }

    fn finished(id: &str, session: Option<&str>) -> Event {
        event(
            "Network.loadingFinished",
            json!({ "requestId": id, "encodedDataLength": 321.0 }),
            session,
        )
    }

    fn started(options: Value) -> (PageCapture, Arc<Capture>) {
        let state = PageCapture::default();
        let options: NetworkOptions = serde_json::from_value(options).unwrap();
        let (capture, _) = install(&state, &options, None, "page").unwrap();
        (state, capture)
    }

    async fn settled(capture: &Capture) {
        for _ in 0..200 {
            if lock(&capture.inner).finishing == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("exchanges never finished");
    }

    #[tokio::test]
    async fn exchanges_follow_their_requests() {
        let (state, capture) = started(json!({}));
        assert!(running(&state));
        assert!(install(&state, &NetworkOptions::default(), None, "page").is_err());
        let first = status(&state);
        assert_eq!(first["active"], true);
        assert_eq!(first["entry_count"], 0);

        // Raw headers can come before their request.
        observe(
            &state,
            &event(
                "Network.requestWillBeSentExtraInfo",
                json!({ "requestId": "1", "headers": { "Accept": "*/*", "Cookie": "sid=abc" } }),
                None,
            ),
            None,
        );
        observe(
            &state,
            &request("1", "http://x.test/", "Document", json!({})),
            None,
        );
        observe(
            &state,
            &response("1", 302, json!({ "Location": "/home" })),
            None,
        );
        // A redirect: the hop before ends with it as its response.
        observe(
            &state,
            &request(
                "1",
                "http://x.test/home",
                "Document",
                json!({ "redirectResponse": { "status": 302, "headers": { "Location": "/home" } } }),
            ),
            None,
        );
        // A frame's document: requested over the page's session, answered
        // over the frame's.
        observe(
            &state,
            &request("2", "http://localhost:1/frame", "Document", json!({})),
            None,
        );
        observe(
            &state,
            &event(
                "Network.responseReceived",
                json!({ "requestId": "2", "response": { "status": 200, "headers": {} } }),
                Some("frame"),
            ),
            Some("http://localhost:1"),
        );
        observe(
            &state,
            &finished("2", Some("frame")),
            Some("http://localhost:1"),
        );
        // A POST with its JSON body inline.
        observe(
            &state,
            &event(
                "Network.requestWillBeSent",
                json!({ "requestId": "3", "type": "Fetch", "request": {
                    "url": "http://x.test/api", "method": "POST",
                    "headers": { "Content-Type": "application/json" },
                    "postData": "{\"password\":\"hunter2hunter2\"}", "hasPostData": true,
                } }),
                None,
            ),
            None,
        );
        observe(
            &state,
            &event(
                "Network.loadingFailed",
                json!({ "requestId": "3", "errorText": "" }),
                None,
            ),
            None,
        );
        // Events about requests the capture doesn't know are ignored.
        observe(&state, &finished("nobody", None), None);
        observe(&state, &response("1", 200, json!({})), None);
        settled(&capture).await;
        let now = status(&state);
        assert_eq!(now["entry_count"], 3, "{now}");
        assert_eq!(now["pending_count"], 1, "{now}");
        assert_eq!(now["failure_count"], 1, "{now}");
        assert_eq!(now["captured_body_bytes"], 29, "{now}");

        let mut exchanges = capture.drain().await;
        let ids: Vec<&str> = exchanges.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["request-1", "request-2", "request-3", "request-4"]);
        let hop = &exchanges[0];
        assert_eq!(hop.response.as_ref().unwrap().status, 302);
        assert_eq!(
            hop.request.headers,
            headers_of(&json!({ "Accept": "*/*", "Cookie": "sid=abc" }))
        );
        assert!(hop.failure.is_none());
        let home = &exchanges[1];
        assert_eq!(home.id, "request-2");
        assert_eq!(home.request.url, "http://x.test/home");
        assert_eq!(
            home.request.redirected_from.as_deref(),
            Some("http://x.test/")
        );
        assert_eq!(home.failure.as_deref(), Some(STOPPED_EARLY));
        assert!(home.response.is_none());
        let frame = &exchanges[2];
        assert_eq!(frame.frame.as_deref(), Some("http://localhost:1"));
        assert_eq!(frame.transfer_size, Some(321));
        assert_eq!(frame.response.as_ref().unwrap().status, 200);
        let post = &exchanges[3];
        assert_eq!(post.failure.as_deref(), Some("Request failed"));
        assert!(post.response.is_none());

        model::redact(&mut exchanges);
        let text = serde_json::to_string(&exchanges).unwrap();
        assert!(
            !text.contains("hunter2hunter2") && !text.contains("sid=abc"),
            "{text}"
        );
        assert_eq!(
            exchanges[3].request.body.as_ref().unwrap().text.as_deref(),
            Some("{\"password\":\"[redacted]\"}")
        );
        // A stopped capture ignores what comes after.
        capture.observe(
            &request("9", "http://x.test/late", "Fetch", json!({})),
            None,
        );
        assert!(lock(&capture.inner).open.is_empty());
    }

    #[tokio::test]
    async fn filters_and_bodies() {
        let (state, capture) = started(json!({
            "url_filter": "/api/", "resource_types": ["fetch", "XHR"], "methods": ["post"],
        }));
        let post = |id: &str, url: &str, kind: &str, method: &str| {
            event(
                "Network.requestWillBeSent",
                json!({ "requestId": id, "type": kind, "request": {
                    "url": url, "method": method, "headers": {},
                } }),
                None,
            )
        };
        observe(
            &state,
            &post("1", "http://x.test/api/a", "Fetch", "POST"),
            None,
        );
        observe(
            &state,
            &post("2", "http://x.test/api/b", "Document", "POST"),
            None,
        );
        observe(
            &state,
            &post("3", "http://x.test/api/c", "XHR", "GET"),
            None,
        );
        observe(
            &state,
            &post("4", "http://x.test/other", "Fetch", "POST"),
            None,
        );
        observe(
            &state,
            &post("5", "http://x.test/API/d", "Fetch", "POST"),
            None,
        );
        assert_eq!(status(&state)["pending_count"], 1);
        drop(capture);

        // Without bodies, none is fetched or described.
        let (state, capture) = started(json!({ "bodies": false }));
        observe(
            &state,
            &event(
                "Network.requestWillBeSent",
                json!({ "requestId": "1", "type": "Fetch", "request": {
                    "url": "http://x.test/", "method": "POST",
                    "headers": { "Content-Type": "application/json" }, "postData": "{}",
                } }),
                None,
            ),
            None,
        );
        observe(
            &state,
            &response("1", 200, json!({ "Content-Type": "application/json" })),
            None,
        );
        observe(&state, &finished("1", None), None);
        settled(&capture).await;
        let exchanges = capture.drain().await;
        assert!(exchanges[0].request.body.is_none());
        assert!(exchanges[0].response.as_ref().unwrap().body.is_none());

        // With bodies: HTML is only described; a body the browser can't be
        // asked for is unavailable; a 204 has none.
        let (state, capture) = started(json!({}));
        for (id, status, mime) in [
            ("1", 200, "text/html"),
            ("2", 200, "application/json"),
            ("3", 204, "application/json"),
        ] {
            observe(
                &state,
                &request(id, "http://x.test/", "Fetch", json!({})),
                None,
            );
            observe(
                &state,
                &response(id, status, json!({ "Content-Type": mime })),
                None,
            );
            observe(
                &state,
                &event(
                    "Network.dataReceived",
                    json!({ "requestId": id, "dataLength": 10 }),
                    None,
                ),
                None,
            );
            observe(&state, &finished(id, None), None);
        }
        settled(&capture).await;
        assert_eq!(status(&state)["truncated_body_count"], 1);
        let exchanges = capture.drain().await;
        let body = |n: usize| exchanges[n].response.as_ref().unwrap().body.clone();
        let html = body(0).unwrap();
        assert_eq!((html.size, html.truncated), (Some(10), false));
        assert_eq!(html.mime_type, "text/html");
        let gone = body(1).unwrap();
        assert!(gone.truncated && gone.text.is_none());
        assert!(body(2).is_none());
    }

    #[tokio::test]
    async fn a_capture_keeps_at_most_its_share() {
        let (state, capture) = started(json!({}));
        for n in 0..(MAX_EXCHANGES + 3) {
            observe(
                &state,
                &request(&n.to_string(), "http://x.test/", "Fetch", json!({})),
                None,
            );
        }
        let now = status(&state);
        assert_eq!(now["pending_count"], MAX_EXCHANGES);
        assert_eq!(now["dropped_entry_count"], 3);
        let exchanges = capture.drain().await;
        assert_eq!(exchanges.len(), MAX_EXCHANGES);
        assert_eq!(
            exchanges.last().unwrap().id,
            format!("request-{MAX_EXCHANGES}")
        );
    }

    #[test]
    fn durations_are_checked() {
        let state = PageCapture::default();
        for wrong in [999, 86_400_001] {
            let options = NetworkOptions {
                max_duration_ms: Some(wrong),
                ..NetworkOptions::default()
            };
            let error = install(&state, &options, None, "page").err().unwrap();
            assert!(error.to_string().contains("max_duration_ms"), "{error}");
        }
        assert!(!running(&state));
        assert_eq!(status(&state), json!({ "active": false }));
    }

    #[test]
    fn a_result_too_big_is_saved_and_given_in_brief() {
        let mut exchange = Exchange {
            id: "request-1".into(),
            started: "2026-01-01T00:00:00.000Z".into(),
            duration_ms: 1,
            transfer_size: None,
            request: Request {
                method: "GET".into(),
                url: "http://x.test/".into(),
                headers: Vec::new(),
                resource_type: "Fetch".into(),
                body: None,
                redirected_from: None,
            },
            response: None,
            failure: None,
            frame: None,
        };
        let small = vec![exchange.clone()];
        let result = json!({ "active": false, "exchanges": small });
        let unsaved = respond(result.clone(), &small, false, |_| unreachable!()).unwrap();
        assert_eq!(unsaved, result);
        let mut file = String::new();
        let saved = respond(result.clone(), &small, true, |text| {
            file = text.to_owned();
            Ok("/tmp/network.json".into())
        })
        .unwrap();
        assert_eq!(saved["path"], "/tmp/network.json");
        assert_eq!(saved["exchanges"], result["exchanges"]);
        assert!(saved.get("note").is_none());
        assert_eq!(serde_json::from_str::<Value>(&file).unwrap(), result);

        exchange.request.headers = vec![Header {
            name: "X-Big".into(),
            value: "x".repeat(RETURNED_AT_MOST),
        }];
        let big = vec![exchange];
        let result = json!({ "active": false, "exchanges": big });
        let brief = respond(result, &big, false, |_| Ok("/tmp/big.json".into())).unwrap();
        assert_eq!(brief["path"], "/tmp/big.json");
        assert!(brief["note"].as_str().unwrap().contains("KB"));
        assert_eq!(
            brief["exchanges"],
            json!([{ "id": "request-1", "method": "GET", "url": "http://x.test/",
                     "resource_type": "Fetch", "duration_ms": 1 }])
        );
    }

    #[test]
    fn post_data_entries() {
        assert_eq!(
            post_data(
                &json!({ "postDataEntries": [{ "bytes": "eyJh" }, { "bytes": "IjoxfQ==" }] })
            )
            .as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(post_data(&json!({})), None);
        assert_eq!(
            decoded(&json!({ "body": "aGk=", "base64Encoded": true }), "body"),
            Some(b"hi".to_vec())
        );
        assert_eq!(
            decoded(&json!({ "body": "!!", "base64Encoded": true }), "body"),
            None
        );
    }
}
