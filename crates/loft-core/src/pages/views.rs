//! Views agents draw (ADR-0018): pages of the daemon's browser that hold an
//! agent's own HTML instead of a site.
//!
//! - **A place of its own.** Each view lives at an address made up for it
//!   (`https://v<item>.view.agentloft.invalid/`) in a browser context apart
//!   from the user's tabs: its own storage, none of their cookies. The
//!   address is never fetched: every request a view makes stops here
//!   first, and only our own answer for its document and a short list of
//!   script and font hosts get through.
//! - **Written, not loaded.** The page is opened once and the HTML is
//!   written into it (`document.write`), so a call that is still being
//!   written shows up as it arrives, the way a slow page loads.
//! - **A bridge in another world.** What the page may ask of Agentloft
//!   (say something as the user, remember state, its height) goes through a
//!   script the page's own script can't touch, and that script alone can
//!   tell a real click from a pretended one.
//! - **As tall as its content**, between [`loft_view::MIN_HEIGHT`] and
//!   [`loft_view::MAX_HEIGHT`].

use super::{Browser, Page, Pages, State, bad, internal, lock, metrics};
use crate::BusMsg;
use base64::Engine as _;
use loft_protocol::{ErrorBody, ErrorCode, ItemId, PageAction, PageId, PageInfo, ThreadId};
use loft_view::{BINDING, BRIDGE_JS, MAX_HEIGHT, MAX_STATE, MIN_HEIGHT, Theme, WORLD};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// The most views live at once. Past it, the one nobody has watched for
/// longest gives way.
const MAX_VIEWS: usize = 20;
/// A view nobody watches is closed after this long. Reopening it is cheap:
/// the HTML is in the thread, and what it remembered is kept.
const IDLE_VIEW: Duration = Duration::from_secs(120);
/// How tall a view is until it says how tall its content is.
const FIRST_HEIGHT: u32 = 160;
/// Height changes this close together are applied as one.
const HEIGHT_SETTLE: Duration = Duration::from_millis(60);
/// No command to a view's page waits longer than this: a page whose script
/// spins must not hold anything else up.
const PATIENCE: Duration = Duration::from_secs(8);
/// What the page starts as, before anything is written into it.
const SHELL: &str =
    "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>";
/// Problems kept per view, and how long each may be.
const MAX_PROBLEMS: usize = 8;
const MAX_PROBLEM: usize = 300;
/// A page that leaves for somewhere else is brought back this many times.
const MAX_RETURNS: u32 = 3;

/// Saves what a view asked to remember.
pub type StateKeeper = Arc<dyn Fn(&ThreadId, &ItemId, &Value) + Send + Sync>;

/// A view to show: whose it is, its HTML so far, and how the viewer looks.
pub struct ViewRequest {
    pub thread: ThreadId,
    pub item: ItemId,
    pub html: String,
    /// The agent has finished writing it.
    pub done: bool,
    pub width: u32,
    pub scale: f32,
    /// `None`: nobody is looking yet (the look comes with the first viewer).
    pub theme: Option<Theme>,
    /// What the view remembered last time.
    pub state: Value,
    pub keep: StateKeeper,
}

/// The view side of a [`Page`].
pub(super) struct ViewPage {
    pub thread: ThreadId,
    pub item: ItemId,
    origin: String,
    keep: StateKeeper,
    /// One writer at a time: what goes into the document must stay in order.
    writing: tokio::sync::Mutex<()>,
    inner: Mutex<Inner>,
}

struct Inner {
    theme: Theme,
    state: Value,
    /// The agent's HTML as it stands, and whether that is all of it.
    html: String,
    done: bool,
    /// How much of it is in the document, and whether the document was
    /// closed after it.
    written: String,
    closed: bool,
    /// The bridge's world in the document now showing.
    world: Option<i64>,
    /// Script errors and refused loads since the document was written.
    problems: Vec<String>,
    /// When the document last changed or complained.
    touched: Instant,
    /// When the last viewer left (or the page opened with none).
    idle_since: Instant,
    /// The height the content asked for last, waiting to be applied.
    wanted: Option<u32>,
    sizing: bool,
    /// Times the page was brought back from somewhere it tried to go.
    returns: u32,
}

impl ViewPage {
    /// Nobody is watching as of now.
    pub fn left_alone(&self) {
        lock(&self.inner).idle_since = Instant::now();
    }
}

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

/// `#rrggbb` as the colour object DevTools takes.
fn rgba(colour: &str) -> Option<Value> {
    let hex = colour.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok();
    Some(json!({ "r": channel(0)?, "g": channel(2)?, "b": channel(4)?, "a": 1 }))
}

