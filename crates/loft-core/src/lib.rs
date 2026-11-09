//! The Agentloft core (PLAN §1.5): handles client requests, runs one actor per
//! active thread that drives its harness session, persists domain events and
//! fans them out on a broadcast bus together with ephemeral streaming deltas.

mod thread;

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
    Event(EventEnvelope),
    Delta(Delta),
}

pub struct CoreOptions {
    pub env: Arc<HashMap<String, String>>,
    pub config: loft_config::Config,
    /// Snapshot cadence for streaming items (ADR-0006: ≈250 ms).
    pub flush_interval: Duration,
}

impl Default for CoreOptions {
    fn default() -> Self {
        Self {
            env: Arc::new(std::env::vars().collect()),
            config: loft_config::Config::default(),
            flush_interval: Duration::from_millis(250),
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
            opts,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusMsg> {
        self.bus.subscribe()
    }

    /// Append events, update projections and broadcast — atomically ordered.
    pub(crate) fn commit(&self, events: Vec<Event>) -> CoreResult<Vec<EventEnvelope>> {
        if events.is_empty() {
            return Ok(vec![]);
        }
        let mut store = lock(&self.store);
        let envs = store.commit(events).map_err(internal)?;
        for env in &envs {
            let _ = self.bus.send(BusMsg::Event(env.clone()));
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
        // Read under the store lock so `seq` is consistent with the data.
        let snap = self.with_store(|s| {
            let seq = s.max_seq()?;
            Ok(match topic {
                Topic::Workspace => Some(Snapshot::Workspace {
                    seq,
                    projects: s.projects()?,
                    threads: s.threads()?,
                }),
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

    pub async fn handle(self: &Arc<Self>, req: Request) -> CoreResult<Response> {
        match req {
            Request::Ping => Ok(Response::Pong),
            Request::AddProject { path, name } => self.add_project(path, name),
            Request::CreateThread {
                project_id,
                harness,
                title,
            } => self.create_thread(project_id, harness, title),
            Request::SendMessage { thread_id, text } => {
                if text.trim().is_empty() {
                    return Err(ErrorBody::new(ErrorCode::BadRequest, "empty message"));
                }
                self.thread_call(&thread_id, |reply| ThreadCmd::SendMessage { text, reply })
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
        let thread = Thread {
            id: ThreadId::new(),
            project_id,
            title: title.unwrap_or_else(|| "New thread".into()),
            harness,
            cwd: project.root_path,
            model: None,
            permission_mode: None,
            native_session_id: None,
            status: ThreadStatus::Idle,
            created_at_ms: now,
            updated_at_ms: now,
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

    /// Stop all harness sessions (daemon shutdown).
    pub async fn shutdown(&self) {
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
