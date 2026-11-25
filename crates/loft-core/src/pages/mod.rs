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

mod agents;
mod automation;
mod capture;
mod cdp;
mod chrome;
mod demonstrate;
mod keys;
mod origins;
mod sound;
mod tools;
mod views;
// Page interaction (A)
mod challenge;
mod dialogs;
mod frames;
mod human;
mod input;
mod interact;
mod uploads;

pub use agents::{HandoffOutcome, Handover, normalize_origins, valid_name};
pub use automation::{AGENT_PAGE_SIZE, DriveOptions, is_file_url};
pub use views::{StateKeeper, ViewRequest};

/// Which browser the daemon would run, or why it can't (`browser_status`).
pub fn find_browser(configured: Option<&str>) -> Result<std::path::PathBuf, String> {
    chrome::find(configured)
}

use crate::BusMsg;
use base64::Engine as _;
use cdp::{Cdp, Event};
use loft_protocol::{
    ErrorBody, ErrorCode, PageAgent, PageButton, PageFrame, PageId, PageInfo, PageInput, PageMouse,
    PageNav, ThreadId,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

/// How a page's screencast is made, for who watches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pictures {
    /// Only viewers elsewhere (a phone, a web client, an SSH window): the
    /// pictures cross a network, so they are small (one device pixel per
    /// CSS pixel, a sharp screenshot once the page holds still) and about
    /// 14 a second at most.
    Sent,
    /// The app on this computer watches too: every device pixel, as often
    /// as the display refreshes, so it scrolls and plays like a page of
    /// its own would, with no blurred picture turning sharp.
    Here,
}

impl Pictures {
    /// The least time between pictures.
    fn pace(self) -> Duration {
        match self {
            Pictures::Sent => Duration::from_millis(70),
            Pictures::Here => Duration::from_millis(16),
        }
    }

    fn quality(self) -> u32 {
        match self {
            Pictures::Sent => MOVING_QUALITY,
            Pictures::Here => HERE_QUALITY,
        }
    }
}

/// Pictures for the app here: sharp enough that a still page needs no
/// screenshot to replace them.
const HERE_QUALITY: u32 = 85;
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
const MAX_PAGES: usize = 32;
const MIN_SIDE: u32 = 120;
const MAX_SIDE: u32 = 3840;
/// How often, and for how long, a preview whose server isn't listening yet
/// is asked for again.
const PREVIEW_RETRY: Duration = Duration::from_millis(700);
const PREVIEW_PATIENCE: Duration = Duration::from_secs(45);
/// A browser tab nobody watches or uses is closed after this long.
const UNUSED_TAB: Duration = Duration::from_secs(60 * 60);
/// A file preview page nobody watches is closed after this long.
const IDLE_PREVIEW: Duration = Duration::from_secs(120);
const UNUSED_CHECK: Duration = Duration::from_secs(60);
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
    /// The user agent of an ordinary (not headless) Chrome, once asked.
    user_agent: Mutex<Option<String>>,
    /// The tab each thread used last (a person or its agent): the one an
    /// agent drives.
    current: Mutex<HashMap<ThreadId, PageId>>,
    /// What agents driving pages need kept (`automation`).
    drivers: automation::Drivers,
    /// Agents' captures of pages: traffic, console, recordings, baselines
    /// and emulation (`capture`).
    captures: capture::Captures,
    /// Mutes the tabs nobody here is listening to (`None`: the whole
    /// browser is muted).
    sound: Option<sound::Sound>,
    /// Held while an agent's paste uses the browser's clipboard (its own,
    /// not this computer's): one paste at a time (`input.rs`).
    clipboard: tokio::sync::Mutex<()>,
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
    /// Set when the page previews a local file in the file viewer rather than
    /// being a browser tab of the thread.
    preview: bool,
    /// Set when the page draws a view an agent made rather than a site.
    view: Option<views::ViewPage>,
}

impl Page {
    /// Whether this is a browser tab of its thread (listed in `PageList`,
    /// tracked as `current`, usable by agents), as opposed to an agent-drawn
    /// view or a file-viewer HTML preview.
    pub(super) fn is_tab(&self) -> bool {
        self.view.is_none() && !self.preview
    }
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
    /// The size its viewers last asked for, before zoom and device
    /// (`info` has the page's own).
    asked: (u32, u32, f32),
    /// Counts the times the tab was sent somewhere (a preview stops
    /// retrying once it was sent elsewhere).
    sent: u64,
    /// When someone last looked at the tab or used it (a person or an
    /// agent): one left alone long enough is closed.
    used: Instant,
    /// How much the page is magnified within its viewport, as its last
    /// picture said: a phone's page that lays out wider than the phone is
    /// shown smaller (below 1), a pinched one bigger.
    page_scale: f32,
    /// The app on this computer watching the page (its hello says
    /// [`loft_protocol::SOUND`]), once per connection. While one does, the
    /// page may be heard (`sound.rs`), and it is pictured as that app would
    /// draw a page of its own: every device pixel, as often as Chrome draws
    /// ([`Pictures::Here`]). Nothing between them is slower than memory.
    here: Vec<u64>,
    /// Whether whoever used the page last can hear it: no one elsewhere (a
    /// phone) did since a viewer here did.
    heard_last: bool,
    /// How the running screencast was asked for.
    cast: Pictures,
    /// Where the viewer's pointer was last put on the page, until the
    /// pointer wanted there has been looked up.
    pointer_at: Option<(f32, f32)>,
    /// Whether that is being looked up now.
    pointer_checking: bool,
}

impl State {
    /// Whether the page's sound may play on this computer's speakers.
    fn loud(&self) -> bool {
        !self.here.is_empty() && self.heard_last
    }

    /// How the page should be pictured, as its viewers are now.
    fn pictures(&self) -> Pictures {
        if self.here.is_empty() {
            Pictures::Sent
        } else {
            Pictures::Here
        }
    }

    /// The page's viewport as its viewers' size, its zoom and its device
    /// make it. Whether it changed.
    fn shape(&mut self) -> bool {
        let (width, height, scale) = self.asked;
        let zoom = self.info.zoom;
        let (width, height) = match &self.info.device {
            Some(device) => (device.width, device.height),
            None => (width, height),
        };
        let shaped = (
            ((width as f32 / zoom).round() as u32).clamp(MIN_SIDE / 2, MAX_SIDE * 2),
            ((height as f32 / zoom).round() as u32).clamp(MIN_SIDE / 2, MAX_SIDE * 2),
            scale * zoom,
        );
        let before = (self.info.width, self.info.height, self.info.scale);
        (self.info.width, self.info.height, self.info.scale) = shaped;
        before != shaped
    }
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

