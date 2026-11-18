//! Agents drive a thread's page (ADR-0017): the `browser_*` tools'
//! [`BrowserCommand`]s run here, over the same DevTools connection the
//! viewers' pictures and input use, so the app, the web client and the
//! agent all see one page.
//!
//! The commands answer with the JSON the desktop panel's WKWebView gave
//! (`loft-browser`'s crate docs have the table) and fail with the same
//! [`BrowserError`] messages and timeouts. Where Chrome does better cheaply,
//! it does: a click is a real one (`isTrusted`) where it lands on the
//! element, and a screenshot is the page at its device scale.
//!
//! - The helpers (`agent.js`) run in an isolated world of the page's
//!   document, so page scripts can't see or patch them; `eval` runs in the
//!   page's own world.
//! - Commands for one page run one at a time.
//! - Local files: a page may show one only when the daemon itself loaded it
//!   (an agent's `navigate`, a client on this computer), or going back to
//!   one. Any other local document a page asks for, for itself or a frame,
//!   is refused before it loads: once a page has shown a local file, the
//!   browser holds its `file:` documents (`Fetch.enable`) for a look here.
//!   Anything else that enables `Fetch` on such a page has to keep that
//!   pattern, or local files are let through unseen.

use super::cdp::Event;
use super::{Browser, Page, Pages, bad, check_url, first_real_entry, gone, lock};
use super::{dialogs, frames, human};
use crate::BusMsg;
use loft_page_script::{
    BrowserCommand, BrowserError, EVAL_TIMEOUT, Helper, NAVIGATION_TIMEOUT, SCRIPT_TIMEOUT,
    action_result, eval_reply, normalize_address, script,
};
use loft_protocol::{ErrorBody, PageId, ThreadId};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The size a page an agent opens has until a viewer gives it its own.
pub const AGENT_PAGE_SIZE: (u32, u32, f32) = (1280, 800, 2.0);
/// Viewers are told an agent is using a page at most this often.
const TELL_EVERY: Duration = Duration::from_secs(1);
/// The isolated world the helpers run in.
const WORLD: &str = "agentloft";
/// How often a load in progress is looked at.
const POLL: Duration = Duration::from_millis(50);
/// A navigation that hasn't started after this long never will (it was
/// within the document, or nothing happened).
const NEVER_STARTED: Duration = Duration::from_millis(1500);
/// How long a click or key press has to start a navigation.
const NAVIGATION_GRACE: Duration = Duration::from_millis(500);
/// A file address longer than this isn't one.
const MAX_URL: usize = 8192;

/// What the pages' agents need kept, per page.
#[derive(Default)]
pub(super) struct Drivers(Mutex<HashMap<PageId, Arc<Driver>>>);

#[derive(Default)]
pub(super) struct Driver {
    /// Held for the length of one command.
    turn: tokio::sync::Mutex<()>,
    loads: Mutex<Loads>,
    world: Mutex<World>,
    /// `file:` addresses (without their fragment) the daemon loaded itself.
    files: Mutex<HashSet<String>>,
    /// Local documents are held for a look (`allow_file`); held while
    /// that is being asked for.
    fetch_on: AtomicBool,
    fetching: tokio::sync::Mutex<()>,
    /// When viewers were last told an agent is using the page.
    told: Mutex<Option<Instant>>,
    // Page interaction (A)
    /// Dialogs held for the agent (`dialogs.rs`).
    pub(super) dialogs: dialogs::Dialogs,
    /// The agent's pointer and keys (`input.rs`).
    pub(super) input: tokio::sync::Mutex<human::InputState>,
    /// What an HTML5 drag the browser handed over carries
    /// (`Input.dragIntercepted`).
    pub(super) drag: Mutex<Option<Value>>,
    /// The page's frames the helpers' world can't reach (`frames.rs`).
    pub(super) frames: Mutex<frames::Frames>,
    // Tabs and handoff (C)
    /// What the agent's tabs need kept (`agents.rs`).
    pub(super) tab: super::agents::TabDriver,
}

/// What a command runs with, besides the page.
#[derive(Debug, Clone, Default)]
pub struct DriveOptions {
    /// Where words typed instead of an address are searched.
    pub search_template: String,
    /// Human-like pointer paths and typing (`[browser] human_input`).
    pub human_input: bool,
    /// Folders whose files `upload` may hand to a page (the thread's
    /// workspace); the system's temporary folder always may.
    pub file_roots: Vec<PathBuf>,
}

/// The main frame's loads, as the browser reports them.
#[derive(Debug, Default, Clone)]
struct Loads {
    /// Counts starts and stops, to order them.
    events: u64,
    /// `events` at the last start and the last stop.
    started_at: u64,
    stopped_at: u64,
    loading: bool,
    /// The document is the browser's error page (the load failed).
    error_page: bool,
    /// A `file:` address the page was stopped from opening, and when.
    blocked: Option<(u64, String)>,
}

/// Where loads stood when a command began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Mark(u64);

#[derive(Debug, Default, PartialEq, Eq)]
struct Progress {
    /// A load began since the mark.
    started: bool,
    /// ... and has finished.
    finished: bool,
    loading: bool,
    error_page: bool,
    blocked: Option<String>,
}

impl Loads {
    fn mark(&self) -> Mark {
        Mark(self.events)
    }

    fn progress(&self, mark: Mark) -> Progress {
        let started = self.started_at > mark.0;
        Progress {
            started,
            finished: started && self.stopped_at > self.started_at,
            loading: self.loading,
            error_page: self.error_page,
            blocked: self
                .blocked
                .as_ref()
                .filter(|(at, _)| *at > mark.0)
                .map(|(_, url)| url.clone()),
        }
    }
}

