//! UI-side state: the daemon connection, the mirrored workspace, and one
//! [`ThreadState`] entity per open thread (ADR-0010: client I/O on tokio,
//! state applied on the GPUI main thread).

use crate::panel::terminal::TerminalPane;
use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::component::text::TextViewState;
use gpui_kit::{App, AppContext as _, AsyncApp, Context, Entity, WeakEntity};
use loft_client::model::{ThreadModel, WorkspaceModel};
use loft_client::{Client, ClientEvent};
use loft_config::Paths;
use loft_protocol::{
    Attachment, ClientKind, DecisionKind, Delta, ErrorBody, Event, HarnessId, ItemBody, ItemId,
    ItemStatus, ProjectId, Request, RequestId, Response, Snapshot, TerminalId, ThreadId,
    ThreadStatus, Topic,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

pub const BUILD: &str = env!("CARGO_PKG_VERSION");

/// A message typed while a turn was running.
#[derive(Debug, Clone)]
pub struct Queued {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

/// App-wide UI events (e.g. a timeline row asking the panel to open a file).
#[derive(Debug, Clone)]
pub enum StoreEvent {
    OpenFile {
        thread_id: ThreadId,
        rel_path: String,
    },
    OpenAgent {
        thread_id: ThreadId,
        item_id: ItemId,
    },
    /// An agent wants to drive the browser panel (answer via `respond_call`).
    BrowserCall {
        call_id: u64,
        command: serde_json::Value,
    },
    /// Show `url` in the side-panel browser.
    Browse { url: String },
    /// A transient message for the user (toast).
    Notify { message: String, error: bool },
    /// Archiving needs confirmation (e.g. the worktree is dirty).
    ConfirmArchive { thread_id: ThreadId, reason: String },
}

impl gpui_kit::EventEmitter<StoreEvent> for AppStore {}

/// Issue a daemon request from any view entity and handle the result there.
pub fn request_in<T: 'static>(
    store: &Entity<AppStore>,
    body: Request,
    cx: &mut Context<T>,
    then: impl FnOnce(&mut T, Result<Response, ErrorBody>, &mut Context<T>) + 'static,
) {
    let fut: std::pin::Pin<Box<dyn Future<Output = Result<Response, ErrorBody>>>> =
        match store.read(cx).client.clone() {
            Some(client) => Box::pin(client.request(body)),
            None => Box::pin(async {
                Err(ErrorBody::new(
                    loft_protocol::ErrorCode::Internal,
                    "not connected to agentloftd",
                ))
            }),
        };
    cx.spawn(async move |this: WeakEntity<T>, cx: &mut AsyncApp| {
        let result = fut.await;
        let _ = this.update(cx, |this, cx| then(this, result, cx));
    })
    .detach();
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected(String),
}

pub struct AppStore {
    runtime: tokio::runtime::Handle,
    paths: Paths,
    client: Option<Client>,
    pub conn: ConnState,
    pub workspace: WorkspaceModel,
    ws_sub: Option<u64>,
    /// Open thread views (subscribed).
    pub threads: HashMap<ThreadId, Entity<ThreadState>>,
    subs: HashMap<u64, ThreadId>,
    pub selected: Option<ThreadId>,
    /// Harness for new threads (the last one the user picked).
    pub default_harness: HarnessId,
    /// Messages typed while a turn was running, sent when it ends.
    pub queued: HashMap<ThreadId, std::collections::VecDeque<Queued>>,
    /// Last request error, shown in the UI.
    pub last_error: Option<String>,
    /// Text to prefill a thread's composer with (a fork's handoff digest).
    pub drafts: HashMap<ThreadId, String>,
    /// Threads that finished a turn while not selected (sidebar dot).
    pub unread: std::collections::HashSet<ThreadId>,
    /// Model pickers' options per harness (`ListModels`).
    pub models: HashMap<HarnessId, Vec<loft_protocol::ModelInfo>>,
    /// Installed CLIs (`ProbeHarness`).
    pub harness_status: HashMap<HarnessId, loft_protocol::HarnessStatus>,
    /// Each project's branches (`ListBranches`), for worktree bases.
    pub branches: HashMap<ProjectId, loft_protocol::BranchList>,
    branches_loading: std::collections::HashSet<ProjectId>,
    /// Agents switched off in Settings (hidden from pickers).
    pub disabled_agents: std::collections::HashSet<String>,
    /// Models left out of the pickers, per agent.
    pub hidden_models: HashMap<String, Vec<String>>,
    /// Terminal panes fed by `Topic::Terminal` subscriptions.
    terminals: HashMap<TerminalId, WeakEntity<TerminalPane>>,
    terminal_subs: HashMap<u64, TerminalId>,
    /// Daemon pid of the current connection: a new pid means the daemon
    /// restarted and its PTYs are gone.
    daemon_pid: Option<u32>,
}

impl AppStore {
    pub fn new(runtime: tokio::runtime::Handle, paths: Paths, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            runtime,
            paths,
            client: None,
            conn: ConnState::Connecting,
            workspace: WorkspaceModel::default(),
            ws_sub: None,
            threads: HashMap::new(),
            subs: HashMap::new(),
            selected: None,
            default_harness: HarnessId::claude(),
            queued: HashMap::new(),
            last_error: None,
            drafts: HashMap::new(),
            unread: Default::default(),
            models: HashMap::new(),
            harness_status: HashMap::new(),
            branches: HashMap::new(),
            branches_loading: Default::default(),
            disabled_agents: Default::default(),
            hidden_models: HashMap::new(),
            terminals: HashMap::new(),
            terminal_subs: HashMap::new(),
            daemon_pid: None,
        };
        this.reload_agent_prefs(cx);
        this.connect(cx);
        this
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.conn = ConnState::Connecting;
        cx.notify();
        let paths = self.paths.clone();
        let task = self.runtime.spawn(async move {
            loft_client::connect_or_spawn(
                &paths,
                &loft_client::daemon_binary(),
                ClientKind::LocalUi,
                BUILD,
                &["browser"],
            )
            .await
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = match task.await {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!(e)),
            };
            match result {
                Ok((client, rx)) => {
                    let ok = this
                        .update(cx, |store, cx| store.on_connected(client, cx))
                        .is_ok();
                    if ok {
                        pump(this, rx, cx).await;
                    }
                }
                Err(err) => {
                    tracing::error!("connect failed: {err:#}");
                    let _ = this.update(cx, |store, cx| {
                        store.conn = ConnState::Disconnected(format!("{err:#}"));
                        cx.notify();
                    });
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    let _ = this.update(cx, |store, cx| store.connect(cx));
                }
            }
        })
        .detach();
    }

    fn on_connected(&mut self, client: Client, cx: &mut Context<Self>) {
        tracing::info!(daemon = %client.welcome().daemon_build, pid = client.welcome().daemon_pid, "connected");
        self.conn = ConnState::Connected;
        self.subs.clear();
        self.ws_sub = Some(client.subscribe(Topic::Workspace));
        for id in self.threads.keys() {
            let sub = client.subscribe(Topic::Thread(id.clone()));
            self.subs.insert(sub, id.clone());
        }
        let pid = client.welcome().daemon_pid;
        let restarted = self.daemon_pid.is_some_and(|old| old != pid);
        self.daemon_pid = Some(pid);
        self.terminal_subs.clear();
        self.terminals.retain(|_, pane| pane.upgrade().is_some());
        let terminals = std::mem::take(&mut self.terminals);
        self.client = Some(client);
        self.probe_harnesses(cx);
        // Model pickers (Codex asks its CLI; answers are cached daemon-side).
        for harness in [
            HarnessId::CLAUDE,
            HarnessId::CODEX,
            HarnessId::OPENCODE,
            HarnessId::GEMINI,
        ] {
            self.fetch_models(HarnessId(harness.into()), cx);
        }
        for (id, pane) in terminals {
            if restarted {
                // The old PTY died with the daemon; the pane opens a new one.
                let _ = pane.update(cx, |p, cx| p.on_lost(cx));
            } else {
                self.subscribe_terminal(id, pane, cx);
            }
        }
        cx.notify();
    }

    /// Fetch `harness`'s models unless they're already here.
    pub fn ensure_models(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if !self.models.get(&harness).is_some_and(|m| !m.is_empty()) {
            self.fetch_models(harness, cx);
        }
    }

    fn fetch_models(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let body = Request::ListModels {
            harness: harness.clone(),
        };
        self.request_result(body, cx, move |store, result, cx| {
            if let Ok(Response::Models { models }) = result {
                store.models.insert(harness, models);
                cx.notify();
            }
        });
    }

    /// Agents to offer: Claude Code and Codex always, ACP agents (OpenCode,
    /// Gemini) once their CLI is found.
    /// Re-read the per-agent picker settings (on/off, hidden models).
    pub fn reload_agent_prefs(&mut self, cx: &mut Context<Self>) {
        let Ok(config) = loft_config::Config::load(&self.paths) else {
            return;
        };
        self.disabled_agents.clear();
        self.hidden_models.clear();
        for (id, h) in [
            (HarnessId::CLAUDE, &config.harness.claude),
            (HarnessId::CODEX, &config.harness.codex),
            (HarnessId::OPENCODE, &config.harness.opencode),
            (HarnessId::GEMINI, &config.harness.gemini),
        ] {
            if !h.enabled {
                self.disabled_agents.insert(id.to_owned());
            }
            if !h.hidden_models.is_empty() {
                self.hidden_models
                    .insert(id.to_owned(), h.hidden_models.clone());
            }
        }
        cx.notify();
    }

    pub fn available_harnesses(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = vec![
            (HarnessId::CLAUDE, "Claude Code"),
            (HarnessId::CODEX, "Codex"),
        ];
        for (id, name) in [
            (HarnessId::OPENCODE, "OpenCode"),
            (HarnessId::GEMINI, "Gemini"),
        ] {
            if self
                .harness_status
                .get(&HarnessId(id.into()))
                .is_some_and(|s| s.installed)
            {
                out.push((id, name));
            }
        }
        out.retain(|(id, _)| !self.disabled_agents.contains(*id));
        out
    }

    /// Refresh which agent CLIs are installed (also after settings change).
    pub fn probe_harnesses(&mut self, cx: &mut Context<Self>) {
        for harness in [
            HarnessId::CLAUDE,
            HarnessId::CODEX,
            HarnessId::OPENCODE,
            HarnessId::GEMINI,
        ]
        .map(|h| HarnessId(h.into()))
        {
            let body = Request::ProbeHarness {
                harness: harness.clone(),
            };
            self.request_result(body, cx, move |store, result, cx| {
                if let Ok(Response::HarnessStatus { status }) = result {
                    store.harness_status.insert(harness, status);
                    cx.notify();
                }
            });
        }
    }

    /// Route a terminal's replay/output/exit to `pane`.
    pub fn subscribe_terminal(
        &mut self,
        id: TerminalId,
        pane: WeakEntity<TerminalPane>,
        _cx: &mut Context<Self>,
    ) {
        self.terminal_subs.retain(|_, t| *t != id);
        if let Some(client) = &self.client {
            let sub = client.subscribe(Topic::Terminal(id.clone()));
            self.terminal_subs.insert(sub, id.clone());
        }
        self.terminals.insert(id, pane);
    }

    pub fn forget_terminal(&mut self, id: &TerminalId) {
        self.terminals.remove(id);
        let subs: Vec<u64> = self
            .terminal_subs
            .iter()
            .filter(|(_, t)| *t == id)
            .map(|(s, _)| *s)
            .collect();
        for sub in subs {
            self.terminal_subs.remove(&sub);
            if let Some(client) = &self.client {
                client.unsubscribe(sub);
            }
        }
    }

    fn terminal_pane(&self, sub_id: u64) -> Option<WeakEntity<TerminalPane>> {
        let id = self.terminal_subs.get(&sub_id)?;
        self.terminals.get(id).cloned()
    }

    fn on_disconnected(&mut self, cx: &mut Context<Self>) {
        self.client = None;
        self.conn = ConnState::Disconnected("connection to agentloftd lost".into());
        cx.notify();
        // Reconnect (respawning the daemon if it died).
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |store, cx| store.connect(cx));
        })
        .detach();
    }

    fn on_client_event(&mut self, event: ClientEvent, cx: &mut Context<Self>) {
        match event {
            ClientEvent::Snapshot { sub_id, snapshot } => {
                if let Snapshot::Terminal {
                    terminal, replay, ..
                } = &snapshot
                {
                    if let Some(pane) = self.terminal_pane(sub_id) {
                        let _ =
                            pane.update(cx, |p, cx| p.on_replay(&replay.0, terminal.exited, cx));
                    }
                } else if Some(sub_id) == self.ws_sub {
                    self.workspace.apply_snapshot(snapshot);
                    self.refresh_badge();
                    // Reopen the most recently active thread on launch.
                    if self.selected.is_none()
                        && let Some(first) = self
                            .workspace
                            .threads
                            .iter()
                            .find(|t| t.status != loft_protocol::ThreadStatus::Archived)
                    {
                        let id = first.id.clone();
                        self.select_thread(id, cx);
                    }
                    cx.notify();
                } else if let Some(id) = self.subs.get(&sub_id)
                    && let Some(entity) = self.threads.get(id)
                {
                    entity.update(cx, |t, cx| t.apply_snapshot(snapshot, cx));
                }
            }
            ClientEvent::Event { sub_id, event } => {
                if Some(sub_id) == self.ws_sub {
                    let before = match &event.event {
                        Event::ThreadUpdated { thread } => self
                            .workspace
                            .threads
                            .iter()
                            .find(|t| t.id == thread.id)
                            .map(|t| t.status),
                        _ => None,
                    };
                    let had_session = match &event.event {
                        Event::ThreadUpdated { thread } => self
                            .workspace
                            .threads
                            .iter()
                            .find(|t| t.id == thread.id)
                            .is_some_and(|t| t.native_session_id.is_some()),
                        _ => true,
                    };
                    if self.workspace.apply_event(&event) {
                        if let Event::ThreadUpdated { thread } = &event.event {
                            self.on_status_change(&thread.id, before, thread.status, cx);
                            // A first session may have told the daemon which
                            // models this agent offers (ACP).
                            let session_started =
                                !had_session && thread.native_session_id.is_some();
                            let turn_ended = before == Some(loft_protocol::ThreadStatus::Running)
                                && thread.status != loft_protocol::ThreadStatus::Running;
                            if (session_started || turn_ended)
                                && self.models.get(&thread.harness).is_none_or(Vec::is_empty)
                            {
                                self.fetch_models(thread.harness.clone(), cx);
                            }
                        }
                        cx.notify();
                    }
                } else if let Some(id) = self.subs.get(&sub_id).cloned()
                    && let Some(entity) = self.threads.get(&id).cloned()
                {
                    entity.update(cx, |t, cx| t.apply_event(&event, cx));
                    self.flush_queue(&id, &entity, cx);
                }
            }
            ClientEvent::Delta { sub_id, delta } => {
                if let Some(id) = self.subs.get(&sub_id)
                    && let Some(entity) = self.threads.get(id)
                {
                    entity.update(cx, |t, cx| t.apply_delta(&delta, cx));
                }
            }
            ClientEvent::TerminalOutput { sub_id, data, .. } => {
                if let Some(pane) = self.terminal_pane(sub_id) {
                    let _ = pane.update(cx, |p, cx| p.on_output(&data, cx));
                }
            }
            ClientEvent::TerminalExited { sub_id, code, .. } => {
                if let Some(pane) = self.terminal_pane(sub_id) {
                    let _ = pane.update(cx, |p, cx| p.on_exit(code, cx));
                }
            }
            ClientEvent::ClientCall { call_id, call } => match call {
                loft_protocol::ClientCall::Browser { command, .. } => {
                    cx.emit(StoreEvent::BrowserCall { call_id, command });
                }
            },
            ClientEvent::Fatal(err) => {
                self.last_error = Some(err.message);
                cx.notify();
            }
            ClientEvent::Disconnected => self.on_disconnected(cx),
        }
    }

    /// Unread dots, the Dock badge and bounce for thread status changes.
    fn on_status_change(
        &mut self,
        id: &ThreadId,
        before: Option<loft_protocol::ThreadStatus>,
        after: loft_protocol::ThreadStatus,
        cx: &mut Context<Self>,
    ) {
        use loft_protocol::ThreadStatus::*;
        let finished =
            matches!(before, Some(Running | WaitingApproval)) && matches!(after, Idle | Error);
        let needs_you = after == WaitingApproval && before != Some(WaitingApproval);
        if finished && self.selected.as_ref() != Some(id) {
            self.unread.insert(id.clone());
        }
        if finished || needs_you {
            let thread = self.workspace.threads.iter().find(|t| &t.id == id);
            let title = thread.map(|t| t.title.clone()).unwrap_or_default();
            let agent = thread
                .map(|t| crate::workspace::harness_name(&t.harness))
                .unwrap_or("The agent");
            let headline = if needs_you {
                format!("{agent} needs your approval")
            } else if after == Error {
                format!("{agent} stopped with an error")
            } else {
                format!("{agent} finished")
            };
            crate::attention::request(cx, &headline, &title, id.as_str());
        }
        self.refresh_badge();
    }

    /// Dock badge: threads waiting for approval.
    fn refresh_badge(&self) {
        let waiting = self
            .workspace
            .threads
            .iter()
            .filter(|t| t.status == loft_protocol::ThreadStatus::WaitingApproval)
            .count();
        crate::attention::set_badge(waiting);
    }

    // -- actions ------------------------------------------------------------

    pub fn select_thread(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        if !self.threads.contains_key(&id) {
            let entity = cx.new(|cx| ThreadState::new(id.clone(), cx));
            self.threads.insert(id.clone(), entity);
            if let Some(client) = &self.client {
                let sub = client.subscribe(Topic::Thread(id.clone()));
                self.subs.insert(sub, id.clone());
            }
        }
        self.unread.remove(&id);
        self.selected = Some(id);
        cx.notify();
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn client(&self) -> Option<Client> {
        self.client.clone()
    }

    pub fn open_file(&mut self, thread_id: ThreadId, rel_path: String, cx: &mut Context<Self>) {
        cx.emit(StoreEvent::OpenFile {
            thread_id,
            rel_path,
        });
    }

    pub fn open_agent(&mut self, thread_id: ThreadId, item_id: ItemId, cx: &mut Context<Self>) {
        cx.emit(StoreEvent::OpenAgent { thread_id, item_id });
    }

    pub fn selected_thread(&self) -> Option<Entity<ThreadState>> {
        self.selected
            .as_ref()
            .and_then(|id| self.threads.get(id).cloned())
    }

    /// Fire a request; errors are surfaced in `last_error`.
    pub fn request(
        &mut self,
        body: Request,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Response, &mut Context<Self>) + 'static,
    ) {
        let Some(client) = self.client.clone() else {
            self.last_error = Some("not connected to agentloftd".into());
            cx.notify();
            return;
        };
        self.last_error = None;
        let fut = client.request(body);
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result: Result<Response, ErrorBody> = fut.await;
            let _ = this.update(cx, |store, cx| match result {
                Ok(resp) => then(store, resp, cx),
                Err(err) => {
                    store.last_error = Some(err.message);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn add_project(&mut self, path: String, cx: &mut Context<Self>) {
        self.request(Request::AddProject { path, name: None }, cx, |_, _, _| {});
    }

    /// Open a new thread in `project_id`. It isn't really started until the
    /// first message, so an untouched one (no messages, no draft) is reused
    /// rather than piling up empty threads.
    pub fn new_thread(&mut self, project_id: ProjectId, cx: &mut Context<Self>) {
        let blank = self
            .workspace
            .threads
            .iter()
            .find(|t| {
                t.project_id == project_id
                    && t.status != ThreadStatus::Archived
                    && t.parent.is_none()
                    && !t.is_started()
                    && t.draft_text().is_none()
            })
            .map(|t| t.id.clone());
        if let Some(id) = blank {
            self.select_thread(id, cx);
            return;
        }
        self.request(
            Request::CreateThread {
                project_id,
                harness: self.default_harness.clone(),
                title: None,
            },
            cx,
            |store, resp, cx| {
                if let Response::Thread { thread } = resp {
                    store.select_thread(thread.id, cx);
                }
            },
        );
    }

    /// Switch an unstarted thread to another harness (and remember it).
    pub fn set_thread_harness(
        &mut self,
        thread_id: ThreadId,
        harness: HarnessId,
        cx: &mut Context<Self>,
    ) {
        self.default_harness = harness.clone();
        self.request(
            Request::SetThreadHarness { thread_id, harness },
            cx,
            |_, _, _| {},
        );
    }

    /// Like [`Self::request`], but the callback also gets errors.
    fn request_result(
        &mut self,
        body: Request,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<Response, ErrorBody>, &mut Context<Self>) + 'static,
    ) {
        let fut: std::pin::Pin<Box<dyn Future<Output = Result<Response, ErrorBody>>>> =
            match self.client.clone() {
                Some(client) => Box::pin(client.request(body)),
                None => Box::pin(async {
                    Err(ErrorBody::new(
                        loft_protocol::ErrorCode::Internal,
                        "not connected to agentloftd",
                    ))
                }),
            };
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = fut.await;
            let _ = this.update(cx, |store, cx| then(store, result, cx));
        })
        .detach();
    }

    fn notify_result(&mut self, result: Result<String, ErrorBody>, cx: &mut Context<Self>) {
        let (message, error) = match result {
            Ok(message) => (message, false),
            Err(err) => (err.message, true),
        };
        cx.emit(StoreEvent::Notify { message, error });
    }

    /// Refresh a project's branch list.
    pub fn fetch_branches(&mut self, project_id: ProjectId, cx: &mut Context<Self>) {
        if !self.branches_loading.insert(project_id.clone()) {
            return;
        }
        let body = Request::ListBranches {
            project_id: project_id.clone(),
        };
        self.request_result(body, cx, move |store, result, cx| {
            store.branches_loading.remove(&project_id);
            if let Ok(Response::Branches { branches }) = result {
                store.branches.insert(project_id, branches);
                cx.notify();
            }
        });
    }

    /// Load a project's branches once.
    pub fn ensure_branches(&mut self, project_id: &ProjectId, cx: &mut Context<Self>) {
        if !self.branches.contains_key(project_id) {
            self.fetch_branches(project_id.clone(), cx);
        }
    }

    /// Project checkout or a new worktree (before the first message),
    /// starting from `base` (`None` = the main branch).
    pub fn set_thread_workspace(
        &mut self,
        thread_id: ThreadId,
        worktree: bool,
        base: Option<loft_protocol::WorktreeBase>,
        cx: &mut Context<Self>,
    ) {
        let body = Request::SetThreadWorkspace {
            thread_id,
            worktree,
            base,
        };
        self.request_result(body, cx, |store, result, cx| {
            if let Err(err) = result {
                store.notify_result(Err(err), cx);
            }
        });
    }

    /// Squash-merge a worktree thread back into its base branch.
    pub fn merge_thread(&mut self, thread_id: ThreadId, cx: &mut Context<Self>) {
        let body = Request::MergeThread {
            thread_id,
            message: None,
        };
        self.request_result(body, cx, |store, result, cx| {
            let result = result.map(|r| match r {
                Response::Text { text } => text,
                _ => "Merged".into(),
            });
            store.notify_result(result, cx);
        });
    }

    /// Fork into `harness` (ADR-0011) and open the fork; a cross-provider
    /// fork's handoff digest lands in its composer for review.
    /// `at_turn` forks from the end of that turn instead of the latest.
    pub fn fork_thread(
        &mut self,
        thread_id: ThreadId,
        harness: HarnessId,
        at_turn: Option<loft_protocol::TurnId>,
        cx: &mut Context<Self>,
    ) {
        let body = Request::ForkThread {
            thread_id,
            harness: Some(harness),
            share_workspace: false,
            at_turn,
        };
        self.request_result(body, cx, |store, result, cx| match result {
            Ok(Response::Forked { thread, draft }) => {
                if let Some(draft) = draft {
                    store.drafts.insert(thread.id.clone(), draft);
                }
                store.select_thread(thread.id, cx);
            }
            Ok(_) => {}
            Err(err) => store.notify_result(Err(err), cx),
        });
    }

    pub fn rename_thread(&mut self, thread_id: ThreadId, title: String, cx: &mut Context<Self>) {
        let body = Request::RenameThread { thread_id, title };
        self.request_result(body, cx, |store, result, cx| {
            if let Err(err) = result {
                store.notify_result(Err(err), cx);
            }
        });
    }

    pub fn unarchive_thread(&mut self, thread_id: ThreadId, cx: &mut Context<Self>) {
        let body = Request::UnarchiveThread {
            thread_id: thread_id.clone(),
        };
        self.request_result(body, cx, move |store, result, cx| match result {
            Ok(_) => store.select_thread(thread_id, cx),
            Err(err) => store.notify_result(Err(err), cx),
        });
    }

    /// Restore the thread's files to before `turn_id`.
    pub fn revert_turn(
        &mut self,
        thread_id: ThreadId,
        turn_id: loft_protocol::TurnId,
        cx: &mut Context<Self>,
    ) {
        let body = Request::RevertTurn { thread_id, turn_id };
        self.request_result(body, cx, |store, result, cx| {
            let result = result.map(|r| match r {
                Response::Text { text } => text,
                _ => "Files restored".into(),
            });
            store.notify_result(result, cx);
        });
    }

    /// Push the worktree branch and open a PR with `gh`; opens it in the
    /// browser when done.
    pub fn create_pull_request(&mut self, thread_id: ThreadId, cx: &mut Context<Self>) {
        cx.emit(StoreEvent::Notify {
            message: "Pushing and opening a pull request…".into(),
            error: false,
        });
        self.request_result(
            Request::CreatePullRequest { thread_id },
            cx,
            |store, result, cx| match result {
                Ok(Response::Text { text: url }) => {
                    cx.open_url(&url);
                    store.notify_result(Ok(format!("Pull request: {url}")), cx);
                }
                Ok(_) => {}
                Err(err) => store.notify_result(Err(err), cx),
            },
        );
    }

    pub fn archive_thread(&mut self, thread_id: ThreadId, force: bool, cx: &mut Context<Self>) {
        let body = Request::ArchiveThread {
            thread_id: thread_id.clone(),
            force,
        };
        self.request_result(body, cx, move |store, result, cx| match result {
            Ok(_) => {
                store.on_archived(&thread_id, cx);
                store.notify_result(Ok("Thread archived".into()), cx);
            }
            Err(err) if err.code == loft_protocol::ErrorCode::Conflict && !force => {
                cx.emit(StoreEvent::ConfirmArchive {
                    thread_id,
                    reason: err.message,
                });
            }
            Err(err) => store.notify_result(Err(err), cx),
        });
    }

    /// Delete a thread for good (after the user confirmed).
    pub fn delete_thread(&mut self, thread_id: ThreadId, cx: &mut Context<Self>) {
        let body = Request::DeleteThread {
            thread_id: thread_id.clone(),
        };
        self.request_result(body, cx, move |store, result, cx| match result {
            Ok(_) => {
                store.on_archived(&thread_id, cx);
                store.threads.remove(&thread_id);
                store.unread.remove(&thread_id);
                store.drafts.remove(&thread_id);
                store.notify_result(Ok("Thread deleted".into()), cx);
            }
            Err(err) => store.notify_result(Err(err), cx),
        });
    }

    /// Move the selection off an archived (or deleted) thread.
    fn on_archived(&mut self, thread_id: &ThreadId, cx: &mut Context<Self>) {
        if self.selected.as_ref() != Some(thread_id) {
            return;
        }
        let next = self
            .workspace
            .threads
            .iter()
            .find(|t| &t.id != thread_id && t.status != loft_protocol::ThreadStatus::Archived)
            .map(|t| t.id.clone());
        self.selected = None;
        if let Some(id) = next {
            self.select_thread(id, cx);
        }
        cx.notify();
    }

    /// Queue a message for when the current turn ends.
    pub fn enqueue(&mut self, thread_id: ThreadId, message: Queued, cx: &mut Context<Self>) {
        self.queued.entry(thread_id).or_default().push_back(message);
        cx.notify();
    }

    pub fn dequeue(&mut self, thread_id: &ThreadId, index: usize, cx: &mut Context<Self>) {
        if let Some(q) = self.queued.get_mut(thread_id) {
            q.remove(index);
        }
        cx.notify();
    }

    /// Send the next queued message once the thread is idle.
    fn flush_queue(&mut self, id: &ThreadId, thread: &Entity<ThreadState>, cx: &mut Context<Self>) {
        let idle = thread.read(cx).model.as_ref().is_some_and(|m| {
            m.running_turn().is_none() && m.thread.status != loft_protocol::ThreadStatus::Running
        });
        if !idle {
            return;
        }
        let Some(next) = self.queued.get_mut(id).and_then(|q| q.pop_front()) else {
            return;
        };
        self.send_message(id.clone(), next.text, next.attachments, cx);
        cx.notify();
    }

    pub fn send_message(
        &mut self,
        thread_id: ThreadId,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut Context<Self>,
    ) {
        let body = Request::SendMessage {
            thread_id,
            text,
            attachments,
        };
        self.request(body, cx, |_, _, _| {});
    }

    pub fn set_permission_mode(
        &mut self,
        thread_id: ThreadId,
        mode: Option<loft_protocol::PermissionMode>,
        cx: &mut Context<Self>,
    ) {
        self.request(
            Request::SetPermissionMode { thread_id, mode },
            cx,
            |_, _, _| {},
        );
    }

    pub fn set_model(
        &mut self,
        thread_id: ThreadId,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.request(Request::SetModel { thread_id, model }, cx, |_, _, _| {});
    }

    pub fn set_effort(
        &mut self,
        thread_id: ThreadId,
        effort: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.request(Request::SetEffort { thread_id, effort }, cx, |_, _, _| {});
    }

    /// Save a thread's unsent composer text (empty clears it).
    pub fn set_draft(&mut self, thread_id: ThreadId, text: String, cx: &mut Context<Self>) {
        self.request(Request::SetDraft { thread_id, text }, cx, |_, _, _| {});
    }

    pub fn interrupt(&mut self, thread_id: ThreadId, cx: &mut Context<Self>) {
        self.request(Request::Interrupt { thread_id }, cx, |_, _, _| {});
    }

    pub fn respond(
        &mut self,
        thread_id: ThreadId,
        request_id: RequestId,
        decision: DecisionKind,
        cx: &mut Context<Self>,
    ) {
        self.request(
            Request::RespondRequest {
                thread_id,
                request_id,
                decision,
            },
            cx,
            |_, _, _| {},
        );
    }
}