impl Pages {
    /// The page that draws the view, opening it (and the browser) if need
    /// be. One that is already open takes the new width, look and HTML.
    pub async fn open_view(
        &self,
        req: ViewRequest,
        executable: Option<&str>,
        profile: &std::path::Path,
        bus: tokio::sync::broadcast::Sender<BusMsg>,
    ) -> Result<PageInfo, ErrorBody> {
        let (width, _, scale) = super::fit(req.width, FIRST_HEIGHT, req.scale);
        let _one_at_a_time = self.opening.lock().await;
        let browser = match self.current() {
            Some(browser) => browser,
            None => {
                let browser = Browser::start(executable, profile, bus).await?;
                *lock(&self.browser) = Some(browser.clone());
                browser
            }
        };
        if let Some(page) = browser.view_of(&req.thread, &req.item) {
            if let Some(theme) = &req.theme {
                browser.set_view_theme(&page, theme).await;
            }
            browser.set_view_width(&page, width, scale).await?;
            browser.set_view_html(&page, req.html, req.done).await;
            return Ok(lock(&page.state).info.clone());
        }
        // Room for one more: the view nobody has watched for longest goes.
        let views: Vec<Arc<Page>> = lock(&browser.pages)
            .values()
            .filter(|p| p.view.is_some())
            .cloned()
            .collect();
        if views.len() >= MAX_VIEWS {
            let idle = views
                .iter()
                .filter(|p| lock(&p.state).viewers == 0)
                .min_by_key(|p| p.view.as_ref().map(|v| lock(&v.inner).idle_since));
            match idle {
                Some(page) => {
                    browser
                        .close_view(page, "Another view needed the room.")
                        .await
                }
                None => {
                    return Err(ErrorBody::new(
                        ErrorCode::Conflict,
                        "Too many views are showing at once.",
                    ));
                }
            }
        }
        browser
            .new_view_page(req, width, scale)
            .await
            .map_err(internal)
    }

    /// The agent wrote more of a view (or finished it). Nothing happens if
    /// its page isn't open: whoever opens it gets the HTML as it is then.
    pub async fn update_view(&self, thread: &ThreadId, item: &ItemId, html: String, done: bool) {
        let Some(browser) = self.current() else {
            return;
        };
        if let Some(page) = browser.view_of(thread, item) {
            browser.set_view_html(&page, html, done).await;
        }
    }

    /// A viewer's look changed.
    pub async fn theme_view(&self, id: &PageId, theme: &Theme) -> Result<(), ErrorBody> {
        let (browser, page) = self.page(id)?;
        if page.view.is_none() {
            return Err(bad("That page isn't a view."));
        }
        browser.set_view_theme(&page, theme).await;
        Ok(())
    }

    /// Whether `id` is a view's page (and not a tab of the user's browser).
    pub fn is_view(&self, id: &PageId) -> bool {
        self.page(id).is_ok_and(|(_, page)| page.view.is_some())
    }