/// The isolated world of the current document, once made.
#[derive(Default)]
struct World {
    /// Counts the main frame's documents.
    document: u64,
    context: Option<i64>,
}

/// An address without its fragment: the same document.
fn without_fragment(url: &str) -> &str {
    url.split_once('#').map_or(url, |(base, _)| base)
}

fn is_file(url: &str) -> bool {
    url.get(..5)
        .is_some_and(|s| s.eq_ignore_ascii_case("file:"))
}

/// A local file's address a client on this computer may open.
pub(super) fn check_file_url(url: &str) -> Result<String, ErrorBody> {
    let url = url.trim();
    let parsed = url::Url::parse(url).map_err(|_| bad("That isn't a file address."))?;
    if parsed.scheme() != "file" || url.len() > MAX_URL || url.contains(char::is_whitespace) {
        return Err(bad("That isn't a file address."));
    }
    Ok(parsed.to_string())
}

/// Whether `url` is a `file:` address (as opposed to a web one).
pub fn is_file_url(url: &str) -> bool {
    is_file(url.trim())
}

impl Drivers {
    pub(super) fn get(&self, id: &PageId) -> Option<Arc<Driver>> {
        lock(&self.0).get(id).cloned()
    }

    /// The page's, made if it has none (forgetting closed pages').
    pub(super) fn of(&self, browser: &Browser, id: &PageId) -> Arc<Driver> {
        let mut drivers = lock(&self.0);
        if let Some(driver) = drivers.get(id) {
            return driver.clone();
        }
        let open = lock(&browser.pages);
        drivers.retain(|id, _| open.contains_key(id));
        drivers.entry(id.clone()).or_default().clone()
    }
}

impl Browser {
    /// Everything a page says, as far as agents care: its loads, its
    /// documents, and the local files it asks for.
    pub(super) fn observe_for_agents(self: &Arc<Self>, page: &Arc<Page>, event: &Event) {
        let params = &event.params;
        let main = |frame: Option<&str>| frame == Some(page.target.as_str());
        let driver = || self.drivers.of(self, &page.id);
        match event.method.as_str() {
            "Page.frameStartedLoading" | "Page.frameStoppedLoading"
                if main(params["frameId"].as_str()) =>
            {
                let driver = driver();
                let mut loads = lock(&driver.loads);
                loads.events += 1;
                loads.loading = event.method.ends_with("StartedLoading");
                if loads.loading {
                    loads.started_at = loads.events;
                } else {
                    loads.stopped_at = loads.events;
                }
            }
            "Page.frameNavigated" if !params["frame"]["parentId"].is_string() => {
                let driver = driver();
                let mut world = lock(&driver.world);
                world.document += 1;
                world.context = None;
                let url = params["frame"]["url"].as_str().unwrap_or_default();
                lock(&driver.loads).error_page = url.starts_with("chrome-error:");
            }
            // A page (or a frame of it) loading a local file: only one the
            // daemon loaded itself (see `allow_file`).
            "Fetch.requestPaused" => {
                let url = params["request"]["url"].as_str().unwrap_or_default();
                let allowed = self
                    .drivers
                    .get(&page.id)
                    .is_some_and(|d| lock(&d.files).contains(without_fragment(url)));
                let id = params["requestId"].clone();
                let answer = if allowed {
                    ("Fetch.continueRequest", json!({ "requestId": id }))
                } else {
                    tracing::warn!("a page asked for a local file by itself; refused");
                    if main(params["frameId"].as_str()) {
                        let driver = driver();
                        let mut loads = lock(&driver.loads);
                        loads.events += 1;
                        loads.blocked = Some((loads.events, url.to_owned()));
                    }
                    (
                        "Fetch.failRequest",
                        json!({ "requestId": id, "errorReason": "BlockedByClient" }),
                    )
                };
                let (browser, page) = (self.clone(), page.clone());
                tokio::spawn(async move {
                    let (method, params) = answer;
                    let _ = browser.cdp.send(method, params, Some(&page.session)).await;
                });
            }
            // Page interaction (A)
            "Page.javascriptDialogClosed" => self.dialog_closed(&page.id),
            "Input.dragIntercepted" => {
                *lock(&driver().drag) = Some(params["data"].clone());
            }
            "Target.attachedToTarget" | "Target.detachedFromTarget"
                if event.session.as_deref() == Some(page.session.as_str()) =>
            {
                lock(&driver().frames).observe(&event.method, params);
            }
            _ => {}
        }
    }

    /// Let the page show `url`, a local file the daemon is about to load,
    /// and from then on refuse the page any other: the first time, the
    /// browser is asked to hold every local document the page asks for
    /// until it is let through (`Fetch.requestPaused` above). A page that
    /// never showed a local file needs no such watch: the browser itself
    /// keeps web pages from opening local files.
    async fn allow_file(self: &Arc<Self>, page: &Page, url: &str) -> Result<(), String> {
        let driver = self.drivers.of(self, &page.id);
        lock(&driver.files).insert(without_fragment(url).to_owned());
        let _once = driver.fetching.lock().await;
        if driver.fetch_on.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.cdp
            .send(
                "Fetch.enable",
                json!({ "patterns": [{ "urlPattern": "file:*", "resourceType": "Document" }] }),
                Some(&page.session),
            )
            .await?;
        driver.fetch_on.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Tell whoever has the page's thread open that an agent is using it.
    fn tell_driven(&self, page: &Page, driver: &Driver) {
        {
            let mut told = lock(&driver.told);
            if told.is_some_and(|at| at.elapsed() < TELL_EVERY) {
                return;
            }
            *told = Some(Instant::now());
        }
        let info = lock(&page.state).info.clone();
        let _ = self.bus.send(BusMsg::PageDriven {
            thread_id: info.thread_id.clone(),
            page: info,
        });
    }
}

impl Pages {
    /// The thread's tab an agent drives: the one used last (by a person or
    /// the agent), else its newest.
    pub fn thread_page(&self, thread: &ThreadId) -> Option<PageId> {
        let browser = self.current()?;
        browser.current_tab(thread).map(|p| p.id.clone())
    }

