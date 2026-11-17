//! JavaScript dialogs (`alert`, `confirm`, `prompt`) on a page an agent is
//! driving: held open for the agent to read and answer (`dialog`), instead
//! of being answered for the page at once as on any other page.
//!
//! - Held only while an agent is driving the page: a command is running, or
//!   one ended less than [`DRIVEN_FOR`] ago. A `beforeunload` question is
//!   always answered (yes: leave), as before.
//! - While one is held the page runs no script and takes no input, so other
//!   commands fail at once saying so, and the command that opened it (a
//!   click, a key, a script) stops waiting and reports it as `dialog`.
//! - Nobody is stuck on it: unanswered for [`DIALOG_WAIT`] it is answered as
//!   it would have been (yes to notices, no to questions), and a person
//!   pressing or typing on the page answers it so too. The agent's next
//!   result says so (`dialog_answered`).

use super::{Browser, Page, lock};
use loft_protocol::PageId;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// How long a held dialog waits for the agent.
pub(super) const DIALOG_WAIT: Duration = Duration::from_secs(30);
/// A page counts as driven this long after an agent's last command.
const DRIVEN_FOR: Duration = Duration::from_secs(30);

/// A dialog the page shows.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Dialog {
    /// `alert`, `confirm`, `prompt` or `beforeunload`.
    pub kind: String,
    pub message: String,
    /// What a prompt offers as its answer.
    pub default: String,
    seq: u64,
}

impl Dialog {
    /// As results show it: `{type, message, default}`.
    pub fn json(&self) -> Value {
        let mut out = json!({ "type": self.kind, "message": self.message });
        if self.kind == "prompt" {
            out["default"] = json!(self.default);
        }
        out
    }

    /// Why a command can't run while it is open.
    pub fn blocking(&self) -> String {
        format!(
            "The page shows a {} dialog and is paused until it is answered: call browser_dialog \
             (accept: true or false). dialog: {}",
            self.kind,
            self.json()
        )
    }

    /// The answer it gets when nobody gives one: yes to notices and to
    /// leaving a page, no to questions.
    fn default_answer(kind: &str) -> bool {
        matches!(kind, "alert" | "beforeunload")
    }
}

/// How a held dialog came to be answered without the agent.
#[derive(Debug, Clone, Copy)]
pub(super) enum Settled {
    TimedOut,
    PersonUsedPage,
}

/// One page's dialogs, as far as its agent goes.
pub(super) struct Dialogs {
    held: watch::Sender<Option<Dialog>>,
    seq: AtomicU64,
    /// Commands running now, and when the last one began or ended.
    driving: AtomicUsize,
    driven_at: Mutex<Option<Instant>>,
    /// What became of a dialog answered without the agent, for its next
    /// result.
    settled: Mutex<Option<Value>>,
}

impl Default for Dialogs {
    fn default() -> Self {
        Self {
            held: watch::channel(None).0,
            seq: AtomicU64::new(0),
            driving: AtomicUsize::new(0),
            driven_at: Mutex::new(None),
            settled: Mutex::new(None),
        }
    }
}

/// Counts a command as driving the page for as long as it lives.
pub(super) struct Driving<'a>(&'a Dialogs);

impl Drop for Driving<'_> {
    fn drop(&mut self) {
        self.0.driving.fetch_sub(1, Ordering::Relaxed);
        *lock(&self.0.driven_at) = Some(Instant::now());
    }
}

impl Dialogs {
    pub fn driving(&self) -> Driving<'_> {
        self.driving.fetch_add(1, Ordering::Relaxed);
        *lock(&self.driven_at) = Some(Instant::now());
        Driving(self)
    }

    fn driven(&self) -> bool {
        self.driving.load(Ordering::Relaxed) > 0
            || lock(&self.driven_at).is_some_and(|at| at.elapsed() < DRIVEN_FOR)
    }

    pub fn held(&self) -> Option<Dialog> {
        self.held.borrow().clone()
    }

    /// Watch for a dialog to be held.
    pub fn watch(&self) -> watch::Receiver<Option<Dialog>> {
        self.held.subscribe()
    }

    /// What became of a dialog answered without the agent, once.
    pub fn take_settled(&self) -> Option<Value> {
        lock(&self.settled).take()
    }

    fn release(&self) -> Option<Dialog> {
        let mut taken = None;
        self.held.send_if_modified(|held| {
            taken = held.take();
            taken.is_some()
        });
        taken
    }
}

