//! The platform web view. macOS embeds a WKWebView (through wry) as a child
//! of GPUI's content NSView; other platforms get a stub whose constructor
//! fails with [`crate::BrowserError::Unsupported`].

#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // objc2 calls into WebKit/AppKit; each block has a SAFETY note.
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::NativeWebView;

#[cfg(not(target_os = "macos"))]
mod stub;
#[cfg(not(target_os = "macos"))]
pub(crate) use stub::NativeWebView;

use crate::address::{SchemePolicy, scheme_policy};
use futures::channel::mpsc::UnboundedSender;
use std::cell::{Cell, RefCell};

/// Something changed in the native view; the owner re-reads its state.
/// Sent from WebKit delegate callbacks, which must never touch GPUI directly.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NativeEvent {
    /// A page load started or finished.
    Load,
    TitleChanged,
    /// `window.open` / `target=_blank`: we navigate in place instead.
    OpenInPlace(String),
    /// The scheme policy denied a navigation (main frame or subframe).
    Blocked(String),
}

/// A point-in-time read of the web view's observable state.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PageState {
    pub url: Option<String>,
    pub title: Option<String>,
    pub loading: bool,
    pub progress: f64,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

/// Which JS world a script runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsWorld {
    /// Isolated "agentloft" world: our helpers, invisible to the page.
    Agent,
    /// The page's own world: user `Eval` scripts see page globals.
    Page,
}

/// Site data the user can clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteData {
    /// Cookies and site storage (signs you out).
    Cookies,
    Cache,
}

#[derive(Debug, Clone)]
pub(crate) struct Screenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Image pixels per CSS pixel (the display's backing scale).
    pub scale: f64,
}

/// Load counters, used to tell when a navigation started and finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoadMark {
    started: u64,
    finished: u64,
    blocked: u64,
}

/// Progress since a [`LoadMark`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoadProgress {
    pub started: bool,
    pub finished: bool,
    pub loading: bool,
    /// The most recent URL the policy blocked since the mark.
    pub blocked: Option<String>,
}

/// Navigation bookkeeping shared with the WebKit delegate closures. Every
/// field is a `Cell`/`RefCell` touched only on the main thread, never across
/// a call back into WebKit, so delegate re-entrancy cannot double-borrow.
pub(crate) struct NavShared {
    events: UnboundedSender<NativeEvent>,
    started: Cell<u64>,
    finished: Cell<u64>,
    blocked_count: Cell<u64>,
    last_blocked: RefCell<Option<String>>,
    /// A `file:` URL the host is about to load. Pages may not navigate to
    /// `file:` themselves.
    file_permit: RefCell<Option<String>>,
}

impl NavShared {
    pub fn new(events: UnboundedSender<NativeEvent>) -> Self {
        Self {
            events,
            started: Cell::new(0),
            finished: Cell::new(0),
            blocked_count: Cell::new(0),
            last_blocked: RefCell::new(None),
            file_permit: RefCell::new(None),
        }
    }

    pub fn send(&self, event: NativeEvent) {
        // The receiver lives as long as the view; a send after it is gone
        // is harmless.
        let _ = self.events.unbounded_send(event);
    }

    /// The navigation policy: `http`/`https`/`about` always; `file` only
    /// for the URL the host permitted; anything else never.
    pub fn allow_navigation(&self, url: &str) -> bool {
        let allowed = match scheme_policy(url) {
            SchemePolicy::Allowed => true,
            SchemePolicy::HostOnly => {
                let mut permit = self.file_permit.borrow_mut();
                let ok = permit.as_deref().is_some_and(|p| same_url(p, url));
                if ok {
                    *permit = None;
                }
                ok
            }
            SchemePolicy::Denied => false,
        };
        if !allowed {
            tracing::info!(%url, "loft-browser: navigation blocked by scheme policy");
            self.blocked_count.set(self.blocked_count.get() + 1);
            *self.last_blocked.borrow_mut() = Some(url.to_owned());
            self.send(NativeEvent::Blocked(url.to_owned()));
        }
        allowed
    }

    /// Lets the next navigation to exactly `url` through if it is `file:`.
    pub fn permit_host_navigation(&self, url: &str) {
        if scheme_policy(url) == SchemePolicy::HostOnly {
            *self.file_permit.borrow_mut() = Some(url.to_owned());
        }
    }

    pub fn on_load_started(&self) {
        self.started.set(self.started.get() + 1);
        self.send(NativeEvent::Load);
    }

    pub fn on_load_finished(&self) {
        self.finished.set(self.finished.get() + 1);
        self.file_permit.borrow_mut().take();
        self.send(NativeEvent::Load);
    }

    pub fn mark(&self) -> LoadMark {
        LoadMark {
            started: self.started.get(),
            finished: self.finished.get(),
            blocked: self.blocked_count.get(),
        }
    }

    pub fn progress(&self, mark: LoadMark, loading: bool) -> LoadProgress {
        LoadProgress {
            started: self.started.get() > mark.started,
            finished: self.finished.get() > mark.finished,
            loading,
            blocked: (self.blocked_count.get() > mark.blocked)
                .then(|| self.last_blocked.borrow().clone())
                .flatten(),
        }
    }
}

/// `file:///a%20b` and `file:///a b` are the same URL.
fn same_url(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_allows_web_and_blocks_others() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let nav = NavShared::new(tx);
        let mark = nav.mark();
        assert!(nav.allow_navigation("https://example.com/"));
        assert!(nav.allow_navigation("about:blank"));
        assert!(!nav.allow_navigation("data:text/html,hi"));
        assert!(!nav.allow_navigation("file:///etc/passwd"));
        let progress = nav.progress(mark, false);
        assert_eq!(progress.blocked.as_deref(), Some("file:///etc/passwd"));
        assert_eq!(
            rx.try_recv().unwrap(),
            NativeEvent::Blocked("data:text/html,hi".into())
        );
    }

    #[test]
    fn file_urls_need_a_one_shot_host_permit() {
        let (tx, _rx) = futures::channel::mpsc::unbounded();
        let nav = NavShared::new(tx);
        nav.permit_host_navigation("file:///tmp/a%20b.html");
        assert!(!nav.allow_navigation("file:///tmp/other.html"));
        assert!(nav.allow_navigation("file:///tmp/a%20b.html"));
        // Consumed: the page cannot reuse it.
        assert!(!nav.allow_navigation("file:///tmp/a%20b.html"));
        // Permits for web URLs are ignored; a finished load clears stale ones.
        nav.permit_host_navigation("https://example.com/");
        nav.permit_host_navigation("file:///tmp/x.html");
        nav.on_load_finished();
        assert!(!nav.allow_navigation("file:///tmp/x.html"));
    }

    #[test]
    fn load_marks_track_progress() {
        let (tx, _rx) = futures::channel::mpsc::unbounded();
        let nav = NavShared::new(tx);
        let mark = nav.mark();
        assert!(!nav.progress(mark, true).started);
        nav.on_load_started();
        let p = nav.progress(mark, true);
        assert!(p.started && !p.finished && p.loading);
        nav.on_load_finished();
        assert!(nav.progress(mark, false).finished);
        assert!(!nav.progress(nav.mark(), false).finished);
    }
}