    /// What went wrong in a view's page, once it has been written and has
    /// had a moment to run (or `wait` is up): script errors, loads that
    /// were refused. Empty if all is well, or if the page isn't open.
    pub async fn view_problems(
        &self,
        thread: &ThreadId,
        item: &ItemId,
        wait: Duration,
    ) -> Vec<String> {
        let deadline = Instant::now() + wait;
        loop {
            let Some(page) = self.current().and_then(|b| b.view_of(thread, item)) else {
                return Vec::new();
            };
            let Some(view) = &page.view else {
                return Vec::new();
            };
            {
                let inner = lock(&view.inner);
                let settled = inner.closed && inner.touched.elapsed() >= Duration::from_millis(450);
                if settled || Instant::now() >= deadline {
                    return inner.problems.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    }
}

impl Browser {
    fn view_of(&self, thread: &ThreadId, item: &ItemId) -> Option<Arc<Page>> {
        lock(&self.pages)
            .values()
            .find(|p| {
                p.view
                    .as_ref()
                    .is_some_and(|v| &v.thread == thread && &v.item == item)
            })
            .cloned()
    }

    /// A command to a view's page that gives up rather than wait forever.
    async fn ask(&self, page: &Page, method: &str, params: Value) -> Result<Value, String> {
        match tokio::time::timeout(PATIENCE, self.cdp.send(method, params, Some(&page.session)))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(format!("the page didn't answer `{method}`")),
        }
    }

    /// Run script in the page: in its own world, or in the bridge's.
    async fn run_in(&self, page: &Page, script: &str, world: Option<i64>) -> Result<Value, String> {
        let mut params = json!({
            "expression": script,
            "returnByValue": true,
            // A page's own script runs inside ours when we write it in:
            // one that spins is cut off.
            "timeout": 5_000,
        });
        if let Some(world) = world {
            params["contextId"] = json!(world);
        }
        let reply = self.ask(page, "Runtime.evaluate", params).await?;
        Ok(reply["result"]["value"].clone())
    }

    /// The browser context views live in, apart from the user's tabs.
    async fn view_context(&self) -> Result<String, String> {
        if let Some(context) = lock(&self.view_context).clone() {
            return Ok(context);
        }
        let context = self
            .cdp
            .send("Target.createBrowserContext", json!({}), None)
            .await?["browserContextId"]
            .as_str()
            .ok_or("the browser made no context for views")?
            .to_owned();
        *lock(&self.view_context) = Some(context.clone());
        Ok(context)
    }

    async fn new_view_page(
        self: &Arc<Self>,
        req: ViewRequest,
        width: u32,
        scale: f32,
    ) -> Result<PageInfo, String> {
        let context = self.view_context().await?;
        let target = self
            .cdp
            .send(
                "Target.createTarget",
                // A window of its own: a tab behind another one in the
                // same window is not drawn, and several views show at once.
                json!({ "url": "about:blank", "browserContextId": context }),
                None,
            )
            .await?["targetId"]
            .as_str()
            .ok_or("the browser made no page for the view")?
            .to_owned();
        let session = self
            .cdp
            .send(
                "Target.attachToTarget",
                json!({ "targetId": target, "flatten": true }),
                None,
            )
            .await?["sessionId"]
            .as_str()
            .ok_or("the browser gave no session")?
            .to_owned();
        let (input, inputs) = mpsc::unbounded_channel();
        let info = PageInfo {
            id: PageId::new(),
            thread_id: req.thread.clone(),
            url: String::new(),
            title: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            width,
            height: FIRST_HEIGHT,
            scale,
            view: true,
            overflow: false,
        };
        let now = Instant::now();
        let page = Arc::new(Page {
            id: info.id.clone(),
            target,
            session: session.clone(),
            input,
            shaping: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                info: info.clone(),
                frame: None,
                seq: 0,
                viewers: 0,
                motion: 0,
                last_cast: 0,
                main_frame: None,
                actor: None,
                picturing: false,
                changed_again: false,
                last_shot: 0,
            }),
            view: Some(ViewPage {
                origin: loft_view::origin(req.item.as_str()),
                thread: req.thread,
                item: req.item,
                keep: req.keep,
                writing: tokio::sync::Mutex::new(()),
                inner: Mutex::new(Inner {
                    theme: req.theme.unwrap_or_default(),
                    state: req.state,
                    html: req.html,
                    done: req.done,
                    written: String::new(),
                    closed: false,
                    world: None,
                    problems: Vec::new(),
                    touched: now,
                    idle_since: now,
                    wanted: None,
                    sizing: false,
                    returns: 0,
                }),
            }),
        });
        lock(&self.sessions).insert(session, page.id.clone());
        lock(&self.pages).insert(page.id.clone(), page.clone());
        tokio::spawn(self.clone().replay_input(page.clone(), inputs));
        if !self.sweeping.swap(true, Ordering::Relaxed) {
            tokio::spawn(self.clone().sweep_views());
        }
        let setup = async {
            self.ask(&page, "Page.enable", json!({})).await?;
            self.ask(&page, "Runtime.enable", json!({})).await?;
            // Every request the page makes stops at `on_view_request`.
            self.ask(
                &page,
                "Fetch.enable",
                json!({ "patterns": [{ "urlPattern": "*" }] }),
            )
            .await?;
            self.ask(&page, "Emulation.setDeviceMetricsOverride", metrics(&info))
                .await?;
            self.ask(
                &page,
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": true }),
            )
            .await?;
            // Reachable from the bridge's world only.
            self.ask(
                &page,
                "Runtime.addBinding",
                json!({ "name": BINDING, "executionContextName": WORLD }),
            )
            .await?;
            self.dress(&page).await;
            self.write_view(&page).await
        };
        if let Err(err) = setup.await {
            self.close_view(&page, "The view didn't open.").await;
            return Err(err);
        }
        Ok(lock(&page.state).info.clone())
    }

    /// What the browser itself must know of the look: light or dark, and
    /// the colour behind the page (so nothing flashes white).
    async fn dress(&self, page: &Page) {
        let Some(view) = &page.view else {
            return;
        };
        let theme = lock(&view.inner).theme.cleaned();
        let _ = self
            .ask(
                page,
                "Emulation.setEmulatedMedia",
                json!({ "features": [{
                    "name": "prefers-color-scheme",
                    "value": if theme.dark { "dark" } else { "light" },
                }] }),
            )
            .await;
        if let Some(colour) = rgba(&theme.background) {
            let _ = self
                .ask(
                    page,
                    "Emulation.setDefaultBackgroundColorOverride",
                    json!({ "color": colour }),
                )
                .await;
        }
    }

    async fn set_view_theme(&self, page: &Arc<Page>, theme: &Theme) {
        let Some(view) = &page.view else {
            return;
        };
        let theme = theme.cleaned();
        {
            let mut inner = lock(&view.inner);
            if inner.theme == theme {
                return;
            }
            inner.theme = theme.clone();
        }
        self.dress(page).await;
        if lock(&view.inner).world.is_some() {
            let _ = self
                .run_in(page, &loft_view::theme_script(&theme), None)
                .await;
        }
    }

    /// A viewer's width: the page lays out to it, and stays as tall as its
    /// content says.
    async fn set_view_width(
        self: &Arc<Self>,
        page: &Arc<Page>,
        width: u32,
        scale: f32,
    ) -> Result<(), ErrorBody> {
        let height = lock(&page.state).info.height;
        self.resize(page, width, height, scale).await
    }

