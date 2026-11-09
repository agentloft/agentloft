//! One actor per active thread. It owns the harness session, maps native ids
//! to app ids, turns harness events into domain events, and coalesces
//! streaming deltas into periodic item snapshots.

use crate::{Core, CoreResult, internal};
use loft_harness::{HarnessEvent, SessionCommand, SessionConfig, SessionHandle, TurnInput};
use loft_protocol::{
    AttachmentRef, DecisionKind, Delta, DeltaChannel, ErrorBody, ErrorCode, Event, ForkKind, Item,
    ItemBody, ItemId, ItemStatus, PermissionMode, RequestId, RequestStatus, Response,
    RuntimeRequest, Thread, ThreadStatus, Turn, TurnId, TurnStatus,
};
use loft_store::now_ms;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub(crate) type Reply = oneshot::Sender<CoreResult<Response>>;

pub(crate) enum ThreadCmd {
    SendMessage {
        text: String,
        attachments: Vec<AttachmentRef>,
        reply: Reply,
    },
    Interrupt {
        reply: Reply,
    },
    Respond {
        request_id: RequestId,
        decision: DecisionKind,
        reply: Reply,
    },
    SetPermissionMode {
        mode: Option<PermissionMode>,
        reply: Reply,
    },
    SetModel {
        model: Option<String>,
        reply: Reply,
    },
    /// The worktree setup command finished (internal).
    SetupDone {
        turn_id: TurnId,
        item_id: ItemId,
        ok: bool,
        text: String,
    },
    /// Files were restored to before turn `ordinal`: show a notice and tell
    /// the agent before its next prompt.
    Reverted {
        ordinal: u32,
        files: usize,
        reply: Reply,
    },
    Shutdown {
        done: oneshot::Sender<()>,
    },
    /// Internal: a checkpoint around a turn was captured (ADR-0008).
    Checkpoint {
        turn_id: TurnId,
        before: bool,
        sha: Option<String>,
    },
}

pub(crate) type ThreadHandle = mpsc::UnboundedSender<ThreadCmd>;

pub(crate) fn spawn(core: Arc<Core>, thread: Thread) -> ThreadHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let actor = ThreadActor {
        self_tx: tx.downgrade(),
        pending_start: None,
        pending_note: None,
        core,
        thread,
        session: None,
        session_pid: None,
        turn: None,
        items: HashMap::new(),
        requests: HashMap::new(),
        dirty: HashSet::new(),
    };
    tokio::spawn(actor.run(rx));
    tx
}

struct ThreadActor {
    self_tx: mpsc::WeakUnboundedSender<ThreadCmd>,
    /// Prompt waiting for the "before" checkpoint before it is sent.
    pending_start: Option<(String, Vec<AttachmentRef>)>,
    /// Said to the agent ahead of the next prompt (e.g. after a revert).
    pending_note: Option<String>,
    core: Arc<Core>,
    thread: Thread,
    session: Option<SessionHandle>,
    session_pid: Option<u32>,
    turn: Option<Turn>,
    /// native item id → app item id (current session).
    items: HashMap<String, ItemId>,
    /// app request id → native request id.
    requests: HashMap<RequestId, String>,
    /// Streaming items whose live text hasn't been snapshotted yet.
    dirty: HashSet<ItemId>,
}