    /// One of the thread's tabs (see [`Which`]), opening it (and the
    /// browser) if need be.
    #[allow(clippy::too_many_arguments)]
    pub async fn open(
        &self,
        thread: ThreadId,
        which: Which,
        url: Option<String>,
        width: u32,
        height: u32,
        scale: f32,
        executable: Option<&str>,
        profile: &Path,
        bus: broadcast::Sender<BusMsg>,
    ) -> Result<PageInfo, ErrorBody> {
        // No size: whatever size the page has (a viewer that shows it
        // small, not in a place of its own), or a laptop's for a new one.
        let keep = width == 0 || height == 0;
        let (width, height, scale) = if keep {
            fit(1280, 800, scale)
        } else {
            fit(width, height, scale)
        };
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
        let existing = match which {
            Which::Tab(id) => browser.tab(&thread, &id),
            Which::Current => browser.current_tab(&thread),
            Which::New => None,
        };
        if let Some(page) = existing {
            lock(&browser.current).insert(thread, page.id.clone());
            if !keep {
                browser.resize(&page, width, height, scale).await?;
            }
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

    /// Open a chromeless preview page for a local file (`file://` URL). It is
    /// not a browser tab of the thread (never in `PageList` or `current`) and
    /// refuses navigation to any other local `file:` document.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_preview(
        &self,
        thread: ThreadId,
        file_url: &str,
        width: u32,
        height: u32,
        scale: f32,
        executable: Option<&str>,
        profile: &Path,
        bus: broadcast::Sender<BusMsg>,
    ) -> Result<PageInfo, ErrorBody> {
        let url = automation::check_file_url(file_url)?;
        let (width, height, scale) = fit(width, height, scale);
        let _one_at_a_time = self.opening.lock().await;
        let browser = match self.current() {
            Some(browser) => browser,
            None => {
                let browser = Browser::start(executable, profile, bus).await?;
                *lock(&self.browser) = Some(browser.clone());
                browser
            }
        };
        let pages = lock(&browser.pages)
            .values()
            .filter(|p| p.view.is_none())
            .count();
        if pages >= MAX_PAGES {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "Too many browser tabs are open. Close one first.",
            ));
        }
        browser
            .new_preview_page(thread, url, width, height, scale)
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
    /// `conn` used the page; `hears`: it plays sound here.
    pub fn touched(&self, id: &PageId, conn: u64, hears: bool) {
        if let Ok((browser, page)) = self.page(id) {
            let (thread, heard) = {
                let mut state = lock(&page.state);
                state.actor = Some(conn);
                state.used = Instant::now();
                let heard = state.heard_last != hears;
                state.heard_last = hears;
                (state.info.thread_id.clone(), heard)
            };
            if page.is_tab() {
                lock(&browser.current).insert(thread, page.id.clone());
            }
            if heard {
                tokio::spawn(async move { browser.update_sound().await });
            }
        }
    }

    /// Connection `conn`, which plays sound on this computer, started
    /// (`true`) or stopped watching the page: while one does, the page may
    /// be heard (`sound.rs`).
    pub async fn hearing(&self, id: &PageId, conn: u64, on: bool) {
        let Ok((browser, page)) = self.page(id) else {
            return;
        };
        let recast = {
            let mut state = lock(&page.state);
            state.here.retain(|c| *c != conn);
            if on {
                state.here.push(conn);
            }
            state.viewers > 0 && state.pictures() != state.cast
        };
        if recast && page.view.is_none() {
            browser.recast(&page).await;
        }
        browser.update_sound().await;
    }

