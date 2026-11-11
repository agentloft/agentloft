//! The daemon's own browser (ADR-0017): pages it loads in a headless Chrome
//! on this machine and streams to whoever is looking.
//!
//! - One browser process, started with the first page and stopped a while
//!   after the last one closes. It has a profile of its own under the
//!   Agentloft home, and is driven over a private pipe ([`cdp`]).
//! - One page per thread. While someone watches, Chrome's screencast sends
//!   JPEG pictures of it; the newest is kept and announced on the bus, and
//!   each viewer takes it when it is ready for one. Nobody watching, no
//!   pictures.
//! - What a viewer does comes back as [`PageInput`] and is replayed into
//!   the page as real input, in the order it was sent.

mod cdp;
mod chrome;
mod keys;
mod views;

pub use views::{StateKeeper, ViewRequest};

use crate::BusMsg;
use base64::Engine as _;
use cdp::{Cdp, Event};
use loft_protocol::{
    ErrorBody, ErrorCode, PageButton, PageFrame, PageId, PageInfo, PageInput, PageMouse, PageNav,
    ThreadId,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

/// The least time between pictures: about 14 a second at most.
const FRAME_PACE: Duration = Duration::from_millis(70);
/// A page that has held still this long gets one sharp picture.
const STILL_AFTER: Duration = Duration::from_millis(250);
/// How often a watched page's title is looked at: Chrome announces a title
/// around navigations, not when a page changes it.
const TITLE_EVERY: Duration = Duration::from_millis(1500);
/// The browser stops this long after its last page closes.
const IDLE_BROWSER: Duration = Duration::from_secs(300);
const MOVING_QUALITY: u32 = 72;
const STILL_QUALITY: u32 = 88;
/// A view (ADR-0018) is small, and mostly text and thin lines that are
/// read while they move (a slider's number, a chart redrawing): every
/// picture of one is taken at the viewer's real scale.
const VIEW_QUALITY: u32 = 90;
/// A view is looked at again no sooner than this after the last look began
/// (about 20 pictures a second while it moves).
const VIEW_EVERY: Duration = Duration::from_millis(50);
const MAX_PAGES: usize = 16;
const MIN_SIDE: u32 = 120;
const MAX_SIDE: u32 = 3840;
/// A paste or an input method's text is cut off here.
const MAX_TEXT: usize = 100_000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn internal(message: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, message)
}

fn bad(message: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

fn gone() -> ErrorBody {
    ErrorBody::new(ErrorCode::NotFound, "That browser tab is closed.")
}

/// The pages and the browser they live in.
#[derive(Default)]
pub struct Pages {
    /// Held while a browser starts or a page opens, so neither happens
    /// twice at once.
    opening: tokio::sync::Mutex<()>,
    browser: Mutex<Option<Arc<Browser>>>,
}

struct Browser {
    cdp: Arc<Cdp>,
    bus: broadcast::Sender<BusMsg>,
    pages: Mutex<HashMap<PageId, Arc<Page>>>,
    /// DevTools session → page.
    sessions: Mutex<HashMap<String, PageId>>,
    /// Windows a page opened, until their address is known: target → opener.
    popups: Mutex<HashMap<String, PageId>>,
    dead: AtomicBool,
    /// When the last page closed (for the idle stop).
    emptied: Mutex<Instant>,
    /// The browser context views live in (ADR-0018), once there is one.
    view_context: Mutex<Option<String>>,
    /// Whether the task that closes idle views is running.
    sweeping: AtomicBool,
}

struct Page {
    id: PageId,
    target: String,
    session: String,
    /// Input in the order it arrived; one task replays it.
    input: mpsc::UnboundedSender<PageInput>,
    state: Mutex<State>,
    /// Held while the page is given a size, while it is photographed and
    /// while it is given input, one at a time. A screenshot is not a quiet
    /// thing in Chrome: for its length the page is drawn scaled to the
    /// viewer's pixel density (so input lands at scaled coordinates), and
    /// when it is done the page is put back to the size it had when the
    /// screenshot began (so a size set in between is undone).
    shaping: tokio::sync::Mutex<()>,
    /// Set when the page draws a view an agent made rather than a site.
    view: Option<views::ViewPage>,
}

struct State {
    info: PageInfo,
    frame: Option<Arc<Vec<u8>>>,
    seq: u64,
    viewers: usize,
    /// Counts screencast pictures, to tell a still page from a moving one.
    motion: u64,
    /// The last screencast picture, hashed: Chrome sends the same one again
    /// after a screenshot, and that is not news.
    last_cast: u64,
    main_frame: Option<String>,
    /// The connection whose input reached the page last: who a view's
    /// request (say this, copy that) is for.
    actor: Option<u64>,
    /// A view is being pictured right now, and has been said to have
    /// changed again since the last look began.
    picturing: bool,
    changed_again: bool,
    /// The last picture taken of a view, hashed: the same one again isn't
    /// sent, and means the view holds still.
    last_shot: u64,
}

impl Browser {
    /// Look at a view again: it changed, or was just clicked or typed in.
    fn picture_soon(self: &Arc<Self>, page: &Arc<Page>) {
        {
            let mut state = lock(&page.state);
            if state.viewers == 0 {
                return;
            }
            if state.picturing {
                state.changed_again = true;
                return;
            }
            state.picturing = true;
        }
        tokio::spawn(self.clone().picture_view(page.clone()));
    }

    /// Picture a view until it holds still: one screenshot at a time,
    /// always of the page as it is now, at its real scale.
    ///
    /// The screencast says "something changed", but not every time: Chrome
    /// drops the pictures it can't send while earlier ones are waiting to
    /// be acknowledged, and what it dropped last it never sends. A view
    /// whose last change went unsaid would show the step before it until
    /// something else moved. So a look that shows a change is followed by
    /// another, until one shows the same picture again.
    async fn picture_view(self: Arc<Self>, page: Arc<Page>) {
        loop {
            let began = tokio::time::Instant::now();
            let shot = self.screenshot(&page, VIEW_QUALITY).await;
            {
                let mut state = lock(&page.state);
                let mut moved = false;
                if let Some(jpeg) = shot {
                    let print = fingerprint(&jpeg);
                    if state.last_shot != print || state.frame.is_none() {
                        state.last_shot = print;
                        state.seq += 1;
                        state.frame = Some(Arc::new(jpeg));
                        let _ = self.bus.send(BusMsg::PageFrame {
                            id: page.id.clone(),
                            seq: state.seq,
                        });
                        moved = true;
                    }
                }
                let told = std::mem::take(&mut state.changed_again);
                if !(moved || told) || state.viewers == 0 || self.dead.load(Ordering::Relaxed) {
                    state.picturing = false;
                    return;
                }
            }
            tokio::time::sleep_until(began + VIEW_EVERY).await;
        }
    }

    /// A picture of the page at its real scale, as a JPEG.
    async fn screenshot(&self, page: &Page, quality: u32) -> Option<Vec<u8>> {
        let _shaping = page.shaping.lock().await;
        let shot = self
            .cdp
            .send(
                "Page.captureScreenshot",
                json!({ "format": "jpeg", "quality": quality, "optimizeForSpeed": true }),
                Some(&page.session),
            )
            .await
            .ok()?;
        base64::engine::general_purpose::STANDARD
            .decode(shot["data"].as_str()?)
            .ok()
    }
}

/// A picture's bytes as one number, to tell "the same again" cheaply.
fn fingerprint(jpeg: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    jpeg.hash(&mut hasher);
    hasher.finish()
}

/// A size a page can have, whatever a client asked for.
fn fit(width: u32, height: u32, scale: f32) -> (u32, u32, f32) {
    let scale = if scale.is_finite() {
        scale.clamp(1.0, 3.0)
    } else {
        1.0
    };
    (
        width.clamp(MIN_SIDE, MAX_SIDE),
        height.clamp(MIN_SIDE, MAX_SIDE),
        scale,
    )
}

/// Addresses a page may be sent to. Local files and the browser's own
/// pages are not among them.
fn check_url(url: &str) -> Result<String, ErrorBody> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if lower == "about:blank"
        || ((lower.starts_with("http://") || lower.starts_with("https://"))
            && url.len() <= 8192
            && !url.contains(char::is_whitespace))
    {
        Ok(url.to_owned())
    } else {
        Err(bad("Only http and https addresses can be opened."))
    }
}

