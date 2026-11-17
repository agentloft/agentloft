//! Captures follow the page's frames of other sites (ADR-0017 §8).
//!
//! Under site isolation another site's `<iframe>` runs in a renderer of its own: a DevTools target with a session of
//! its own, whose requests and console messages the page's session never sees (and whose document's response ends
//! there, so without it the frame's document looked unfinished).
//!
//! - While a network or console capture runs on a page, the page auto-attaches to its frames of other sites
//!   (`Target.setAutoAttach`, iframes only, as `pages/frames.rs` does for agents' reads; popups are not auto-attached,
//!   `agents.rs` takes them from `Target.targetCreated`) with `waitForDebuggerOnStart`: a frame made meanwhile waits
//!   until the captures' domains are on in it, then runs (`Runtime.runIfWaitingForDebugger`), so its first request
//!   and message are captured. It is always let run, capture or not. When no capture runs any more, new frames no
//!   longer wait (auto-attach itself stays on: agents' reads use those sessions).
//! - Frames already in the page when a capture starts are followed from a moment after.
//! - Each frame's session gets the domains the page's captures use (`Network`; `Runtime` and `Log` only while a
//!   console capture runs, the same stealth rule as the page's), and loses them when the capture ends.
//! - The frames' sessions' events reach the page's captures (`Browser::run` looks their session up here), marked with
//!   the frame's origin. Everything comes through the browser's one pipe, so they are in time order with the page's
//!   own events. An exchange whose request the page's session saw (a frame's own document) and whose response the
//!   frame's did is one exchange: request ids are the browser's, not a session's.
//! - A frame's own frames of other sites are followed the same way.

use super::super::automation::Agent;
use super::super::cdp::{Cdp, Event};
use super::super::frames::auto_attach;
use super::super::{Page, lock};
use super::{Captures, PageCapture, console, network};
use loft_page_script::SCRIPT_TIMEOUT;
use loft_protocol::PageId;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A frame of another site, attached over its own session.
#[derive(Clone, Debug)]
struct Followed {
    page: PageId,
    target: String,
    /// The session it was announced on: the page's, or a frame's.
    parent: String,
    /// What captures call it: its origin.
    origin: String,
}

/// The pages' frames of other sites, by their session.
#[derive(Default)]
pub(super) struct FrameSessions(Mutex<HashMap<String, Followed>>);

impl FrameSessions {
    /// The page a frame's session belongs to.
    pub(super) fn page_of(&self, session: &str) -> Option<PageId> {
        lock(&self.0).get(session).map(|f| f.page.clone())
    }

    /// What captures call the frame whose session it is.
    pub(super) fn origin_of(&self, session: &str) -> Option<String> {
        lock(&self.0).get(session).map(|f| f.origin.clone())
    }

    /// The page's frames' sessions.
    fn sessions_of(&self, page: &PageId) -> Vec<String> {
        lock(&self.0)
            .iter()
            .filter(|(_, f)| &f.page == page)
            .map(|(session, _)| session.clone())
            .collect()
    }

    /// A frame's session ended: its own frames' too.
    fn detached(&self, session: &str) {
        let mut frames = lock(&self.0);
        let mut gone = vec![session.to_owned()];
        while let Some(session) = gone.pop() {
            frames.remove(&session);
            gone.extend(
                frames
                    .iter()
                    .filter(|(_, f)| f.parent == session)
                    .map(|(s, _)| s.clone()),
            );
        }
    }

    /// A frame went to another address (`Target.targetInfoChanged`).
    pub(super) fn moved(&self, target: &str, url: &str) {
        for frame in lock(&self.0).values_mut().filter(|f| f.target == target) {
            frame.origin = origin(url);
        }
    }

    /// The page closed.
    pub(super) fn forget(&self, page: &PageId) {
        lock(&self.0).retain(|_, f| &f.page != page);
    }
}

/// A frame's name in a capture: its address's origin (never its path or query, which may hold what redaction would
/// take out), or its address when it has no origin of its own.
fn origin(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) if parsed.origin().is_tuple() => parsed.origin().ascii_serialization(),
        _ => url.split(['?', '#']).next().unwrap_or_default().to_owned(),
    }
}