    /// Run an agent's command on the page and answer with its result
    /// (plain input, uploads from the temporary folder only).
    #[cfg(test)]
    pub async fn drive(
        &self,
        id: &PageId,
        command: BrowserCommand,
        search_template: &str,
    ) -> Result<Value, BrowserError> {
        let options = DriveOptions {
            search_template: search_template.to_owned(),
            ..DriveOptions::default()
        };
        self.drive_with(id, command, &options).await
    }

    /// Run an agent's command on the page and answer with its result.
    pub async fn drive_with(
        &self,
        id: &PageId,
        command: BrowserCommand,
        options: &DriveOptions,
    ) -> Result<Value, BrowserError> {
        let (browser, page) = self.page(id).map_err(|_| BrowserError::Closed)?;
        // In use: not a tab to close for going unused.
        lock(&page.state).used = Instant::now();
        let driver = browser.drivers.of(&browser, &page.id);
        // Tabs and handoff (C): what the tab was opened with allows it.
        browser.may_drive(&page, &command)?;
        browser.tell_driven(&page, &driver);
        // Driving from now: a dialog the command opens is held for it.
        let _driving = driver.dialogs.driving();
        let _turn = driver.turn.lock().await;
        tracing::debug!(action = command.action(), page = %id.0, "an agent drives a page");
        let agent = Agent {
            browser: &browser,
            page: &page,
            driver: &driver,
            options,
        };
        let _busy = driver.tab.busy();
        agent.run(command).await
    }

    /// Send the page to a local file: a client on this computer asked (a
    /// browser session may not).
    pub async fn open_file(&self, id: &PageId, url: &str) -> Result<(), ErrorBody> {
        let url = check_file_url(url)?;
        let (browser, page) = self.page(id)?;
        browser
            .allow_file(&page, &url)
            .await
            .map_err(super::internal)?;
        browser
            .cdp
            .send("Page.navigate", json!({ "url": url }), Some(&page.session))
            .await
            .map(|_| ())
            .map_err(|e| {
                if browser.dead.load(Ordering::Relaxed) {
                    gone()
                } else {
                    super::internal(e)
                }
            })
    }
}

/// Where a script runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum In {
    /// The helpers' isolated world.
    Helpers,
    /// The page's own.
    Page,
}

/// One command on one page.
pub(super) struct Agent<'a> {
    pub(super) browser: &'a Arc<Browser>,
    pub(super) page: &'a Arc<Page>,
    pub(super) driver: &'a Driver,
    pub(super) options: &'a DriveOptions,
}