impl Pages {
    fn current(&self) -> Option<Arc<Browser>> {
        lock(&self.browser)
            .clone()
            .filter(|b| !b.dead.load(Ordering::Relaxed))
    }

    fn page(&self, id: &PageId) -> Result<(Arc<Browser>, Arc<Page>), ErrorBody> {
        let browser = self.current().ok_or_else(gone)?;
        let page = lock(&browser.pages).get(id).cloned().ok_or_else(gone)?;
        Ok((browser, page))
    }

    /// The thread's page, opening it (and the browser) if need be.
    #[allow(clippy::too_many_arguments)]
    pub async fn open(
        &self,
        thread: ThreadId,
        url: Option<String>,
        width: u32,
        height: u32,
        scale: f32,
        executable: Option<&str>,
        profile: &Path,
        bus: broadcast::Sender<BusMsg>,
    ) -> Result<PageInfo, ErrorBody> {
        let (width, height, scale) = fit(width, height, scale);
        let start = url.as_deref().map(check_url).transpose()?;
        let _one_at_a_time = self.opening.lock().await;
        let browser = match self.current() {
            Some(browser) => browser,
            None => {
                let browser = Browser::start(executable, profile, bus).await?;
                *lock(&self.browser) = Some(browser.clone());
                browser
            }
        };
        let existing = lock(&browser.pages)
            .values()
            .find(|p| p.view.is_none() && lock(&p.state).info.thread_id == thread)
            .cloned();
        if let Some(page) = existing {
            browser.resize(&page, width, height, scale).await?;
            return Ok(lock(&page.state).info.clone());
        }
        let tabs = lock(&browser.pages)
            .values()
            .filter(|p| p.view.is_none())
            .count();
        if tabs >= MAX_PAGES {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "Too many browser tabs are open. Close one first.",
            ));
        }
        browser
            .new_page(thread, start, width, height, scale)
            .await
            .map_err(internal)
    }

    pub async fn navigate(&self, id: &PageId, nav: PageNav) -> Result<(), ErrorBody> {
        let (browser, page) = self.page(id)?;
        browser.navigate(&page, nav).await
    }

    /// Queue what a viewer did. Never waits: the page's own task replays
    /// input in order.
    pub fn input(&self, id: &PageId, input: PageInput) -> Result<(), ErrorBody> {
        let (_, page) = self.page(id)?;
        page.input.send(input).map_err(|_| gone())
    }

    /// Connection `conn` is the one using the page now.
    pub fn touched(&self, id: &PageId, conn: u64) {
        if let Ok((_, page)) = self.page(id) {
            lock(&page.state).actor = Some(conn);
        }
    }

    pub async fn resize(
        &self,
        id: &PageId,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<(), ErrorBody> {
        let (browser, page) = self.page(id)?;
        let (width, height, scale) = fit(width, height, scale);
        // A view is as tall as its content, whatever its viewer's size.
        let height = if page.view.is_some() {
            lock(&page.state).info.height
        } else {
            height
        };
        browser.resize(&page, width, height, scale).await
    }

    /// The text selected in the page.
    pub async fn selection(&self, id: &PageId) -> Result<String, ErrorBody> {
        let (browser, page) = self.page(id)?;
        let result = browser
            .cdp
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": SELECTION_SCRIPT,
                    "returnByValue": true,
                }),
                Some(&page.session),
            )
            .await
            .map_err(internal)?;
        Ok(result["result"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }

    pub async fn close(&self, id: &PageId) -> Result<(), ErrorBody> {
        let (browser, page) = self.page(id)?;
        let _ = browser
            .cdp
            .send(
                "Target.closeTarget",
                json!({ "targetId": page.target }),
                None,
            )
            .await;
        browser.forget(&page.id, "The tab was closed.");
        Ok(())
    }

    /// The page now: its state, and its latest picture with its number.
    pub fn snapshot(&self, id: &PageId) -> Result<(PageInfo, Option<PageFrame>, u64), ErrorBody> {
        let (_, page) = self.page(id)?;
        let state = lock(&page.state);
        let frame = state.frame.as_ref().map(|jpeg| PageFrame {
            seq: state.seq,
            jpeg: jpeg.as_ref().clone().into(),
        });
        Ok((state.info.clone(), frame, state.seq))
    }

    /// The latest picture, if it is newer than `seq`.
    pub fn frame_after(&self, id: &PageId, seq: u64) -> Option<PageFrame> {
        let (_, page) = self.page(id).ok()?;
        let state = lock(&page.state);
        let jpeg = state.frame.as_ref().filter(|_| state.seq > seq)?;
        Some(PageFrame {
            seq: state.seq,
            jpeg: jpeg.as_ref().clone().into(),
        })
    }

    pub fn info(&self, id: &PageId) -> Option<PageInfo> {
        let (_, page) = self.page(id).ok()?;
        Some(lock(&page.state).info.clone())
    }

    /// Someone started (`true`) or stopped watching the page. Pictures are
    /// only made while someone does.
    pub async fn watching(&self, id: &PageId, joined: bool) {
        let Ok((browser, page)) = self.page(id) else {
            return;
        };
        let (start, stop) = {
            let mut state = lock(&page.state);
            let before = state.viewers;
            state.viewers = if joined {
                before + 1
            } else {
                before.saturating_sub(1)
            };
            (
                before == 0 && state.viewers == 1,
                before == 1 && state.viewers == 0,
            )
        };
        if start || stop {
            tracing::debug!(page = %id.0, watched = start, "a page's first watcher came or its last one left");
        }
        if start {
            browser.start_pictures(&page).await;
        } else if stop {
            if let Some(view) = &page.view {
                view.left_alone();
            }
            let _ = browser
                .cdp
                .send("Page.stopScreencast", json!({}), Some(&page.session))
                .await;
        }
    }

    /// Stop the browser (the daemon is going down). It is asked to close
    /// first, so its profile is left tidy, and killed if it doesn't.
    pub async fn shutdown(&self) {
        let Some(browser) = lock(&self.browser).take() else {
            return;
        };
        if !browser.dead.load(Ordering::Relaxed) {
            let _ = tokio::time::timeout(
                Duration::from_millis(1500),
                browser.cdp.send("Browser.close", json!({}), None),
            )
            .await;
        }
        browser.stop("Agentloft stopped.");
    }
}