    /// The thread's browser tabs, oldest first.
    pub fn tabs(&self, thread: &ThreadId) -> Vec<PageInfo> {
        self.current()
            .map(|browser| browser.tabs_of(thread))
            .unwrap_or_default()
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
        browser.watching(&page, joined).await;
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

/// Which of a thread's tabs [`Pages::open`] opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Which {
    /// The one the thread used last, or a new one if it has none.
    Current,
    /// This one if it is still open, else a new one.
    Tab(PageId),
    /// Another one.
    New,
}

impl Which {
    pub fn asked(tab: Option<PageId>, new_tab: bool) -> Self {
        match (tab, new_tab) {
            (Some(id), _) => Which::Tab(id),
            (None, true) => Which::New,
            (None, false) => Which::Current,
        }
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
    /// Someone (a viewer, or a recording) started (`true`) or stopped
    /// watching the page. Pictures are only made while someone does.
    async fn watching(self: &Arc<Self>, page: &Arc<Page>, joined: bool) {
        let (start, stop) = {
            let mut state = lock(&page.state);
            state.used = Instant::now();
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
            tracing::debug!(page = %page.id.0, watched = start, "a page's first watcher came or its last one left");
        }
        if start {
            self.start_pictures(page).await;
        } else if stop {
            if let Some(view) = &page.view {
                view.left_alone();
            }
            let _ = self
                .cdp
                .send("Page.stopScreencast", json!({}), Some(&page.session))
                .await;
        }
    }

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
        let (cdp, events, sound) = Self::launch(&exe, profile).await?;
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
            user_agent: Mutex::default(),
            current: Mutex::default(),
            drivers: automation::Drivers::default(),
            captures: capture::Captures::default(),
            sound,
            clipboard: tokio::sync::Mutex::new(()),
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

    /// Mute the tabs nobody here is listening to; unmute the others.
    async fn update_sound(&self) {
        if let Some(sound) = &self.sound {
            sound
                .tell(&self.cdp, || {
                    lock(&self.pages)
                        .values()
                        .filter(|page| lock(&page.state).loud())
                        .map(|page| page.target.clone())
                        .collect()
                })
                .await;
        }
    }

    /// Run the browser with its sound extension; one that won't load it
    /// runs again, muted.
    async fn launch(
        exe: &Path,
        profile: &Path,
    ) -> Result<
        (
            Arc<Cdp>,
            mpsc::UnboundedReceiver<Event>,
            Option<sound::Sound>,
        ),
        ErrorBody,
    > {
        let (cdp, events) = Cdp::launch(exe, &chrome::args(profile, true)).map_err(internal)?;
        match sound::Sound::load(&cdp, profile).await {
            Ok(sound) => Ok((cdp, events, Some(sound))),
            Err(err) => {
                tracing::warn!(%err, "the browser won't load its sound extension; it runs muted");
                drop(events);
                cdp.kill_and_wait().await;
                let (cdp, events) =
                    Cdp::launch(exe, &chrome::args(profile, false)).map_err(internal)?;
                Ok((cdp, events, None))
            }
        }
    }

    /// Kill the process and tell every viewer.
    fn stop(&self, reason: &str) {
        if self.dead.swap(true, Ordering::Relaxed) {
            return;
        }
        self.cdp.kill();
        lock(&self.sessions).clear();
        lock(&self.popups).clear();
        lock(&self.current).clear();
        let mut threads = std::collections::HashSet::new();
        let pages: Vec<(PageId, Arc<Page>)> = lock(&self.pages).drain().collect();
        for (id, page) in pages {
            if page.is_tab() {
                threads.insert(lock(&page.state).info.thread_id.clone());
            }
            self.end_handover(&page, HandoffOutcome::Closed);
            let _ = self.bus.send(BusMsg::PageClosed {
                id,
                reason: reason.to_owned(),
            });
        }
        for thread_id in threads {
            let _ = self.bus.send(BusMsg::PageTabs {
                thread_id,
                pages: Vec::new(),
            });
        }
    }

    fn forget(self: &Arc<Self>, id: &PageId, reason: &str) {
        let Some(page) = lock(&self.pages).remove(id) else {
            return;
        };
        // Its own session, and a view's frames' (`views::on_view_frame`).
        lock(&self.sessions).retain(|_, of| of != id);
        self.captures.forget_frames(id);
        // An agent waiting for a person there hears it closed.
        self.end_handover(&page, HandoffOutcome::Closed);
        let _ = self.bus.send(BusMsg::PageClosed {
            id: id.clone(),
            reason: reason.to_owned(),
        });
        if page.is_tab() {
            let thread = lock(&page.state).info.thread_id.clone();
            {
                let mut current = lock(&self.current);
                if current.get(&thread) == Some(id) {
                    current.remove(&thread);
                }
            }
            self.announce_tabs(&thread);
        }
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

    /// The thread's tabs, oldest first (ids count up with time).
    fn tabs_of(&self, thread: &ThreadId) -> Vec<PageInfo> {
        let mut tabs: Vec<PageInfo> = lock(&self.pages)
            .values()
            .filter(|p| p.is_tab())
            .map(|p| lock(&p.state).info.clone())
            .filter(|info| info.thread_id == *thread)
            .collect();
        tabs.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        tabs
    }

    /// Tell whoever has the thread open what tabs it has now.
    fn announce_tabs(&self, thread: &ThreadId) {
        let _ = self.bus.send(BusMsg::PageTabs {
            thread_id: thread.clone(),
            pages: self.tabs_of(thread),
        });
    }

    /// Tab `id`, if it is one of `thread`'s.
    fn tab(&self, thread: &ThreadId, id: &PageId) -> Option<Arc<Page>> {
        lock(&self.pages)
            .get(id)
            .filter(|p| p.is_tab() && lock(&p.state).info.thread_id == *thread)
            .cloned()
    }

    /// The tab the thread used last, else its newest.
    fn current_tab(&self, thread: &ThreadId) -> Option<Arc<Page>> {
        let last = lock(&self.current).get(thread).cloned();
        if let Some(page) = last.and_then(|id| self.tab(thread, &id)) {
            return Some(page);
        }
        let newest = self.tabs_of(thread).pop()?;
        self.tab(thread, &newest.id)
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
        self.adopt(
            target,
            thread,
            start,
            (width, height, scale),
            PageAgent::default(),
            true,
        )
        .await
    }

    async fn new_preview_page(
        self: &Arc<Self>,
        thread: ThreadId,
        file_url: String,
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
        self.adopt_inner(
            target,
            thread,
            Some(file_url),
            (width, height, scale),
            PageAgent::default(),
            false,
            true,
        )
        .await
    }

    /// Make the browser's tab `target` one of the thread's: attach to it,
    /// set it up as every tab is, and send it to `start`. `current`: it is
    /// the thread's tab used last now (a tab a page opened is not).
    async fn adopt(
        self: &Arc<Self>,
        target: String,
        thread: ThreadId,
        start: Option<String>,
        size: (u32, u32, f32),
        agent: PageAgent,
        current: bool,
    ) -> Result<PageInfo, String> {
        self.adopt_inner(target, thread, start, size, agent, current, false)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn adopt_inner(
        self: &Arc<Self>,
        target: String,
        thread: ThreadId,
        start: Option<String>,
        (width, height, scale): (u32, u32, f32),
        agent: PageAgent,
        current: bool,
        preview: bool,
    ) -> Result<PageInfo, String> {
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
            zoom: 1.0,
            device: None,
            annotating: false,
            editing: false,
            fields: Vec::new(),
            agent,
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
                asked: (width, height, scale),
                sent: 0,
                used: Instant::now(),
                page_scale: 1.0,
                here: Vec::new(),
                heard_last: true,
                cast: Pictures::Sent,
                pointer_at: None,
                pointer_checking: false,
            }),
            preview,
            view: None,
        });
        lock(&self.sessions).insert(session.clone(), page.id.clone());
        lock(&self.pages).insert(page.id.clone(), page.clone());
        tokio::spawn(self.clone().replay_input(page.clone(), inputs));
        tokio::spawn(self.clone().follow_title(page.clone()));
        tokio::spawn(self.clone().close_when_left(page.clone()));
        let setup = async {
            let s = Some(session.as_str());
            // First, for a window a page opened: its first page may be on
            // its way already, and some sites turn a headless one away.
            self.set_user_agent(&page, None).await;
            self.cdp.send("Page.enable", json!({}), s).await?;
            // Passkeys: Chrome asks for one in a window of its own, which a
            // headless browser never shows, so the page would wait for it
            // forever ("Verifying it's you…"). An authenticator that
            // declines everything answers at once, as a person cancelling
            // would, and the site offers another way in. Nothing is ever
            // made on it: a passkey a site made there would be gone when the
            // browser stops.
            if self
                .cdp
                .send("WebAuthn.enable", json!({ "enableUI": false }), s)
                .await
                .is_ok()
            {
                let _ = self
                    .cdp
                    .send(
                        "WebAuthn.addVirtualAuthenticator",
                        json!({ "options": {
                            "protocol": "ctap2",
                            "transport": "internal",
                            "hasResidentKey": true,
                            "hasUserVerification": true,
                            "isUserVerified": false,
                            "automaticPresenceSimulation": true,
                            "isUserConsenting": false,
                        } }),
                        s,
                    )
                    .await;
            }
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
                if preview {
                    self.allow_file(&page, &url).await?;
                }
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
        if !current && !preview {
            // A window a page opened may have shown its page before the
            // events about it were asked for.
            if let Ok(reply) = self
                .cdp
                .send(
                    "Target.getTargetInfo",
                    json!({ "targetId": page.target }),
                    None,
                )
                .await
            {
                let url = reply["targetInfo"]["url"].as_str().unwrap_or_default();
                let title = reply["targetInfo"]["title"].as_str().unwrap_or_default();
                let mut state = lock(&page.state);
                if state.info.url.is_empty() && url != "about:blank" {
                    state.info.url = shown_url(url);
                }
                if state.info.title.is_empty() && title != "about:blank" && title != url {
                    state.info.title = title.to_owned();
                }
            }
        }
        let info = lock(&page.state).info.clone();
        if !preview {
            if current {
                lock(&self.current).insert(info.thread_id.clone(), info.id.clone());
            }
            self.announce_tabs(&info.thread_id);
        }
        Ok(info)
    }