/// Drain client events into the store until the connection ends.
async fn pump(
    this: WeakEntity<AppStore>,
    mut rx: UnboundedReceiver<ClientEvent>,
    cx: &mut AsyncApp,
) {
    while let Some(event) = rx.recv().await {
        let disconnected = matches!(event, ClientEvent::Disconnected);
        if this
            .update(cx, |store, cx| store.on_client_event(event, cx))
            .is_err()
        {
            return;
        }
        if disconnected {
            return;
        }
    }
}

/// A display row in the timeline. Consecutive tool calls fold into one
/// group row; each finished turn gets a footer row.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Item(usize),
    /// A run of top-level tool calls (item indices).
    ToolGroup {
        members: Vec<usize>,
    },
    /// A turn's work (thinking, tools, interim notes) under one header:
    /// "Working for 12s" while it runs, "Worked for 1m 58s ›" after.
    Work {
        turn_ix: usize,
        open: bool,
        steps: usize,
    },
    /// The agent is between steps (or hasn't answered yet).
    Pending,
    TurnFooter {
        turn_ix: usize,
    },
}

/// One open thread: its mirrored model plus view-side state (display rows,
/// scroll list, markdown documents, expansion state).
pub struct ThreadState {
    pub id: ThreadId,
    pub model: Option<ThreadModel>,
    pub rows: Vec<Row>,
    pub scroller: Entity<MessageScrollerState>,
    pub markdown: HashMap<ItemId, Entity<TextViewState>>,
    /// Tool groups / reasoning items the user expanded or collapsed,
    /// keyed by their first item id. `true` = expanded.
    pub expanded: HashMap<ItemId, bool>,
    /// Repaints while a turn runs (its elapsed time).
    ticker: Option<gpui_kit::Task<()>>,
}