/// Which of the page's captures run, so which domains its frames need on.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(super) struct Wanted {
    pub(super) network: bool,
    pub(super) console: bool,
}

impl Wanted {
    pub(super) const NETWORK: Wanted = Wanted {
        network: true,
        console: false,
    };
    pub(super) const CONSOLE: Wanted = Wanted {
        network: false,
        console: true,
    };

    fn of(state: &PageCapture) -> Wanted {
        Wanted {
            network: network::running(state),
            console: console::running(state),
        }
    }

    fn any(self) -> bool {
        self.network || self.console
    }

    /// What `self` has that `now` doesn't.
    fn without(self, now: Wanted) -> Wanted {
        Wanted {
            network: self.network && !now.network,
            console: self.console && !now.console,
        }
    }
}

/// Turn `what` on in a frame.
async fn turn_on(cdp: &Cdp, session: &str, what: Wanted) {
    let mut commands = Vec::new();
    if what.network {
        // As the page's (`Agent::network_on`).
        commands.push(("Network.enable", json!({ "maxPostDataSize": 65_536 })));
    }
    if what.console {
        commands.push(("Runtime.enable", json!({})));
        commands.push(("Log.enable", json!({})));
    }
    for (method, params) in commands {
        let _ = cdp
            .send_within(method, params, Some(session), SCRIPT_TIMEOUT)
            .await;
    }
}

/// Turn `what` off in a frame.
async fn turn_off(cdp: &Cdp, session: &str, what: Wanted) {
    let mut commands = Vec::new();
    if what.network {
        commands.push("Network.disable");
    }
    if what.console {
        commands.push("Runtime.disable");
        commands.push("Log.disable");
    }
    for method in commands {
        let _ = cdp
            .send_within(method, json!({}), Some(session), SCRIPT_TIMEOUT)
            .await;
    }
}

/// `Target.setAutoAttach` on a session (`paused`: new frames wait to be let run).
async fn set_auto_attach(cdp: &Cdp, session: &str, paused: bool) {
    let _ = cdp
        .send_within(
            "Target.setAutoAttach",
            auto_attach(paused),
            Some(session),
            SCRIPT_TIMEOUT,
        )
        .await;
}

/// `Target.attachedToTarget` / `detachedFromTarget`, on the page's session or a frame's: a frame of another site
/// came or went. One that came gets the domains the page's captures use, then is let run if it waits.
pub(super) fn observe(cdp: &Arc<Cdp>, captures: &Captures, page: &Page, event: &Event) {
    let params = &event.params;
    let Some(session) = params["sessionId"].as_str() else {
        return;
    };
    if event.method == "Target.detachedFromTarget" {
        captures.frames.detached(session);
        return;
    }
    let info = &params["targetInfo"];
    let frame = info["type"] == "iframe";
    let waiting = params["waitingForDebugger"].as_bool().unwrap_or(false);
    if frame {
        lock(&captures.frames.0).insert(
            session.to_owned(),
            Followed {
                page: page.id.clone(),
                target: info["targetId"].as_str().unwrap_or_default().to_owned(),
                parent: event.session.clone().unwrap_or_default(),
                origin: origin(info["url"].as_str().unwrap_or_default()),
            },
        );
    }
    let state = captures.get(&page.id).filter(|_| frame);
    let wanted = state.as_deref().map(Wanted::of).unwrap_or_default();
    if !waiting && !wanted.any() {
        return;
    }
    let (cdp, session) = (cdp.clone(), session.to_owned());
    tokio::spawn(async move {
        if wanted.any() {
            turn_on(&cdp, &session, wanted).await;
            set_auto_attach(&cdp, &session, true).await;
        }
        // Never left waiting, whatever happened above.
        if waiting {
            let _ = cdp
                .send_within(
                    "Runtime.runIfWaitingForDebugger",
                    json!({}),
                    Some(&session),
                    SCRIPT_TIMEOUT,
                )
                .await;
        }
        // A capture that ended meanwhile may have missed this frame.
        if let Some(state) = state {
            let ended = wanted.without(Wanted::of(&state));
            if ended.any() {
                turn_off(&cdp, &session, ended).await;
            }
        }
    });
}

