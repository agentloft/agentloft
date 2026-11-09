//! The Agentloft core (PLAN §1.5): handles client requests, runs one actor per
//! active thread that drives its harness session, persists domain events and
//! fans them out on a broadcast bus together with ephemeral streaming deltas.

mod clients;
mod forks;
mod handoff;
mod terminals;
mod thread;
mod workbench;
mod worktrees;

pub use clients::{ClientRegistry, OutgoingCall};

use anyhow::Result;
use loft_harness::Harness;
use loft_protocol::{
    Delta, ErrorBody, ErrorCode, Event, EventEnvelope, HarnessId, Item, ItemId, ItemStatus,
    Project, ProjectId, Request, RequestStatus, Response, Snapshot, Thread, ThreadId, ThreadStatus,
    Topic, TurnStatus,
};
use loft_store::{Store, now_ms};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use thread::{ThreadCmd, ThreadHandle};
use tokio::sync::{broadcast, oneshot};

/// Message on the core bus: a committed event or a streaming delta.
#[derive(Debug, Clone)]
pub enum BusMsg {
    /// Shared: the bus clones each message per subscriber.
    Event(Arc<EventEnvelope>),
    Delta(Delta),
    /// A terminal's output stream now ends at byte `end`. Carries no data:
    /// subscribers read what's new from the terminal's ring buffer, so a
    /// burst costs no memory beyond the ring and coalesces naturally.
    TerminalOutput {
        id: loft_protocol::TerminalId,
        end: u64,
    },
    TerminalExited {
        id: loft_protocol::TerminalId,
        code: Option<i32>,
    },
}

/// How agent sessions reach the Agentloft MCP server (`agentloftd mcp`).
#[derive(Debug, Clone)]
pub struct McpBridge {
    pub exe: PathBuf,
    pub home: PathBuf,
}

pub struct CoreOptions {
    pub env: Arc<HashMap<String, String>>,
    pub config: loft_config::Config,
    /// Snapshot cadence for streaming items (ADR-0006: ≈250 ms).
    pub flush_interval: Duration,
    /// Attach the Agentloft MCP server to agent sessions.
    pub mcp_bridge: Option<McpBridge>,
    /// Where per-thread worktrees go (`~/.agentloft/worktrees`).
    pub worktrees_dir: PathBuf,
    /// `config.toml`, re-read on `Request::ReloadConfig`.
    pub config_file: Option<PathBuf>,
    /// Where uploaded attachments are stored (`~/.agentloft/attachments`).
    pub attachments_dir: PathBuf,
}

impl Default for CoreOptions {
    fn default() -> Self {
        Self {
            env: Arc::new(std::env::vars().collect()),
            config: loft_config::Config::default(),
            flush_interval: Duration::from_millis(250),
            mcp_bridge: None,
            worktrees_dir: std::env::temp_dir().join("agentloft-worktrees"),
            config_file: None,
            attachments_dir: std::env::temp_dir().join("agentloft-attachments"),
        }
    }
}

pub struct Core {
    store: Mutex<Store>,
    bus: broadcast::Sender<BusMsg>,
    harnesses: HashMap<String, Arc<dyn Harness>>,
    threads: Mutex<HashMap<ThreadId, ThreadHandle>>,
    /// In-progress items being streamed, overlaid on snapshots.
    live: Mutex<HashMap<ItemId, Item>>,
    terminals: terminals::Terminals,
    clients: ClientRegistry,
    /// Serializes worktree creation (two first messages racing).
    worktree_lock: tokio::sync::Mutex<()>,
    /// Live config (starts as `opts.config`; replaced on reload).
    config: std::sync::RwLock<loft_config::Config>,
    /// `ListModels` answers per harness (probing Codex starts its CLI).
    models: Mutex<HashMap<String, Vec<loft_protocol::ModelInfo>>>,
    /// Worktree setup commands to run before a thread's first prompt.
    pending_setup: Mutex<HashMap<ThreadId, String>>,
    opts: CoreOptions,
}