impl ThreadState {
    fn new(id: ThreadId, cx: &mut Context<Self>) -> Self {
        Self {
            id,
            model: None,
            rows: Vec::new(),
            scroller: cx.new(|cx| MessageScrollerState::new(0, cx)),
            markdown: HashMap::new(),
            expanded: HashMap::new(),
            ticker: None,
        }
    }

    pub fn toggle(&mut self, key: ItemId, default_open: bool, cx: &mut Context<Self>) {
        let open = self.expanded.get(&key).copied().unwrap_or(default_open);
        self.expanded.insert(key.clone(), !open);
        if let Some(row) = self.row_of_item(&key) {
            self.scroller.update(cx, |s, cx| {
                s.remeasure_items(row..row + 1, cx);
            });
        }
        cx.notify();
    }

    pub fn is_expanded(&self, key: &ItemId, default_open: bool) -> bool {
        self.expanded.get(key).copied().unwrap_or(default_open)
    }

    fn row_of_item(&self, id: &ItemId) -> Option<usize> {
        let model = self.model.as_ref()?;
        // Group and fold keys are an item's id, prefixed.
        let id =
            id.0.strip_prefix("group:")
                .or_else(|| id.0.strip_prefix("fold:"))
                .unwrap_or(&id.0);
        let ix = model.items.iter().position(|i| i.id.0 == id)?;
        self.rows.iter().position(|r| match r {
            Row::Item(i) => *i == ix,
            Row::ToolGroup { members } => members.contains(&ix),
            Row::Work { .. } | Row::Pending | Row::TurnFooter { .. } => false,
        })
    }

