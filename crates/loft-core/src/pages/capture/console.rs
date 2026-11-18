//! Opt-in capture of a page's console messages and uncaught errors, and the
//! browser's own reports about it (ADR-0017 §8).
//!
//! It needs the page's `Runtime` domain, which a page can detect, so it is
//! on only between `console_start` and `console_stop` (nothing else turns
//! `Runtime` on in the page's session). What `Runtime.enable` replays from
//! before the start is told apart by its timestamp; messages from isolated
//! worlds (our own helpers') are left out. Frames of other sites are
//! followed (`cross_site.rs`); workers aren't.

use super::super::automation::Agent;
use super::super::cdp::Event;
use super::super::lock;
use super::cross_site::{self, Unfollow, Wanted};
use super::network_model::cut;
use super::redaction::{MIN_IN_TEXT, Redactor};
use super::{PageCapture, now_iso, now_ms};
use loft_page_script::{BrowserError, ConsoleReadOptions, SCRIPT_TIMEOUT};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};

/// Messages kept; the oldest go first.
const KEPT: usize = 1000;
/// The most of a message's text kept, in bytes.
const TEXT_LIMIT: usize = 4000;
/// The most of a message's text looked through for credentials, in bytes:
/// more than is kept, so a token isn't judged cut in half.
const SCANNED: usize = 4 * TEXT_LIMIT;
/// Entries a read returns when not told.
const DEFAULT_LIMIT: u32 = 200;

/// A page's console capture, if one runs.
#[derive(Default)]
pub(super) struct ConsoleState {
    running: Option<Console>,
}

struct Console {
    /// Ms since 1970: earlier messages are replays.
    started: f64,
    entries: VecDeque<Entry>,
    /// Dropped for room since the last read that cleared.
    dropped: u64,
    /// The last `seq` given.
    seq: u64,
    /// Isolated worlds' execution contexts, by session (`""`: the page's)
    /// and id: ids are per renderer.
    isolated: HashSet<(String, i64)>,
}

#[derive(Serialize, Clone, Debug)]
struct Entry {
    seq: u64,
    time: String,
    #[serde(rename = "type")]
    kind: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frame: Option<String>,
}

/// A message before it is numbered.
struct Message {
    kind: String,
    text: String,
    source: Option<String>,
    url: Option<String>,
    line: Option<u64>,
    logged: Option<f64>,
}

impl Console {
    fn new(started: f64) -> Console {
        Console {
            started,
            entries: VecDeque::new(),
            dropped: 0,
            seq: 0,
            isolated: HashSet::new(),
        }
    }