/// Resolves with the dialog once one is held (at once if one is).
pub(super) async fn opened(rx: &mut watch::Receiver<Option<Dialog>>) -> Dialog {
    loop {
        if let Some(dialog) = rx.borrow_and_update().clone() {
            return dialog;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl Browser {
    /// `Page.javascriptDialogOpening`: held for the page's agent, else
    /// answered for the page.
    pub(super) async fn on_dialog(self: &Arc<Self>, page: &Arc<Page>, params: &Value) {
        let kind = params["type"].as_str().unwrap_or("alert").to_owned();
        let driver = self.drivers.of(self, &page.id);
        if kind != "beforeunload" && page.view.is_none() && driver.dialogs.driven() {
            let seq = driver.dialogs.seq.fetch_add(1, Ordering::Relaxed) + 1;
            let dialog = Dialog {
                kind,
                message: params["message"].as_str().unwrap_or_default().to_owned(),
                default: params["defaultPrompt"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                seq,
            };
            tracing::debug!(kind = %dialog.kind, "a dialog is held for the page's agent");
            driver.dialogs.held.send_replace(Some(dialog));
            let (browser, page) = (self.clone(), page.clone());
            tokio::spawn(async move {
                tokio::time::sleep(DIALOG_WAIT).await;
                let driver = browser.drivers.of(&browser, &page.id);
                let still = driver.dialogs.held().is_some_and(|d| d.seq == seq);
                if still {
                    browser.settle_dialog(&page, Settled::TimedOut).await;
                }
            });
            return;
        }
        let accept = Dialog::default_answer(&kind);
        let _ = self
            .cdp
            .send(
                "Page.handleJavaScriptDialog",
                json!({ "accept": accept }),
                Some(&page.session),
            )
            .await;
    }

    /// `Page.javascriptDialogClosed`: whatever answered it, it isn't held.
    pub(super) fn dialog_closed(&self, page: &PageId) {
        if let Some(driver) = self.drivers.get(page) {
            driver.dialogs.release();
        }
    }

    /// Whether a dialog is held open on the page for its agent.
    pub(super) fn dialog_held(&self, page: &PageId) -> bool {
        self.drivers
            .get(page)
            .is_some_and(|d| d.dialogs.held().is_some())
    }

    /// Answer a held dialog as nobody's would be, and tell the agent why.
    pub(super) async fn settle_dialog(&self, page: &Arc<Page>, why: Settled) {
        let Some(driver) = self.drivers.get(&page.id) else {
            return;
        };
        let Some(dialog) = driver.dialogs.release() else {
            return;
        };
        let accept = Dialog::default_answer(&dialog.kind);
        let _ = self
            .cdp
            .send(
                "Page.handleJavaScriptDialog",
                json!({ "accept": accept }),
                Some(&page.session),
            )
            .await;
        let because = match why {
            Settled::TimedOut => format!("nobody answered it within {} s", DIALOG_WAIT.as_secs()),
            Settled::PersonUsedPage => "a person used the page".to_owned(),
        };
        *lock(&driver.dialogs.settled) = Some(json!({
            "dialog": dialog.json(),
            "accepted": accept,
            "because": because,
        }));
    }

    /// The agent's answer to the dialog held for it.
    pub(super) async fn answer_dialog(
        &self,
        page: &Page,
        dialogs: &Dialogs,
        accept: bool,
        prompt_text: Option<String>,
    ) -> Result<Dialog, String> {
        let dialog = dialogs.release().ok_or("No dialog is open on the page.")?;
        let mut params = json!({ "accept": accept });
        if let Some(text) = prompt_text.filter(|_| dialog.kind == "prompt" && accept) {
            params["promptText"] = json!(text);
        }
        self.cdp
            .send("Page.handleJavaScriptDialog", params, Some(&page.session))
            .await?;
        Ok(dialog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dialog_reads_well() {
        let dialog = Dialog {
            kind: "prompt".into(),
            message: "Your name?".into(),
            default: "Ada".into(),
            seq: 1,
        };
        assert_eq!(
            dialog.json(),
            json!({ "type": "prompt", "message": "Your name?", "default": "Ada" })
        );
        assert!(dialog.blocking().contains("browser_dialog"));
        assert!(Dialog::default_answer("alert"));
        assert!(!Dialog::default_answer("confirm"));
    }

    #[tokio::test]
    async fn a_held_dialog_is_seen_by_whoever_waits_for_one() {
        let dialogs = Dialogs::default();
        assert!(!dialogs.driven());
        {
            let _driving = dialogs.driving();
            assert!(dialogs.driven());
        }
        // Still counts a moment after.
        assert!(dialogs.driven());
        let mut rx = dialogs.watch();
        let dialog = Dialog {
            kind: "confirm".into(),
            message: "Sure?".into(),
            default: String::new(),
            seq: 1,
        };
        let held = dialog.clone();
        let waiting = tokio::spawn(async move { opened(&mut rx).await });
        dialogs.held.send_replace(Some(held));
        assert_eq!(waiting.await.unwrap(), dialog);
        // And one already held is seen at once.
        let mut rx = dialogs.watch();
        assert_eq!(opened(&mut rx).await, dialog);
        assert_eq!(dialogs.release(), Some(dialog));
        assert_eq!(dialogs.held(), None);
    }
}