    async fn navigate(self: &Arc<Self>, page: &Arc<Page>, nav: PageNav) -> Result<(), ErrorBody> {
        let s = Some(page.session.as_str());
        let turn = {
            let mut state = lock(&page.state);
            state.used = Instant::now();
            state.sent += 1;
            state.sent
        };
        let sent = match nav {
            PageNav::Preview { url } => {
                let url = check_url(&url)?;
                tokio::spawn(self.clone().preview(page.clone(), url, turn));
                return Ok(());
            }
            PageNav::Url { url } => {
                let url = check_url(&url)?;
                self.cdp
                    .send("Page.navigate", json!({ "url": url }), s)
                    .await
            }
            PageNav::Reload => self.cdp.send("Page.reload", json!({}), s).await,
            PageNav::HardReload => {
                self.cdp
                    .send("Page.reload", json!({ "ignoreCache": true }), s)
                    .await
            }
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

    /// Load a page whose server may not be listening yet, again and again
    /// until it is (or the tab is sent elsewhere, or it takes too long).
    async fn preview(self: Arc<Self>, page: Arc<Page>, url: String, turn: u64) {
        let began = Instant::now();
        loop {
            let reply = self
                .cdp
                .send("Page.navigate", json!({ "url": url }), Some(&page.session))
                .await;
            let refused = reply.as_ref().is_ok_and(|r| {
                r["errorText"].as_str().is_some_and(|e| {
                    e.contains("CONNECTION_REFUSED")
                        || e.contains("CONNECTION_RESET")
                        || e.contains("EMPTY_RESPONSE")
                        || e.contains("CONNECTION_CLOSED")
                })
            });
            if !refused || began.elapsed() > PREVIEW_PATIENCE {
                return;
            }
            tokio::time::sleep(PREVIEW_RETRY).await;
            if self.dead.load(Ordering::Relaxed)
                || !lock(&self.pages).contains_key(&page.id)
                || lock(&page.state).sent != turn
            {
                return;
            }
        }
    }

    async fn resize(
        self: &Arc<Self>,
        page: &Arc<Page>,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<(), ErrorBody> {
        {
            let mut state = lock(&page.state);
            state.asked = (width, height, scale);
            if page.view.is_some() {
                let info = &mut state.info;
                if (info.width, info.height) == (width, height) && info.scale == scale {
                    return Ok(());
                }
                (info.width, info.height, info.scale) = (width, height, scale);
            } else if !state.shape() {
                return Ok(());
            }
        }
        self.reshape(page).await
    }

    /// Give the page the viewport its state says, and picture it afresh.
    async fn reshape(self: &Arc<Self>, page: &Arc<Page>) -> Result<(), ErrorBody> {
        let watched = lock(&page.state).viewers > 0;
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
            // Pictures at the new size.
            self.recast(page).await;
        }
        let _ = self.bus.send(BusMsg::PageState {
            id: page.id.clone(),
        });
        Ok(())
    }

    /// Start the page's screencast over: it keeps the limits it was started
    /// with (the size, how sharp).
    async fn recast(self: &Arc<Self>, page: &Arc<Page>) {
        let _ = self
            .cdp
            .send("Page.stopScreencast", json!({}), Some(&page.session))
            .await;
        lock(&page.state).last_cast = 0;
        self.start_pictures(page).await;
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
        let (info, pictures) = {
            let mut state = lock(&page.state);
            state.cast = state.pictures();
            (state.info.clone(), state.cast)
        };
        // As many pixels as the viewer has room for, whatever the zoom: its
        // points for a viewer elsewhere, every device pixel for one here.
        let density = match pictures {
            Pictures::Sent => info.zoom,
            Pictures::Here => info.scale,
        };
        self.cdp
            .send(
                "Page.startScreencast",
                json!({
                    "format": "jpeg",
                    "quality": pictures.quality(),
                    "maxWidth": (info.width as f32 * density).round() as u32,
                    "maxHeight": (info.height as f32 * density).round() as u32,
                    "everyNthFrame": 1,
                }),
                Some(&page.session),
            )
            .await
            .map(|_| ())
    }

    /// One task per page replays its input, so keys arrive in order.
    ///
    /// Chrome takes longer to handle a wheel or a pointer move than a
    /// trackpad takes to send the next one, so what queued up meanwhile is
    /// merged first ([`merge`]): a page scrolled by the whole of it, once,
    /// instead of seconds of scrolling after the fingers have stopped.
    async fn replay_input(
        self: Arc<Self>,
        page: Arc<Page>,
        mut inputs: mpsc::UnboundedReceiver<PageInput>,
    ) {
        let mut next = None;
        loop {
            let mut input = match next.take() {
                Some(input) => input,
                None => match inputs.recv().await {
                    Some(input) => input,
                    None => break,
                },
            };
            while let Ok(queued) = inputs.try_recv() {
                match merge(&input, &queued) {
                    Some(merged) => input = merged,
                    None => {
                        next = Some(queued);
                        break;
                    }
                }
            }
            if self.dead.load(Ordering::Relaxed) {
                break;
            }
            // A dialog held for the page's agent: a person using the page
            // answers it as nobody's would be, and goes on; the pointer
            // merely passing waits (the page takes no input meanwhile).
            if self.dialog_held(&page.id) {
                if matches!(
                    input,
                    PageInput::Mouse {
                        action: PageMouse::Move,
                        ..
                    }
                ) {
                    continue;
                }
                self.settle_dialog(&page, dialogs::Settled::PersonUsedPage)
                    .await;
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
            let touches_focus = matches!(
                input,
                PageInput::Mouse {
                    action: PageMouse::Up,
                    ..
                } | PageInput::Key { down: false, .. }
            );
            // A viewer points at the picture; the page counts in its own
            // CSS pixels, which a magnified page has fewer of.
            let page_scale = lock(&page.state).page_scale;
            let input = unscaled(input, page_scale);
            // Tabs and handoff (C): a person showing the agent something.
            if page.is_tab() {
                self.record_input(&page, &input).await;
            }
            let at = match &input {
                PageInput::Mouse { x, y, .. } | PageInput::Wheel { x, y, .. } => Some((*x, *y)),
                _ => None,
            };
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
            // The viewer's own pointer is over a picture: it shows what the
            // page wants under it (a hand on a link, an I-beam on text).
            if let Some(at) = at {
                self.follow_pointer(&page, at);
            }
            // A click or a key may have moved the focus into a text field
            // or out of one.
            if page.view.is_none() && touches_focus && next.is_none() && inputs.is_empty() {
                self.check_fields(&page).await;
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
                    } else if let Some(page) = self
                        .captures
                        .frame_page(&session)
                        .and_then(|id| lock(&self.pages).get(&id).cloned())
                    {
                        // A page's frame of another site, over its own
                        // session: only captures follow those.
                        self.observe_for_capture(&page, &event);
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
                    // A window a page opened may show only what tabs may.
                    self.close_if_unsafe(&page, info["url"].as_str().unwrap_or_default());
                    return;
                }
                if info["type"] == "iframe" {
                    self.captures
                        .frame_moved(&target, info["url"].as_str().unwrap_or_default());
                }
                if info["type"] != "page" {
                    return;
                }
                let known = lock(&self.popups).get(&target).cloned();
                let opener = match info["openerId"].as_str().and_then(by_target) {
                    Some(opener) => Some(opener),
                    None => known
                        .as_ref()
                        .and_then(|id| lock(&self.pages).get(id).cloned()),
                };
                let Some(opener) = opener else {
                    return;
                };
                // A window a tab's page opened (a popup, a `target=_blank`
                // link) becomes another tab of the thread (ADR-0017 §9).
                if opener.is_tab() {
                    if known.is_some() {
                        return; // being made a tab already
                    }
                    let tabs = lock(&self.pages)
                        .values()
                        .filter(|p| p.view.is_none())
                        .count();
                    if tabs >= MAX_PAGES {
                        let _ = self
                            .cdp
                            .send("Target.closeTarget", json!({ "targetId": target }), None)
                            .await;
                        return;
                    }
                    lock(&self.popups).insert(target.clone(), opener.id.clone());
                    tokio::spawn(self.clone().adopt_popup(target, opener));
                    return;
                }
                // A view's: it has no tab here, so the link opens for the
                // viewer once its address is known.
                lock(&self.popups).insert(target.clone(), opener.id.clone());
                let url = info["url"].as_str().unwrap_or_default();
                if check_url(url).is_ok() && url != "about:blank" {
                    lock(&self.popups).remove(&target);
                    let _ = self
                        .cdp
                        .send("Target.closeTarget", json!({ "targetId": target }), None)
                        .await;
                    let to = lock(&opener.state).actor;
                    let _ = self.bus.send(BusMsg::PageAction {
                        id: opener.id.clone(),
                        action: loft_protocol::PageAction::Open {
                            target: url.to_owned(),
                        },
                        to,
                    });
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
        self.observe_for_agents(&page, &event);
        self.observe_for_capture(&page, &event);
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
                {
                    let mut state = lock(&page.state);
                    state.main_frame = frame["id"].as_str().map(str::to_owned);
                    state.info.url = shown_url(&url);
                }
                if page.is_tab() {
                    // Tabs and handoff (C): a person showing the agent
                    // something; where an agent may send the tab.
                    self.record_navigation(&page, &url);
                    self.close_if_unsafe(&page, &url);
                    self.check_origin(&page, &url);
                }
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
                // Held for an agent driving the page, else answered for it
                // (`dialogs.rs`).
                self.on_dialog(&page, params).await;
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
            // A page held by a dialog runs no script until it is answered.
            if !self.dialog_held(&page.id) {
                self.check_fields(&page).await;
            }
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
            // Its tab is named after it, in windows that haven't opened it.
            if page.is_tab() {
                let thread = lock(&page.state).info.thread_id.clone();
                self.announce_tabs(&thread);
            }
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

    /// Close the tab once nobody has looked at it or used it for a long
    /// while. Closing a view of it doesn't close it (the web client or an
    /// agent may be using it), so nothing else would.
    async fn close_when_left(self: Arc<Self>, page: Arc<Page>) {
        loop {
            tokio::time::sleep(UNUSED_CHECK).await;
            if self.dead.load(Ordering::Relaxed) || !lock(&self.pages).contains_key(&page.id) {
                return;
            }
            // A tab an agent named, or one waiting for a person, stays.
            let limit = if page.preview {
                IDLE_PREVIEW
            } else {
                UNUSED_TAB
            };
            let left = {
                let state = lock(&page.state);
                state.viewers == 0 && state.used.elapsed() >= limit
            } && !self.kept(&page);
            if left {
                tracing::info!(page = %page.id.0, "closing a browser tab nobody used for a while");
                let _ = self
                    .cdp
                    .send(
                        "Target.closeTarget",
                        json!({ "targetId": page.target }),
                        None,
                    )
                    .await;
                self.forget(&page.id, "The tab was closed after going unused.");
                return;
            }
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
        if let Some(scale) = params["metadata"]["pageScaleFactor"].as_f64()
            && scale.is_finite()
            && scale > 0.
        {
            lock(&page.state).page_scale = scale as f32;
        }
        let ack = params["sessionId"].clone();
        let (browser, paced) = (self.clone(), page.clone());
        // A view's screencast is only the sign that it changed (its picture
        // is a screenshot), so nothing is gained by slowing it, and a sign
        // that waits is a sign Chrome drops.
        let pace = if page.view.is_some() {
            Duration::ZERO
        } else {
            lock(&page.state).cast.pace()
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
            // Held still: where its text fields are now (it may have
            // scrolled, or laid out anew).
            browser.check_fields(&page).await;
        });
    }
}

fn metrics(info: &PageInfo) -> Value {
    json!({
        "width": info.width,
        "height": info.height,
        "deviceScaleFactor": info.scale,
        "mobile": info.device.as_ref().is_some_and(|d| d.mobile),
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

/// `first` and `then` as one input, when nothing is lost by it: two wheel
/// turns scroll by their sum where the second one points, and a pointer
/// that moved twice (with the same button held) is where it moved last.
/// Anything else (a press, a key) is replayed as it came.
fn merge(first: &PageInput, then: &PageInput) -> Option<PageInput> {
    match (first, then) {
        (
            PageInput::Wheel {
                dx: dx1,
                dy: dy1,
                modifiers: m1,
                ..
            },
            PageInput::Wheel {
                x,
                y,
                dx,
                dy,
                modifiers,
            },
        ) if m1 == modifiers => Some(PageInput::Wheel {
            x: *x,
            y: *y,
            dx: dx1 + dx,
            dy: dy1 + dy,
            modifiers: *modifiers,
        }),
        (
            PageInput::Mouse {
                action: PageMouse::Move,
                button: b1,
                modifiers: m1,
                ..
            },
            PageInput::Mouse {
                action: PageMouse::Move,
                button,
                modifiers,
                ..
            },
        ) if b1 == button && m1 == modifiers => Some(then.clone()),
        _ => None,
    }
}

/// `input` with its position in the page's CSS pixels, for a page shown at
/// `page_scale` (what a viewer sees is the page magnified that much).
fn unscaled(input: PageInput, page_scale: f32) -> PageInput {
    if (page_scale - 1.0).abs() < 1e-3 {
        return input;
    }
    match input {
        PageInput::Mouse {
            action,
            x,
            y,
            button,
            click_count,
            modifiers,
        } => PageInput::Mouse {
            action,
            x: x / page_scale,
            y: y / page_scale,
            button,
            click_count,
            modifiers,
        },
        PageInput::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers,
        } => PageInput::Wheel {
            x: x / page_scale,
            y: y / page_scale,
            dx,
            dy,
            modifiers,
        },
        other => other,
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
            oninput=\"document.title = 'typed:' + this.value\">\
            <script>document.title = 'webdriver:' + navigator.webdriver</script>";
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
                Which::Current,
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
                Which::Current,
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

        // The page sees an ordinary browser, not one a program drives: some
        // sites stop what they play for those (YouTube, after ~40 s).
        tokio::time::timeout(Duration::from_secs(20), async {
            while pages.info(&page.id).unwrap().title != "webdriver:false" {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("navigator.webdriver is false");

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

    /// A thread's tabs: several, the one used last is the current one, and
    /// each opening and closing is announced with all of them. Needs Chrome:
    /// `cargo test -p loft-core pages::tests::live -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_thread_has_several_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        let (bus, mut heard) = broadcast::channel(256);
        let pages = Pages::default();
        let thread = ThreadId::new();
        let open = |which: Which| {
            let (pages, thread, profile, bus) =
                (&pages, thread.clone(), profile.clone(), bus.clone());
            async move {
                pages
                    .open(thread, which, None, 400, 300, 1.0, None, &profile, bus)
                    .await
                    .unwrap()
            }
        };
        let tabs_said = |heard: &mut broadcast::Receiver<BusMsg>| {
            let mut last = None;
            while let Ok(msg) = heard.try_recv() {
                if let BusMsg::PageTabs { thread_id, pages } = msg
                    && thread_id == thread
                {
                    last = Some(pages.into_iter().map(|p| p.id).collect::<Vec<_>>());
                }
            }
            last
        };
        let first = open(Which::Current).await;
        let second = open(Which::New).await;
        assert_ne!(first.id, second.id);
        assert_eq!(
            tabs_said(&mut heard),
            Some(vec![first.id.clone(), second.id.clone()])
        );
        // Oldest first; the newest is the one used last.
        let listed: Vec<PageId> = pages.tabs(&thread).into_iter().map(|p| p.id).collect();
        assert_eq!(listed, vec![first.id.clone(), second.id.clone()]);
        assert_eq!(pages.thread_page(&thread), Some(second.id.clone()));
        // Used: the first is current again, and joining the thread joins it.
        pages.touched(&first.id, 7, false);
        assert_eq!(pages.thread_page(&thread), Some(first.id.clone()));
        assert_eq!(open(Which::Current).await.id, first.id);
        // A tab asked for by name is that tab; a closed one is a new tab.
        assert_eq!(open(Which::Tab(second.id.clone())).await.id, second.id);
        pages.close(&second.id).await.unwrap();
        assert_eq!(tabs_said(&mut heard), Some(vec![first.id.clone()]));
        let third = open(Which::Tab(second.id.clone())).await;
        assert!(third.id != second.id && third.id != first.id);
        assert_eq!(pages.tabs(&thread).len(), 2);
        pages.shutdown().await;
    }

    /// A tab is muted until an app on this computer watches it, and again
    /// once someone elsewhere (a phone) uses it; Chrome's own tab muting,
    /// through the sound extension. Needs Chrome.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_tab_is_heard_only_while_someone_here_watches_it() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        let (bus, _heard) = broadcast::channel(256);
        let pages = Pages::default();
        let thread = ThreadId::new();
        let open = |which: Which| {
            let (pages, thread, profile, bus) =
                (&pages, thread.clone(), profile.clone(), bus.clone());
            async move {
                pages
                    .open(thread, which, None, 400, 300, 1.0, None, &profile, bus)
                    .await
                    .unwrap()
            }
        };
        let first = open(Which::Current).await;
        let second = open(Which::New).await;
        assert!(
            pages.current().unwrap().sound.is_some(),
            "the sound extension loaded"
        );
        let muted = |id: &PageId| {
            let (browser, page) = pages.page(id).unwrap();
            async move {
                let sound = browser.sound.as_ref().unwrap();
                sound.muted(&browser.cdp, &page.target).await.unwrap()
            }
        };
        assert!(muted(&first.id).await, "a new tab is quiet");
        assert!(muted(&second.id).await);
        // The app here watches the first: it may be heard, the other not.
        pages.hearing(&first.id, 1, true).await;
        assert!(!muted(&first.id).await);
        assert!(muted(&second.id).await);
        // Used from a phone: quiet here, though the app still watches.
        pages.touched(&first.id, 2, false);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(muted(&first.id).await);
        // Used here again: heard.
        pages.touched(&first.id, 1, true);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!muted(&first.id).await);
        // The app stops watching: quiet.
        pages.hearing(&first.id, 1, false).await;
        assert!(muted(&first.id).await);
        pages.shutdown().await;
    }

    /// A moving page is pictured at every device pixel while the app on
    /// this computer watches it, and at one pixel per CSS pixel while only
    /// viewers elsewhere do. Needs Chrome.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_the_app_here_sees_every_pixel_of_a_moving_page() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        const HTML: &str = "<!doctype html><body style='margin:0'><div id=b \
            style='position:absolute;width:50px;height:50px;background:red'></div>\
            <script>requestAnimationFrame(function f(t){b.style.left=(t/5%300)+'px';\
            requestAnimationFrame(f)})</script>";
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
                Which::Current,
                Some(url),
                400,
                300,
                2.0,
                None,
                &dir.path().join("profile"),
                bus,
            )
            .await
            .unwrap();
        // The height of the next few screencast pictures, from a moment
        // after `pages` changed: the page never holds still, so none is a
        // screenshot.
        async fn heights(pages: &Pages, heard: &mut broadcast::Receiver<BusMsg>) -> Vec<u32> {
            tokio::time::sleep(Duration::from_millis(500)).await;
            *heard = heard.resubscribe();
            let mut seen = Vec::new();
            tokio::time::timeout(Duration::from_secs(20), async {
                while seen.len() < 5 {
                    if let Ok(BusMsg::PageFrame { id, .. }) = heard.recv().await {
                        let (_, page) = pages.page(&id).unwrap();
                        let jpeg = lock(&page.state).frame.clone().unwrap();
                        seen.extend(views::tests::jpeg_height(&jpeg));
                    }
                }
            })
            .await
            .expect("pictures come");
            seen
        }
        pages.watching(&page.id, true).await;
        let small = heights(&pages, &mut heard).await;
        assert!(
            small.iter().all(|h| *h == 300),
            "a phone's pictures are small: {small:?}"
        );
        pages.hearing(&page.id, 1, true).await;
        let here = heights(&pages, &mut heard).await;
        assert!(
            here.iter().all(|h| *h == 600),
            "the app here gets every pixel: {here:?}"
        );
        pages.hearing(&page.id, 1, false).await;
        let small = heights(&pages, &mut heard).await;
        assert!(small.iter().all(|h| *h == 300), "{small:?}");
        pages.shutdown().await;
    }

    /// Serves `html` to every request, on a port of this computer. Each
    /// connection on its own: Chrome opens spare ones it may never use.
    async fn serve(html: &'static str) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0u8; 2048];
                    let _ = socket.read(&mut request).await;
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                        html.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        addr
    }

    async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));
    }

    /// The viewer's pointer becomes what the page wants under it. Needs
    /// Chrome.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_tab_says_which_pointer_it_wants() {
        const HTML: &str = "<!doctype html><body style='margin:0;font:20px sans-serif'>\
            <a href='#x' style='position:absolute;left:10px;top:10px'>a link</a>\
            <p style='position:absolute;left:10px;top:60px;margin:0'>some text</p>\
            <input style='position:absolute;left:10px;top:110px;width:200px;height:30px'>\
            <div style='position:absolute;left:10px;top:160px;width:100px;height:40px;cursor:grab'></div>";
        let addr = serve(HTML).await;
        let dir = tempfile::tempdir().unwrap();
        let pages = Pages::default();
        let page = pages
            .open(
                ThreadId::new(),
                Which::Current,
                Some(format!("http://{addr}/")),
                600,
                400,
                1.0,
                None,
                &dir.path().join("profile"),
                broadcast::channel(256).0,
            )
            .await
            .unwrap();
        for (x, y, wanted) in [
            (30., 20., "pointer"),
            (30., 72., "text"),
            (300., 72., ""),
            (50., 125., "text"),
            (50., 180., "grab"),
            (400., 300., ""),
        ] {
            let at = PageInput::Mouse {
                action: PageMouse::Move,
                x,
                y,
                button: PageButton::None,
                click_count: 0,
                modifiers: 0,
            };
            // Again until it shows: the page may still be loading.
            wait_for(&format!("{wanted:?} at {x},{y}"), || {
                pages.input(&page.id, at.clone()).unwrap();
                pages.info(&page.id).unwrap().cursor == wanted
            })
            .await;
        }
    }

    /// A site asking for a passkey is answered at once, as if the person
    /// cancelled (a headless browser shows no passkey window, and the page
    /// would wait for it), and no passkey can be made. Also after the tab
    /// went to another site. Needs Chrome.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_passkey_request_is_declined_at_once() {
        const HTML: &str = "<!doctype html><title>asking</title><script>\
            const t0 = performance.now();\
            const outcome = (p) => p.then(() => 'made', (e) => e.name);\
            (async () => {\
              const got = await outcome(navigator.credentials.get({ publicKey: {\
                challenge: new Uint8Array(32), timeout: 60000, rpId: 'localhost' } }));\
              const made = await outcome(navigator.credentials.create({ publicKey: {\
                challenge: new Uint8Array(32), rp: { name: 'x', id: 'localhost' },\
                user: { id: new Uint8Array(8), name: 'a', displayName: 'a' },\
                pubKeyCredParams: [{ type: 'public-key', alg: -7 }], timeout: 60000 } }));\
              document.title = got + ' ' + made + (performance.now() - t0 < 5000 ? ' soon' : ' late');\
            })();\
            </script>";
        let addr = serve(HTML).await;
        let dir = tempfile::tempdir().unwrap();
        let pages = Pages::default();
        let page = pages
            .open(
                ThreadId::new(),
                Which::Current,
                Some(format!("http://{addr}/")),
                600,
                400,
                1.0,
                None,
                &dir.path().join("profile"),
                broadcast::channel(256).0,
            )
            .await
            .unwrap();
        // Watched, as a tab someone uses is: one nobody watches draws no
        // frames and runs no timers.
        pages.watching(&page.id, true).await;
        // 127.0.0.1 can't use passkeys at all; `localhost` is another site.
        wait_for("the first site has asked", || {
            pages.info(&page.id).unwrap().title.ends_with("soon")
        })
        .await;
        pages
            .navigate(
                &page.id,
                PageNav::Url {
                    url: format!("http://localhost:{}/", addr.port()),
                },
            )
            .await
            .unwrap();
        wait_for("the passkey requests are answered", || {
            let info = pages.info(&page.id).unwrap();
            info.url.contains("localhost")
                && (info.title.ends_with("soon") || info.title.ends_with("late"))
        })
        .await;
        assert_eq!(
            pages.info(&page.id).unwrap().title,
            "NotAllowedError NotAllowedError soon"
        );
    }

    /// Where a tab's text fields are, and whether one has the focus: what
    /// tells a phone to show its keyboard for a tap. Needs Chrome.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_tab_says_where_its_text_fields_are() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        const HTML: &str = "<!doctype html><body style='margin:0'>\
            <p style='height:100px;margin:0'>text</p>\
            <input id=f style='position:absolute;left:20px;top:200px;width:200px;height:30px'>\
            <button style='position:absolute;left:20px;top:300px'>go</button>";
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
        let (bus, _heard) = broadcast::channel(256);
        let pages = Pages::default();
        let page = pages
            .open(
                ThreadId::new(),
                Which::Current,
                Some(url),
                600,
                500,
                1.0,
                None,
                &dir.path().join("profile"),
                bus,
            )
            .await
            .unwrap();
        let fields = |pages: &Pages| pages.info(&page.id).unwrap();
        let found = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let info = fields(&pages);
                if !info.fields.is_empty() {
                    return info;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("the text field is found");
        assert_eq!(found.fields.len(), 1, "the button is no text field");
        let f = found.fields[0];
        assert_eq!((f.x, f.y), (20.0, 200.0));
        assert!(!found.editing);
        // A click into it: something is typed into now.
        for action in [PageMouse::Down, PageMouse::Up] {
            pages
                .input(
                    &page.id,
                    PageInput::Mouse {
                        action,
                        x: 60.,
                        y: 210.,
                        button: PageButton::Left,
                        click_count: 1,
                        modifiers: 0,
                    },
                )
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while !fields(&pages).editing {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the focused field is noticed");
        pages.shutdown().await;
    }

    fn state(asked: (u32, u32, f32)) -> State {
        State {
            info: PageInfo {
                id: PageId::new(),
                thread_id: ThreadId::new(),
                url: String::new(),
                title: String::new(),
                loading: false,
                can_go_back: false,
                can_go_forward: false,
                width: asked.0,
                height: asked.1,
                scale: asked.2,
                view: false,
                overflow: false,
                cursor: String::new(),
                zoom: 1.0,
                device: None,
                annotating: false,
                editing: false,
                fields: Vec::new(),
                agent: Default::default(),
            },
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
            asked,
            sent: 0,
            used: Instant::now(),
            page_scale: 1.0,
            here: Vec::new(),
            heard_last: true,
            cast: Pictures::Sent,
            pointer_at: None,
            pointer_checking: false,
        }
    }

    #[test]
    fn zoom_and_devices_shape_the_viewport() {
        let mut s = state((1000, 800, 2.0));
        assert!(!s.shape(), "the viewers' own size: nothing to change");
        // Zoomed in, the page has fewer, bigger CSS pixels in the same room.
        s.info.zoom = 2.0;
        assert!(s.shape());
        assert_eq!((s.info.width, s.info.height, s.info.scale), (500, 400, 4.0));
        // Shown as a phone, it is the phone's size whatever the viewers'.
        s.info.zoom = 1.0;
        s.info.device = Some(loft_protocol::PageDevice {
            name: "Phone".into(),
            width: 402,
            height: 874,
            mobile: true,
            user_agent: None,
        });
        assert!(s.shape());
        assert_eq!((s.info.width, s.info.height, s.info.scale), (402, 874, 2.0));
        assert_eq!(metrics(&s.info)["mobile"], true);
        s.asked = (1400, 900, 1.0);
        s.shape();
        assert_eq!((s.info.width, s.info.height, s.info.scale), (402, 874, 1.0));
        // Back to responsive: the viewers' size again.
        s.info.device = None;
        assert!(s.shape());
        assert_eq!((s.info.width, s.info.height), (1400, 900));
        assert_eq!(metrics(&s.info)["mobile"], false);
    }

    #[test]
    fn a_magnified_page_is_pointed_at_in_its_own_pixels() {
        let click = PageInput::Mouse {
            action: PageMouse::Down,
            x: 41.0,
            y: 82.0,
            button: PageButton::Left,
            click_count: 1,
            modifiers: 0,
        };
        assert_eq!(unscaled(click.clone(), 1.0), click);
        let PageInput::Mouse { x, y, .. } = unscaled(click, 0.5) else {
            panic!()
        };
        assert_eq!((x, y), (82.0, 164.0));
    }

    #[test]
    fn queued_wheels_and_moves_are_replayed_as_one() {
        let wheel = |dy: f32, modifiers: u8| PageInput::Wheel {
            x: 10.,
            y: dy,
            dx: 1.,
            dy,
            modifiers,
        };
        let mouse = |action, x: f32, button| PageInput::Mouse {
            action,
            x,
            y: 5.,
            button,
            click_count: 0,
            modifiers: 0,
        };
        // Two wheel turns scroll by their sum, where the last one points.
        assert_eq!(
            merge(&wheel(3., 0), &wheel(4., 0)),
            Some(PageInput::Wheel {
                x: 10.,
                y: 4.,
                dx: 2.,
                dy: 7.,
                modifiers: 0,
            })
        );
        // ⌘ or ⇧ held for one of them only: each says something else.
        assert_eq!(merge(&wheel(3., 0), &wheel(4., 4)), None);
        // A pointer that moved twice is where it moved last…
        let (first, then) = (
            mouse(PageMouse::Move, 1., PageButton::None),
            mouse(PageMouse::Move, 2., PageButton::None),
        );
        assert_eq!(merge(&first, &then), Some(then.clone()));
        // …but a drag doesn't merge with a hover, and a press is never lost.
        assert_eq!(
            merge(&first, &mouse(PageMouse::Move, 2., PageButton::Left)),
            None
        );
        assert_eq!(
            merge(&first, &mouse(PageMouse::Down, 2., PageButton::Left)),
            None
        );
        assert_eq!(merge(&first, &wheel(1., 0)), None);
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

    /// A file-viewer HTML preview loads its local file, streams pictures,
    /// stays out of the thread's browser tabs, and closes cleanly. Needs Chrome.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_a_file_preview_is_separate_from_browser_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.html");
        std::fs::write(
            &file,
            "<!doctype html><html><head><title>Preview OK</title></head><body><h1>Hi</h1></body></html>",
        )
        .unwrap();
        let file_url = url::Url::from_file_path(&file).unwrap().to_string();
        let pages = Pages::default();
        let thread = ThreadId::new();
        let (bus, _) = broadcast::channel(256);
        let page = pages
            .open_preview(
                thread.clone(),
                &file_url,
                600,
                400,
                1.0,
                None,
                &dir.path().join("profile"),
                bus,
            )
            .await
            .unwrap();
        // Not listed as a browser tab and not selected as the agent's tab.
        assert!(pages.tabs(&thread).is_empty());
        assert!(pages.thread_page(&thread).is_none());
        pages.watching(&page.id, true).await;
        wait_for("preview loads its title and draws a picture", || {
            let Ok((info, frame, _)) = pages.snapshot(&page.id) else {
                return false;
            };
            info.title == "Preview OK" && frame.is_some()
        })
        .await;
        pages.close(&page.id).await.unwrap();
        assert!(pages.info(&page.id).is_none());
        pages.shutdown().await;
    }
}