    async fn set_view_html(self: &Arc<Self>, page: &Arc<Page>, html: String, done: bool) {
        let Some(view) = &page.view else {
            return;
        };
        {
            let mut inner = lock(&view.inner);
            if inner.html == html && inner.done == done {
                return;
            }
            // Word of an earlier moment arriving late: the page is ahead.
            if !done && (inner.done || inner.html.starts_with(&html)) {
                return;
            }
            inner.html = html;
            inner.done = done;
        }
        if let Err(err) = self.write_view(page).await {
            tracing::debug!(%err, "writing a view");
        }
    }

    /// Load the empty page at the view's address and get it ready to be
    /// written into: the bridge's world, an open document, the bridge.
    async fn load_view(&self, page: &Page, view: &ViewPage) -> Result<(), String> {
        {
            let mut inner = lock(&view.inner);
            inner.written.clear();
            inner.closed = false;
            inner.world = None;
            inner.problems.clear();
            inner.touched = Instant::now();
        }
        let nav = self
            .ask(page, "Page.navigate", json!({ "url": view.origin }))
            .await?;
        let frame = nav["frameId"]
            .as_str()
            .ok_or("the view's page has no frame")?
            .to_owned();
        // Until the empty page is really there: writing into one that is
        // still arriving would be lost.
        let there = format!(
            "document.readyState === 'complete' && location.href === {}",
            Value::String(view.origin.clone())
        );
        let mut loaded = false;
        for _ in 0..150 {
            if self.run_in(page, &there, None).await? == Value::Bool(true) {
                loaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if !loaded {
            return Err("the view's page didn't load".into());
        }
        let world = self
            .ask(
                page,
                "Page.createIsolatedWorld",
                json!({ "frameId": frame, "worldName": WORLD }),
            )
            .await?["executionContextId"]
            .as_i64()
            .ok_or("the view's page has no world for the bridge")?;
        // Opening the document drops every listener on it, so the bridge
        // goes in after.
        self.run_in(page, "document.open()", None).await?;
        self.run_in(page, BRIDGE_JS, Some(world)).await?;
        let head = {
            let inner = lock(&view.inner);
            loft_view::head(&inner.theme, &inner.state)
        };
        self.run_in(
            page,
            &format!("document.write({})", Value::String(head)),
            None,
        )
        .await?;
        lock(&view.inner).world = Some(world);
        Ok(())
    }

    /// Bring the document up to the HTML as it stands: write what is new,
    /// close it once the agent is done, start over if what was written is
    /// no longer how the HTML begins.
    async fn write_view(&self, page: &Page) -> Result<(), String> {
        let Some(view) = &page.view else {
            return Ok(());
        };
        let _in_order = view.writing.lock().await;
        for _ in 0..3 {
            let (more, close, afresh) = {
                let inner = lock(&view.inner);
                let follows = inner.html.starts_with(&inner.written);
                let more = if follows {
                    inner.html[inner.written.len()..].to_owned()
                } else {
                    String::new()
                };
                let afresh =
                    inner.world.is_none() || !follows || (inner.closed && !more.is_empty());
                (more, inner.done && !inner.closed, afresh)
            };
            if afresh {
                self.load_view(page, view).await?;
                continue;
            }
            if !more.is_empty() {
                self.run_in(
                    page,
                    &format!("document.write({})", Value::String(more.clone())),
                    None,
                )
                .await?;
                let mut inner = lock(&view.inner);
                inner.written.push_str(&more);
                inner.touched = Instant::now();
            }
            if close {
                self.run_in(page, "document.close()", None).await?;
                let mut inner = lock(&view.inner);
                inner.closed = true;
                inner.touched = Instant::now();
            }
            return Ok(());
        }
        Err("the view's page wouldn't take its HTML".into())
    }

    fn note_problem(&self, view: &ViewPage, problem: String) {
        let mut inner = lock(&view.inner);
        inner.touched = Instant::now();
        let problem = clip(problem.trim(), MAX_PROBLEM);
        if inner.problems.len() < MAX_PROBLEMS && !inner.problems.contains(&problem) {
            inner.problems.push(problem);
        }
    }

    /// Something a view's page did. `true` if that is all there is to it
    /// (a view has no address bar or history for the rest to keep up).
    pub(super) async fn on_view_event(
        self: &Arc<Self>,
        page: &Arc<Page>,
        event: &super::Event,
    ) -> bool {
        let Some(view) = &page.view else {
            return false;
        };
        let params = &event.params;
        match event.method.as_str() {
            "Fetch.requestPaused" => self.on_view_request(page, view, params).await,
            "Runtime.bindingCalled" if params["name"] == BINDING => {
                if let Ok(call) =
                    serde_json::from_str::<Value>(params["payload"].as_str().unwrap_or_default())
                {
                    self.on_view_call(page, view, &call);
                }
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                let what = details["exception"]["description"]
                    .as_str()
                    .or_else(|| details["text"].as_str())
                    .unwrap_or("a script error");
                // The first line says what; the rest is a stack of ours.
                let what = what.lines().next().unwrap_or(what);
                self.note_problem(view, format!("Script error: {what}"));
            }
            "Runtime.consoleAPICalled" if params["type"] == "error" => {
                let said: Vec<String> = params["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| {
                        a["value"]
                            .as_str()
                            .map(str::to_owned)
                            .or_else(|| a["description"].as_str().map(str::to_owned))
                            .or_else(|| a.get("value").map(Value::to_string))
                    })
                    .collect();
                self.note_problem(view, format!("console.error: {}", said.join(" ")));
            }
            "Page.frameNavigated" => {
                let frame = &params["frame"];
                let url = frame["url"].as_str().unwrap_or_default();
                // Our own page, or the blank one it starts from.
                if frame["parentId"].is_string() || url == view.origin || url == "about:blank" {
                    return true;
                }
                // It went somewhere (nowhere, in fact: the request was
                // refused and this is the browser's error page). Bring the
                // view back, a few times at most.
                let again = {
                    let mut inner = lock(&view.inner);
                    inner.returns += 1;
                    inner.world = None;
                    inner.returns <= MAX_RETURNS
                };
                if again {
                    let (browser, page) = (self.clone(), page.clone());
                    tokio::spawn(async move {
                        if let Err(err) = browser.write_view(&page).await {
                            tracing::debug!(%err, "bringing a view back");
                        }
                    });
                }
            }
            // A view has no address, history or loading state to keep.
            "Page.navigatedWithinDocument"
            | "Page.frameStartedLoading"
            | "Page.frameStoppedLoading" => {}
            // Pictures, dialogs, crashes: as for any page.
            _ => return false,
        }
        true
    }

    /// A request the page made: its own document is answered here, the few
    /// hosts a view may load from go through, the rest never leaves.
    async fn on_view_request(&self, page: &Page, view: &ViewPage, params: &Value) {
        let id = params["requestId"].clone();
        let request = &params["request"];
        let url = request["url"].as_str().unwrap_or_default();
        let document = params["resourceType"] == "Document";
        let ours = url.starts_with(&view.origin);
        let (method, answer) = if ours && document {
            (
                "Fetch.fulfillRequest",
                json!({
                    "requestId": id,
                    "responseCode": 200,
                    "responseHeaders": [
                        { "name": "Content-Type", "value": "text/html; charset=utf-8" },
                        { "name": "Cache-Control", "value": "no-store" },
                    ],
                    "body": base64::engine::general_purpose::STANDARD.encode(SHELL),
                }),
            )
        } else if ours {
            // Its icon, and whatever else a browser asks its site for.
            (
                "Fetch.fulfillRequest",
                json!({ "requestId": id, "responseCode": 404 }),
            )
        } else if !document && request["method"] == "GET" && loft_view::allowed_url(url) {
            ("Fetch.continueRequest", json!({ "requestId": id }))
        } else {
            self.note_problem(
                view,
                if document {
                    format!(
                        "The page tried to go to {}: a view can't navigate. Use agentloft.open(url) on a click to open a link.",
                        clip(url, 120)
                    )
                } else {
                    format!(
                        "Blocked {}: a view has no network beyond the script and font hosts show_ui lists. Put everything else in the page.",
                        clip(url, 120)
                    )
                },
            );
            (
                "Fetch.failRequest",
                json!({ "requestId": id, "errorReason": "BlockedByClient" }),
            )
        };
        if let Err(err) = self.ask(page, method, answer).await {
            tracing::debug!(%err, "answering a view's request");
        }
    }

    /// What the bridge passed on from the page.
    fn on_view_call(self: &Arc<Self>, page: &Arc<Page>, view: &ViewPage, call: &Value) {
        let text = || call["text"].as_str().unwrap_or_default().to_owned();
        tracing::debug!(kind = call["kind"].as_str().unwrap_or("?"), "a view asked");
        let action = match call["kind"].as_str() {
            Some("size") => {
                let Some(height) = call["height"].as_u64() else {
                    return;
                };
                let start = {
                    let mut inner = lock(&view.inner);
                    inner.wanted = Some(height.min(u64::from(u32::MAX)) as u32);
                    !std::mem::replace(&mut inner.sizing, true)
                };
                if start {
                    tokio::spawn(self.clone().size_view(page.clone()));
                }
                return;
            }
            Some("state") => {
                let state = call["value"].clone();
                if state.to_string().len() > MAX_STATE {
                    self.note_problem(
                        view,
                        format!("agentloft.setState: over {MAX_STATE} bytes, not kept"),
                    );
                    return;
                }
                lock(&view.inner).state = state.clone();
                (view.keep)(&view.thread, &view.item, &state);
                return;
            }
            Some("refused") => {
                self.note_problem(
                    view,
                    format!(
                        "agentloft.{}() was called with no click behind it: it only works from the user's own click or key press",
                        call["what"].as_str().unwrap_or("send")
                    ),
                );
                return;
            }
            Some("send") => PageAction::Send { text: text() },
            Some("copy") => PageAction::Copy { text: text() },
            Some("open") => PageAction::Open { target: text() },
            _ => return,
        };
        let to = lock(&page.state).actor;
        let _ = self.bus.send(BusMsg::PageAction {
            id: page.id.clone(),
            action,
            to,
        });
    }

    /// Make the page as tall as its content asked, once the asking has
    /// settled (content arriving changes its mind many times a second).
    async fn size_view(self: Arc<Self>, page: Arc<Page>) {
        let Some(view) = &page.view else {
            return;
        };
        loop {
            tokio::time::sleep(HEIGHT_SETTLE).await;
            let Some(wanted) = lock(&view.inner).wanted.take() else {
                lock(&view.inner).sizing = false;
                return;
            };
            if self.dead.load(Ordering::Relaxed) || !lock(&self.pages).contains_key(&page.id) {
                lock(&view.inner).sizing = false;
                return;
            }
            let height = wanted.clamp(MIN_HEIGHT, MAX_HEIGHT);
            let (width, scale) = {
                let mut state = lock(&page.state);
                let overflow = wanted > MAX_HEIGHT;
                if state.info.overflow != overflow {
                    state.info.overflow = overflow;
                    let _ = self.bus.send(BusMsg::PageState {
                        id: page.id.clone(),
                    });
                }
                (state.info.width, state.info.scale)
            };
            if let Err(err) = self.resize(&page, width, height, scale).await {
                tracing::debug!(err = %err.message, "sizing a view");
            }
        }
    }

    async fn close_view(self: &Arc<Self>, page: &Arc<Page>, reason: &str) {
        let _ = self
            .cdp
            .send(
                "Target.closeTarget",
                json!({ "targetId": page.target }),
                None,
            )
            .await;
        self.forget(&page.id, reason);
    }

    /// Close views nobody has watched for a while.
    async fn sweep_views(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            if self.dead.load(Ordering::Relaxed) {
                return;
            }
            let idle: Vec<Arc<Page>> = lock(&self.pages)
                .values()
                .filter(|p| {
                    lock(&p.state).viewers == 0
                        && p.view
                            .as_ref()
                            .is_some_and(|v| lock(&v.inner).idle_since.elapsed() >= IDLE_VIEW)
                })
                .cloned()
                .collect();
            for page in idle {
                self.close_view(&page, "Nobody was looking at the view.")
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_become_devtools_colours() {
        assert_eq!(
            rgba("#18181b"),
            Some(json!({ "r": 24, "g": 24, "b": 27, "a": 1 }))
        );
        assert_eq!(rgba("rgb(1,2,3)"), None);
        assert_eq!(rgba("#fff"), None);
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
    }

    /// The whole path against a real browser: a view is written a piece at
    /// a time, sizes itself, reaches Agentloft only through the bridge and
    /// only on a real click, remembers state, and has its mistakes read
    /// back. Needs Chrome (or Chromium, Edge, Brave) installed:
    /// `cargo test -p loft-core pages::views::tests::live -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_view_is_written_bridged_and_checked() {
        use loft_protocol::{PageButton, PageInput, PageMouse};
        use tokio::sync::broadcast;

        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        let (bus, mut heard) = broadcast::channel(1024);
        let pages = Pages::default();
        let (thread, item) = (ThreadId::new(), ItemId::new());
        let kept: Arc<Mutex<Vec<Value>>> = Arc::default();
        let keep: StateKeeper = {
            let kept = kept.clone();
            Arc::new(move |_, _, state| lock(&kept).push(state.clone()))
        };
        const HTML: &str = "<div style=\"height:300px\">tall</div>\
            <button id=\"go\" style=\"position:fixed;left:0;top:0;width:200px;height:60px\" \
              onclick=\"agentloft.setState({ clicks: (agentloft.state?.clicks || 0) + 1 }); \
                       agentloft.send('picked ' + agentloft.state.clicks)\">go</button>\
            <script>agentloft.send('nobody clicked'); fetch('https://example.com/x').catch(() => {});\
            </script><script>missing()</script>";
        let request = |html: &str, done: bool| ViewRequest {
            thread: thread.clone(),
            item: item.clone(),
            html: html.to_owned(),
            done,
            width: 500,
            scale: 1.0,
            theme: Some(Theme::default()),
            state: json!({ "clicks": 4 }),
            keep: keep.clone(),
        };

        // Half the page first, as an agent writes it; then the rest.
        let half = HTML.len() / 2;
        let page = pages
            .open_view(request(&HTML[..half], false), None, &profile, bus.clone())
            .await
            .unwrap();
        assert!(page.view && pages.is_view(&page.id));
        let whole = pages
            .open_view(request(HTML, true), None, &profile, bus.clone())
            .await
            .unwrap();
        assert_eq!(whole.id, page.id, "the same view is the same page");

        // It says what went wrong in it: the script error, the load that
        // was refused, the send nobody clicked for.
        let problems = pages
            .view_problems(&thread, &item, Duration::from_secs(10))
            .await;
        let said = problems.join(" | ");
        assert!(said.contains("missing is not defined"), "{said}");
        assert!(said.contains("Blocked https://example.com/x"), "{said}");
        assert!(
            said.contains("agentloft.send() was called with no click"),
            "{said}"
        );

        // It is as tall as its content.
        let sized = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let height = pages.info(&page.id).unwrap().height;
                if height >= 300 {
                    return height;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the view takes its content's height");
        assert!((300..400).contains(&sized), "{sized}");

        // A real click: the state is kept (counting on from what it was
        // given) and the message goes out, once, to whoever clicked.
        pages.touched(&page.id, 7);
        for action in [PageMouse::Down, PageMouse::Up] {
            pages
                .input(
                    &page.id,
                    PageInput::Mouse {
                        action,
                        x: 30.0,
                        y: 20.0,
                        button: PageButton::Left,
                        click_count: 1,
                        modifiers: 0,
                    },
                )
                .unwrap();
        }
        let action = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(BusMsg::PageAction { id, action, to }) = heard.recv().await {
                    assert_eq!((id, to), (page.id.clone(), Some(7)));
                    return action;
                }
            }
        })
        .await
        .expect("the click's message");
        assert_eq!(
            action,
            PageAction::Send {
                text: "picked 5".into()
            }
        );
        assert_eq!(lock(&kept).as_slice(), [json!({ "clicks": 5 })]);
        pages.shutdown().await;
    }