/// The selection of the page or of the text field that has it.
const SELECTION_SCRIPT: &str = r#"(() => {
  const el = document.activeElement;
  if (el && typeof el.selectionStart === "number" && el.selectionEnd > el.selectionStart
      && el.type !== "password") {
    return el.value.slice(el.selectionStart, el.selectionEnd);
  }
  return String(window.getSelection() || "");
})()"#;

impl Browser {
    async fn start(
        executable: Option<&str>,
        profile: &Path,
        bus: broadcast::Sender<BusMsg>,
    ) -> Result<Arc<Self>, ErrorBody> {
        let exe = chrome::find(executable)
            .map_err(|reason| ErrorBody::new(ErrorCode::Unsupported, reason))?;
        std::fs::create_dir_all(profile).map_err(|e| internal(format!("browser profile: {e}")))?;
        #[cfg(unix)]
        {
            // Cookies and logins live here: the user's eyes only.
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(profile, std::fs::Permissions::from_mode(0o700));
        }
        let (cdp, events) = Cdp::launch(&exe, &chrome::args(profile)).map_err(internal)?;
        let browser = Arc::new(Self {
            cdp,
            bus,
            pages: Mutex::default(),
            sessions: Mutex::default(),
            popups: Mutex::default(),
            dead: AtomicBool::new(false),
            emptied: Mutex::new(Instant::now()),
            view_context: Mutex::default(),
            sweeping: AtomicBool::new(false),
        });
        tokio::spawn(browser.clone().run(events));
        // Hear about titles, and about windows pages open.
        browser
            .cdp
            .send(
                "Target.setDiscoverTargets",
                json!({ "discover": true }),
                None,
            )
            .await
            .map_err(|e| {
                browser.stop("The browser didn't start.");
                internal(format!("the browser didn't start: {e}"))
            })?;
        // Nothing lands on this machine's disk because a page said so.
        let _ = browser
            .cdp
            .send(
                "Browser.setDownloadBehavior",
                json!({ "behavior": "deny" }),
                None,
            )
            .await;
        tracing::info!(exe = %exe.display(), "browser started");
        Ok(browser)
    }

