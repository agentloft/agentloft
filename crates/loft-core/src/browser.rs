//! Agents' browser calls (ADR-0017 §5, §9): every `browser_*` tool comes
//! here as a [`BrowserCommand`] for one of the thread's tabs. Commands on a
//! page run in `pages::automation`; the ones about tabs and people (list,
//! open and close tabs, hand one to a person, the browser's status) run here.
//!
//! [`Core::browser_call`] is the one entry point, for the MCP bridge's
//! `BrowserCall` and for anything else in the daemon that runs browser
//! commands for an agent.

use crate::pages::{self, HandoffOutcome, Handover};
use crate::thread::ThreadCmd;
use crate::{Core, CoreResult};
use loft_page_script::{BrowserCommand, BrowserError, TabsOp};
use loft_protocol::{
    BrowserReply, ErrorBody, ErrorCode, HandoffKind, PageId, Response, ThreadId, ThreadStatus,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// How long one `browser_handoff` call waits when not told, and at most.
const HANDOFF_WAIT: Duration = Duration::from_secs(600);
const HANDOFF_WAIT_MAX: Duration = Duration::from_secs(3600);
/// A handoff nobody finishes ends after this long, whoever still waits.
const HANDOFF_LIFETIME: Duration = Duration::from_secs(60 * 60);
/// How often a handoff's supervisor looks whether the agent's turn ended.
const TURN_CHECK: Duration = Duration::from_millis(500);

fn bad(message: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

/// An agent's failed command, as the protocol says it.
fn browser_error(err: BrowserError) -> ErrorBody {
    let code = match err {
        BrowserError::InvalidUrl(_) | BrowserError::Blocked(_) | BrowserError::RefNotFound(_) => {
            ErrorCode::BadRequest
        }
        BrowserError::Unsupported(_) => ErrorCode::Unsupported,
        BrowserError::Closed => ErrorCode::NotFound,
        _ => ErrorCode::Internal,
    };
    ErrorBody::new(code, err.to_string())
}

impl Core {
    /// `Request::BrowserCall`: the MCP bridge's way in.
    pub(crate) async fn browser_request(
        self: &Arc<Self>,
        thread_id: ThreadId,
        command: Value,
        tab: Option<String>,
    ) -> CoreResult<Response> {
        let command: BrowserCommand = serde_json::from_value(command)
            .map_err(|e| bad(format!("invalid browser command: {e}")))?;
        // Scripts (D): a script runs here, and drives the tabs through
        // `browser_call` itself.
        if let BrowserCommand::Execute { script, timeout_ms } = command {
            self.with_store(|s| s.thread(&thread_id))?
                .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "no such thread"))?;
            return self
                .browser_execute(thread_id, script, tab, timeout_ms)
                .await;
        }
        let reply = self
            .browser_call(&thread_id, tab.as_deref(), command)
            .await?;
        Ok(Response::Browser {
            reply: Box::new(reply),
        })
    }

    /// Run an agent's browser command for `thread_id`, in its tab `tab`
    /// (an id, or the name the agent gave it) or, with none, the tab the
    /// thread used last (one is opened if it has none and the command needs
    /// a page). The reply says which tab it ran in, the tabs its pages
    /// opened meanwhile, and anything else the agent should know.
    pub async fn browser_call(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        tab: Option<&str>,
        command: BrowserCommand,
    ) -> CoreResult<BrowserReply> {
        let thread = self
            .with_store(|s| s.thread(thread_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "no such thread"))?;
        let tab = tab.map(str::trim).filter(|t| !t.is_empty());
        match command {
            BrowserCommand::Status => Ok(self.browser_status(thread_id)),
            BrowserCommand::Tabs {
                op,
                name,
                url,
                allowed_origins,
                read_only,
                all,
            } => match op {
                TabsOp::List => Ok(BrowserReply {
                    value: json!({ "tabs": self.pages.tab_list(thread_id) }),
                    tab: None,
                    new_tabs: Vec::new(),
                    notes: Vec::new(),
                }),
                TabsOp::New => {
                    self.open_agent_tab(thread_id, name, url, allowed_origins, read_only)
                        .await
                }
                TabsOp::Close => self.close_agent_tabs(thread_id, tab, all).await,
            },
            BrowserCommand::Handoff {
                reason,
                timeout_secs,
            } => {
                self.hand_over(thread_id, tab, HandoffKind::Handoff, reason, timeout_secs)
                    .await
            }
            BrowserCommand::Demonstrate { what, timeout_secs } => {
                self.hand_over(thread_id, tab, HandoffKind::Demonstrate, what, timeout_secs)
                    .await
            }
            command => {
                let id = self.agent_tab(thread_id, tab, true).await?;
                self.pages.used_by_agent(&id);
                let options = self.drive_options(&thread.cwd);
                let ran = self.pages.drive_with(&id, command, &options).await;
                let (new_tabs, notes) = self.pages.take_news(&id);
                match ran {
                    Ok(value) => Ok(BrowserReply {
                        value,
                        tab: self.pages.brief(&id),
                        new_tabs,
                        notes,
                    }),
                    Err(err) => {
                        let mut err = browser_error(err);
                        for note in notes {
                            err.message.push_str("\n\n");
                            err.message.push_str(&note);
                        }
                        Err(err)
                    }
                }
            }
        }
    }

    /// The thread's tab an agent means: `handle` (an id or a name), else
    /// the one it used last. `open`: a thread with none gets one.
    pub(crate) async fn agent_tab(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        handle: Option<&str>,
        open: bool,
    ) -> CoreResult<PageId> {
        if let Some(handle) = handle {
            return self.pages.find_tab(thread_id, handle).ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::NotFound,
                    format!(
                        "This thread has no browser tab `{handle}`. See its tabs with \
                         browser_tabs; to open one with that name, call browser_tabs with action \
                         `new` and name `{handle}`."
                    ),
                )
            });
        }
        if let Some(id) = self.pages.thread_page(thread_id) {
            return Ok(id);
        }
        if !open {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                "This thread has no browser tab open yet. Open one first, with \
                 browser_navigate or browser_tabs new.",
            ));
        }
        let id = self.open_page(thread_id, pages::Which::Current).await?;
        self.pages.set_agent(&id, |agent| agent.opened = true);
        Ok(id)
    }

    /// A tab of the thread in the daemon's browser (started if need be), at
    /// the size agents' tabs have until a viewer gives them theirs.
    async fn open_page(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        which: pages::Which,
    ) -> CoreResult<PageId> {
        let (profile, executable) = self.browser_setup()?;
        let (width, height, scale) = pages::AGENT_PAGE_SIZE;
        Ok(self
            .pages
            .open(
                thread_id.clone(),
                which,
                None,
                width,
                height,
                scale,
                executable.as_deref(),
                &profile,
                self.bus.clone(),
            )
            .await?
            .id)
    }

    /// `browser_tabs` `new`: another tab for the agent, beside the person's.
    /// A name already open answers with that tab, as it is.
    async fn open_agent_tab(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        name: Option<String>,
        url: Option<String>,
        allowed_origins: Vec<String>,
        read_only: bool,
    ) -> CoreResult<BrowserReply> {
        let name = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
        if let Some(name) = &name {
            if !pages::valid_name(name) {
                return Err(bad(format!(
                    "{name:?} can't name a tab. Use a short lowercase name such as `docs` or \
                     `work-mail`: letters, digits and dashes, starting with a letter, 64 \
                     characters at most."
                )));
            }
            if let Some(id) = self.pages.find_tab(thread_id, name) {
                return Ok(BrowserReply {
                    value: json!({
                        "existed": true,
                        "note": format!("A tab named `{name}` was open already: here it is, left where it was."),
                    }),
                    tab: self.pages.brief(&id),
                    new_tabs: Vec::new(),
                    notes: Vec::new(),
                });
            }
        }
        let allowed_origins = pages::normalize_origins(&allowed_origins).map_err(bad)?;
        let id = self.open_page(thread_id, pages::Which::New).await?;
        self.pages.set_agent(&id, |agent| {
            agent.opened = true;
            agent.name = name.clone();
            agent.allowed_origins = allowed_origins;
        });
        let mut value = json!({ "opened": true });
        let mut notes = Vec::new();
        if let Some(url) = url.filter(|u| !u.trim().is_empty()) {
            let cwd = self
                .with_store(|s| s.thread(thread_id))?
                .map(|thread| thread.cwd)
                .unwrap_or_default();
            let options = self.drive_options(&cwd);
            match self
                .pages
                .drive_with(&id, BrowserCommand::Navigate { url }, &options)
                .await
            {
                Ok(loaded) => value = loaded,
                Err(err) => notes.push(format!(
                    "The tab is open, but loading the address failed: {err}"
                )),
            }
        }
        // Read-only from now on: the address it opened at was the last.
        if read_only {
            self.pages.set_agent(&id, |agent| agent.read_only = true);
        }
        let (new_tabs, more) = self.pages.take_news(&id);
        notes.extend(more);
        Ok(BrowserReply {
            value,
            tab: self.pages.brief(&id),
            new_tabs,
            notes,
        })
    }

    /// `browser_tabs` `close`: the tab named, or every tab the agent
    /// opened.
    async fn close_agent_tabs(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        tab: Option<&str>,
        all: bool,
    ) -> CoreResult<BrowserReply> {
        let mut closing: Vec<PageId> = Vec::new();
        if let Some(handle) = tab {
            closing.push(self.agent_tab(thread_id, Some(handle), false).await?);
        }
        if all {
            closing.extend(
                self.pages
                    .tabs(thread_id)
                    .into_iter()
                    .filter(|t| t.agent.opened)
                    .map(|t| t.id),
            );
        }
        if closing.is_empty() && !all {
            return Err(bad(
                "Name the tab to close (`tab`), or pass `all` to close every tab you opened.",
            ));
        }
        closing.dedup();
        let mut closed: Vec<String> = Vec::new();
        for id in closing {
            let handle = self
                .pages
                .brief(&id)
                .map(|t| t.handle().to_owned())
                .unwrap_or_else(|| id.0.clone());
            if self.pages.close(&id).await.is_ok() {
                closed.push(handle);
            }
        }
        Ok(BrowserReply {
            value: json!({ "closed": closed }),
            tab: None,
            new_tabs: Vec::new(),
            notes: Vec::new(),
        })
    }

    /// `browser_status`: whether the browser can run (and with what), the
    /// thread's tabs, and what waits for a person.
    fn browser_status(self: &Arc<Self>, thread_id: &ThreadId) -> BrowserReply {
        let configured = self.config().browser.executable;
        let running = self.pages.browser_running();
        let mut browser = json!({ "running": running });
        match (
            self.browser_setup(),
            pages::find_browser(configured.as_deref()),
        ) {
            (Err(err), _) => {
                browser["available"] = json!(false);
                browser["problem"] = json!(err.message);
            }
            (Ok(_), Ok(exe)) => {
                browser["available"] = json!(true);
                browser["executable"] = json!(exe.display().to_string());
            }
            (Ok(_), Err(problem)) => {
                browser["available"] = json!(false);
                browser["problem"] = json!(problem);
                browser["next_step"] = json!(if configured.is_some() {
                    "Settings ▸ Browser ▸ Browser in Agentloft names a program that isn't there. \
                     Ask the user to correct it, or to clear it so an installed Chrome is used, \
                     then try again."
                } else {
                    "No Chrome-family browser was found on the computer Agentloft runs on. Ask \
                     the user to install Google Chrome (or Chromium, Microsoft Edge or Brave) \
                     there, or to choose an installed one in Settings ▸ Browser, then try again."
                });
            }
        }
        let tabs = self.pages.tab_list(thread_id);
        let waiting: Vec<Value> = tabs
            .iter()
            .filter_map(|t| {
                let wait = t.get("waiting_for_person")?;
                Some(json!({ "tab": t["tab"], "kind": wait["kind"], "reason": wait["reason"] }))
            })
            .collect();
        BrowserReply {
            value: json!({ "browser": browser, "tabs": tabs, "waiting_for_person": waiting }),
            tab: None,
            new_tabs: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// `browser_handoff` and `browser_demonstrate`: hand the tab to a
    /// person and wait for them, up to `timeout_secs` in this call. Still
    /// waiting then, the card stays and the agent may call again.
    async fn hand_over(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        tab: Option<&str>,
        kind: HandoffKind,
        reason: String,
        timeout_secs: Option<u64>,
    ) -> CoreResult<BrowserReply> {
        let reason = reason.trim().to_owned();
        if reason.is_empty() {
            return Err(bad(match kind {
                HandoffKind::Handoff => "`reason` is required: say what the user should do.",
                HandoffKind::Demonstrate => {
                    "`what` is required: say what the user should show you."
                }
            }));
        }
        let id = self.agent_tab(thread_id, tab, false).await?;
        let (handover, fresh) = self
            .pages
            .hand_over(&id, kind, &reason)
            .map_err(browser_error)?;
        if fresh {
            self.supervise(handover.clone(), reason);
        }
        let wait = timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(HANDOFF_WAIT)
            .clamp(Duration::from_secs(1), HANDOFF_WAIT_MAX);
        let outcome = handover.wait(wait).await;
        if outcome.is_some() {
            handover.told();
        }
        let brief = self.pages.brief(&id);
        let mut value = json!({
            "outcome": outcome.map_or("waiting", HandoffOutcome::as_str),
            "url": brief.as_ref().map(|t| t.url.clone()),
            "title": brief.as_ref().map(|t| t.title.clone()),
        });
        if kind == HandoffKind::Demonstrate {
            value["started_url"] = json!(handover.started_url);
            value["steps"] = json!(handover.steps());
            value["summary"] = json!(handover.summary());
        }
        value["note"] = json!(match outcome {
            None =>
                "The user isn't done yet and the card is still up. Call again with the same tab \
                     to keep waiting.",
            Some(HandoffOutcome::Continued) => match kind {
                HandoffKind::Handoff => {
                    "The user is done. The page has probably changed: read it again before acting."
                }
                HandoffKind::Demonstrate => {
                    "The user has finished showing you. Repeat the steps with the browser tools, \
                     taking refs from a new snapshot."
                }
            },
            Some(HandoffOutcome::Closed) => "The tab closed before the user was done.",
            Some(HandoffOutcome::Stopped) => "Your turn ended before the user was done.",
            Some(HandoffOutcome::Expired) => "Nobody came within an hour, so the card was removed.",
        });
        Ok(BrowserReply {
            value,
            tab: brief,
            new_tabs: Vec::new(),
            notes: Vec::new(),
        })
    }

    /// Mark the thread as waiting for its person while the handoff lasts,
    /// and end it when the agent's turn ends (it was stopped) or nobody
    /// comes for too long.
    fn supervise(self: &Arc<Self>, handover: Arc<Handover>, reason: String) {
        let core = self.clone();
        let thread = handover.thread.clone();
        // Only a turn that was running can end; a call with none running
        // (a client of this computer) waits for the person alone.
        let turn_running = self
            .with_store(|s| s.thread(&thread))
            .ok()
            .flatten()
            .is_some_and(|t| {
                matches!(
                    t.status,
                    ThreadStatus::Running | ThreadStatus::WaitingApproval
                )
            });
        if let Ok(handle) = self.thread_handle(&thread) {
            let _ = handle.send(ThreadCmd::Handover {
                reason: Some(reason),
            });
        }
        tokio::spawn(async move {
            let turn_ended = async {
                loop {
                    tokio::time::sleep(TURN_CHECK).await;
                    if !turn_running {
                        continue;
                    }
                    let status = core
                        .with_store(|s| s.thread(&thread))
                        .ok()
                        .flatten()
                        .map(|t| t.status);
                    if !matches!(
                        status,
                        Some(ThreadStatus::Running | ThreadStatus::WaitingApproval)
                    ) {
                        return;
                    }
                }
            };
            tokio::select! {
                outcome = handover.wait(HANDOFF_LIFETIME) => {
                    if outcome.is_none() {
                        core.pages.end_handover(&handover.tab, HandoffOutcome::Expired);
                    }
                }
                () = turn_ended => {
                    core.pages.end_handover(&handover.tab, HandoffOutcome::Stopped);
                }
            }
            tracing::info!(
                page = %handover.tab.0,
                outcome = handover.outcome().map_or("?", HandoffOutcome::as_str),
                "a handoff ended"
            );
            if let Ok(handle) = core.thread_handle(&thread) {
                let _ = handle.send(ThreadCmd::Handover { reason: None });
            }
        });
    }
}