    fn rebuild_rows(&mut self) -> Vec<Row> {
        let Some(model) = self.model.as_ref() else {
            return std::mem::take(&mut self.rows);
        };
        let mut rows = Vec::with_capacity(model.items.len());
        let items = &model.items;
        // Subagent activity lives in its own panel tab, not the main timeline.
        // Claude's internal deferred-tool loading is noise in the timeline, and
        // a finished thought with nothing to show is just a blank row.
        let hidden = |i: usize| match &items[i].body {
            ItemBody::ToolCall { tool, .. } => tool == "ToolSearch",
            ItemBody::Reasoning { text } => {
                text.trim().is_empty() && items[i].status != ItemStatus::InProgress
            }
            _ => false,
        };
        let top: Vec<usize> = (0..items.len())
            .filter(|&i| items[i].parent_id.is_none() && !hidden(i))
            .collect();
        let mut k = 0;
        while k < top.len() {
            let turn_id = items[top[k]].turn_id.clone();
            let mut end = k + 1;
            while end < top.len() && turn_id.is_some() && items[top[end]].turn_id == turn_id {
                end += 1;
            }
            let run = &top[k..end];
            k = end;
            let turn_ix = turn_id
                .as_ref()
                .and_then(|id| model.turns.iter().position(|t| &t.id == id));
            let Some(turn_ix) = turn_ix else {
                push_runs(&mut rows, items, run);
                continue;
            };
            let turn = &model.turns[turn_ix];
            let done = turn.status.is_terminal();
            // The prompt, then the work, then (once done) the answer.
            let lead = run
                .iter()
                .take_while(|&&i| matches!(items[i].body, ItemBody::UserMessage { .. }))
                .count();
            push_runs(&mut rows, items, &run[..lead]);
            let rest = &run[lead..];
            let answer_at = done
                .then(|| {
                    rest.iter()
                        .rposition(|&i| matches!(items[i].body, ItemBody::AssistantMessage { .. }))
                })
                .flatten()
                .unwrap_or(rest.len());
            let is_error = |i: usize| matches!(items[i].body, ItemBody::Error { .. });
            let work: Vec<usize> = rest[..answer_at]
                .iter()
                .copied()
                .filter(|&i| !(done && is_error(i)))
                .collect();
            if !done || !work.is_empty() {
                let key = ItemId(format!("work:{}", turn.id.0));
                let open = !done || self.expanded.get(&key).copied().unwrap_or(false);
                rows.push(Row::Work {
                    turn_ix,
                    open,
                    steps: work.len(),
                });
                if open {
                    push_runs(&mut rows, items, &work);
                }
            }
            if !done {
                // Between steps: say so, instead of looking stuck.
                let busy = work
                    .iter()
                    .any(|&i| items[i].status == ItemStatus::InProgress)
                    || model.open_requests().next().is_some();
                if !busy {
                    rows.push(Row::Pending);
                }
                continue;
            }
            let visible: Vec<usize> = rest[..answer_at]
                .iter()
                .copied()
                .filter(|&i| is_error(i))
                .chain(rest[answer_at..].iter().copied())
                .collect();
            push_runs(&mut rows, items, &visible);
            rows.push(Row::TurnFooter { turn_ix });
        }
        std::mem::replace(&mut self.rows, rows)
    }