    /// Kill the process and tell every viewer.
    fn stop(&self, reason: &str) {
        if self.dead.swap(true, Ordering::Relaxed) {
            return;
        }
        self.cdp.kill();
        lock(&self.sessions).clear();
        lock(&self.popups).clear();
        for (id, _) in lock(&self.pages).drain() {
            let _ = self.bus.send(BusMsg::PageClosed {
                id,
                reason: reason.to_owned(),
            });
        }
    }

    fn forget(self: &Arc<Self>, id: &PageId, reason: &str) {
        let Some(page) = lock(&self.pages).remove(id) else {
            return;
        };
        lock(&self.sessions).remove(&page.session);
        let _ = self.bus.send(BusMsg::PageClosed {
            id: id.clone(),
            reason: reason.to_owned(),
        });
        if lock(&self.pages).is_empty() {
            let emptied = Instant::now();
            *lock(&self.emptied) = emptied;
            let browser = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(IDLE_BROWSER).await;
                if lock(&browser.pages).is_empty() && *lock(&browser.emptied) == emptied {
                    tracing::info!("browser idle; stopping it");
                    browser.stop("The browser was idle.");
                }
            });
        }
    }

    async fn new_page(
        self: &Arc<Self>,
        thread: ThreadId,
        start: Option<String>,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<PageInfo, String> {
        let target = self
            .cdp
            .send("Target.createTarget", json!({ "url": "about:blank" }), None)
            .await?["targetId"]
            .as_str()
            .ok_or("the browser made no tab")?
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
            thread_id: thread,
            url: String::new(),
            title: String::new(),
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            width,
            height,
            scale,
            view: false,
            overflow: false,
            cursor: String::new(),
        };
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
            view: None,
        });
        lock(&self.sessions).insert(session.clone(), page.id.clone());
        lock(&self.pages).insert(page.id.clone(), page.clone());
        tokio::spawn(self.clone().replay_input(page.clone(), inputs));
        tokio::spawn(self.clone().follow_title(page.clone()));
        let setup = async {
            let s = Some(session.as_str());
            self.cdp.send("Page.enable", json!({}), s).await?;
            self.cdp
                .send("Emulation.setDeviceMetricsOverride", metrics(&info), s)
                .await?;
            // Headless pages count as in the background; this one is being
            // looked at, and should behave so (carets, focus events).
            self.cdp
                .send(
                    "Emulation.setFocusEmulationEnabled",
                    json!({ "enabled": true }),
                    s,
                )
                .await?;
            if let Some(url) = start.filter(|u| u != "about:blank") {
                self.cdp
                    .send("Page.navigate", json!({ "url": url }), s)
                    .await?;
            }
            Ok::<(), String>(())
        };
        if let Err(err) = setup.await {
            self.forget(&page.id, "The tab didn't open.");
            return Err(err);
        }
        Ok(lock(&page.state).info.clone())
    }

    async fn navigate(&self, page: &Page, nav: PageNav) -> Result<(), ErrorBody> {
        let s = Some(page.session.as_str());
        let sent = match nav {
            PageNav::Url { url } => {
                let url = check_url(&url)?;
                self.cdp
                    .send("Page.navigate", json!({ "url": url }), s)
                    .await
            }
            PageNav::Reload => self.cdp.send("Page.reload", json!({}), s).await,
            PageNav::Stop => self.cdp.send("Page.stopLoading", json!({}), s).await,
            PageNav::Back | PageNav::Forward => {
                let history = self
                    .cdp
                    .send("Page.getNavigationHistory", json!({}), s)
                    .await
                    .map_err(internal)?;
                let current = history["currentIndex"].as_u64().unwrap_or(0);
                let wanted = if nav == PageNav::Back {
                    current
                        .checked_sub(1)
                        .filter(|i| *i >= first_real_entry(&history))
                } else {
                    Some(current + 1)
                };
                let Some(entry) = wanted
                    .and_then(|i| history["entries"].get(i as usize))
                    .and_then(|e| e["id"].as_i64())
                else {
                    return Ok(());
                };
                self.cdp
                    .send(
                        "Page.navigateToHistoryEntry",
                        json!({ "entryId": entry }),
                        s,
                    )
                    .await
            }
        };
        sent.map(|_| ()).map_err(internal)
    }

    async fn resize(
        self: &Arc<Self>,
        page: &Arc<Page>,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<(), ErrorBody> {
        let watched = {
            let mut state = lock(&page.state);
            let info = &mut state.info;
            if (info.width, info.height) == (width, height) && info.scale == scale {
                return Ok(());
            }
            (info.width, info.height, info.scale) = (width, height, scale);
            state.viewers > 0
        };
        {
            // Not while a screenshot is being taken (see `Page::shaping`),
            // and with the size as it is by then: the last one asked for.
            let _shaping = page.shaping.lock().await;
            let info = lock(&page.state).info.clone();
            self.cdp
                .send(
                    "Emulation.setDeviceMetricsOverride",
                    metrics(&info),
                    Some(&page.session),
                )
                .await
                .map_err(internal)?;
        }
        if watched {
            // Pictures at the new size: a screencast keeps the limits it
            // was started with, so it is started over.
            let _ = self
                .cdp
                .send("Page.stopScreencast", json!({}), Some(&page.session))
                .await;
            lock(&page.state).last_cast = 0;
            self.start_pictures(page).await;
        }
        let _ = self.bus.send(BusMsg::PageState {
            id: page.id.clone(),
        });
        Ok(())
    }

    /// Ask Chrome for pictures of the page. It refuses for a moment while a
    /// navigation is being committed ("not attached to an active page"), so
    /// a refusal is tried again a few times while someone is still watching.
    async fn start_pictures(self: &Arc<Self>, page: &Arc<Page>) {
        if self.try_pictures(page).await.is_ok() {
            return;
        }
        let (browser, page) = (self.clone(), page.clone());
        tokio::spawn(async move {
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(150)).await;
                if browser.dead.load(Ordering::Relaxed) || lock(&page.state).viewers == 0 {
                    return;
                }
                match browser.try_pictures(&page).await {
                    Ok(()) => return,
                    Err(err) => tracing::debug!(%err, "no pictures yet"),
                }
            }
            tracing::warn!("the browser wouldn't send pictures of a page");
        });
    }

    async fn try_pictures(&self, page: &Page) -> Result<(), String> {
        let info = lock(&page.state).info.clone();
        self.cdp
            .send(
                "Page.startScreencast",
                json!({
                    "format": "jpeg",
                    "quality": MOVING_QUALITY,
                    "maxWidth": info.width,
                    "maxHeight": info.height,
                    "everyNthFrame": 1,
                }),
                Some(&page.session),
            )
            .await
            .map(|_| ())
    }

    /// One task per page replays its input, so keys arrive in order.
    async fn replay_input(
        self: Arc<Self>,
        page: Arc<Page>,
        mut inputs: mpsc::UnboundedReceiver<PageInput>,
    ) {
        while let Some(input) = inputs.recv().await {
            if self.dead.load(Ordering::Relaxed) {
                break;
            }
            // What a click or a key did to a view is looked at, whether or
            // not the screencast says anything came of it.
            let acts = page.view.is_some()
                && !matches!(
                    input,
                    PageInput::Mouse {
                        action: PageMouse::Move,
                        ..
                    }
                );
            let (method, params) = input_command(input);
            let sent = {
                // Never while the page is being photographed. For the
                // length of a screenshot Chrome draws the page scaled to
                // the viewer's pixel density, and a click that lands
                // meanwhile lands at its coordinates divided by that: on a
                // dense screen, halfway to where it was aimed.
                let _shaping = page.shaping.lock().await;
                self.cdp.send(method, params, Some(&page.session)).await
            };
            if let Err(err) = sent {
                tracing::debug!(%err, "page input");
            }
            if acts {
                self.picture_soon(&page);
            }
        }
    }

    /// Everything the browser says, until it stops.
    async fn run(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<Event>) {
        while let Some(event) = events.recv().await {
            match event.session.clone() {
                Some(session) => {
                    let page = lock(&self.sessions)
                        .get(&session)
                        .and_then(|id| lock(&self.pages).get(id).cloned());
                    if let Some(page) = page {
                        self.on_page_event(page, event).await;
                    }
                }
                None => self.on_browser_event(event).await,
            }
        }
        if !self.dead.load(Ordering::Relaxed) {
            tracing::warn!("the browser exited");
        }
        self.stop("The browser stopped.");
    }

    async fn on_browser_event(self: &Arc<Self>, event: Event) {
        let info = &event.params["targetInfo"];
        let target = info["targetId"]
            .as_str()
            .or_else(|| event.params["targetId"].as_str())
            .unwrap_or_default()
            .to_owned();
        let by_target = |target: &str| {
            lock(&self.pages)
                .values()
                .find(|p| p.target == target)
                .cloned()
        };
        match event.method.as_str() {
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                if let Some(page) = by_target(&target) {
                    self.set_title(&page, info["title"].as_str().unwrap_or_default());
                    return;
                }
                // A window one of our pages opened: it has no tab of its
                // own here, so its address loads in the page that opened it.
                if info["type"] != "page" {
                    return;
                }
                let opener = match info["openerId"].as_str().and_then(by_target) {
                    Some(opener) => {
                        lock(&self.popups).insert(target.clone(), opener.id.clone());
                        Some(opener)
                    }
                    None => lock(&self.popups)
                        .get(&target)
                        .and_then(|id| lock(&self.pages).get(id).cloned()),
                };
                let url = info["url"].as_str().unwrap_or_default();
                if let Some(opener) = opener
                    && check_url(url).is_ok()
                    && url != "about:blank"
                {
                    lock(&self.popups).remove(&target);
                    let _ = self
                        .cdp
                        .send("Target.closeTarget", json!({ "targetId": target }), None)
                        .await;
                    // A view doesn't go anywhere: the link opens for the
                    // viewer instead.
                    if opener.view.is_some() {
                        let to = lock(&opener.state).actor;
                        let _ = self.bus.send(BusMsg::PageAction {
                            id: opener.id.clone(),
                            action: loft_protocol::PageAction::Open {
                                target: url.to_owned(),
                            },
                            to,
                        });
                        return;
                    }
                    let _ = self
                        .cdp
                        .send(
                            "Page.navigate",
                            json!({ "url": url }),
                            Some(&opener.session),
                        )
                        .await;
                }
            }
            "Target.targetDestroyed" | "Target.targetCrashed" => {
                lock(&self.popups).remove(&target);
                if let Some(page) = by_target(&target) {
                    let reason = if event.method.ends_with("Crashed") {
                        "The page crashed."
                    } else {
                        "The tab was closed."
                    };
                    self.forget(&page.id, reason);
                }
            }
            _ => {}
        }
    }

    async fn on_page_event(self: &Arc<Self>, page: Arc<Page>, event: Event) {
        if page.view.is_some() && self.on_view_event(&page, &event).await {
            return;
        }
        let params = &event.params;
        let session = Some(page.session.as_str());
        let changed = match event.method.as_str() {
            "Page.screencastFrame" => {
                self.on_picture(&page, params);
                false
            }
            "Page.frameNavigated" => {
                let frame = &params["frame"];
                if frame["parentId"].is_string() {
                    return;
                }
                let url = format!(
                    "{}{}",
                    frame["url"].as_str().unwrap_or_default(),
                    frame["urlFragment"].as_str().unwrap_or_default()
                );
                let mut state = lock(&page.state);
                state.main_frame = frame["id"].as_str().map(str::to_owned);
                state.info.url = shown_url(&url);
                true
            }
            "Page.navigatedWithinDocument" => {
                let mut state = lock(&page.state);
                if state.main_frame.as_deref() != params["frameId"].as_str() {
                    return;
                }
                state.info.url = shown_url(params["url"].as_str().unwrap_or_default());
                true
            }
            "Page.frameStartedLoading" | "Page.frameStoppedLoading" => {
                let mut state = lock(&page.state);
                let main = state.main_frame.as_deref();
                if main.is_some() && main != params["frameId"].as_str() {
                    return;
                }
                state.info.loading = event.method.ends_with("StartedLoading");
                true
            }
            "Page.javascriptDialogOpening" => {
                // Nobody is shown the dialog yet: say yes to notices and to
                // leaving a page, no to questions, and let the page go on.
                let accept = matches!(params["type"].as_str(), Some("alert" | "beforeunload"));
                let _ = self
                    .cdp
                    .send(
                        "Page.handleJavaScriptDialog",
                        json!({ "accept": accept }),
                        session,
                    )
                    .await;
                false
            }
            "Inspector.targetCrashed" => {
                self.forget(&page.id, "The page crashed.");
                return;
            }
            _ => false,
        };
        if !changed {
            return;
        }
        // Where back and forward lead now.
        if let Ok(history) = self
            .cdp
            .send("Page.getNavigationHistory", json!({}), session)
            .await
        {
            let current = history["currentIndex"].as_u64().unwrap_or(0);
            let entries = history["entries"].as_array().map_or(0, Vec::len) as u64;
            let mut state = lock(&page.state);
            state.info.can_go_back = current > first_real_entry(&history);
            state.info.can_go_forward = current + 1 < entries;
        }
        let _ = self.bus.send(BusMsg::PageState {
            id: page.id.clone(),
        });
        if event.method == "Page.frameStoppedLoading" {
            self.refresh_title(&page).await;
        }
    }

    fn set_title(&self, page: &Page, title: &str) {
        // A blank tab has no title worth showing.
        let title = if title == "about:blank" { "" } else { title };
        let changed = {
            let mut state = lock(&page.state);
            let changed = state.info.title != title;
            state.info.title = title.to_owned();
            changed
        };
        if changed {
            let _ = self.bus.send(BusMsg::PageState {
                id: page.id.clone(),
            });
        }
    }

    /// Ask the browser what the page is called now.
    async fn refresh_title(&self, page: &Page) {
        if let Ok(reply) = self
            .cdp
            .send(
                "Target.getTargetInfo",
                json!({ "targetId": page.target }),
                None,
            )
            .await
        {
            self.set_title(
                page,
                reply["targetInfo"]["title"].as_str().unwrap_or_default(),
            );
        }
    }

    /// While someone watches the page, keep its title current.
    async fn follow_title(self: Arc<Self>, page: Arc<Page>) {
        loop {
            tokio::time::sleep(TITLE_EVERY).await;
            if self.dead.load(Ordering::Relaxed) || !lock(&self.pages).contains_key(&page.id) {
                return;
            }
            if lock(&page.state).viewers > 0 {
                self.refresh_title(&page).await;
            }
        }
    }

    /// A screencast picture: keep it, announce it, and let Chrome send the
    /// next one after a beat.
    fn on_picture(self: &Arc<Self>, page: &Arc<Page>, params: &Value) {
        let Some(jpeg) = params["data"]
            .as_str()
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
        else {
            return;
        };
        let ack = params["sessionId"].clone();
        let (browser, paced) = (self.clone(), page.clone());
        // A view's screencast is only the sign that it changed (its picture
        // is a screenshot), so nothing is gained by slowing it, and a sign
        // that waits is a sign Chrome drops.
        let pace = if page.view.is_some() {
            Duration::ZERO
        } else {
            FRAME_PACE
        };
        tokio::spawn(async move {
            tokio::time::sleep(pace).await;
            let _ = browser
                .cdp
                .send(
                    "Page.screencastFrameAck",
                    json!({ "sessionId": ack }),
                    Some(&paced.session),
                )
                .await;
        });
        let fingerprint = fingerprint(&jpeg);
        // A view: the screencast only says "it changed"; the picture shown
        // is a screenshot at the real scale.
        if page.view.is_some() {
            {
                let mut state = lock(&page.state);
                if state.last_cast == fingerprint && state.frame.is_some() {
                    return;
                }
                state.last_cast = fingerprint;
            }
            self.picture_soon(page);
            return;
        }
        let (seq, motion) = {
            let mut state = lock(&page.state);
            if state.last_cast == fingerprint && state.frame.is_some() {
                // The page as it already was (Chrome repeats itself after
                // a screenshot): nothing to show, nothing to sharpen.
                return;
            }
            state.last_cast = fingerprint;
            state.seq += 1;
            state.motion += 1;
            state.frame = Some(Arc::new(jpeg));
            (state.seq, state.motion)
        };
        let _ = self.bus.send(BusMsg::PageFrame {
            id: page.id.clone(),
            seq,
        });
        // Once the page holds still, one screenshot replaces the screencast
        // pictures. It is at the page's real scale (they are one device
        // pixel per CSS pixel), and it is what is really there: Chrome
        // drops the pictures it can't send while earlier ones wait to be
        // acknowledged, the last change included.
        let (browser, page) = (self.clone(), page.clone());
        tokio::spawn(async move {
            tokio::time::sleep(STILL_AFTER).await;
            {
                let state = lock(&page.state);
                if state.motion != motion || state.viewers == 0 {
                    return;
                }
            }
            let Some(jpeg) = browser.screenshot(&page, STILL_QUALITY).await else {
                return;
            };
            let seq = {
                let mut state = lock(&page.state);
                if state.motion != motion {
                    return;
                }
                state.seq += 1;
                state.frame = Some(Arc::new(jpeg));
                state.seq
            };
            let _ = browser.bus.send(BusMsg::PageFrame {
                id: page.id.clone(),
                seq,
            });
        });
    }
}