    /// The height in pixels a JPEG says it has.
    fn jpeg_height(jpeg: &[u8]) -> Option<u32> {
        let mut at = 2;
        while at + 9 < jpeg.len() {
            if jpeg[at] != 0xff {
                at += 1;
                continue;
            }
            if matches!(jpeg[at + 1], 0xc0..=0xc2) {
                return Some(u32::from(u16::from_be_bytes([jpeg[at + 5], jpeg[at + 6]])));
            }
            at += 2 + usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        }
        None
    }

    /// Views that are watched from the moment they open, on a dense screen,
    /// and never hold still: each ends up pictured as tall as its content.
    /// (Chrome puts a page back to the size it had when a screenshot began,
    /// so a height set while one was being taken used to be lost, and the
    /// view stayed a strip of its first 160 px.)
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_views_watched_from_the_start_are_pictured_whole() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        let (bus, _heard) = tokio::sync::broadcast::channel(4096);
        let pages = Pages::default();
        let thread = ThreadId::new();
        let keep: StateKeeper = Arc::new(|_, _, _| {});
        let mut opened = Vec::new();
        for tall in [420u32, 260, 610, 340] {
            let html = format!(
                "<div id=\"box\" style=\"height:{tall}px\"></div>\
                 <script>setInterval(() => box.textContent = Date.now(), 20)</script>"
            );
            let request = ViewRequest {
                thread: thread.clone(),
                item: ItemId::new(),
                html,
                done: true,
                width: 500,
                scale: 2.0,
                theme: Some(Theme::default()),
                state: Value::Null,
                keep: keep.clone(),
            };
            let page = pages
                .open_view(request, None, &profile, bus.clone())
                .await
                .unwrap();
            pages.watching(&page.id, true).await;
            opened.push((page.id, tall));
        }
        for (id, tall) in opened {
            let whole = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let (info, frame, _) = pages.snapshot(&id).unwrap();
                    let pictured = frame.and_then(|frame| jpeg_height(&frame.jpeg.0));
                    if info.height >= tall && pictured == Some(info.height * 2) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            let (info, frame, _) = pages.snapshot(&id).unwrap();
            assert!(
                whole.is_ok(),
                "a view {} px tall is pictured {:?} px tall (at twice the scale)",
                info.height,
                frame.and_then(|frame| jpeg_height(&frame.jpeg.0)),
            );
        }
        pages.shutdown().await;
    }

    /// Quick clicks, each changing the page: the picture left at the end
    /// shows the last change, not the one before it. (The screencast drops
    /// what it can't send, so the last change of a burst could go unsaid
    /// and the view stayed a step behind until something else moved.)
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_views_last_change_is_pictured() {
        use loft_protocol::{PageButton, PageInput, PageMouse};

        // Every click swaps a patch between flat grey and noise, which a
        // JPEG's size tells apart without decoding it.
        const HTML: &str = "<canvas id=\"c\" width=\"120\" height=\"120\" \
            style=\"position:fixed;left:0;top:0;width:120px;height:120px\"></canvas>\
            <div style=\"height:200px\"></div>\
            <script>let n = 0; const x = c.getContext('2d');\
            function draw() { if (n % 2) { const d = x.createImageData(120, 120);\
              for (let i = 0; i < d.data.length; i++) d.data[i] = (Math.imul(i, 2654435761) >>> 13) & 255;\
              x.putImageData(d, 0, 0); } else { x.fillStyle = '#888'; x.fillRect(0, 0, 120, 120); } }\
            c.onclick = () => { n++; draw(); }; draw();</script>";
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        let (bus, _heard) = tokio::sync::broadcast::channel(4096);
        let pages = Pages::default();
        let request = ViewRequest {
            thread: ThreadId::new(),
            item: ItemId::new(),
            html: HTML.to_owned(),
            done: true,
            width: 500,
            scale: 2.0,
            theme: Some(Theme::default()),
            state: Value::Null,
            keep: Arc::new(|_, _, _| {}),
        };
        let page = pages
            .open_view(request, None, &profile, bus.clone())
            .await
            .unwrap();
        pages.watching(&page.id, true).await;
        let click = |count: u8| {
            for action in [PageMouse::Down, PageMouse::Up] {
                pages
                    .input(
                        &page.id,
                        PageInput::Mouse {
                            action,
                            x: 60.0,
                            y: 60.0,
                            button: PageButton::Left,
                            click_count: count,
                            modifiers: 0,
                        },
                    )
                    .unwrap();
            }
        };
        let weight = || {
            let (_, frame, _) = pages.snapshot(&page.id).unwrap();
            frame.map_or(0, |frame| frame.jpeg.0.len())
        };
        let rest = |ms: u64| tokio::time::sleep(Duration::from_millis(ms));

        rest(800).await;
        let flat = weight();
        click(1);
        rest(600).await;
        let noisy = weight();
        assert!(noisy > flat + 2_000, "flat {flat}, noisy {noisy}");
        click(1);
        rest(600).await;
        let middle = (flat + noisy) / 2;

        let mut odd = false;
        for round in 0..20u64 {
            let clicks = 1 + round * 7 % 4;
            for n in 0..clicks {
                click((n + 1).min(3) as u8);
                rest([10, 20, 40][((round + n) % 3) as usize]).await;
            }
            odd ^= clicks % 2 == 1;
            rest(500).await;
            assert_eq!(
                weight() > middle,
                odd,
                "round {round}: after {clicks} quick clicks the picture is a step behind"
            );
        }
        pages.shutdown().await;
    }

    /// A click lands where it was aimed while the view is being pictured.
    /// (For the length of a screenshot Chrome draws a page scaled to the
    /// viewer's pixel density. Input that landed meanwhile landed at its
    /// coordinates divided by that: on a dense screen, a click on a button
    /// in a view that was changing selected text halfway to it instead.)
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_click_lands_where_it_was_aimed_while_the_view_is_pictured() {
        use loft_protocol::{PageButton, PageInput, PageMouse};

        // Something always moves, so pictures are always being taken; every
        // press is remembered with where it arrived.
        const HTML: &str = "<div id=\"spin\" style=\"position:absolute;right:10px;top:10px;\
            width:30px;height:30px;background:#2a78d6\"></div><div style=\"height:400px\"></div>\
            <script>const log = []; let turn = 0;\
            setInterval(() => { spin.style.transform = 'rotate(' + (turn += 7) + 'deg)'; }, 30);\
            document.addEventListener('mousedown', (e) => {\
              log.push([Math.round(e.clientX), Math.round(e.clientY)]);\
              agentloft.setState({ log }); });</script>";
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        let (bus, _heard) = tokio::sync::broadcast::channel(4096);
        let pages = Pages::default();
        let kept: Arc<Mutex<Vec<Value>>> = Arc::default();
        let keep: StateKeeper = {
            let kept = kept.clone();
            Arc::new(move |_, _, state| lock(&kept).push(state.clone()))
        };
        let request = ViewRequest {
            thread: ThreadId::new(),
            item: ItemId::new(),
            html: HTML.to_owned(),
            done: true,
            width: 600,
            scale: 2.0,
            theme: Some(Theme::default()),
            state: Value::Null,
            keep,
        };
        let page = pages
            .open_view(request, None, &profile, bus.clone())
            .await
            .unwrap();
        pages.watching(&page.id, true).await;
        let rest = |ms: u64| tokio::time::sleep(Duration::from_millis(ms));
        rest(1_000).await;

        let mut aimed = Vec::new();
        for n in 0..25u32 {
            let (x, y) = (150 + n * 37 % 400, 120 + n * 53 % 250);
            for action in [PageMouse::Down, PageMouse::Up] {
                pages
                    .input(
                        &page.id,
                        PageInput::Mouse {
                            action,
                            x: x as f32,
                            y: y as f32,
                            button: PageButton::Left,
                            click_count: 1,
                            modifiers: 0,
                        },
                    )
                    .unwrap();
                rest(40).await;
            }
            aimed.push(json!([x, y]));
            rest(60).await;
        }
        rest(800).await;
        let landed = lock(&kept).last().cloned().unwrap_or_default();
        assert_eq!(landed["log"], Value::Array(aimed));
        pages.shutdown().await;
    }
}