    /// Fold or unfold a finished turn's work.
    pub fn toggle_work(&mut self, turn: &loft_protocol::TurnId, cx: &mut Context<Self>) {
        let key = ItemId(format!("work:{}", turn.0));
        let open = self.expanded.get(&key).copied().unwrap_or(false);
        self.expanded.insert(key, !open);
        self.sync_rows(None, cx);
        cx.notify();
    }

    /// While a turn runs, repaint every second so "Working for 12s" ticks.
    fn ensure_ticker(&mut self, cx: &mut Context<Self>) {
        let running = self
            .model
            .as_ref()
            .is_some_and(|m| m.running_turn().is_some());
        if !running || self.ticker.is_some() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |t, cx| {
                    let running = t.model.as_ref().is_some_and(|m| m.running_turn().is_some());
                    cx.notify();
                    if !running {
                        t.ticker = None;
                    }
                    running
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// Rebuild rows and tell the virtual list what changed.
    fn sync_rows(&mut self, changed_item: Option<&ItemId>, cx: &mut Context<Self>) {
        let old = self.rebuild_rows();
        self.ensure_ticker(cx);
        let new_len = self.rows.len();
        let common = old
            .iter()
            .zip(&self.rows)
            .take_while(|(a, b)| a == b)
            .count();
        let changed_row = changed_item.and_then(|id| self.row_of_item(id));
        self.scroller.update(cx, |s, cx| {
            if common == old.len() {
                // Pure append (the common streaming case).
                if new_len > old.len() {
                    s.append(new_len - old.len(), cx);
                }
            } else {
                // Rows from `common` changed shape (e.g. a tool group grew).
                s.splice(common..old.len(), new_len - common, cx);
            }
            if let Some(row) = changed_row {
                s.remeasure_items(row..row + 1, cx);
            }
        });
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        let Some(model) = ThreadModel::from_snapshot(snapshot) else {
            return;
        };
        self.markdown.clear();
        for item in &model.items {
            self.sync_markdown(&item.id, &item.body, cx);
        }
        self.model = Some(model);
        self.rebuild_rows();
        self.ensure_ticker(cx);
        let count = self.rows.len();
        self.scroller.update(cx, |s, cx| {
            s.reset(count, cx);
            s.scroll_to_end(cx);
        });
        cx.notify();
    }

    fn apply_event(&mut self, env: &loft_protocol::EventEnvelope, cx: &mut Context<Self>) {
        let Some(model) = self.model.as_mut() else {
            return;
        };
        if !model.apply_event(env) {
            return;
        }
        let changed = match &env.event {
            Event::ItemUpserted { item } => {
                let (id, body) = (item.id.clone(), item.body.clone());
                self.sync_markdown(&id, &body, cx);
                Some(id)
            }
            Event::RequestOpened { request } | Event::RequestUpdated { request } => {
                request.item_id.clone()
            }
            _ => None,
        };
        self.sync_rows(changed.as_ref(), cx);
        cx.notify();
    }

    fn apply_delta(&mut self, delta: &Delta, cx: &mut Context<Self>) {
        let Some(model) = self.model.as_mut() else {
            return;
        };
        if !model.apply_delta(delta) {
            return;
        }
        let Some(ix) = model.items.iter().position(|i| i.id == delta.item_id) else {
            return;
        };
        let (id, body) = (model.items[ix].id.clone(), model.items[ix].body.clone());
        // `set_text` appends incrementally when the new text extends the old.
        self.sync_markdown(&id, &body, cx);
        if let Some(row) = self.row_of_item(&id) {
            self.scroller.update(cx, |s, cx| {
                s.remeasure_items(row..row + 1, cx);
            });
        }
        cx.notify();
    }

    /// Keep a markdown document per assistant message in sync with its text.
    fn sync_markdown(&mut self, id: &ItemId, body: &ItemBody, cx: &mut Context<Self>) {
        let ItemBody::AssistantMessage { text } = body else {
            return;
        };
        match self.markdown.get(id) {
            Some(md) => md.update(cx, |md, cx| md.set_text(text, cx)),
            None => {
                let md = cx.new(|cx| TextViewState::markdown(text, cx).selectable(true));
                self.markdown.insert(id.clone(), md);
            }
        }
    }
}

/// Rows for a list of top-level items: consecutive tool calls fold into
/// one group row.
fn push_runs(rows: &mut Vec<Row>, items: &[loft_protocol::Item], ixs: &[usize]) {
    let is_tool = |i: usize| matches!(items[i].body, ItemBody::ToolCall { .. });
    let mut k = 0;
    while k < ixs.len() {
        if is_tool(ixs[k]) {
            let mut members = Vec::new();
            while k < ixs.len() && is_tool(ixs[k]) {
                members.push(ixs[k]);
                k += 1;
            }
            rows.push(Row::ToolGroup { members });
        } else {
            rows.push(Row::Item(ixs[k]));
            k += 1;
        }
    }
}

/// Extension used by views to read the store.
pub fn store_read<'a>(store: &'a Entity<AppStore>, cx: &'a App) -> &'a AppStore {
    store.read(cx)
}
