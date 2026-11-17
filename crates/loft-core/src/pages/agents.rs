//! Agents' tabs (ADR-0017 §9): the tab handle an agent passes (an id, or a
//! name it chose), what a tab was opened with (allowed origins, read-only),
//! the tabs pages open (popups), and a tab handed to a person
//! (`browser_handoff`, `browser_demonstrate`).

use super::demonstrate::Recording;
use super::{Browser, Page, Pages, check_url, lock, origins};
use crate::BusMsg;
use loft_page_script::{BrowserCommand, BrowserError};
use loft_protocol::{BrowserTab, HandoffKind, PageAgent, PageHandoff, PageId, PageInfo, ThreadId};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::watch;

/// A name an agent may give a tab: lowercase letters, digits and dashes,
/// starting with a letter, at most 64 characters (`docs`, `work-mail`).
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The allowed origins as kept (checked and tidied), or why not.
pub fn normalize_origins(raw: &[String]) -> Result<Vec<String>, String> {
    origins::normalize(raw)
}

/// What a tab's agent side keeps, beside its loads (`automation::Driver`).
#[derive(Default)]
pub(crate) struct TabDriver {
    /// An agent's command runs on the tab: what the page does meanwhile is
    /// the agent's doing (its allowed origins are enforced on it).
    busy: AtomicBool,
    /// Tabs this one's pages opened that the agent hasn't been told of.
    popups: Mutex<Vec<PageId>>,
    /// What to tell the agent with its next result.
    notes: Mutex<Vec<String>>,
    /// The tab that opened this one (a popup), and whether that was while
    /// an agent drove it: then its first document is the agent's doing too.
    opener: Mutex<Option<PageId>>,
    agent_popup: AtomicBool,
    /// The person was handed the tab.
    handover: Mutex<Option<Arc<Handover>>>,
    /// The last handoff that ended: an agent whose call ran out before
    /// the person pressed Continue hears how it ended when it asks again,
    /// rather than handing the tab over a second time.
    finished: Mutex<Option<Arc<Handover>>>,
}

/// Clears [`TabDriver::busy`] when the command is done.
pub(crate) struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl TabDriver {
    pub(crate) fn busy(&self) -> Busy<'_> {
        self.busy.store(true, Ordering::Relaxed);
        Busy(&self.busy)
    }

    fn note(&self, note: String) {
        let mut notes = lock(&self.notes);
        if notes.len() < 20 && !notes.contains(&note) {
            notes.push(note);
        }
    }
}

/// How a handoff ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffOutcome {
    /// A person pressed Continue (or Done).
    Continued,
    /// The tab closed, or the browser stopped.
    Closed,
    /// The agent's turn ended (it was stopped, or it finished).
    Stopped,
    /// Nobody came for too long.
    Expired,
}

impl HandoffOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            HandoffOutcome::Continued => "continued",
            HandoffOutcome::Closed => "closed",
            HandoffOutcome::Stopped => "stopped",
            HandoffOutcome::Expired => "expired",
        }
    }
}

/// A tab handed to a person.
pub struct Handover {
    pub kind: HandoffKind,
    pub tab: PageId,
    pub thread: ThreadId,
    /// Set once, when it ends.
    done: watch::Sender<Option<HandoffOutcome>>,
    /// What the person does, for `browser_demonstrate`.
    pub(super) recording: Option<Arc<Recording>>,
    /// Where the tab was when it was handed over.
    pub started_url: String,
    /// The agent was told how it ended.
    told: AtomicBool,
}

impl Handover {
    /// The agent has been told how it ended.
    pub fn told(&self) {
        self.told.store(true, Ordering::Relaxed);
    }

    /// Wait until it ends, or `limit` passes (`None`: still going).
    pub async fn wait(&self, limit: std::time::Duration) -> Option<HandoffOutcome> {
        let mut done = self.done.subscribe();
        let ended = tokio::time::timeout(limit, done.wait_for(Option::is_some)).await;
        match ended {
            Ok(Ok(outcome)) => *outcome,
            // The sender is gone: it ended with the browser.
            Ok(Err(_)) => Some(HandoffOutcome::Closed),
            Err(_) => None,
        }
    }