impl ThreadActor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<ThreadCmd>) {
        let mut flush = tokio::time::interval(self.core.opts().flush_interval);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(ThreadCmd::Shutdown { done }) => {
                        self.stop_session().await;
                        let _ = done.send(());
                        return;
                    }
                    Some(cmd) => self.on_command(cmd),
                    None => {
                        self.stop_session().await;
                        return;
                    }
                },
                event = next_event(&mut self.session) => match event {
                    Some(event) => self.on_harness_event(event),
                    None => self.on_session_lost(None, String::new()),
                },
                _ = flush.tick() => self.flush_dirty(),
            }
        }
    }

    // -- commands -----------------------------------------------------------

    fn on_command(&mut self, cmd: ThreadCmd) {
        match cmd {
            ThreadCmd::SendMessage {
                text,
                attachments,
                reply,
            } => {
                let _ = reply.send(self.send_message(text, attachments));
            }
            ThreadCmd::Interrupt { reply } if self.pending_start.is_some() => {
                // The prompt never reached the agent; just end the turn.
                self.pending_start = None;
                let res = self
                    .finish_turn(TurnStatus::Interrupted, None, None)
                    .map(|_| Response::Ok);
                let _ = reply.send(res);
            }
            ThreadCmd::Checkpoint {
                turn_id,
                before,
                sha,
            } => self.on_checkpoint(turn_id, before, sha),
            ThreadCmd::SetupDone {
                turn_id,
                item_id,
                ok,
                text,
            } => self.on_setup_done(turn_id, item_id, ok, text),
            ThreadCmd::Interrupt { reply } => {
                let res = match (&self.session, &self.turn) {
                    (Some(s), Some(_)) => {
                        let _ = s.commands.send(SessionCommand::Interrupt);
                        Ok(Response::Ok)
                    }
                    _ => Err(ErrorBody::new(ErrorCode::Conflict, "no running turn")),
                };
                let _ = reply.send(res);
            }
            ThreadCmd::Respond {
                request_id,
                decision,
                reply,
            } => {
                let _ = reply.send(self.respond(request_id, decision));
            }
            ThreadCmd::SetPermissionMode { mode, reply } => {
                self.thread.permission_mode = mode;
                if let Some(s) = &self.session {
                    let _ = s.commands.send(SessionCommand::SetPermissionMode(mode));
                }
                let res = self.save_thread().map(|_| Response::Ok);
                let _ = reply.send(res);
            }
            ThreadCmd::SetModel { model, reply } => {
                self.thread.model = model.clone();
                if let Some(s) = &self.session {
                    let _ = s.commands.send(SessionCommand::SetModel(model));
                }
                let res = self.save_thread().map(|_| Response::Ok);
                let _ = reply.send(res);
            }
            ThreadCmd::Reverted {
                ordinal,
                files,
                reply,
            } => {
                let text = if files == 0 {
                    format!(
                        "Nothing to undo: files already match how they were before turn {ordinal}."
                    )
                } else {
                    self.pending_note = Some(format!(
                        "[Agentloft: the user reverted the workspace files to their state before turn {ordinal}. Changes made from that turn on are gone; re-read files before relying on earlier edits.]"
                    ));
                    format!(
                        "Restored {files} file{} to how they were before turn {ordinal}.",
                        if files == 1 { "" } else { "s" }
                    )
                };
                let res = self
                    .new_item(
                        None,
                        ItemBody::Notice {
                            level: loft_protocol::NoticeLevel::Info,
                            text: text.clone(),
                        },
                        ItemStatus::Completed,
                    )
                    .and_then(|item| self.core.commit(vec![Event::ItemUpserted { item }]))
                    .map(|_| Response::Text { text });
                let _ = reply.send(res);
            }
            ThreadCmd::Shutdown { .. } => unreachable!("handled in run loop"),
        }
    }

    fn send_message(
        &mut self,
        text: String,
        attachments: Vec<AttachmentRef>,
    ) -> CoreResult<Response> {
        if self.turn.is_some() {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "a turn is already running in this thread",
            ));
        }
        self.ensure_session()?;

        let now = now_ms();
        let ordinal = self
            .core
            .with_store(|s| s.next_turn_ordinal(&self.thread.id))?;
        let turn = Turn {
            id: TurnId::new(),
            thread_id: self.thread.id.clone(),
            ordinal,
            status: TurnStatus::Running,
            started_at_ms: now,
            ended_at_ms: None,
            usage: None,
            error: None,
            checkpoint_before: None,
            checkpoint_after: None,
        };
        let user_item = self.new_item(
            Some(turn.id.clone()),
            ItemBody::UserMessage {
                text: text.clone(),
                attachments: attachments.clone(),
            },
            ItemStatus::Completed,
        )?;
        if ordinal == 1 && self.thread.title == "New thread" {
            self.thread.title = title_from(&text);
        }
        self.thread.status = ThreadStatus::Running;
        self.thread.updated_at_ms = now;
        self.core.commit(vec![
            Event::TurnStarted { turn: turn.clone() },
            Event::ItemUpserted { item: user_item },
            Event::ThreadUpdated {
                thread: self.thread.clone(),
            },
        ])?;
        self.turn = Some(turn.clone());

        // Snapshot the work tree first; the prompt goes out once that's done
        // so the turn's diff covers exactly the agent's changes.
        self.pending_start = Some((text, attachments));
        // A new worktree's setup runs first, so the turn's checkpoint (and
        // diff) start after it.
        match self.core.take_pending_setup(&self.thread.id) {
            Some(cmd) => self.run_setup(&turn, cmd)?,
            None => self.capture_checkpoint(&turn, true),
        }
        Ok(Response::Ok)
    }

    fn run_setup(&mut self, turn: &Turn, cmd: String) -> CoreResult<()> {
        let item = self.new_item(
            Some(turn.id.clone()),
            ItemBody::Notice {
                level: loft_protocol::NoticeLevel::Info,
                text: format!("Setting up the worktree: `{cmd}`…"),
            },
            ItemStatus::InProgress,
        )?;
        let item_id = item.id.clone();
        self.core.commit(vec![Event::ItemUpserted { item }])?;
        let env = self.core.opts().env.clone();
        let shell = env
            .get("SHELL")
            .cloned()
            .unwrap_or_else(|| "/bin/sh".into());
        let cwd = self.thread.cwd.clone();
        let tx = self.self_tx.clone();
        let turn_id = turn.id.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let run = tokio::process::Command::new(&shell)
                .args(["-lc", &cmd])
                .current_dir(&cwd)
                .env_clear()
                .envs(env.iter())
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output();
            let (ok, text) =
                match tokio::time::timeout(std::time::Duration::from_secs(15 * 60), run).await {
                    Ok(Ok(out)) if out.status.success() => (
                        true,
                        format!(
                            "Worktree set up: `{cmd}` ({:.1}s)",
                            started.elapsed().as_secs_f64()
                        ),
                    ),
                    Ok(Ok(out)) => {
                        let all = format!(
                            "{}{}",
                            String::from_utf8_lossy(&out.stdout),
                            String::from_utf8_lossy(&out.stderr)
                        );
                        let tail: Vec<&str> = all.lines().rev().take(12).collect();
                        let tail: Vec<&str> = tail.into_iter().rev().collect();
                        (
                            false,
                            format!(
                                "Setup failed ({}): `{cmd}`\n{}",
                                out.status,
                                tail.join("\n")
                            ),
                        )
                    }
                    Ok(Err(err)) => (false, format!("Setup couldn't start: `{cmd}`: {err}")),
                    Err(_) => (false, format!("Setup timed out after 15 minutes: `{cmd}`")),
                };
            if let Some(tx) = tx.upgrade() {
                let _ = tx.send(ThreadCmd::SetupDone {
                    turn_id,
                    item_id,
                    ok,
                    text,
                });
            }
        });
        Ok(())
    }

    fn on_setup_done(&mut self, turn_id: TurnId, item_id: ItemId, ok: bool, text: String) {
        if let Ok(Some(mut item)) = self.load_item(&item_id) {
            item.status = ItemStatus::Completed;
            item.body = ItemBody::Notice {
                level: if ok {
                    loft_protocol::NoticeLevel::Info
                } else {
                    loft_protocol::NoticeLevel::Warning
                },
                text: text.clone(),
            };
            item.updated_at_ms = now_ms();
            let _ = self.core.commit(vec![Event::ItemUpserted { item }]);
        }
        // Stopped while setting up: nothing more to do.
        let Some(turn) = self.turn.clone().filter(|t| t.id == turn_id) else {
            return;
        };
        if self.pending_start.is_none() {
            return;
        }
        if !ok {
            // The agent goes ahead, but knows the environment may be incomplete.
            self.pending_note = Some(format!(
                "[Agentloft: the worktree setup command failed, so dependencies may be missing.]\n{text}"
            ));
        }
        self.capture_checkpoint(&turn, true);
    }

    fn capture_checkpoint(&self, turn: &Turn, before: bool) {
        let dir = std::path::PathBuf::from(&self.thread.cwd);
        let ref_name = format!(
            "{}/{}/{}-{}",
            loft_git::REF_PREFIX,
            self.thread.id,
            turn.ordinal,
            if before { "before" } else { "after" }
        );
        let message = format!(
            "agentloft checkpoint: thread {} turn {} ({})",
            self.thread.id,
            turn.ordinal,
            if before { "before" } else { "after" }
        );
        let tx = self.self_tx.clone();
        let turn_id = turn.id.clone();
        tokio::spawn(async move {
            let sha = tokio::task::spawn_blocking(move || {
                loft_git::repo_root(&dir)?;
                match loft_git::checkpoint(&dir, &ref_name, &message) {
                    Ok(sha) => Some(sha),
                    Err(err) => {
                        tracing::warn!(%err, "checkpoint failed");
                        None
                    }
                }
            })
            .await
            .ok()
            .flatten();
            if let Some(tx) = tx.upgrade() {
                let _ = tx.send(ThreadCmd::Checkpoint {
                    turn_id,
                    before,
                    sha,
                });
            }
        });
    }

    fn on_checkpoint(&mut self, turn_id: TurnId, before: bool, sha: Option<String>) {
        if before {
            let Some(turn) = self.turn.as_mut().filter(|t| t.id == turn_id) else {
                return;
            };
            if sha.is_some() {
                turn.checkpoint_before = sha;
                let turn = turn.clone();
                let _ = self.core.commit(vec![Event::TurnUpdated { turn }]);
            }
            let Some((text, attachments)) = self.pending_start.take() else {
                return;
            };
            let text = match self.pending_note.take() {
                Some(note) => format!("{note}\n\n{text}"),
                None => text,
            };
            let images = attachments
                .into_iter()
                .map(|a| loft_harness::ImageInput {
                    path: a.path.into(),
                    mime: a.mime,
                })
                .collect();
            let sent = self
                .session
                .as_ref()
                .map(|s| {
                    s.commands
                        .send(SessionCommand::StartTurn(TurnInput { text, images }))
                        .is_ok()
                })
                .unwrap_or(false);
            if !sent {
                let _ = self.finish_turn(
                    TurnStatus::Failed,
                    None,
                    Some("The agent session closed before the turn started.".into()),
                );
            }
        } else if let Some(sha) = sha {
            let turn = self
                .core
                .with_store(|s| s.turns(&self.thread.id))
                .ok()
                .and_then(|turns| turns.into_iter().find(|t| t.id == turn_id));
            if let Some(mut turn) = turn {
                turn.checkpoint_after = Some(sha);
                let _ = self.core.commit(vec![Event::TurnUpdated { turn }]);
            }
        }
    }

    fn respond(&mut self, request_id: RequestId, decision: DecisionKind) -> CoreResult<Response> {
        let native = self
            .requests
            .get(&request_id)
            .cloned()
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "request is not open"))?;
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| ErrorBody::new(ErrorCode::Conflict, "session is not running"))?;
        session
            .commands
            .send(SessionCommand::Respond {
                native_request_id: native.clone(),
                decision,
            })
            .map_err(|_| internal("harness session closed"))?;
        self.resolve_request(&native, Some(decision))?;
        Ok(Response::Ok)
    }

    /// For a native fork's first session: the parent's provider session.
    fn native_fork_source(&self) -> Option<String> {
        let parent = self.thread.parent.as_ref()?;
        if self.thread.native_session_id.is_some() || parent.kind != ForkKind::Native {
            return None;
        }
        self.core
            .with_store(|s| s.thread(&parent.thread_id))
            .ok()
            .flatten()
            .and_then(|p| p.native_session_id)
    }

    fn ensure_session(&mut self) -> CoreResult<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let harness = self.core.harness(&self.thread.harness).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Harness,
                format!("harness `{}` is not available", self.thread.harness),
            )
        })?;
        let opts = self.core.opts();
        let hcfg = crate::harness_config(&self.core.config(), &self.thread.harness);
        let new_session_id = (self.thread.native_session_id.is_none()
            && harness.capabilities().client_session_id)
            .then(|| uuid::Uuid::new_v4().to_string());
        let cfg = SessionConfig {
            thread_label: self.thread.id.to_string(),
            cwd: self.thread.cwd.clone().into(),
            model: self.thread.model.clone(),
            permission_mode: self.thread.permission_mode,
            resume: self.thread.native_session_id.clone(),
            new_session_id: new_session_id.clone(),
            fork_from: self.native_fork_source(),
            env: opts.env.clone(),
            binary: (hcfg.path != "auto").then(|| hcfg.path.clone().into()),
            extra_args: hcfg.extra_args.clone(),
            mcp_servers: opts
                .mcp_bridge
                .as_ref()
                .map(|b| {
                    vec![loft_harness::McpServer {
                        name: "agentloft".into(),
                        command: b.exe.to_string_lossy().into_owned(),
                        args: vec![
                            "mcp".into(),
                            "--thread".into(),
                            self.thread.id.to_string(),
                            "--home".into(),
                            b.home.to_string_lossy().into_owned(),
                        ],
                    }]
                })
                .unwrap_or_default(),
            // Our own tools (browser panel, thread reading) don't need prompts.
            allowed_tools: if opts.mcp_bridge.is_some() {
                vec!["mcp__agentloft".into()]
            } else {
                vec![]
            },
        };
        let session = harness
            .open_session(cfg)
            .map_err(|e| ErrorBody::new(ErrorCode::Harness, format!("{e:#}")))?;
        if let Some(pid) = session.pid {
            self.core
                .record_process(pid, self.thread.harness.as_str(), &self.thread.id);
            self.session_pid = Some(pid);
        }
        self.session = Some(session);
        if let Some(id) = new_session_id {
            // Persist the pre-assigned id right away so a crash can resume it.
            self.thread.native_session_id = Some(id);
            self.save_thread()?;
        }
        Ok(())
    }

    async fn stop_session(&mut self) {
        self.flush_dirty();
        if let Some(session) = self.session.take() {
            let _ = session.commands.send(SessionCommand::Shutdown);
            // Give the adapter a moment to close stdin and stop the child.
            let mut events = session.events;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(8), async {
                while events.recv().await.is_some() {}
            })
            .await;
        }
        if let Some(pid) = self.session_pid.take() {
            self.core.forget_process(pid);
        }
    }

    // -- harness events -----------------------------------------------------

    fn on_harness_event(&mut self, event: HarnessEvent) {
        if let Err(err) = self.apply_harness_event(event) {
            tracing::error!(thread = %self.thread.id, %err, "failed to apply harness event");
        }
    }

    fn apply_harness_event(&mut self, event: HarnessEvent) -> CoreResult<()> {
        match event {
            HarnessEvent::SessionReady {
                native_session_id,
                model,
                ..
            } => {
                let mut changed = false;
                if !native_session_id.is_empty()
                    && self.thread.native_session_id.as_deref() != Some(&native_session_id)
                {
                    self.thread.native_session_id = Some(native_session_id);
                    changed = true;
                }
                if self.thread.model.is_none() && model.is_some() {
                    self.thread.model = model;
                    changed = true;
                }
                if changed {
                    self.save_thread()?;
                }
            }
            HarnessEvent::TurnStarted => {}
            HarnessEvent::ItemStarted {
                native_id,
                body,
                parent,
            } => {
                let mut item = self.new_item(self.turn_id(), body, ItemStatus::InProgress)?;
                item.parent_id = parent.and_then(|p| self.items.get(&p).cloned());
                self.items.insert(native_id, item.id.clone());
                self.core.live().insert(item.id.clone(), item.clone());
                self.core.commit(vec![Event::ItemUpserted { item }])?;
            }
            HarnessEvent::ItemDelta {
                native_id,
                channel,
                text,
            } => self.on_delta(&native_id, channel, text),
            HarnessEvent::ItemCompleted {
                native_id,
                body,
                tool_result,
                status,
            } => {
                let item_id = match self.items.get(&native_id) {
                    Some(id) => id.clone(),
                    None => return Ok(()),
                };
                let mut item = match self.core.live().remove(&item_id) {
                    Some(item) => item,
                    None => match self.load_item(&item_id)? {
                        Some(item) => item,
                        None => return Ok(()),
                    },
                };
                self.dirty.remove(&item_id);
                if let Some(body) = body {
                    item.body = body;
                }
                if let (
                    Some(result),
                    ItemBody::ToolCall {
                        output, is_error, ..
                    },
                ) = (tool_result, &mut item.body)
                {
                    *output = Some(result.output);
                    *is_error = result.is_error;
                }
                item.status = status;
                item.updated_at_ms = now_ms();
                self.core.commit(vec![Event::ItemUpserted { item }])?;
            }
            HarnessEvent::RequestOpened {
                native_id,
                kind,
                title,
                detail,
                options,
                related_item,
            } => {
                let request = RuntimeRequest {
                    id: RequestId::new(),
                    thread_id: self.thread.id.clone(),
                    turn_id: self.turn_id(),
                    item_id: related_item.and_then(|n| self.items.get(&n).cloned()),
                    kind,
                    title,
                    detail,
                    options,
                    status: RequestStatus::Open,
                    decision: None,
                    opened_at_ms: now_ms(),
                };
                self.requests.insert(request.id.clone(), native_id);
                self.thread.status = ThreadStatus::WaitingApproval;
                self.thread.updated_at_ms = now_ms();
                self.core.commit(vec![
                    Event::RequestOpened { request },
                    Event::ThreadUpdated {
                        thread: self.thread.clone(),
                    },
                ])?;
            }
            HarnessEvent::RequestResolved { native_id } => {
                self.resolve_request(&native_id, None)?;
            }
            HarnessEvent::TurnCompleted {
                status,
                usage,
                error,
            } => self.finish_turn(status, usage, error)?,
            HarnessEvent::Notice { level, text } => {
                let item = self.new_item(
                    self.turn_id(),
                    ItemBody::Notice { level, text },
                    ItemStatus::Completed,
                )?;
                self.core.commit(vec![Event::ItemUpserted { item }])?;
            }
            HarnessEvent::Exited { code, stderr_tail } => {
                self.session = None;
                self.on_session_lost(code, stderr_tail);
            }
        }
        Ok(())
    }

    fn on_delta(&mut self, native_id: &str, channel: DeltaChannel, text: String) {
        let Some(item_id) = self.items.get(native_id).cloned() else {
            return;
        };
        let offset = {
            let mut live = self.core.live();
            let Some(item) = live.get_mut(&item_id) else {
                return;
            };
            let Some(buf) = item.body.stream_text_mut(channel) else {
                return;
            };
            let offset = buf.len();
            buf.push_str(&text);
            item.updated_at_ms = now_ms();
            offset
        };
        self.dirty.insert(item_id.clone());
        self.core.publish_delta(Delta {
            thread_id: self.thread.id.clone(),
            item_id,
            channel,
            offset,
            text,
        });
    }

    fn on_session_lost(&mut self, code: Option<i32>, stderr_tail: String) {
        self.session = None;
        if let Some(pid) = self.session_pid.take() {
            self.core.forget_process(pid);
        }
        if self.turn.is_some() {
            let mut msg = match code {
                Some(c) => format!("The agent process exited unexpectedly (code {c})."),
                None => "The agent process exited unexpectedly.".to_owned(),
            };
            let tail = stderr_tail.trim();
            if !tail.is_empty() {
                msg.push_str("\n\n");
                msg.push_str(tail);
            }
            if let Err(err) = self.finish_turn(TurnStatus::Failed, None, Some(msg)) {
                tracing::error!(%err, "failed to record session loss");
            }
        }
    }

    fn finish_turn(
        &mut self,
        status: TurnStatus,
        usage: Option<loft_protocol::Usage>,
        error: Option<String>,
    ) -> CoreResult<()> {
        let mut events = Vec::new();
        // Close anything still streaming.
        for item_id in self.items.values() {
            if let Some(mut item) = self.core.live().remove(item_id) {
                item.status = if status == TurnStatus::Failed {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Completed
                };
                events.push(Event::ItemUpserted { item });
            }
        }
        self.dirty.clear();
        self.items.clear();
        // Expire approvals that were never answered.
        let open: Vec<String> = self.requests.drain().map(|(_, n)| n).collect();
        for native in open {
            if let Some(mut request) = self.find_open_request(&native)? {
                request.status = RequestStatus::Expired;
                events.push(Event::RequestUpdated { request });
            }
        }
        if let Some(error) = error.clone().filter(|_| status == TurnStatus::Failed) {
            let item = self.new_item(
                self.turn_id(),
                ItemBody::Error { text: error },
                ItemStatus::Completed,
            )?;
            events.push(Event::ItemUpserted { item });
        }
        self.pending_start = None;
        let mut finished = None;
        if let Some(mut turn) = self.turn.take() {
            turn.status = status;
            turn.usage = usage;
            turn.error = error;
            turn.ended_at_ms = Some(now_ms());
            finished = Some(turn.clone());
            events.push(Event::TurnUpdated { turn });
        }
        self.thread.status = ThreadStatus::Idle;
        self.thread.updated_at_ms = now_ms();
        events.push(Event::ThreadUpdated {
            thread: self.thread.clone(),
        });
        self.core.commit(events)?;
        if let Some(turn) = finished.filter(|t| t.checkpoint_before.is_some()) {
            self.capture_checkpoint(&turn, false);
        }
        Ok(())
    }

    // -- helpers ------------------------------------------------------------

    fn turn_id(&self) -> Option<TurnId> {
        self.turn.as_ref().map(|t| t.id.clone())
    }

    fn new_item(
        &self,
        turn_id: Option<TurnId>,
        body: ItemBody,
        status: ItemStatus,
    ) -> CoreResult<Item> {
        let ordinal = self
            .core
            .with_store(|s| s.next_item_ordinal(&self.thread.id))?;
        // Live (uncommitted-ordinal) items must also be counted.
        let live_max = self
            .core
            .live()
            .values()
            .filter(|i| i.thread_id == self.thread.id)
            .map(|i| i.ordinal + 1)
            .max()
            .unwrap_or(0);
        Ok(Item {
            id: ItemId::new(),
            thread_id: self.thread.id.clone(),
            turn_id,
            ordinal: ordinal.max(live_max),
            status,
            body,
            updated_at_ms: now_ms(),
            parent_id: None,
        })
    }

    fn load_item(&self, id: &ItemId) -> CoreResult<Option<Item>> {
        Ok(self
            .core
            .with_store(|s| s.items(&self.thread.id))?
            .into_iter()
            .find(|i| &i.id == id))
    }

    fn find_open_request(&self, native: &str) -> CoreResult<Option<RuntimeRequest>> {
        // Map native → app id through our table first.
        let app_id = self
            .requests
            .iter()
            .find(|(_, n)| n.as_str() == native)
            .map(|(id, _)| id.clone());
        let requests = self.core.with_store(|s| s.requests(&self.thread.id))?;
        Ok(requests.into_iter().find(|r| {
            r.status == RequestStatus::Open && app_id.as_ref().is_none_or(|id| &r.id == id)
        }))
    }

    fn resolve_request(&mut self, native: &str, decision: Option<DecisionKind>) -> CoreResult<()> {
        let Some(mut request) = self.find_open_request(native)? else {
            return Ok(());
        };
        self.requests.retain(|_, n| n != native);
        request.status = RequestStatus::Resolved;
        if decision.is_some() {
            request.decision = decision;
        }
        let mut events = vec![Event::RequestUpdated { request }];
        if self.requests.is_empty() && self.thread.status == ThreadStatus::WaitingApproval {
            self.thread.status = if self.turn.is_some() {
                ThreadStatus::Running
            } else {
                ThreadStatus::Idle
            };
            self.thread.updated_at_ms = now_ms();
            events.push(Event::ThreadUpdated {
                thread: self.thread.clone(),
            });
        }
        self.core.commit(events)?;
        Ok(())
    }

    fn flush_dirty(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        let items: Vec<Item> = {
            let live = self.core.live();
            self.dirty
                .drain()
                .filter_map(|id| live.get(&id).cloned())
                .collect()
        };
        let _ = self.core.with_store(|s| {
            for item in &items {
                s.snapshot_item(item)?;
            }
            Ok(())
        });
    }

    fn save_thread(&mut self) -> CoreResult<()> {
        self.thread.updated_at_ms = now_ms();
        self.core.commit(vec![Event::ThreadUpdated {
            thread: self.thread.clone(),
        }])?;
        Ok(())
    }
}

async fn next_event(session: &mut Option<SessionHandle>) -> Option<HarnessEvent> {
    match session {
        Some(s) => s.events.recv().await,
        None => std::future::pending().await,
    }
}

fn title_from(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("New thread")
        .trim();
    let mut title: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        title.push('…');
    }
    title
}