pub type CoreResult<T> = std::result::Result<T, ErrorBody>;

fn internal(e: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, e.to_string())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Core {
    pub fn new(store: Store, harnesses: Vec<Arc<dyn Harness>>, opts: CoreOptions) -> Arc<Self> {
        let (bus, _) = broadcast::channel(4096);
        Arc::new(Self {
            store: Mutex::new(store),
            bus,
            harnesses: harnesses.into_iter().map(|h| (h.id().0, h)).collect(),
            threads: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            terminals: terminals::Terminals::default(),
            clients: ClientRegistry::default(),
            worktree_lock: tokio::sync::Mutex::new(()),
            config: std::sync::RwLock::new(opts.config.clone()),
            models: Mutex::new(HashMap::new()),
            pending_setup: Mutex::new(HashMap::new()),
            opts,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusMsg> {
        self.bus.subscribe()
    }

    pub fn clients(&self) -> &ClientRegistry {
        &self.clients
    }

    /// Append events, update projections and broadcast — atomically ordered.
    pub(crate) fn commit(&self, events: Vec<Event>) -> CoreResult<Vec<EventEnvelope>> {
        if events.is_empty() {
            return Ok(vec![]);
        }
        let mut store = lock(&self.store);
        let envs = store.commit(events).map_err(internal)?;
        for env in &envs {
            let _ = self.bus.send(BusMsg::Event(Arc::new(env.clone())));
        }
        Ok(envs)
    }

    pub(crate) fn publish_delta(&self, delta: Delta) {
        let _ = self.bus.send(BusMsg::Delta(delta));
    }

    pub(crate) fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> Result<T>) -> CoreResult<T> {
        f(&mut lock(&self.store)).map_err(internal)
    }

    pub(crate) fn live(&self) -> MutexGuard<'_, HashMap<ItemId, Item>> {
        lock(&self.live)
    }

    pub(crate) fn take_pending_setup(&self, thread: &ThreadId) -> Option<String> {
        lock(&self.pending_setup).remove(thread)
    }

    /// Terminal output from stream offset `from` to its current end.
    /// `None` when `from` has already scrolled out of the ring (re-snapshot).
    pub fn terminal_read_since(
        &self,
        id: &loft_protocol::TerminalId,
        from: u64,
    ) -> CoreResult<Option<(Vec<u8>, u64)>> {
        self.terminals.read_since(id, from)
    }

    /// The current config (reloaded by the settings window).
    pub(crate) fn config(&self) -> loft_config::Config {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Validate and store uploaded images; the agent gets file paths.
    async fn store_attachments(
        &self,
        thread_id: &ThreadId,
        attachments: Vec<loft_protocol::Attachment>,
    ) -> CoreResult<Vec<loft_protocol::AttachmentRef>> {
        const MAX_COUNT: usize = 10;
        const MAX_BYTES: usize = 20 * 1024 * 1024;
        if attachments.len() > MAX_COUNT {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("at most {MAX_COUNT} attachments per message"),
            ));
        }
        let dir = self.opts.attachments_dir.join(thread_id.as_str());
        let mut stored = Vec::new();
        for a in attachments {
            let ext = match a.mime.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                other => {
                    return Err(ErrorBody::new(
                        ErrorCode::BadRequest,
                        format!("unsupported attachment type `{other}` (images only)"),
                    ));
                }
            };
            if a.data.0.len() > MAX_BYTES {
                return Err(ErrorBody::new(
                    ErrorCode::BadRequest,
                    format!("`{}` is larger than 20 MB", a.name),
                ));
            }
            let path = dir.join(format!("{}.{ext}", uuid::Uuid::now_v7()));
            let (d, p, bytes) = (dir.clone(), path.clone(), a.data.0);
            tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(&d)?;
                std::fs::write(&p, bytes)
            })
            .await
            .map_err(internal)?
            .map_err(internal)?;
            stored.push(loft_protocol::AttachmentRef {
                name: a.name,
                mime: a.mime,
                path: path.to_string_lossy().into_owned(),
            });
        }
        Ok(stored)
    }

    async fn list_models(&self, harness: &HarnessId) -> CoreResult<Response> {
        if let Some(models) = lock(&self.models).get(harness.as_str()) {
            return Ok(Response::Models {
                models: models.clone(),
            });
        }
        let h = self.harness(harness).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::BadRequest,
                format!("unknown harness `{harness}`"),
            )
        })?;
        let hcfg = harness_config(&self.config(), harness);
        let binary = (hcfg.path != "auto").then(|| PathBuf::from(&hcfg.path));
        let env = self.opts.env.clone();
        let models = tokio::task::spawn_blocking(move || h.list_models(&env, binary))
            .await
            .map_err(internal)?
            .map_err(|e| ErrorBody::new(ErrorCode::Harness, format!("{e:#}")))?;
        lock(&self.models).insert(harness.as_str().to_owned(), models.clone());
        Ok(Response::Models { models })
    }

    fn reload_config(&self) -> CoreResult<Response> {
        let Some(file) = &self.opts.config_file else {
            return Ok(Response::Ok);
        };
        let config = loft_config::Config::load_file(file)
            .map_err(|e| ErrorBody::new(ErrorCode::BadRequest, format!("{e:#}")))?;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
        // Binary paths may have changed.
        lock(&self.models).clear();
        tracing::info!("config reloaded");
        Ok(Response::Ok)
    }

    pub(crate) fn opts(&self) -> &CoreOptions {
        &self.opts
    }

    pub(crate) fn harness(&self, id: &HarnessId) -> Option<Arc<dyn Harness>> {
        self.harnesses.get(id.as_str()).cloned()
    }

    // -- startup ------------------------------------------------------------

    /// Crash recovery (PLAN §5.6): turns left running become interrupted,
    /// open approvals expire, streaming items are closed, threads go idle.
    pub fn recover(&self) -> CoreResult<usize> {
        let (turns, requests, items, threads) = self.with_store(|s| {
            Ok((
                s.running_turns()?,
                s.open_requests()?,
                s.in_progress_items()?,
                s.threads()?,
            ))
        })?;
        let now = now_ms();
        let mut events = Vec::new();
        for mut item in items {
            item.status = ItemStatus::Completed;
            item.updated_at_ms = now;
            events.push(Event::ItemUpserted { item });
        }
        for mut request in requests {
            request.status = RequestStatus::Expired;
            events.push(Event::RequestUpdated { request });
        }
        let recovered_turns = turns.len();
        for mut turn in turns {
            turn.status = TurnStatus::Interrupted;
            turn.ended_at_ms = Some(now);
            turn.error = Some("Agentloft daemon restarted during this turn".into());
            events.push(Event::TurnUpdated { turn });
        }
        for mut thread in threads {
            if matches!(
                thread.status,
                ThreadStatus::Running | ThreadStatus::WaitingApproval
            ) {
                thread.status = ThreadStatus::Idle;
                thread.updated_at_ms = now;
                events.push(Event::ThreadUpdated { thread });
            }
        }
        self.commit(events)?;
        Ok(recovered_turns)
    }

    // -- queries ------------------------------------------------------------

    pub fn snapshot(&self, topic: &Topic) -> CoreResult<Snapshot> {
        if let Topic::Terminal(id) = topic {
            let (terminal, replay, end) = self.terminals.snapshot(id)?;
            return Ok(Snapshot::Terminal {
                seq: end,
                terminal,
                replay: replay.into(),
            });
        }
        // Read under the store lock so `seq` is consistent with the data.
        let snap = self.with_store(|s| {
            let seq = s.max_seq()?;
            Ok(match topic {
                Topic::Workspace => Some(Snapshot::Workspace {
                    seq,
                    projects: s.projects()?,
                    threads: s.threads()?,
                }),
                Topic::Terminal(_) => None,
                Topic::Thread(id) => match s.thread(id)? {
                    None => None,
                    Some(thread) => Some(Snapshot::Thread {
                        seq,
                        turns: s.turns(id)?,
                        items: s.items(id)?,
                        requests: s.requests(id)?,
                        thread,
                    }),
                },
            })
        })?;
        let mut snap =
            snap.ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        if let Snapshot::Thread { items, .. } = &mut snap {
            let live = self.live();
            for item in items.iter_mut() {
                if let Some(l) = live.get(&item.id) {
                    *item = l.clone();
                }
            }
        }
        Ok(snap)
    }

    // -- requests -----------------------------------------------------------

    /// Requests that must run in arrival order on the connection (keystrokes
    /// to a terminal). Returns `None` for everything else.
    pub fn handle_inline(&self, req: &Request) -> Option<CoreResult<Response>> {
        match req {
            Request::TerminalInput { id, data } => {
                Some(self.terminals.input(id, &data.0).map(|_| Response::Ok))
            }
            Request::TerminalResize { id, cols, rows } => Some(
                self.terminals
                    .resize(id, *cols, *rows)
                    .map(|_| Response::Ok),
            ),
            _ => None,
        }
    }

    pub async fn handle(self: &Arc<Self>, req: Request) -> CoreResult<Response> {
        match req {
            Request::Ping => Ok(Response::Pong),
            Request::AddProject { path, name } => self.add_project(path, name),
            Request::CreateThread {
                project_id,
                harness,
                title,
            } => self.create_thread(project_id, harness, title),
            Request::SendMessage {
                thread_id,
                text,
                attachments,
            } => {
                if text.trim().is_empty() && attachments.is_empty() {
                    return Err(ErrorBody::new(ErrorCode::BadRequest, "empty message"));
                }
                let attachments = self.store_attachments(&thread_id, attachments).await?;
                self.ensure_workspace(&thread_id, &text).await?;
                self.thread_call(&thread_id, |reply| ThreadCmd::SendMessage {
                    text,
                    attachments,
                    reply,
                })
                .await
            }
            Request::Interrupt { thread_id } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::Interrupt { reply })
                    .await
            }
            Request::RespondRequest {
                thread_id,
                request_id,
                decision,
            } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::Respond {
                    request_id,
                    decision,
                    reply,
                })
                .await
            }
            Request::SetPermissionMode { thread_id, mode } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::SetPermissionMode {
                    mode,
                    reply,
                })
                .await
            }
            Request::Shutdown { .. } => Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "shutdown is handled by the daemon",
            )),
            Request::ListDir { root, rel_path } => {
                let core = self.clone();
                let entries = tokio::task::spawn_blocking(move || core.list_dir(&root, &rel_path))
                    .await
                    .map_err(internal)??;
                Ok(Response::DirListing { entries })
            }
            Request::ReadFile { root, rel_path } => {
                let core = self.clone();
                let file = tokio::task::spawn_blocking(move || core.read_file(&root, &rel_path))
                    .await
                    .map_err(internal)??;
                Ok(Response::File { file })
            }
            Request::GetDiff { thread_id, scope } => Ok(Response::Diff {
                diff: self.diff(&thread_id, scope).await?,
            }),
            Request::TerminalOpen {
                root,
                cols,
                rows,
                key,
            } => {
                if let Some(terminal) = key
                    .as_deref()
                    .and_then(|k| self.terminals.reattach(k, cols, rows))
                {
                    return Ok(Response::Terminal { terminal });
                }
                let dir = self.root_dir(&root)?;
                let terminal =
                    self.terminals
                        .open(&dir, cols, rows, key, &self.opts.env, self.bus.clone())?;
                Ok(Response::Terminal { terminal })
            }
            Request::TerminalInput { id, data } => {
                self.terminals.input(&id, &data.0)?;
                Ok(Response::Ok)
            }
            Request::TerminalResize { id, cols, rows } => {
                self.terminals.resize(&id, cols, rows)?;
                Ok(Response::Ok)
            }
            Request::ListTerminals { key_prefix } => Ok(Response::Terminals {
                terminals: self.terminals.list_prefix(&key_prefix),
            }),
            Request::TerminalClose { id } => {
                self.terminals.close(&id)?;
                Ok(Response::Ok)
            }
            Request::BrowserCall { thread_id, command } => {
                let value = self
                    .clients
                    .call(
                        loft_protocol::ClientCall::Browser { thread_id, command },
                        Duration::from_secs(90),
                    )
                    .await?;
                Ok(Response::Json { value })
            }
            Request::SetModel { thread_id, model } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::SetModel { model, reply })
                    .await
            }
            Request::SetThreadHarness { thread_id, harness } => {
                self.set_thread_harness(&thread_id, harness)
            }
            Request::SetThreadWorkspace {
                thread_id,
                worktree,
            } => self.set_thread_workspace(&thread_id, worktree),
            Request::MergeThread { thread_id, message } => {
                self.merge_thread(&thread_id, message).await
            }
            Request::ForkThread {
                thread_id,
                harness,
                share_workspace,
            } => self.fork_thread(&thread_id, harness, share_workspace).await,
            Request::ReloadConfig => self.reload_config(),
            Request::ListModels { harness } => self.list_models(&harness).await,
            Request::CreatePullRequest { thread_id } => self.create_pull_request(&thread_id).await,
            Request::RevertTurn { thread_id, turn_id } => {
                self.revert_turn(&thread_id, &turn_id).await
            }
            Request::RenameThread { thread_id, title } => {
                let title = title.trim().to_owned();
                if title.is_empty() {
                    return Err(ErrorBody::new(ErrorCode::BadRequest, "empty title"));
                }
                self.thread_call(&thread_id, |reply| ThreadCmd::Rename { title, reply })
                    .await
            }
            Request::UnarchiveThread { thread_id } => self.unarchive_thread(&thread_id).await,
            Request::ArchiveThread { thread_id, force } => {
                self.archive_thread(&thread_id, force).await
            }
            Request::ListThreads { query, project_id } => Ok(Response::Json {
                value: self.list_threads_json(query.as_deref(), project_id.as_ref())?,
            }),
            Request::ReadThread { thread_id, detail } => Ok(Response::Text {
                text: self.read_thread_text(&thread_id, detail)?,
            }),
        }
    }

    fn add_project(&self, path: String, name: Option<String>) -> CoreResult<Response> {
        let path = expand_tilde(&path);
        let canonical = std::fs::canonicalize(&path).map_err(|e| {
            ErrorBody::new(ErrorCode::BadRequest, format!("{}: {e}", path.display()))
        })?;
        if !canonical.is_dir() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("{} is not a directory", canonical.display()),
            ));
        }
        let root_path = canonical.to_string_lossy().into_owned();
        if let Some(project) = self.with_store(|s| s.project_by_path(&root_path))? {
            return Ok(Response::Project { project });
        }
        let project = Project {
            id: ProjectId::new(),
            name: name.unwrap_or_else(|| {
                canonical
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root_path.clone())
            }),
            root_path,
            created_at_ms: now_ms(),
        };
        self.commit(vec![Event::ProjectAdded {
            project: project.clone(),
        }])?;
        Ok(Response::Project { project })
    }

    fn set_thread_harness(&self, id: &ThreadId, harness: HarnessId) -> CoreResult<Response> {
        if self.harness(&harness).is_none() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("unknown harness `{harness}`"),
            ));
        }
        let (thread, turns) = self.with_store(|s| Ok((s.thread(id)?, s.turns(id)?)))?;
        let mut thread =
            thread.ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        if !turns.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "a thread keeps its harness once it has started; fork it instead",
            ));
        }
        // Drop any idle actor holding the old thread state.
        lock(&self.threads).remove(id);
        let (model, mode) = thread_defaults(&self.config(), &harness);
        thread.harness = harness;
        thread.native_session_id = None;
        thread.model = model;
        thread.permission_mode = mode;
        thread.updated_at_ms = now_ms();
        self.commit(vec![Event::ThreadUpdated {
            thread: thread.clone(),
        }])?;
        Ok(Response::Thread { thread })
    }

    fn create_thread(
        &self,
        project_id: ProjectId,
        harness: HarnessId,
        title: Option<String>,
    ) -> CoreResult<Response> {
        if self.harness(&harness).is_none() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("unknown harness `{harness}`"),
            ));
        }
        let project = self
            .with_store(|s| s.project(&project_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        let now = now_ms();
        let workspace = self.default_workspace(&project.root_path);
        let (model, permission_mode) = thread_defaults(&self.config(), &harness);
        let thread = Thread {
            id: ThreadId::new(),
            project_id,
            title: title.unwrap_or_else(|| "New thread".into()),
            harness,
            cwd: project.root_path,
            model,
            permission_mode,
            native_session_id: None,
            status: ThreadStatus::Idle,
            created_at_ms: now,
            updated_at_ms: now,
            workspace,
            parent: None,
        };
        self.commit(vec![Event::ThreadCreated {
            thread: thread.clone(),
        }])?;
        Ok(Response::Thread { thread })
    }

    async fn thread_call(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        make: impl FnOnce(oneshot::Sender<CoreResult<Response>>) -> ThreadCmd,
    ) -> CoreResult<Response> {
        let handle = self.thread_handle(thread_id)?;
        let (tx, rx) = oneshot::channel();
        handle
            .send(make(tx))
            .map_err(|_| internal("thread actor stopped"))?;
        rx.await
            .map_err(|_| internal("thread actor dropped request"))?
    }

    fn thread_handle(self: &Arc<Self>, thread_id: &ThreadId) -> CoreResult<ThreadHandle> {
        let mut threads = lock(&self.threads);
        if let Some(h) = threads.get(thread_id).filter(|h| !h.is_closed()) {
            return Ok(h.clone());
        }
        let thread = self
            .with_store(|s| s.thread(thread_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        let handle = thread::spawn(self.clone(), thread);
        threads.insert(thread_id.clone(), handle.clone());
        Ok(handle)
    }

    /// Stop all harness sessions and terminals (daemon shutdown).
    pub async fn shutdown(&self) {
        self.terminals.shutdown();
        let handles: Vec<ThreadHandle> = lock(&self.threads).drain().map(|(_, h)| h).collect();
        let mut waits = Vec::new();
        for h in handles {
            let (tx, rx) = oneshot::channel();
            if h.send(ThreadCmd::Shutdown { done: tx }).is_ok() {
                waits.push(rx);
            }
        }
        for w in waits {
            let _ = tokio::time::timeout(Duration::from_secs(10), w).await;
        }
    }

    /// Whether any thread currently has a running turn (upgrade gating).
    pub fn is_busy(&self) -> bool {
        self.with_store(|s| Ok(!s.running_turns()?.is_empty()))
            .unwrap_or(true)
    }

    pub(crate) fn record_process(&self, pid: u32, kind: &str, thread: &ThreadId) {
        let _ = self.with_store(|s| s.record_process(pid, kind, Some(thread)));
    }

    pub(crate) fn forget_process(&self, pid: u32) {
        let _ = self.with_store(|s| s.forget_process(pid));
    }
}

/// `[harness.<id>]` settings for a harness (defaults when unknown).
pub(crate) fn harness_config(
    config: &loft_config::Config,
    harness: &HarnessId,
) -> loft_config::HarnessConfig {
    match harness.as_str() {
        HarnessId::CODEX => config.harness.codex.clone(),
        HarnessId::CLAUDE => config.harness.claude.clone(),
        _ => loft_config::HarnessConfig::default(),
    }
}

/// The user's per-harness defaults for a new thread: model and mode.
pub(crate) fn thread_defaults(
    config: &loft_config::Config,
    harness: &HarnessId,
) -> (Option<String>, Option<loft_protocol::PermissionMode>) {
    let h = harness_config(config, harness);
    let model =
        (h.default_model != "auto" && !h.default_model.is_empty()).then(|| h.default_model.clone());
    let mode = serde_json::from_value(serde_json::json!(h.default_permission_mode)).ok();
    (model, mode)
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests;