    pub fn outcome(&self) -> Option<HandoffOutcome> {
        *self.done.borrow()
    }

    /// What the person did so far (`browser_demonstrate`).
    pub fn steps(&self) -> Vec<Value> {
        self.recording
            .as_ref()
            .map(|r| r.steps())
            .unwrap_or_default()
    }

    pub fn summary(&self) -> Vec<String> {
        self.recording
            .as_ref()
            .map(|r| r.summary())
            .unwrap_or_default()
    }
}

/// The origin of an address, to show on a handoff's card.
fn origin_of(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u.origin().ascii_serialization(),
        Ok(u) if u.scheme() == "file" => "a file on this computer".to_owned(),
        _ => String::new(),
    }
}

/// The tool that hands a tab over this way.
fn tool_of(kind: HandoffKind) -> &'static str {
    match kind {
        HandoffKind::Handoff => "browser_handoff",
        HandoffKind::Demonstrate => "browser_demonstrate",
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl Browser {
    /// Whether an agent's `command` may run on the tab, as it was opened
    /// and as it is now.
    pub(super) fn may_drive(
        &self,
        page: &Page,
        command: &BrowserCommand,
    ) -> Result<(), BrowserError> {
        let (agent, url) = {
            let state = lock(&page.state);
            (state.info.agent.clone(), state.info.url.clone())
        };
        if let Some(handoff) = &agent.handoff {
            let tool = tool_of(handoff.kind);
            return Err(BrowserError::Action(format!(
                "This tab is handed to the person ({tool}: {:?}). Wait for that {tool} call to \
                 return before using the tab again.",
                handoff.reason
            )));
        }
        if agent.read_only && !command.allowed_read_only() {
            return Err(BrowserError::Blocked(format!(
                "this tab is read-only, so `{}` is refused. Read it with browser_snapshot, \
                 browser_screenshot, browser_eval or browser_scroll; clicking, typing and \
                 navigating are off. To act on a page, open a tab without `read_only` \
                 (browser_tabs new).",
                command.action()
            )));
        }
        if !agent.allowed_origins.is_empty()
            && !command.goes_somewhere()
            && !url.is_empty()
            && let Ok(rules) = origins::parse(&agent.allowed_origins)
            && let origins::Decision::Refused(reason) = origins::check(&url, &rules)
        {
            return Err(BrowserError::Blocked(format!(
                "the tab is at {url}, where you may not use it: {reason}. Navigate it back \
                 inside its allowed origins first (browser_navigate or browser_back). The \
                 person may take a tab anywhere; you may not."
            )));
        }
        Ok(())
    }

    /// Whether an agent may send the tab to `url` (its allowed origins).
    pub(super) async fn may_go(&self, page: &Page, url: &str) -> Result<(), BrowserError> {
        let allowed = lock(&page.state).info.agent.allowed_origins.clone();
        if allowed.is_empty() {
            return Ok(());
        }
        let rules = origins::parse(&allowed).map_err(BrowserError::Blocked)?;
        match origins::check_navigation(url, &rules).await {
            origins::Decision::Allowed { .. } => Ok(()),
            origins::Decision::Refused(reason) => Err(BrowserError::Blocked(format!(
                "{reason}. Ask the user before going anywhere else. A tab's allowed origins are \
                 fixed when it opens: for others, open a new tab with browser_tabs new and \
                 `allowed_origins`."
            ))),
        }
    }

    /// The tab's main frame showed `url`. If an agent's command (or a popup
    /// it caused) took it outside its allowed origins, it is sent back to a
    /// blank page, or the popup closed, and the agent told. What a person
    /// does in a tab is theirs: it isn't checked.
    pub(super) fn check_origin(self: &Arc<Self>, page: &Arc<Page>, url: &str) {
        let Some(driver) = self.drivers.get(&page.id) else {
            return;
        };
        let allowed = lock(&page.state).info.agent.allowed_origins.clone();
        if url.is_empty() || url.starts_with("about:") || url.starts_with("chrome-error:") {
            return;
        }
        let fresh_popup = driver.tab.agent_popup.swap(false, Ordering::Relaxed);
        let agents_doing = driver.tab.busy.load(Ordering::Relaxed) || fresh_popup;
        if allowed.is_empty() || !agents_doing {
            return;
        }
        let Ok(rules) = origins::parse(&allowed) else {
            return;
        };
        let origins::Decision::Refused(reason) = origins::check(url, &rules) else {
            return;
        };
        let opener = lock(&driver.tab.opener).clone();
        let (browser, page) = (self.clone(), page.clone());
        if fresh_popup {
            let note = format!(
                "A popup this tab opened went to {url}, which isn't allowed ({reason}), so it \
                 was closed. Ask the user before going elsewhere."
            );
            match opener.and_then(|id| self.drivers.get(&id)) {
                Some(opener) => opener.tab.note(note),
                None => driver.tab.note(note),
            }
            tokio::spawn(async move {
                let _ = browser
                    .cdp
                    .send(
                        "Target.closeTarget",
                        json!({ "targetId": page.target }),
                        None,
                    )
                    .await;
                browser.forget(&page.id, "The popup went somewhere its tab may not go.");
            });
            return;
        }
        driver.tab.note(format!(
            "This tab went to {url}, which isn't allowed ({reason}), so it was sent back to a \
             blank page. Ask the user before going elsewhere."
        ));
        tokio::spawn(async move {
            let s = Some(page.session.as_str());
            let _ = browser.cdp.send("Page.stopLoading", json!({}), s).await;
            let _ = browser
                .cdp
                .send("Page.navigate", json!({ "url": "about:blank" }), s)
                .await;
        });
    }

    /// A page of `opener` opened a window (`target`): it becomes another tab
    /// of the same thread, with the opener's limits, beside it (a window
    /// shows it without leaving the tab it shows).
    pub(super) async fn adopt_popup(self: Arc<Self>, target: String, opener: Arc<Page>) {
        let (thread, size, agent) = {
            let state = lock(&opener.state);
            let agent = &state.info.agent;
            (
                state.info.thread_id.clone(),
                state.asked,
                PageAgent {
                    opened: agent.opened,
                    allowed_origins: agent.allowed_origins.clone(),
                    read_only: agent.read_only,
                    ..PageAgent::default()
                },
            )
        };
        let opener_driver = self.drivers.of(&self, &opener.id);
        let while_driven = opener_driver.tab.busy.load(Ordering::Relaxed);
        let adopted = self
            .adopt(target.clone(), thread, None, size, agent, false)
            .await;
        lock(&self.popups).remove(&target);
        match adopted {
            Ok(info) => {
                let driver = self.drivers.of(&self, &info.id);
                *lock(&driver.tab.opener) = Some(opener.id.clone());
                driver
                    .tab
                    .agent_popup
                    .store(while_driven, Ordering::Relaxed);
                lock(&opener_driver.tab.popups).push(info.id.clone());
                tracing::debug!(page = %info.id.0, "a page's window became a tab");
                // Its first address may already be known.
                if !info.url.is_empty()
                    && let Some(page) = lock(&self.pages).get(&info.id).cloned()
                {
                    self.check_origin(&page, &info.url);
                }
            }
            Err(err) => {
                tracing::debug!(%err, "a page's window didn't become a tab");
                let _ = self
                    .cdp
                    .send("Target.closeTarget", json!({ "targetId": target }), None)
                    .await;
            }
        }
    }

    /// A tab a page opened went to an address tabs may not be sent to (a
    /// local file, the browser's own pages): it is closed. The browser's
    /// page for a failed load is no such address.
    pub(super) fn close_if_unsafe(self: &Arc<Self>, page: &Arc<Page>, url: &str) {
        if url.is_empty()
            || check_url(url).is_ok()
            || url.starts_with("chrome-error:")
            || page.view.is_some()
        {
            return;
        }
        // Only a popup is closed: a tab of the thread's own may show a
        // local file the daemon loaded itself (`automation::allow_file`).
        let popup = self
            .drivers
            .get(&page.id)
            .is_some_and(|d| lock(&d.tab.opener).is_some());
        if !popup {
            return;
        }
        tracing::warn!("a popup went to an address tabs may not show; closed");
        let (browser, page) = (self.clone(), page.clone());
        tokio::spawn(async move {
            let _ = browser
                .cdp
                .send(
                    "Target.closeTarget",
                    json!({ "targetId": page.target }),
                    None,
                )
                .await;
            browser.forget(&page.id, "The popup went to an address tabs may not show.");
        });
    }

    /// End the tab's handoff, if it has one.
    pub(super) fn end_handover(&self, page: &Page, outcome: HandoffOutcome) {
        let Some(driver) = self.drivers.get(&page.id) else {
            return;
        };
        let Some(handover) = lock(&driver.tab.handover).take() else {
            return;
        };
        if let Some(recording) = &handover.recording {
            recording.stop();
        }
        handover.done.send_replace(Some(outcome));
        *lock(&driver.tab.finished) = Some(handover.clone());
        let thread = {
            let mut state = lock(&page.state);
            state.info.agent.handoff = None;
            state.info.thread_id.clone()
        };
        let _ = self.bus.send(BusMsg::PageState {
            id: page.id.clone(),
        });
        self.announce_tabs(&thread);
    }

    /// What `browser_demonstrate` records on the page now, if anything.
    pub(super) fn recording(&self, page: &PageId) -> Option<Arc<Recording>> {
        let driver = self.drivers.get(page)?;
        let handover = lock(&driver.tab.handover);
        handover.as_ref()?.recording.clone()
    }

    /// Whether the tab is one never closed for going unused: a named one,
    /// or one handed to a person.
    pub(super) fn kept(&self, page: &Page) -> bool {
        let state = lock(&page.state);
        state.info.agent.name.is_some() || state.info.agent.handoff.is_some()
    }
}

impl Pages {
    /// The thread's tab an agent names: by its id, or by the name it gave
    /// it.
    pub fn find_tab(&self, thread: &ThreadId, handle: &str) -> Option<PageId> {
        let handle = handle.trim();
        let browser = self.current()?;
        browser
            .tabs_of(thread)
            .into_iter()
            .find(|t| t.id.as_str() == handle || t.agent.name.as_deref() == Some(handle))
            .map(|t| t.id)
    }

    /// The agent is using tab `id`: it is the thread's tab used last (the
    /// one its next call without a `tab` goes to).
    pub fn used_by_agent(&self, id: &PageId) {
        if let Ok((browser, page)) = self.page(id)
            && page.view.is_none()
        {
            let thread = lock(&page.state).info.thread_id.clone();
            lock(&browser.current).insert(thread, id.clone());
        }
    }

    /// The tab as an agent sees it.
    pub fn brief(&self, id: &PageId) -> Option<BrowserTab> {
        self.info(id).map(|info| BrowserTab::of(&info))
    }

    /// The tab was opened by or for an agent: name it, and fix what it may
    /// do. Every window showing the thread hears of it.
    pub fn set_agent(&self, id: &PageId, change: impl FnOnce(&mut PageAgent)) {
        let Ok((browser, page)) = self.page(id) else {
            return;
        };
        let thread = {
            let mut state = lock(&page.state);
            change(&mut state.info.agent);
            state.info.thread_id.clone()
        };
        let _ = browser.bus.send(BusMsg::PageState { id: id.clone() });
        browser.announce_tabs(&thread);
    }

    /// The thread's tabs, for `browser_tabs` and `browser_status`: what an
    /// agent needs to pick one.
    pub fn tab_list(&self, thread: &ThreadId) -> Vec<Value> {
        let Some(browser) = self.current() else {
            return Vec::new();
        };
        let current = lock(&browser.current).get(thread).cloned();
        browser
            .tabs_of(thread)
            .into_iter()
            .map(|info| {
                let (used, busy) = {
                    let page = lock(&browser.pages).get(&info.id).cloned();
                    let used = page
                        .as_ref()
                        .map_or(0, |p| lock(&p.state).used.elapsed().as_secs());
                    let busy = browser
                        .drivers
                        .get(&info.id)
                        .is_some_and(|d| d.tab.busy.load(Ordering::Relaxed));
                    (used, busy)
                };
                describe(&info, current.as_ref() == Some(&info.id), used, busy)
            })
            .collect()
    }

    /// Tabs pages opened, and what the agent should hear, since it was last
    /// told (after a command on `id`).
    pub fn take_news(&self, id: &PageId) -> (Vec<BrowserTab>, Vec<String>) {
        let Some(browser) = self.current() else {
            return (Vec::new(), Vec::new());
        };
        let Some(driver) = browser.drivers.get(id) else {
            return (Vec::new(), Vec::new());
        };
        let popups: Vec<PageId> = std::mem::take(&mut *lock(&driver.tab.popups));
        let tabs = popups
            .iter()
            .filter_map(|p| lock(&browser.pages).get(p).cloned())
            .map(|p| BrowserTab::of(&lock(&p.state).info))
            .collect();
        let notes = std::mem::take(&mut *lock(&driver.tab.notes));
        (tabs, notes)
    }

    /// Hand the tab to a person: its card shows in every window, and what
    /// they do is recorded for `Demonstrate`. If it already waits for them
    /// (the agent asking again after its call timed out), that handoff.
    /// `fresh`: it is a new one.
    pub fn hand_over(
        &self,
        id: &PageId,
        kind: HandoffKind,
        reason: &str,
    ) -> Result<(Arc<Handover>, bool), BrowserError> {
        let (browser, page) = self.page(id).map_err(|_| BrowserError::Closed)?;
        let driver = browser.drivers.of(&browser, id);
        let (url, thread) = {
            let state = lock(&page.state);
            (state.info.url.clone(), state.info.thread_id.clone())
        };
        {
            let pending = lock(&driver.tab.handover);
            if let Some(handover) = pending.as_ref() {
                if handover.kind == kind {
                    return Ok((handover.clone(), false));
                }
                return Err(BrowserError::Action(format!(
                    "This tab is already with the person for {}. Wait for that call to return \
                     first.",
                    tool_of(handover.kind)
                )));
            }
        }
        // It ended between the agent's calls: that is the answer.
        if let Some(ended) = lock(&driver.tab.finished)
            .take_if(|h| h.kind == kind && !h.told.load(Ordering::Relaxed))
        {
            return Ok((ended, false));
        }
        let recording = match kind {
            HandoffKind::Demonstrate => Some(Arc::new(Recording::new())),
            HandoffKind::Handoff => None,
        };
        let (done, _) = watch::channel(None);
        let handover = Arc::new(Handover {
            kind,
            tab: id.clone(),
            thread: thread.clone(),
            done,
            recording,
            started_url: url.clone(),
            told: AtomicBool::new(false),
        });
        *lock(&driver.tab.handover) = Some(handover.clone());
        {
            let mut state = lock(&page.state);
            state.used = Instant::now();
            state.info.agent.handoff = Some(PageHandoff {
                kind,
                reason: reason.trim().to_owned(),
                origin: origin_of(&url),
                since_ms: now_ms(),
            });
        }
        let _ = browser.bus.send(BusMsg::PageState { id: id.clone() });
        browser.announce_tabs(&thread);
        tracing::info!(page = %id.0, ?kind, "an agent handed a tab to a person");
        Ok((handover, true))
    }

    /// A person pressed Continue (or Done) on the tab's card.
    pub fn handover_done(&self, id: &PageId) -> Result<(), loft_protocol::ErrorBody> {
        let (browser, page) = self.page(id)?;
        browser.end_handover(&page, HandoffOutcome::Continued);
        Ok(())
    }

    /// End the tab's handoff (its turn ended, or nobody came).
    pub fn end_handover(&self, id: &PageId, outcome: HandoffOutcome) {
        if let Ok((browser, page)) = self.page(id) {
            browser.end_handover(&page, outcome);
        }
    }

    /// Whether the daemon's browser runs now, and with which program.
    pub fn browser_running(&self) -> bool {
        self.current().is_some()
    }
}

/// One tab, for an agent's list.
fn describe(info: &PageInfo, current: bool, idle_secs: u64, busy: bool) -> Value {
    let mut tab = json!({
        "tab": info.agent.name.clone().unwrap_or_else(|| info.id.0.clone()),
        "id": info.id,
        "url": info.url,
        "title": info.title,
        "opened_by": if info.agent.opened { "agent" } else { "person" },
        "idle_seconds": idle_secs,
    });
    if let Some(name) = &info.agent.name {
        tab["name"] = json!(name);
    }
    if current {
        tab["current"] = json!(true);
    }
    if busy {
        tab["busy"] = json!(true);
    }
    if info.loading {
        tab["loading"] = json!(true);
    }
    if !info.agent.allowed_origins.is_empty() {
        tab["allowed_origins"] = json!(info.agent.allowed_origins);
    }
    if info.agent.read_only {
        tab["read_only"] = json!(true);
    }
    if let Some(handoff) = &info.agent.handoff {
        tab["waiting_for_person"] = json!({
            "kind": handoff.kind,
            "reason": handoff.reason,
        });
    }
    tab
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_names_are_slugs() {
        assert!(valid_name("docs"));
        assert!(valid_name("work-mail-2"));
        assert!(!valid_name(""));
        assert!(!valid_name("Docs"));
        assert!(!valid_name("2fa"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(&"a".repeat(65)));
    }

    #[test]
    fn a_handoff_card_names_the_site() {
        assert_eq!(
            origin_of("https://github.com/login?x=1"),
            "https://github.com"
        );
        assert_eq!(origin_of("about:blank"), "");
        assert_eq!(
            origin_of("file:///tmp/page.html"),
            "a file on this computer"
        );
    }

    /// A tiny site: every address answers `html`, each connection in a task
    /// of its own (Chrome keeps spare connections open).
    async fn serve(html: &'static str) -> u16 {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0u8; 2048];
                    let _ = socket.read(&mut request).await;
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{html}",
                        html.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        port
    }

    async fn eventually<T>(what: &str, mut found: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Some(found) = found() {
                return found;
            }
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Agents' tabs, end to end in a real browser: a named tab with allowed
    /// origins, popups that become tabs (or are closed when they go where
    /// the tab may not), read-only, a handoff a person finishes, and a
    /// demonstration recorded from a viewer's input. Needs Chrome:
    /// `cargo test -p loft-core pages:: -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_agents_tabs_popups_limits_and_handoffs() {
        use crate::pages::Which;
        use loft_protocol::{PageInput, PageMouse};
        const HTML: &str = "<!doctype html><meta charset=utf-8><title>Home</title>\
            <body style='margin:0;font:16px sans-serif'>\
            <a id=pop href='/other' target=_blank>pop</a>\
            <p><input id=name aria-label='Your name' style='width:200px'></p>\
            <p><input id=pw type=password aria-label='Password' style='width:200px'></p>\
            <button id=go>Sign in</button>";
        let home = serve(HTML).await;
        let elsewhere = serve(HTML).await;
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        let (bus, _heard) = tokio::sync::broadcast::channel(1024);
        let pages = Pages::default();
        let thread = ThreadId::new();
        let open = |which: Which| {
            let (pages, thread, profile, bus) =
                (&pages, thread.clone(), profile.clone(), bus.clone());
            async move {
                pages
                    .open(thread, which, None, 800, 600, 1.0, None, &profile, bus)
                    .await
                    .unwrap()
                    .id
            }
        };
        let search = "https://example.com/?q={query}";
        let persons = open(Which::Current).await;
        let docs = open(Which::New).await;
        let allowed = vec![format!("http://127.0.0.1:{home}")];
        pages.set_agent(&docs, |a| {
            a.opened = true;
            a.name = Some("docs".into());
            a.allowed_origins = normalize_origins(&allowed).unwrap();
        });
        assert_eq!(pages.find_tab(&thread, "docs"), Some(docs.clone()));
        assert_eq!(pages.find_tab(&thread, docs.as_str()), Some(docs.clone()));
        assert_eq!(pages.find_tab(&thread, "nope"), None);
        let list = pages.tab_list(&thread);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["opened_by"], "person");
        assert_eq!(list[1]["tab"], "docs");
        assert_eq!(list[1]["opened_by"], "agent");

        // Only where the tab may go.
        let home_url = format!("http://127.0.0.1:{home}/");
        let nav = BrowserCommand::Navigate {
            url: home_url.clone(),
        };
        pages.drive(&docs, nav, search).await.unwrap();
        let away = BrowserCommand::Navigate {
            url: format!("http://127.0.0.1:{elsewhere}/"),
        };
        let refused = pages.drive(&docs, away, search).await.unwrap_err();
        assert!(refused.to_string().contains("allowed origins"), "{refused}");

        // A link that opens a window: another tab of the thread, told to
        // the agent, with the same limits.
        let click = BrowserCommand::Eval {
            script: "document.getElementById('pop').click(); \
                     await new Promise(r => setTimeout(r, 800)); return 1"
                .into(),
        };
        pages.drive(&docs, click, search).await.unwrap();
        let popup = eventually("the popup became a tab", || {
            let (tabs, _) = pages.take_news(&docs);
            tabs.into_iter().next()
        })
        .await;
        let popup_info = pages.info(&popup.id).unwrap();
        assert!(popup_info.agent.opened);
        assert_eq!(popup_info.agent.allowed_origins, allowed);
        eventually("the popup's address", || {
            pages.info(&popup.id).filter(|i| i.url.ends_with("/other"))
        })
        .await;
        assert_eq!(pages.tabs(&thread).len(), 3);
        // Opening a tab doesn't make it the one used last.
        pages.used_by_agent(&docs);
        assert_eq!(pages.thread_page(&thread), Some(docs.clone()));
        // A window sent where the tab may not go is closed, and the agent
        // told.
        let wander = BrowserCommand::Eval {
            script: format!(
                "window.open('http://127.0.0.1:{elsewhere}/'); \
                 await new Promise(r => setTimeout(r, 2500)); return 1"
            ),
        };
        pages.drive(&docs, wander, search).await.unwrap();
        let note = eventually("the agent was told", || {
            let (_, notes) = pages.take_news(&docs);
            notes.into_iter().find(|n| n.contains("popup"))
        })
        .await;
        assert!(note.contains("closed"), "{note}");
        eventually("the wandering popup closed", || {
            (pages.tabs(&thread).len() == 3).then_some(())
        })
        .await;

        // Where the person's input will land (before anything is handed).
        let at = |id: &str| BrowserCommand::Eval {
            script: format!(
                "const r = document.getElementById('{id}').getBoundingClientRect(); \
                 return [r.x + r.width / 2, r.y + r.height / 2]"
            ),
        };
        let mut points = Vec::new();
        for id in ["name", "pw", "go"] {
            let xy = pages.drive(&docs, at(id), search).await.unwrap();
            points.push((
                xy[0].as_f64().unwrap() as f32,
                xy[1].as_f64().unwrap() as f32,
            ));
        }

        // Read-only: reads only.
        pages.set_agent(&docs, |a| a.read_only = true);
        let clicked = pages
            .drive(
                &docs,
                BrowserCommand::Click {
                    target: loft_page_script::Target::by_ref(1),
                    button: Default::default(),
                    count: 1,
                    modifiers: Vec::new(),
                },
                search,
            )
            .await
            .unwrap_err();
        assert!(clicked.to_string().contains("read-only"), "{clicked}");
        pages
            .drive(&docs, BrowserCommand::Snapshot { within: None }, search)
            .await
            .unwrap();
        pages.set_agent(&docs, |a| a.read_only = false);

        // Handed to the person: the agent waits until they press Continue,
        // and can't use the tab meanwhile.
        let (handover, fresh) = pages
            .hand_over(&docs, HandoffKind::Handoff, "Sign in")
            .unwrap();
        assert!(fresh);
        let card = pages.info(&docs).unwrap().agent.handoff.unwrap();
        assert_eq!(card.reason, "Sign in");
        assert_eq!(card.origin, format!("http://127.0.0.1:{home}"));
        let (again, fresh) = pages
            .hand_over(&docs, HandoffKind::Handoff, "Sign in")
            .unwrap();
        assert!(!fresh && Arc::ptr_eq(&again, &handover));
        let busy = pages
            .drive(&docs, BrowserCommand::Snapshot { within: None }, search)
            .await
            .unwrap_err();
        assert!(busy.to_string().contains("handed to the person"), "{busy}");
        assert_eq!(
            handover.wait(std::time::Duration::from_millis(100)).await,
            None
        );
        pages.handover_done(&docs).unwrap();
        assert_eq!(
            handover.wait(std::time::Duration::from_secs(1)).await,
            Some(HandoffOutcome::Continued)
        );
        assert!(pages.info(&docs).unwrap().agent.handoff.is_none());

        // Shown how: what the person does from a window is written down,
        // their password isn't.
        let (shown, _) = pages
            .hand_over(&docs, HandoffKind::Demonstrate, "Show me how to sign in")
            .unwrap();
        let press = |(x, y): (f32, f32)| {
            for action in [PageMouse::Down, PageMouse::Up] {
                pages
                    .input(
                        &docs,
                        PageInput::Mouse {
                            action,
                            x,
                            y,
                            button: loft_protocol::PageButton::Left,
                            click_count: 1,
                            modifiers: 0,
                        },
                    )
                    .unwrap();
            }
        };
        let typing = |text: &str| {
            for c in text.chars() {
                let key = c.to_string();
                for down in [true, false] {
                    pages
                        .input(
                            &docs,
                            PageInput::Key {
                                down,
                                key: key.clone(),
                                code: format!("Key{}", c.to_ascii_uppercase()),
                                text: down.then(|| key.clone()),
                                modifiers: 0,
                                repeat: false,
                            },
                        )
                        .unwrap();
                }
            }
        };
        press(points[0]);
        typing("ada");
        press(points[1]);
        typing("hunter");
        press(points[2]);
        let summary = eventually("the steps were written down", || {
            let summary = shown.summary();
            (summary.len() >= 5).then_some(summary)
        })
        .await;
        pages.handover_done(&docs).unwrap();
        let all = summary.join("\n");
        for step in [
            r#"Clicked the textbox "Your name" (#name)"#,
            r#"Typed "ada" into the textbox "Your name" (#name)"#,
            "a secret of 6 characters",
            r#"Clicked the button "Sign in" (#go)"#,
        ] {
            assert!(all.contains(step), "{step}: {all}");
        }
        assert!(!all.contains("hunter"), "{all}");
        assert_eq!(
            shown.wait(std::time::Duration::from_secs(1)).await,
            Some(HandoffOutcome::Continued)
        );

        // A handoff ends when its tab closes.
        let (closing, _) = pages
            .hand_over(&popup.id, HandoffKind::Handoff, "Pay")
            .unwrap();
        pages.close(&popup.id).await.unwrap();
        assert_eq!(
            closing.wait(std::time::Duration::from_secs(1)).await,
            Some(HandoffOutcome::Closed)
        );
        let _ = persons;
        pages.shutdown().await;
    }
}