    fn observe(&mut self, event: &Event, frame: Option<&str>) {
        let params = &event.params;
        let session = event.session.clone().unwrap_or_default();
        let message = match event.method.as_str() {
            "Runtime.executionContextCreated" => {
                let context = &params["context"];
                if context["auxData"]["isDefault"] == false
                    && let Some(id) = context["id"].as_i64()
                {
                    self.isolated.insert((session, id));
                }
                return;
            }
            "Runtime.executionContextDestroyed" => {
                if let Some(id) = params["executionContextId"].as_i64() {
                    self.isolated.remove(&(session, id));
                }
                return;
            }
            "Runtime.executionContextsCleared" => {
                self.isolated.retain(|(s, _)| *s != session);
                return;
            }
            "Runtime.consoleAPICalled" => {
                if self.is_isolated(&session, &params["executionContextId"]) {
                    return;
                }
                let text = params["args"]
                    .as_array()
                    .map(|args| args.iter().map(describe).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                let top = &params["stackTrace"]["callFrames"][0];
                Message {
                    kind: params["type"].as_str().unwrap_or("log").to_owned(),
                    text,
                    source: None,
                    url: non_empty(&top["url"]),
                    line: line(&top["lineNumber"]),
                    logged: params["timestamp"].as_f64(),
                }
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                if self.is_isolated(&session, &details["executionContextId"]) {
                    return;
                }
                let text = details["exception"]["description"]
                    .as_str()
                    .or_else(|| details["text"].as_str())
                    .unwrap_or("Uncaught error");
                Message {
                    kind: "pageerror".into(),
                    text: text.to_owned(),
                    source: None,
                    url: non_empty(&details["url"]),
                    line: line(&details["lineNumber"]),
                    logged: params["timestamp"].as_f64(),
                }
            }
            "Log.entryAdded" => {
                let entry = &params["entry"];
                let level = entry["level"].as_str().unwrap_or("info");
                if level == "verbose" {
                    return;
                }
                Message {
                    kind: level.to_owned(),
                    text: entry["text"].as_str().unwrap_or_default().to_owned(),
                    source: entry["source"].as_str().map(str::to_owned),
                    url: non_empty(&entry["url"]),
                    line: line(&entry["lineNumber"]),
                    logged: entry["timestamp"].as_f64(),
                }
            }
            _ => return,
        };
        if message.logged.is_some_and(|at| at < self.started) {
            return;
        }
        self.keep(message, frame);
    }

    fn is_isolated(&self, session: &str, context: &Value) -> bool {
        context
            .as_i64()
            .is_some_and(|id| self.isolated.contains(&(session.to_owned(), id)))
    }

    fn keep(&mut self, message: Message, frame: Option<&str>) {
        self.seq += 1;
        if self.entries.len() >= KEPT {
            self.entries.pop_front();
            self.dropped += 1;
        }
        // Credentials out (as of a network capture's) before anything
        // keeps the text, so a read's filter can't find them either. Cut
        // after: a token cut in half no longer looks like one.
        let mut redactor = Redactor::default();
        let url = message.url.map(|url| redactor.url(&url));
        let frame = frame.map(|frame| redactor.url(frame));
        let text = redactor.text(&cut(&message.text, SCANNED));
        let known = |s: String| redactor.known_in_text(&s, MIN_IN_TEXT);
        self.entries.push_back(Entry {
            seq: self.seq,
            time: now_iso(),
            kind: message.kind,
            text: cut(&text, TEXT_LIMIT),
            source: message.source,
            url: url.map(known),
            line: message.line,
            frame: frame.map(known),
        });
    }

    fn read(&mut self, options: &ConsoleReadOptions) -> Value {
        let limit = options.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, KEPT as u32) as usize;
        let since = options.since.unwrap_or(0);
        let clear = options.clear.unwrap_or(true);
        let levels: Vec<String> = options.levels.iter().map(|l| l.to_lowercase()).collect();
        let filter = options
            .filter
            .as_deref()
            .filter(|f| !f.is_empty())
            .map(str::to_lowercase);
        let matches = |e: &Entry| {
            e.seq > since
                && (!options.errors_only
                    || matches!(e.kind.as_str(), "error" | "pageerror" | "assert"))
                && (levels.is_empty() || levels.contains(&e.kind))
                && filter
                    .as_ref()
                    .is_none_or(|f| e.text.to_lowercase().contains(f.as_str()))
        };
        let matching: Vec<u64> = self
            .entries
            .iter()
            .filter(|e| matches(e))
            .map(|e| e.seq)
            .collect();
        let omitted = matching.len().saturating_sub(limit);
        let returned: HashSet<u64> = matching[omitted..].iter().copied().collect();
        let entries: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| returned.contains(&e.seq))
            .collect();
        let mut result = json!({ "entries": entries, "last_seq": self.seq });
        if omitted > 0 {
            result["omitted"] = json!(omitted);
        }
        if self.dropped > 0 {
            result["dropped_oldest"] = json!(self.dropped);
        }
        if clear {
            self.entries.retain(|e| !returned.contains(&e.seq));
            self.dropped = 0;
        }
        result
    }
}

/// A `console.*` argument as text: its value, else how DevTools describes
/// it (`Object`, `Array(3)`, …).
fn describe(arg: &Value) -> String {
    match arg.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(value) => value.to_string(),
        None => ["unserializableValue", "description", "type"]
            .iter()
            .find_map(|key| arg[*key].as_str())
            .unwrap_or_default()
            .to_owned(),
    }
}

fn non_empty(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// A 0-based DevTools line number, 1-based.
fn line(value: &Value) -> Option<u64> {
    value.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64 + 1)
}