impl Agent<'_> {
    async fn run(&self, command: BrowserCommand) -> Result<Value, BrowserError> {
        // A page held by a dialog runs nothing else until it is answered.
        if !matches!(command, BrowserCommand::Dialog { .. })
            && let Some(dialog) = self.driver.dialogs.held()
        {
            return Err(BrowserError::DialogOpen(dialog.blocking()));
        }
        let acts = !matches!(
            command,
            BrowserCommand::Snapshot { .. }
                | BrowserCommand::Screenshot(_)
                | BrowserCommand::ScreenshotDiff(_)
                | BrowserCommand::NetworkStatus
                | BrowserCommand::ConsoleRead(_)
                | BrowserCommand::RecordingStatus
                | BrowserCommand::Locate { .. }
                | BrowserCommand::EvalOn { .. }
        );
        let result = match self.run_command(command).await {
            // What the command did opened a dialog, which stopped it: that
            // is its result.
            Err(BrowserError::DialogOpen(_)) if acts => {
                let state = lock(&self.page.state).info.clone();
                Ok(json!({
                    "url": state.url,
                    "title": state.title,
                    "stopped": "a dialog opened",
                }))
            }
            other => other,
        };
        let mut value = result?;
        if let Some(object) = value.as_object_mut() {
            if let Some(dialog) = self.driver.dialogs.held() {
                object.insert("dialog".into(), dialog.json());
                object.insert(
                    "note".into(),
                    json!(format!(
                        "The page shows this dialog and is paused until it is answered: call \
                         browser_dialog. Unanswered, it is answered by itself in {} s.",
                        dialogs::DIALOG_WAIT.as_secs()
                    )),
                );
            }
            if let Some(settled) = self.driver.dialogs.take_settled() {
                object.insert("dialog_answered".into(), settled);
            }
        }
        Ok(value)
    }

    async fn run_command(&self, command: BrowserCommand) -> Result<Value, BrowserError> {
        let search = self.options.search_template.as_str();
        match command {
            BrowserCommand::Navigate { url } => {
                let result = self.navigate(&url, search).await?;
                Ok(self.with_challenge(result).await)
            }
            BrowserCommand::Back => {
                let result = self.history(-1).await?;
                Ok(self.with_challenge(result).await)
            }
            BrowserCommand::Forward => {
                let result = self.history(1).await?;
                Ok(self.with_challenge(result).await)
            }
            BrowserCommand::Reload => {
                let result = self.reload().await?;
                Ok(self.with_challenge(result).await)
            }
            BrowserCommand::Snapshot { within } => self.snapshot(within.as_deref()).await,
            BrowserCommand::Click {
                target,
                button,
                count,
                modifiers,
            } => {
                let before = self.before_action().await;
                let result = self.click(&target, button, count, &modifiers).await?;
                Ok(self.after_action(before, result).await)
            }
            BrowserCommand::Type {
                target,
                text,
                submit,
                insert,
            } => {
                let before = self.before_action().await;
                let result = self.type_text(&target, &text, submit, insert).await?;
                Ok(self.after_action(before, result).await)
            }
            BrowserCommand::Paste { target, text, html } => {
                let before = self.before_action().await;
                let result = self.paste(&target, &text, html.as_deref()).await?;
                Ok(self.after_action(before, result).await)
            }
            BrowserCommand::Press { key, target, count } => {
                let before = self.before_action().await;
                let result = self.press(&target, &key, count).await?;
                Ok(self.after_action(before, result).await)
            }
            BrowserCommand::Scroll { dy, dx, target, to } => self.scroll(&target, dx, dy, to).await,
            BrowserCommand::Eval { script } => self.eval(&script).await,
            BrowserCommand::Screenshot(options) => self.screenshot(options).await,
            // Page interaction (A)
            BrowserCommand::Fill {
                target,
                value,
                checked,
                submit,
            } => {
                let before = self.before_action().await;
                let result = self.fill(&target, value, checked, submit).await?;
                Ok(self.after_action(before, result).await)
            }
            BrowserCommand::Select { target, values } => {
                let before = self.before_action().await;
                let result = self.select(&target, &values).await?;
                Ok(self.after_action(before, result).await)
            }
            BrowserCommand::Hover { target } => self.hover(&target).await,
            BrowserCommand::Drag { from, to, steps } => {
                let before = self.before_action().await;
                let result = self.drag(&from, &to, steps).await?;
                Ok(self.after_action(before, result).await)
            }
            BrowserCommand::WaitFor {
                text,
                text_gone,
                target,
                state,
                url,
                timeout_ms,
            } => {
                self.wait_for(text, text_gone, &target, state, url, timeout_ms)
                    .await
            }
            BrowserCommand::Dialog {
                accept,
                prompt_text,
            } => {
                let before = self.before_action().await;
                let result = self.dialog(accept, prompt_text).await?;
                Ok(self.after_action(before, result).await)
            }
            BrowserCommand::Upload { target, files } => {
                let before = self.before_action().await;
                let result = self.upload(&target, &files).await?;
                Ok(self.after_action(before, result).await)
            }
            BrowserCommand::WebMcp { call, input } => self.webmcp(call, input).await,
            // Scripts (D)
            BrowserCommand::Execute { .. } => Err(BrowserError::Unsupported(
                "a script runs in the daemon (`browser_execute`), not on a page".into(),
            )),
            BrowserCommand::EvalOn {
                refs,
                function,
                arg,
                all,
            } => self.eval_on(&refs, &function, arg, all).await,
            BrowserCommand::Locate { steps, limit } => action_result(
                self.helper(Helper::Locate, json!({ "steps": steps, "limit": limit }))
                    .await?,
                None,
            ),
            // Capture (B): `capture/`.
            command @ (BrowserCommand::ScreenshotDiff(_)
            | BrowserCommand::NetworkStart(_)
            | BrowserCommand::NetworkStatus
            | BrowserCommand::NetworkStop(_)
            | BrowserCommand::NetworkCancel
            | BrowserCommand::ConsoleStart
            | BrowserCommand::ConsoleRead(_)
            | BrowserCommand::ConsoleStop
            | BrowserCommand::RecordingStart(_)
            | BrowserCommand::RecordingStatus
            | BrowserCommand::RecordingStop
            | BrowserCommand::RecordingCancel
            | BrowserCommand::Emulate(_)) => self.capture(command).await,
            // Tabs and handoff (C): about tabs and people, not this page's
            // content; `Core::browser_call` runs them.
            BrowserCommand::Tabs { .. }
            | BrowserCommand::Handoff { .. }
            | BrowserCommand::Demonstrate { .. }
            | BrowserCommand::Status => Err(BrowserError::Unsupported(format!(
                "`{}` isn't a command for a page",
                command.action()
            ))),
        }
    }

    // -- the browser ----------------------------------------------------------

    /// One DevTools command to the page, waiting at most `timeout`. A
    /// dialog the page opens meanwhile stops the wait
    /// ([`BrowserError::DialogOpen`]): the page does nothing else until it
    /// is answered.
    pub(super) async fn send(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        what: &str,
    ) -> Result<Value, BrowserError> {
        self.send_on(&self.page.session, method, params, timeout, what)
            .await
    }

    /// [`Agent::send`] to one of the page's sessions (its own, or a frame
    /// of another site's, `frames.rs`).
    pub(super) async fn send_on(
        &self,
        session: &str,
        method: &str,
        params: Value,
        timeout: Duration,
        what: &str,
    ) -> Result<Value, BrowserError> {
        let mut dialog = self.driver.dialogs.watch();
        // Its own task: one stopped waiting mustn't stop half-way through
        // writing to the browser's pipe.
        let (browser, session, method_name) =
            (self.browser.clone(), session.to_owned(), method.to_owned());
        let sent = tokio::spawn(async move {
            // A little longer than ours, so that it is ours that runs out.
            browser
                .cdp
                .send_within(
                    &method_name,
                    params,
                    Some(&session),
                    timeout + Duration::from_secs(1),
                )
                .await
        });
        tokio::select! {
            sent = tokio::time::timeout(timeout, sent) => match sent {
                Err(_) => Err(BrowserError::Timeout {
                    what: what.to_owned(),
                    after: timeout,
                }),
                Ok(Ok(Ok(value))) => Ok(value),
                Ok(_) if self.closed() => Err(BrowserError::Closed),
                Ok(Ok(Err(err))) => Err(BrowserError::Action(format!("{what}: {err}"))),
                Ok(Err(err)) => Err(BrowserError::Action(format!("{what}: {err}"))),
            },
            opened = dialogs::opened(&mut dialog) => Err(BrowserError::DialogOpen(opened.blocking())),
        }
    }

    pub(super) fn closed(&self) -> bool {
        self.browser.dead.load(Ordering::Relaxed)
            || !lock(&self.browser.pages).contains_key(&self.page.id)
    }

    fn mark(&self) -> Mark {
        lock(&self.driver.loads).mark()
    }

    fn progress(&self, mark: Mark) -> Progress {
        lock(&self.driver.loads).progress(mark)
    }

    /// The helpers' world in the current document, made if need be.
    pub(super) async fn world(&self) -> Result<i64, BrowserError> {
        let document = {
            let world = lock(&self.driver.world);
            if let Some(context) = world.context {
                return Ok(context);
            }
            world.document
        };
        let made = self
            .send(
                "Page.createIsolatedWorld",
                json!({ "frameId": self.page.target, "worldName": WORLD }),
                SCRIPT_TIMEOUT,
                "preparing the page for the automation script",
            )
            .await?;
        let context = made["executionContextId"].as_i64().ok_or_else(|| {
            BrowserError::Script("the page has no room for the automation script".into())
        })?;
        let mut world = lock(&self.driver.world);
        // Kept only if it is still the same document.
        if world.document == document {
            world.context = Some(context);
        }
        Ok(context)
    }

    /// Run an async function `body` and return the string it returns.
    pub(super) async fn evaluate(
        &self,
        body: &str,
        world: In,
        timeout: Duration,
        what: &str,
    ) -> Result<Option<String>, BrowserError> {
        let mut params = json!({
            "expression": script::as_expression(body),
            "awaitPromise": true,
            "returnByValue": true,
            "userGesture": true,
        });
        let mut tries = 0;
        loop {
            tries += 1;
            if world == In::Helpers {
                params["contextId"] = self.world().await?.into();
            }
            let reply = self
                .send("Runtime.evaluate", params.clone(), timeout, what)
                .await;
            let reply = match reply {
                // The document went away under the script (a navigation):
                // once more, in the one there is now.
                Err(BrowserError::Action(err))
                    if tries == 1
                        && (err.contains("Cannot find context")
                            || err.contains("context was destroyed")) =>
                {
                    lock(&self.driver.world).context = None;
                    continue;
                }
                other => other?,
            };
            if let Some(details) = reply.get("exceptionDetails") {
                return Err(BrowserError::Script(exception_message(details)));
            }
            let result = &reply["result"];
            return match result["value"].as_str() {
                Some(text) => Ok(Some(text.to_owned())),
                None if matches!(result["type"].as_str(), Some("undefined"))
                    || result["subtype"] == "null" =>
                {
                    Ok(None)
                }
                None => Err(BrowserError::Script(format!(
                    "the script returned a non-string value ({})",
                    result["type"].as_str().unwrap_or("unknown")
                ))),
            };
        }
    }

    /// Run a helper from `agent.js` and parse its JSON.
    pub(super) async fn helper(&self, helper: Helper, args: Value) -> Result<Value, BrowserError> {
        let reply = self
            .evaluate(
                &script::helper_call(helper, &args),
                In::Helpers,
                SCRIPT_TIMEOUT,
                "waiting for the page to run the automation script",
            )
            .await?
            .ok_or_else(|| BrowserError::Script("the automation script returned nothing".into()))?;
        serde_json::from_str(&reply)
            .map_err(|e| BrowserError::Script(format!("unreadable automation result: {e}")))
    }

    /// `{url, title}` of the page now.
    pub(super) async fn summary(&self) -> Value {
        let body = "return JSON.stringify({ url: location.href, title: document.title });";
        let read = self
            .evaluate(
                body,
                In::Helpers,
                SCRIPT_TIMEOUT,
                "reading the page's address",
            )
            .await
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        match read {
            Some(page) => json!({ "url": page["url"], "title": page["title"] }),
            None => {
                let state = lock(&self.page.state);
                json!({ "url": state.info.url, "title": state.info.title })
            }
        }
    }

    /// Wait until a load that began after `mark` finishes.
    ///
    /// A navigation that never starts (within the document, or a no-op)
    /// succeeds after a short grace period.
    pub(super) async fn wait_for_load(&self, mark: Mark, what: &str) -> Result<(), BrowserError> {
        let start = Instant::now();
        loop {
            let progress = self.progress(mark);
            if let Some(url) = &progress.blocked {
                return Err(local_file_blocked(what, url));
            }
            if progress.finished && !progress.loading {
                if progress.error_page {
                    return Err(BrowserError::NavigationFailed(format!(
                        "{what} did not load (the host could not be reached, the connection failed, \
                         or the load was cancelled)"
                    )));
                }
                return Ok(());
            }
            if !progress.started && start.elapsed() >= NEVER_STARTED {
                return Ok(());
            }
            if self.closed() {
                return Err(BrowserError::Closed);
            }
            if start.elapsed() >= NAVIGATION_TIMEOUT {
                let at = self.summary().await["url"]
                    .as_str()
                    .map_or_else(|| "no page".to_owned(), str::to_owned);
                return Err(BrowserError::Timeout {
                    what: format!("waiting for {what} to load (still loading; currently at {at})"),
                    after: NAVIGATION_TIMEOUT,
                });
            }
            tokio::time::sleep(POLL).await;
        }
    }

    // -- the commands -------------------------------------------------------------

    async fn navigate(&self, input: &str, search: &str) -> Result<Value, BrowserError> {
        let url = normalize_address(input, search)?;
        let target = url.to_string();
        // Tabs and handoff (C): only where the tab was opened to go.
        self.browser.may_go(self.page, &target).await?;
        if url.scheme() == "file" {
            // Agents open pages they built; the page itself may not go on
            // to other local files.
            self.browser
                .allow_file(self.page, &target)
                .await
                .map_err(|e| BrowserError::NavigationFailed(format!("{target}: {e}")))?;
        } else if check_url(&target).is_err() {
            return Err(BrowserError::Blocked(format!(
                "{target} can't be opened here (only http, https, about:blank and file addresses)"
            )));
        }
        let mark = self.mark();
        let what = format!("waiting for {target} to load");
        let reply = self
            .send(
                "Page.navigate",
                json!({ "url": target }),
                NAVIGATION_TIMEOUT,
                &what,
            )
            .await?;
        if let Some(error) = reply["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(BrowserError::NavigationFailed(format!(
                "{target} did not load ({error})"
            )));
        }
        // No loader: it moved within the document, and is there already.
        if reply["loaderId"].as_str().is_some_and(|l| !l.is_empty()) {
            self.wait_for_load(mark, &target).await?;
        }
        Ok(self.summary().await)
    }

    /// Back (`-1`) or forward (`1`) in the page's history.
    async fn history(&self, step: i64) -> Result<Value, BrowserError> {
        let history = self
            .send(
                "Page.getNavigationHistory",
                json!({}),
                SCRIPT_TIMEOUT,
                "reading the history",
            )
            .await?;
        let current = history["currentIndex"].as_u64().unwrap_or(0);
        let wanted = if step < 0 {
            current
                .checked_sub(1)
                .filter(|i| *i >= first_real_entry(&history))
        } else {
            Some(current + 1)
        };
        let (what, missing) = if step < 0 {
            (
                "the previous page",
                "there is no previous page in the history",
            )
        } else {
            ("the next page", "there is no next page in the history")
        };
        let entry = wanted
            .and_then(|i| history["entries"].get(i as usize))
            .and_then(|e| e["id"].as_i64())
            .ok_or_else(|| BrowserError::Action(missing.into()))?;
        let mark = self.mark();
        self.send(
            "Page.navigateToHistoryEntry",
            json!({ "entryId": entry }),
            SCRIPT_TIMEOUT,
            &format!("going to {what}"),
        )
        .await?;
        self.wait_for_load(mark, what).await?;
        Ok(self.summary().await)
    }

    async fn reload(&self) -> Result<Value, BrowserError> {
        let what = match self.summary().await["url"].as_str() {
            Some(url) if !url.is_empty() => url.to_owned(),
            _ => "the page".to_owned(),
        };
        let mark = self.mark();
        self.send(
            "Page.reload",
            json!({}),
            SCRIPT_TIMEOUT,
            &format!("reloading {what}"),
        )
        .await?;
        self.wait_for_load(mark, &what).await?;
        Ok(self.summary().await)
    }

    /// Where things stood before a click, typing or a key press.
    pub(super) async fn before_action(&self) -> (Mark, Value) {
        (self.mark(), self.summary().await["url"].clone())
    }

    /// After a click, typing or a key press: wait for any navigation it
    /// caused, and say where the page is now (and whether it is a bot
    /// check). Not while a dialog it opened holds the page.
    pub(super) async fn after_action(
        &self,
        (mark, url_before): (Mark, Value),
        mut result: Value,
    ) -> Value {
        let held = || self.driver.dialogs.held().is_some();
        let start = Instant::now();
        let mut navigated = false;
        while start.elapsed() < NAVIGATION_GRACE && !held() {
            let progress = self.progress(mark);
            if progress.started || progress.blocked.is_some() {
                navigated = true;
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        if navigated
            && !held()
            && let Err(err) = self.wait_for_load(mark, "the new page").await
        {
            result["navigation_error"] = json!(err.to_string());
        }
        let now = self.summary().await;
        result["navigated"] = json!(navigated || now["url"] != url_before);
        result["url"] = now["url"].clone();
        result["title"] = now["title"].clone();
        if navigated && !held() {
            result = self.with_challenge(result).await;
        }
        result
    }

    /// `Eval`: an expression, or an async function body if it isn't one.
    async fn eval(&self, script: &str) -> Result<Value, BrowserError> {
        // Compiles without running, so nothing runs twice.
        let probe = self
            .evaluate(
                &script::eval_probe(script),
                In::Page,
                SCRIPT_TIMEOUT,
                "compiling the script",
            )
            .await;
        let body = if matches!(probe, Ok(Some(_))) {
            script::eval_expression(script)
        } else {
            script::eval_function_body(script)
        };
        let reply = self
            .evaluate(
                &body,
                In::Page,
                EVAL_TIMEOUT,
                "waiting for the script to finish",
            )
            .await?;
        eval_reply(reply)
    }
}

/// What a page was stopped from opening, for the agent.
fn local_file_blocked(what: &str, url: &str) -> BrowserError {
    BrowserError::Blocked(format!(
        "loading {what} went to {url}, a local file the page may not open by itself \
         (navigate to it instead)"
    ))
}

/// The message of a script's exception: its first line (`TypeError: …`),
/// without the wrapper's stack.
pub(super) fn exception_message(details: &Value) -> String {
    let described = details["exception"]["description"]
        .as_str()
        .and_then(|d| d.lines().next())
        .filter(|d| !d.is_empty());
    let thrown = details["exception"]["value"]
        .as_str()
        .map(|v| format!("Uncaught {v}"));
    described
        .map(str::to_owned)
        .or(thrown)
        .or_else(|| details["text"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "the script failed".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_load_counts_from_the_mark_on() {
        let mut loads = Loads::default();
        let event = |loads: &mut Loads, start: bool| {
            loads.events += 1;
            loads.loading = start;
            if start {
                loads.started_at = loads.events;
            } else {
                loads.stopped_at = loads.events;
            }
        };
        // A load already under way when the command begins...
        event(&mut loads, true);
        let mark = loads.mark();
        assert!(!loads.progress(mark).started);
        // ...ending is not the new one finishing.
        event(&mut loads, false);
        let p = loads.progress(mark);
        assert!(!p.started && !p.finished && !p.loading);
        event(&mut loads, true);
        let p = loads.progress(mark);
        assert!(p.started && !p.finished && p.loading);
        event(&mut loads, false);
        let p = loads.progress(mark);
        assert!(p.started && p.finished && !p.loading);
        // A file the page was stopped from opening counts from the mark on.
        loads.events += 1;
        loads.blocked = Some((loads.events, "file:///etc/hosts".into()));
        assert_eq!(
            loads.progress(mark).blocked.as_deref(),
            Some("file:///etc/hosts")
        );
        assert_eq!(loads.progress(loads.mark()).blocked, None);
    }

    #[test]
    fn file_addresses() {
        assert!(is_file_url(" FILE:///tmp/a.html"));
        assert!(!is_file_url("https://example.com/file:x"));
        assert_eq!(
            check_file_url("file:///tmp/a b.html").map_err(|e| e.code),
            Err(loft_protocol::ErrorCode::BadRequest)
        );
        assert_eq!(
            check_file_url("file:///tmp/report.html#top").unwrap(),
            "file:///tmp/report.html#top"
        );
        assert!(check_file_url("https://example.com").is_err());
        assert_eq!(without_fragment("file:///a.html#x"), "file:///a.html");
        assert_eq!(without_fragment("file:///a.html"), "file:///a.html");
    }

    #[test]
    fn script_exceptions_read_well() {
        let thrown = json!({
            "text": "Uncaught",
            "exception": {
                "type": "object",
                "description": "ReferenceError: nope is not defined\n    at <anonymous>:3:1",
            },
        });
        assert_eq!(
            exception_message(&thrown),
            "ReferenceError: nope is not defined"
        );
        let string =
            json!({ "text": "Uncaught", "exception": { "type": "string", "value": "boom" } });
        assert_eq!(exception_message(&string), "Uncaught boom");
        assert_eq!(
            exception_message(&json!({ "text": "Uncaught" })),
            "Uncaught"
        );
    }

    /// The ref of the element an outline describes as `what` (`button "Go"`).
    fn ref_of(outline: &str, what: &str) -> u32 {
        outline
            .split("[ref=")
            .skip(1)
            .find_map(|rest| {
                let (id, element) = rest.split_once("] ")?;
                element.starts_with(what).then(|| id.parse().ok())?
            })
            .unwrap_or_else(|| panic!("no {what} in {outline}"))
    }

    /// An agent's whole path against a real browser: navigate, read the
    /// page, click, type, eval, screenshot, history, and local files.
    /// Needs Chrome (or Chromium, Edge, Brave) installed:
    /// `cargo test -p loft-core pages::automation::tests::live -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_an_agent_drives_a_page() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::sync::broadcast;
        const FORM: &str = "<!doctype html><title>Form</title>\
            <h1>Sign up</h1>\
            <input id=\"name\" placeholder=\"Your name\" \
              oninput=\"document.title = 'typed:' + this.value\">\
            <button id=\"go\" onclick=\"window.trusted = event.isTrusted; \
              document.title = 'clicked:' + event.isTrusted\">Go</button>\
            <a href=\"/next\">Next page</a>";
        const NEXT: &str = "<!doctype html><title>Next</title><p>Second page</p>";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 2048];
                let n = socket.read(&mut request).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&request[..n]);
                let body = if head.starts_with("GET /next") {
                    NEXT
                } else {
                    FORM
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let (bus, mut heard) = broadcast::channel(1024);
        let pages = Pages::default();
        let thread = ThreadId::new();
        assert_eq!(pages.thread_page(&thread), None);
        let (width, height, scale) = AGENT_PAGE_SIZE;
        let page = pages
            .open(
                thread.clone(),
                crate::pages::Which::Current,
                None,
                width,
                height,
                scale,
                None,
                &dir.path().join("profile"),
                bus,
            )
            .await
            .unwrap();
        assert_eq!(pages.thread_page(&thread), Some(page.id.clone()));
        let search = loft_page_script::DEFAULT_SEARCH_TEMPLATE;
        let drive = async |command: Value| {
            let command: BrowserCommand = serde_json::from_value(command).unwrap();
            pages.drive(&page.id, command, search).await
        };

        // Navigate: it waits for the load, and says where it ended up.
        let nav = drive(json!({ "action": "navigate", "url": base }))
            .await
            .unwrap();
        assert_eq!(nav, json!({ "url": format!("{base}/"), "title": "Form" }));
        // Viewers of the thread were told an agent is at it.
        let told = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(BusMsg::PageDriven { thread_id, page }) = heard.recv().await {
                    return (thread_id, page);
                }
            }
        })
        .await
        .expect("a PageDriven notice");
        assert_eq!((told.0, told.1.id), (thread.clone(), page.id.clone()));

        // The outline, with refs.
        let snap = drive(json!({ "action": "snapshot" })).await.unwrap();
        let text = snap["snapshot"].as_str().unwrap();
        assert!(text.contains("# Sign up"), "{text}");
        let (field, button, link) = (
            ref_of(text, "input"),
            ref_of(text, "button \"Go\""),
            ref_of(text, "link \"Next page\""),
        );

        // A click is a real one.
        let click = drive(json!({ "action": "click", "ref_id": button }))
            .await
            .unwrap();
        assert_eq!(click["clicked"], "button \"Go\"", "{click}");
        assert_eq!(click["navigated"], false);
        let trusted = drive(json!({ "action": "eval", "script": "window.trusted" }))
            .await
            .unwrap();
        assert_eq!(trusted, json!(true));

        // Typing.
        let typed = drive(json!({ "action": "type", "ref_id": field, "text": "Ada" }))
            .await
            .unwrap();
        assert_eq!(typed["title"], "typed:Ada", "{typed}");
        // Eval: an expression, a function body, an error.
        let eval =
            drive(json!({ "action": "eval", "script": "document.querySelector('#name').value" }))
                .await
                .unwrap();
        assert_eq!(eval, json!("Ada"));
        let body =
            drive(json!({ "action": "eval", "script": "const a = 2; return { a, b: [a * 2] };" }))
                .await
                .unwrap();
        assert_eq!(body, json!({ "a": 2, "b": [4] }));
        let failed = drive(json!({ "action": "eval", "script": "nope()" }))
            .await
            .unwrap_err();
        assert!(
            matches!(&failed, BrowserError::Script(m) if m.contains("nope is not defined")),
            "{failed}"
        );
        assert_eq!(
            drive(json!({ "action": "click", "ref_id": 9999 }))
                .await
                .unwrap_err(),
            BrowserError::RefNotFound(9999)
        );

        // A screenshot is the viewport at the page's scale.
        let shot = drive(json!({ "action": "screenshot" })).await.unwrap();
        assert_eq!(
            (
                &shot["mime_type"],
                &shot["width"],
                &shot["height"],
                &shot["scale"]
            ),
            (&json!("image/png"), &json!(2560), &json!(1600), &json!(2.0))
        );

        // A link that navigates, then history both ways.
        let next = drive(json!({ "action": "click", "ref_id": link }))
            .await
            .unwrap();
        assert_eq!(
            (&next["navigated"], &next["title"]),
            (&json!(true), &json!("Next")),
            "{next}"
        );
        let back = drive(json!({ "action": "back" })).await.unwrap();
        // (The page as it was left, typing and all, if it was kept.)
        assert_eq!(back["url"], format!("{base}/"), "{back}");
        let forward = drive(json!({ "action": "forward" })).await.unwrap();
        assert_eq!(forward["title"], "Next");
        assert_eq!(
            drive(json!({ "action": "forward" })).await.unwrap_err(),
            BrowserError::Action("there is no next page in the history".into())
        );

        // Local files: an agent may open one; the page may not go on to
        // another by itself.
        let built = dir.path().join("built.html");
        let other = dir.path().join("other.html");
        std::fs::write(&other, "<title>Other</title>secret").unwrap();
        std::fs::write(
            &built,
            format!(
                "<title>Built</title><a href=\"{}\">Other</a>",
                url::Url::from_file_path(&other).unwrap()
            ),
        )
        .unwrap();
        let opened = drive(json!({ "action": "navigate", "url": built.to_string_lossy() }))
            .await
            .unwrap();
        assert_eq!(opened["title"], "Built", "{opened}");
        let snap = drive(json!({ "action": "snapshot" })).await.unwrap();
        let text = snap["snapshot"].as_str().unwrap();
        let other_ref = ref_of(text, "link \"Other\"");
        let stopped = drive(json!({ "action": "click", "ref_id": other_ref }))
            .await
            .unwrap();
        assert_ne!(stopped["title"], "Other", "{stopped}");
        assert!(
            stopped["navigation_error"]
                .as_str()
                .is_some_and(|e| e.contains("local file")),
            "{stopped}"
        );
        // Other schemes stay out.
        assert!(matches!(
            drive(json!({ "action": "navigate", "url": "chrome://settings" })).await,
            Err(BrowserError::Blocked(_))
        ));
        // A viewer's address bar: local files only for a client on this
        // computer (`open_file`), never through the general path.
        let other_url = url::Url::from_file_path(&other).unwrap().to_string();
        let nav = loft_protocol::PageNav::Url {
            url: other_url.clone(),
        };
        assert!(pages.navigate(&page.id, nav).await.is_err());
        pages.open_file(&page.id, &other_url).await.unwrap();
        let now = drive(json!({ "action": "eval", "script": "document.title" }))
            .await
            .unwrap();
        assert_eq!(now, json!("Other"));

        pages.close(&page.id).await.unwrap();
        assert_eq!(
            drive(json!({ "action": "snapshot" })).await.unwrap_err(),
            BrowserError::Closed
        );
        pages.shutdown().await;
    }
}