/// A capture of `what` started on the agent's page: its frames of other sites are followed.
pub(super) async fn follow(agent: &Agent<'_>, what: Wanted) {
    agent.auto_attach_frames(true).await;
    let cdp = &agent.browser.cdp;
    for session in agent.browser.captures.frames.sessions_of(&agent.page.id) {
        turn_on(cdp, &session, what).await;
        set_auto_attach(cdp, &session, true).await;
    }
}

/// What a capture's end needs to let the frames go, without an agent (a capture also ends by itself).
#[derive(Clone)]
pub(super) struct Unfollow {
    cdp: Arc<Cdp>,
    frames: Arc<FrameSessions>,
    page: PageId,
    session: String,
}

impl Unfollow {
    pub(super) fn of(agent: &Agent<'_>) -> Unfollow {
        Unfollow {
            cdp: agent.browser.cdp.clone(),
            frames: agent.browser.captures.frames.clone(),
            page: agent.page.id.clone(),
            session: agent.page.session.clone(),
        }
    }

    /// A capture of `what` ended: its domains off in the frames, and once no capture runs, new frames no longer
    /// wait.
    pub(super) async fn run(&self, state: &PageCapture, what: Wanted) {
        let sessions = self.frames.sessions_of(&self.page);
        for session in &sessions {
            turn_off(&self.cdp, session, what).await;
        }
        if !Wanted::of(state).any() {
            for session in std::iter::once(&self.session).chain(&sessions) {
                set_auto_attach(&self.cdp, session, false).await;
            }
        }
    }
}

impl Captures {
    /// The page whose frame's session it is.
    pub(in crate::pages) fn frame_page(&self, session: &str) -> Option<PageId> {
        self.frames.page_of(session)
    }

    /// A frame of another site went to `url`.
    pub(in crate::pages) fn frame_moved(&self, target: &str, url: &str) {
        self.frames.moved(target, url);
    }

    /// The page closed.
    pub(in crate::pages) fn forget_frames(&self, page: &PageId) {
        self.frames.forget(page);
    }
}

/// What a frame's session said, as captures name the frame (`None`: the page's own session).
pub(super) fn frame_of(captures: &Captures, page: &Page, event: &Event) -> Option<String> {
    let session = event.session.as_deref().filter(|s| *s != page.session)?;
    Some(
        captures
            .frames
            .origin_of(session)
            .unwrap_or_else(|| "a frame of another site".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn followed(page: &PageId, parent: &str) -> Followed {
        Followed {
            page: page.clone(),
            target: format!("T-{parent}"),
            parent: parent.into(),
            origin: origin("http://localhost:1234/a/b?token=x"),
        }
    }

    #[test]
    fn frames_are_known_by_session_and_go_with_their_parents() {
        let (p1, p2) = (PageId::new(), PageId::new());
        let frames = FrameSessions::default();
        lock(&frames.0).insert("child".into(), followed(&p1, "page"));
        lock(&frames.0).insert("grandchild".into(), followed(&p1, "child"));
        lock(&frames.0).insert("other".into(), followed(&p2, "page2"));
        assert_eq!(
            frames.origin_of("child").as_deref(),
            Some("http://localhost:1234")
        );
        let mut sessions = frames.sessions_of(&p1);
        sessions.sort();
        assert_eq!(sessions, ["child", "grandchild"]);
        frames.moved("T-page", "https://example.com:8443/x");
        assert_eq!(
            frames.origin_of("child").as_deref(),
            Some("https://example.com:8443")
        );
        frames.detached("child");
        assert_eq!(frames.page_of("grandchild"), None);
        assert!(frames.page_of("other").is_some());
        frames.forget(&p2);
        assert_eq!(frames.page_of("other"), None);
        assert_eq!(origin("about:blank"), "about:blank");
        assert_eq!(Wanted::NETWORK.without(Wanted::default()), Wanted::NETWORK);
        assert!(!Wanted::NETWORK.without(Wanted::NETWORK).any());
    }
}