/// Whether a capture runs.
pub(super) fn running(state: &PageCapture) -> bool {
    lock(&state.console).running.is_some()
}

/// A `Runtime.*` or `Log.*` event of the page's (or of its frame of another
/// site, `frame`), for its console capture if one runs.
pub(super) fn observe(state: &PageCapture, event: &Event, frame: Option<&str>) {
    if let Some(console) = &mut lock(&state.console).running {
        console.observe(event, frame);
    }
}

/// A capture, running from `started` (ms since 1970); `false` if one
/// already was.
fn begin(state: &PageCapture, started: f64) -> bool {
    let mut console = lock(&state.console);
    if console.running.is_some() {
        return false;
    }
    console.running = Some(Console::new(started));
    true
}

/// `console_start`.
pub(super) async fn start(agent: &Agent<'_>, state: &PageCapture) -> Result<Value, BrowserError> {
    if !begin(state, now_ms()) {
        return Ok(json!({ "capturing": true, "note": "it was already running" }));
    }
    if let Err(error) = agent
        .send(
            "Runtime.enable",
            json!({}),
            SCRIPT_TIMEOUT,
            "turning on the page's console messages",
        )
        .await
    {
        lock(&state.console).running = None;
        return Err(error);
    }
    let _ = agent
        .send(
            "Log.enable",
            json!({}),
            SCRIPT_TIMEOUT,
            "turning on the browser's reports",
        )
        .await;
    cross_site::follow(agent, Wanted::CONSOLE).await;
    Ok(json!({ "capturing": true }))
}

/// `console_read`.
pub(super) fn read(
    state: &PageCapture,
    options: &ConsoleReadOptions,
) -> Result<Value, BrowserError> {
    match &mut lock(&state.console).running {
        Some(console) => Ok(console.read(options)),
        None => Err(BrowserError::Action(
            "the console isn't being captured in this tab: console_start first".into(),
        )),
    }
}