fn metrics(info: &PageInfo) -> Value {
    json!({
        "width": info.width,
        "height": info.height,
        "deviceScaleFactor": info.scale,
        "mobile": false,
    })
}

/// Where a tab's history really starts: the blank page every tab opens on
/// isn't somewhere to go back to.
fn first_real_entry(history: &Value) -> u64 {
    u64::from(history["entries"][0]["url"] == "about:blank")
}

/// A blank page shows an empty address.
fn shown_url(url: &str) -> String {
    if url == "about:blank" {
        String::new()
    } else {
        url.to_owned()
    }
}

/// The DevTools command that replays `input`.
fn input_command(input: PageInput) -> (&'static str, Value) {
    match input {
        PageInput::Mouse {
            action,
            x,
            y,
            button,
            click_count,
            modifiers,
        } => {
            let (kind, held) = match action {
                PageMouse::Move => ("mouseMoved", button),
                PageMouse::Down => ("mousePressed", button),
                PageMouse::Up => ("mouseReleased", PageButton::None),
            };
            let name = |b: PageButton| match b {
                PageButton::None => "none",
                PageButton::Left => "left",
                PageButton::Middle => "middle",
                PageButton::Right => "right",
            };
            let buttons = match held {
                PageButton::None => 0,
                PageButton::Left => 1,
                PageButton::Right => 2,
                PageButton::Middle => 4,
            };
            (
                "Input.dispatchMouseEvent",
                json!({
                    "type": kind,
                    "x": x,
                    "y": y,
                    "button": name(button),
                    "buttons": buttons,
                    "clickCount": if action == PageMouse::Move { 0 } else { click_count.max(1) },
                    "modifiers": modifiers,
                }),
            )
        }
        PageInput::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers,
        } => (
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": x,
                "y": y,
                "deltaX": dx,
                "deltaY": dy,
                "modifiers": modifiers,
            }),
        ),
        PageInput::Key {
            down,
            key,
            code,
            text,
            modifiers,
            repeat,
        } => {
            // Enter types a line break; without one, forms don't submit.
            let text = text.or_else(|| (key == "Enter").then(|| "\r".to_owned()));
            let vk = keys::virtual_key(&code, &key);
            let mut params = json!({
                "type": match (down, &text) {
                    (false, _) => "keyUp",
                    (true, Some(_)) => "keyDown",
                    (true, None) => "rawKeyDown",
                },
                "key": key,
                "code": code,
                "modifiers": modifiers,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
                "autoRepeat": repeat,
            });
            if down {
                if let Some(text) = text {
                    params["text"] = text.clone().into();
                    params["unmodifiedText"] = text.into();
                }
                if cfg!(target_os = "macos")
                    && let Some(command) = keys::mac_command(&key, modifiers)
                {
                    params["commands"] = json!([command]);
                }
            }
            ("Input.dispatchKeyEvent", params)
        }
        PageInput::Text { text } => {
            let text: String = text.chars().take(MAX_TEXT).collect();
            ("Input.insertText", json!({ "text": text }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::page_modifiers::{META, SHIFT};

    /// The whole path against a real browser: a page opens, is pictured,
    /// and takes input. Needs Chrome (or Chromium, Edge, Brave) installed:
    /// `cargo test -p loft-core pages::tests::live -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_page_is_pictured_and_takes_input() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        const HTML: &str = "<!doctype html><title>Live</title><input autofocus \
            oninput=\"document.title = 'typed:' + this.value\">";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML}",
                    HTML.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let (bus, mut heard) = broadcast::channel(256);
        let pages = Pages::default();
        let page = pages
            .open(
                ThreadId::new(),
                Some(url.clone()),
                400,
                300,
                1.0,
                None,
                &dir.path().join("profile"),
                bus,
            )
            .await
            .unwrap();
        // The same thread again is the same page.
        let again = pages
            .open(
                page.thread_id.clone(),
                None,
                400,
                300,
                1.0,
                None,
                dir.path(),
                broadcast::channel(1).0,
            )
            .await
            .unwrap();
        assert_eq!(again.id, page.id);

        // Someone watches: a picture comes.
        pages.watching(&page.id, true).await;
        let pictured = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(BusMsg::PageFrame { id, seq }) = heard.recv().await {
                    assert_eq!(id, page.id);
                    return seq;
                }
            }
        })
        .await
        .expect("a picture of the page");
        let frame = pages.frame_after(&page.id, pictured - 1).unwrap();
        assert_eq!(&frame.jpeg.0[..2], &[0xFF, 0xD8], "a JPEG");
        assert!(pages.frame_after(&page.id, frame.seq).is_none());

        // A click into the field, then typing: the page says what it got
        // in its title.
        for action in [PageMouse::Down, PageMouse::Up] {
            let click = PageInput::Mouse {
                action,
                x: 40.0,
                y: 18.0,
                button: PageButton::Left,
                click_count: 1,
                modifiers: 0,
            };
            pages.input(&page.id, click).unwrap();
        }
        pages
            .input(&page.id, PageInput::Text { text: "hi".into() })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while pages.info(&page.id).unwrap().title != "typed:hi" {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the typed text");
        let info = pages.info(&page.id).unwrap();
        assert_eq!(info.url, url);
        assert!(
            !info.can_go_back,
            "a tab's first page has nothing behind it"
        );

        // Local files stay out of reach, and a closed page is gone.
        let nav = PageNav::Url {
            url: "file:///etc/hosts".into(),
        };
        assert!(pages.navigate(&page.id, nav).await.is_err());
        pages.close(&page.id).await.unwrap();
        assert!(pages.info(&page.id).is_none());
        assert!(
            pages
                .input(&page.id, PageInput::Text { text: "x".into() })
                .is_err()
        );
        pages.shutdown().await;
    }

    #[test]
    fn only_web_addresses_open() {
        for ok in [
            "https://example.com/a?b=c#d",
            "http://localhost:5173",
            " about:blank ",
        ] {
            assert!(check_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "chrome://settings",
            "javascript:alert(1)",
            "data:text/html,hi",
            "view-source:https://example.com",
            "https://exa mple.com",
            "example.com",
            "",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sizes_are_kept_sane() {
        assert_eq!(fit(900, 600, 2.0), (900, 600, 2.0));
        assert_eq!(fit(0, 99_999, 9.0), (MIN_SIDE, MAX_SIDE, 3.0));
        assert_eq!(fit(900, 600, f32::NAN).2, 1.0);
        assert_eq!(fit(900, 600, 0.2).2, 1.0);
    }

    #[test]
    fn mouse_input_becomes_devtools_events() {
        let (method, p) = input_command(PageInput::Mouse {
            action: PageMouse::Down,
            x: 10.5,
            y: 20.0,
            button: PageButton::Left,
            click_count: 2,
            modifiers: SHIFT,
        });
        assert_eq!(method, "Input.dispatchMouseEvent");
        assert_eq!(
            (&p["type"], &p["button"], &p["buttons"]),
            (&json!("mousePressed"), &json!("left"), &json!(1))
        );
        assert_eq!((&p["clickCount"], &p["modifiers"]), (&json!(2), &json!(8)));
        // A drag: the held button rides along on the moves.
        let (_, p) = input_command(PageInput::Mouse {
            action: PageMouse::Move,
            x: 1.0,
            y: 1.0,
            button: PageButton::Left,
            click_count: 0,
            modifiers: 0,
        });
        assert_eq!(
            (&p["type"], &p["buttons"], &p["clickCount"]),
            (&json!("mouseMoved"), &json!(1), &json!(0))
        );
        let (_, p) = input_command(PageInput::Mouse {
            action: PageMouse::Up,
            x: 1.0,
            y: 1.0,
            button: PageButton::Left,
            click_count: 1,
            modifiers: 0,
        });
        assert_eq!(
            (&p["type"], &p["buttons"]),
            (&json!("mouseReleased"), &json!(0))
        );
        let (_, p) = input_command(PageInput::Wheel {
            x: 5.0,
            y: 6.0,
            dx: 0.0,
            dy: 120.0,
            modifiers: 0,
        });
        assert_eq!(
            (&p["type"], &p["deltaY"]),
            (&json!("mouseWheel"), &json!(120.0))
        );
    }

    #[test]
    fn key_input_becomes_devtools_events() {
        let key = |down, key: &str, code: &str, text: Option<&str>, modifiers| {
            input_command(PageInput::Key {
                down,
                key: key.into(),
                code: code.into(),
                text: text.map(str::to_owned),
                modifiers,
                repeat: false,
            })
            .1
        };
        // A character: a full key press that types it.
        let p = key(true, "a", "KeyA", Some("a"), 0);
        assert_eq!(
            (&p["type"], &p["text"], &p["windowsVirtualKeyCode"]),
            (&json!("keyDown"), &json!("a"), &json!(65))
        );
        // A key that types nothing.
        let p = key(true, "ArrowLeft", "ArrowLeft", None, 0);
        assert_eq!(
            (&p["type"], &p["windowsVirtualKeyCode"]),
            (&json!("rawKeyDown"), &json!(37))
        );
        assert!(p.get("text").is_none());
        // Enter submits forms.
        let p = key(true, "Enter", "Enter", None, 0);
        assert_eq!((&p["type"], &p["text"]), (&json!("keyDown"), &json!("\r")));
        let p = key(false, "Enter", "Enter", None, 0);
        assert_eq!(p["type"], "keyUp");
        assert!(p.get("text").is_none());
        // ⌘A is "select all" on a Mac, and only there.
        let p = key(true, "a", "KeyA", None, META);
        assert_eq!(
            p.get("commands").is_some(),
            cfg!(target_os = "macos"),
            "{p}"
        );
        // Long text is cut off, not refused.
        let (method, p) = input_command(PageInput::Text {
            text: "x".repeat(MAX_TEXT + 50),
        });
        assert_eq!(method, "Input.insertText");
        assert_eq!(p["text"].as_str().unwrap().len(), MAX_TEXT);
    }
}