/// `console_stop`.
pub(super) async fn stop(agent: &Agent<'_>, state: &PageCapture) -> Result<Value, BrowserError> {
    if lock(&state.console).running.take().is_none() {
        return Ok(json!({ "capturing": false }));
    }
    let cdp = &agent.browser.cdp;
    for method in ["Runtime.disable", "Log.disable"] {
        let _ = cdp
            .send_within(method, json!({}), Some(&agent.page.session), SCRIPT_TIMEOUT)
            .await;
    }
    Unfollow::of(agent).run(state, Wanted::CONSOLE).await;
    Ok(json!({ "capturing": false, "stopped": true }))
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

    fn log(args: &[Value], context: i64, at: f64, session: Option<&str>) -> Event {
        let args: Vec<Value> = args
            .iter()
            .map(|a| match a {
                Value::Object(_) => a.clone(),
                value => json!({ "type": "string", "value": value }),
            })
            .collect();
        event(
            "Runtime.consoleAPICalled",
            json!({ "type": "log", "args": args, "executionContextId": context, "timestamp": at }),
            session,
        )
    }

    fn read(state: &PageCapture, options: Value) -> Value {
        super::read(state, &serde_json::from_value(options).unwrap()).unwrap()
    }

    fn kinds(read: &Value) -> Vec<&str> {
        read["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn keeps_the_pages_messages_while_on() {
        let state = PageCapture::default();
        observe(&state, &log(&[json!("early")], 1, 0.0, None), None);
        assert!(super::read(&state, &ConsoleReadOptions::default()).is_err());

        let t = 1_700_000_000_000.0;
        assert!(begin(&state, t));
        assert!(!begin(&state, t), "one at a time");
        observe(
            &state,
            &log(&[json!("replayed")], 1, t - 5000.0, None),
            None,
        );
        observe(
            &state,
            &event(
                "Runtime.executionContextCreated",
                json!({ "context": { "id": 9, "auxData": { "isDefault": false } } }),
                None,
            ),
            None,
        );
        observe(&state, &log(&[json!("helper")], 9, t + 1.0, None), None);
        observe(
            &state,
            &log(&[json!("hello"), json!(2)], 1, t + 2.0, None),
            None,
        );
        observe(
            &state,
            &log(
                &[json!("framed"), json!(2)],
                9,
                t + 3.0,
                Some("frame-session"),
            ),
            Some("http://localhost:1"),
        );
        observe(
            &state,
            &event(
                "Runtime.exceptionThrown",
                json!({ "timestamp": t + 4.0, "exceptionDetails": {
                    "executionContextId": 1, "text": "Uncaught", "url": "http://x.test/app.js",
                    "lineNumber": 4,
                    "exception": { "description": "TypeError: x is undefined" },
                } }),
                None,
            ),
            None,
        );
        observe(
            &state,
            &event(
                "Log.entryAdded",
                json!({ "entry": { "level": "verbose", "text": "chatter", "timestamp": t + 5.0 } }),
                None,
            ),
            None,
        );
        observe(
            &state,
            &event(
                "Log.entryAdded",
                json!({ "entry": { "level": "error", "source": "network",
                    "text": "Failed to load resource", "url": "http://x.test/gone",
                    "timestamp": t + 6.0 } }),
                None,
            ),
            None,
        );

        let errors = read(&state, json!({ "errors_only": true, "clear": false }));
        assert_eq!(kinds(&errors), ["pageerror", "error"], "{errors}");
        let entries = &errors["entries"];
        assert_eq!(entries[0]["text"], "TypeError: x is undefined");
        assert_eq!(entries[0]["line"], 5);
        assert_eq!(entries[0]["url"], "http://x.test/app.js");
        assert_eq!(entries[1]["source"], "network");
        let keys: Vec<&str> = entries[1]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["seq", "time", "type", "text", "source", "url"]);

        let newest = read(&state, json!({ "limit": 2 }));
        assert_eq!(kinds(&newest), ["pageerror", "error"], "{newest}");
        assert_eq!(newest["omitted"], 2);
        assert_eq!(newest["last_seq"], 4);
        assert!(newest.get("dropped_oldest").is_none());

        let logs = read(&state, json!({ "levels": ["LOG"] }));
        let texts: Vec<&str> = logs["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["hello 2", "framed 2"], "{logs}");
        assert!(logs["entries"][0].get("frame").is_none());
        assert_eq!(logs["entries"][1]["frame"], "http://localhost:1");

        assert_eq!(read(&state, json!({ "since": 3 }))["entries"], json!([]));

        // A context destroyed is no longer isolated; a cleared session's
        // contexts neither.
        observe(
            &state,
            &event(
                "Runtime.executionContextDestroyed",
                json!({ "executionContextId": 9 }),
                None,
            ),
            None,
        );
        observe(&state, &log(&[json!("reused")], 9, t + 7.0, None), None);
        assert_eq!(read(&state, json!({}))["entries"][0]["text"], "reused");
    }

    #[test]
    fn arguments_and_filters() {
        assert_eq!(describe(&json!({ "type": "number", "value": 42 })), "42");
        assert_eq!(
            describe(&json!({ "type": "object", "className": "Object", "description": "Object" })),
            "Object"
        );
        assert_eq!(
            describe(
                &json!({ "type": "number", "unserializableValue": "NaN", "description": "NaN" })
            ),
            "NaN"
        );
        assert_eq!(describe(&json!({ "type": "undefined" })), "undefined");
        assert_eq!(describe(&json!({})), "");

        let state = PageCapture::default();
        begin(&state, 0.0);
        observe(&state, &log(&[json!("Careful NOW")], 1, 1.0, None), None);
        observe(&state, &log(&[json!("fine")], 1, 2.0, None), None);
        let found = read(&state, json!({ "filter": "careful", "clear": false }));
        assert_eq!(found["entries"].as_array().unwrap().len(), 1);
        // Still there (`clear: false`), and the other one left by the filter
        // stays after a clearing read.
        read(&state, json!({ "filter": "careful" }));
        let rest = read(&state, json!({}));
        assert_eq!(rest["entries"].as_array().unwrap().len(), 1);
        assert_eq!(rest["entries"][0]["text"], "fine");
    }

    /// Credentials are taken out of what the console says as they are out
    /// of a network capture, before the text is kept: a filter can't find
    /// them either.
    #[test]
    fn credentials_are_redacted() {
        const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let state = PageCapture::default();
        begin(&state, 0.0);
        observe(
            &state,
            &log(
                &[
                    json!("fetching"),
                    json!("https://api.test/v1/me?access_token=hunter2hunter2&page=2"),
                ],
                1,
                1.0,
                None,
            ),
            None,
        );
        observe(
            &state,
            &log(&[json!(format!("session {JWT}"))], 1, 2.0, None),
            None,
        );
        observe(
            &state,
            &log(
                &[
                    json!("headers"),
                    json!({ "type": "object", "description": "Object" }),
                ],
                1,
                3.0,
                None,
            ),
            None,
        );
        observe(
            &state,
            &log(
                &[json!(
                    "Authorization: Bearer xyzzy-plugh, then {\"password\": \"open sesame\"} and Authorization: Basic dXNlcjpwYXNz"
                )],
                1,
                4.0,
                None,
            ),
            None,
        );
        observe(
            &state,
            &event(
                "Runtime.exceptionThrown",
                json!({ "timestamp": 5.0, "exceptionDetails": {
                    "executionContextId": 1, "text": "Uncaught",
                    "url": "https://x.test/app.js?sig=abc123def456",
                    "lineNumber": 4,
                    "exception": { "description": format!(
                        "Error: 401 for /api/{JWT}/items\n    at load (https://x.test/app.js?sig=abc123def456:5:3)"
                    ) },
                } }),
                None,
            ),
            None,
        );
        observe(
            &state,
            &event(
                "Log.entryAdded",
                json!({ "entry": { "level": "error", "source": "network",
                    "text": "Failed to load resource: https://x.test/reset?token=s3cr3t-reset-token",
                    "url": "https://x.test/reset?token=s3cr3t-reset-token",
                    "timestamp": 6.0 } }),
                None,
            ),
            None,
        );

        // Nothing to find by filtering for it.
        for secret in [
            "hunter2",
            "xyzzy",
            "sesame",
            "dXNlcjpwYXNz",
            "s3cr3t",
            "abc123",
        ] {
            let found = read(&state, json!({ "filter": secret, "clear": false }));
            assert_eq!(found["entries"], json!([]), "{secret}: {found}");
        }
        let all = read(&state, json!({}));
        let all_text = all.to_string();
        for secret in [
            "hunter2",
            JWT,
            "xyzzy-plugh",
            "open sesame",
            "dXNlcjpwYXNz",
            "abc123def456",
            "s3cr3t",
        ] {
            assert!(!all_text.contains(secret), "{secret} in {all}");
        }
        let texts: Vec<&str> = all["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            [
                "fetching https://api.test/v1/me?access_token=[redacted]&page=2",
                "session [redacted]",
                "headers Object",
                "Authorization: Bearer [redacted], then {\"password\": \"[redacted]\"} and \
                 Authorization: Basic [redacted]",
                "Error: 401 for /api/[redacted]/items\n    at load \
                 (https://x.test/app.js?sig=[redacted])",
                "Failed to load resource: https://x.test/reset?token=[redacted]",
            ]
        );
        assert_eq!(
            all["entries"][4]["url"],
            "https://x.test/app.js?sig=[redacted]"
        );
        assert_eq!(
            all["entries"][5]["url"],
            "https://x.test/reset?token=[redacted]"
        );
    }

    #[test]
    fn bounded() {
        let state = PageCapture::default();
        begin(&state, 0.0);
        let long = "é".repeat(4000);
        for n in 0..1005 {
            observe(&state, &log(&[json!(long)], 1, n as f64, None), None);
        }
        let all = read(&state, json!({ "limit": 5000 }));
        let entries = all["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1000);
        assert_eq!(all["dropped_oldest"], 5);
        assert_eq!(entries[0]["seq"], 6);
        for entry in entries {
            let text = entry["text"].as_str().unwrap();
            assert!(text.len() <= TEXT_LIMIT + '…'.len_utf8() && text.ends_with('…'));
        }
        assert!(read(&state, json!({})).get("dropped_oldest").is_none());
    }
}
