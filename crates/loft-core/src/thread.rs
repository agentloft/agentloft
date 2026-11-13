//! One actor per active thread. It owns the harness session, maps native ids
//! to app ids, turns harness events into domain events, and coalesces
//! streaming deltas into periodic item snapshots.

use crate::{Core, CoreResult, internal};
use loft_harness::{HarnessEvent, SessionCommand, SessionConfig, SessionHandle, TurnInput};
use loft_protocol::{
    AttachmentRef, DecisionKind, Delta, DeltaChannel, ErrorBody, ErrorCode, Event, ForkKind, Item,
    ItemBody, ItemId, ItemStatus, LimitStop, PermissionMode, QueuedMessage, RequestId, RequestKind,
    RequestStatus, Response, RuntimeRequest, Thread, ThreadStatus, Turn, TurnId, TurnStatus,
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
    Steer {
        text: String,
        attachments: Vec<AttachmentRef>,
        reply: Reply,
    },
    Interrupt {
        reply: Reply,
    },
    /// Hold a message until the running turn completes (see
    /// `Request::Enqueue`).
    Enqueue {
        text: String,
        attachments: Vec<AttachmentRef>,
        reply: Reply,
    },
    Dequeue {
        id: u64,
        reply: Reply,
    },
    MoveQueued {
        id: u64,
        to: u64,
        reply: Reply,
    },
    SendQueued {
        id: u64,
        reply: Reply,
    },
    /// Carry on when the usage limit resets, or don't.
    ResumeAtReset {
        on: bool,
        reply: Reply,
    },
    /// Internal: the usage limit that stopped this thread has reset.
    LimitReset,
    /// The agent's `show_ui` call, as its bridge saw it: answer with the
    /// item that holds it (`{ item_id }`). `make`: if the harness reported
    /// no call of its own (it ran inside some other tool), add an item for
    /// it; otherwise `item_id` is null and the caller asks again.
    ShowView {
        title: Option<String>,
        html: String,
        make: bool,
        reply: Reply,
    },
    StopSubagent {
        item_id: ItemId,
        reply: Reply,
    },
    AgentInfo {
        kind: loft_protocol::AgentInfoKind,
        reply: Reply,
    },
    Respond {
        request_id: RequestId,
        decision: DecisionKind,
        /// Answers to a question, or feedback on a plan.
        answer: Option<loft_protocol::RequestReply>,
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
    SetEffort {
        effort: Option<String>,
        reply: Reply,
    },
    Rename {
        title: String,
        reply: Reply,
    },
    SetDraft {
        draft: Option<String>,
        reply: Reply,
    },
    /// Internal: an agent named the thread. Applied only while the title
    /// is still `placeholder` (a rename in between wins).
    GeneratedTitle {
        placeholder: String,
        title: String,
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
        /// After a turn: the files it changed.
        changes: Option<Vec<loft_protocol::DiffFileStat>>,
    },
    /// Internal: what GitHub says about the thread's pull request
    /// (`crate::pull_requests`). Saved only if it differs.
    SetPullRequest {
        pull_request: loft_protocol::PullRequest,
        reply: Reply,
    },
    /// Internal: whether the worktree's work is in its base branch
    /// (`crate::cleanup`). Saved only if it changed.
    SetLanded {
        landed: bool,
        reply: Reply,
    },
}

pub(crate) type ThreadHandle = mpsc::UnboundedSender<ThreadCmd>;

pub(crate) fn spawn(core: Arc<Core>, thread: Thread) -> ThreadHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let queue = core.with_store(|s| s.queue(&thread.id)).unwrap_or_default();
    let actor = ThreadActor {
        queue,
        limit_hit: None,
        resumed: false,
        send_next: false,
        self_tx: tx.downgrade(),
        pending_start: None,
        after_compact: None,
        session_model: None,
        pending_note: None,
        core,
        thread,
        session: None,
        session_pid: None,
        restart_session: false,
        turn: None,
        items: HashMap::new(),
        subagents: HashMap::new(),
        requests: HashMap::new(),
        dirty: HashSet::new(),
    };
    tokio::spawn(actor.run(rx));
    tx
}

struct ThreadActor {
    /// Messages waiting for the running turn to complete, in order
    /// (mirrored in the store on every change).
    queue: Vec<QueuedMessage>,
    /// The agent said its usage limit is what ends the running turn.
    limit_hit: Option<loft_harness::LimitHit>,
    /// The running turn is one started by a limit's reset.
    resumed: bool,
    /// A turn completed with messages queued: send the first.
    send_next: bool,
    self_tx: mpsc::WeakUnboundedSender<ThreadCmd>,
    /// Prompt waiting for the "before" checkpoint before it is sent.
    pending_start: Option<(String, Vec<AttachmentRef>)>,
    /// The user's message, held while the conversation is compacted first
    /// (it had sat idle): it goes out, in the same turn, once that's done.
    after_compact: Option<(String, Vec<AttachmentRef>)>,
    /// The model the running session said it uses (recorded on each turn).
    session_model: Option<String>,
    /// Said to the agent ahead of the next prompt (e.g. after a revert).
    pending_note: Option<String>,
    core: Arc<Core>,
    thread: Thread,
    session: Option<SessionHandle>,
    session_pid: Option<u32>,
    /// A setting the running session can't take live changed: restart it
    /// (resuming the conversation) once it's idle.
    restart_session: bool,
    turn: Option<Turn>,
    /// native item id → app item id (current session).
    items: HashMap<String, ItemId>,
    /// Subagents (native id → item) for the whole session: background ones
    /// keep working after their turn ends, and their steps must still find
    /// their parent.
    subagents: HashMap<String, ItemId>,
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
                // Only while there is live text to save: an idle thread's
                // actor sleeps until a command or an event, instead of
                // waking four times a second for as long as the daemon runs.
                _ = flush.tick(), if !self.dirty.is_empty() => self.flush_dirty(),
            }
            if self.restart_session && self.turn.is_none() && self.pending_start.is_none() {
                self.restart_session = false;
                self.stop_session().await;
            }
            if std::mem::take(&mut self.send_next) {
                self.send_next_queued();
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
            ThreadCmd::Steer {
                text,
                attachments,
                reply,
            } => {
                let _ = reply.send(self.steer(text, attachments));
            }
            ThreadCmd::Enqueue {
                text,
                attachments,
                reply,
            } => {
                let _ = reply.send(self.enqueue(text, attachments));
            }
            ThreadCmd::Dequeue { id, reply } => {
                self.queue.retain(|m| m.id != id);
                let _ = reply.send(self.save_queue().map(|_| Response::Ok));
            }
            ThreadCmd::MoveQueued { id, to, reply } => {
                let res = if move_in_line(&mut self.queue, id, to) {
                    self.save_queue()
                } else {
                    Ok(())
                };
                let _ = reply.send(res.map(|_| Response::Ok));
            }
            ThreadCmd::SendQueued { id, reply } => {
                let _ = reply.send(self.send_queued(id));
            }
            ThreadCmd::ResumeAtReset { on, reply } => {
                let _ = reply.send(self.resume_at_reset(on));
            }
            ThreadCmd::LimitReset => self.on_limit_reset(),
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
                changes,
            } => self.on_checkpoint(turn_id, before, sha, changes),
            ThreadCmd::SetDraft { draft, reply } => {
                // Not a change worth re-sorting the sidebar for.
                let res = if self.thread.draft == draft {
                    Ok(Response::Ok)
                } else {
                    self.thread.draft = draft;
                    // Keep `updated_at_ms`: typing mustn't re-sort the sidebar.
                    self.core
                        .commit(vec![Event::ThreadUpdated {
                            thread: self.thread.clone(),
                        }])
                        .map(|_| Response::Ok)
                };
                let _ = reply.send(res);
            }
            ThreadCmd::Rename { title, reply } => {
                self.thread.title = title;
                self.thread.updated_at_ms = now_ms();
                let res = self.save_thread().map(|_| Response::Thread {
                    thread: self.thread.clone(),
                });
                let _ = reply.send(res);
            }
            ThreadCmd::GeneratedTitle { placeholder, title } => {
                if self.thread.title == placeholder && title != placeholder {
                    self.thread.title = title;
                    // Keep `updated_at_ms`: a new name mustn't re-sort the
                    // sidebar.
                    if let Err(err) = self.core.commit(vec![Event::ThreadUpdated {
                        thread: self.thread.clone(),
                    }]) {
                        tracing::warn!(?err, "save generated title");
                    }
                }
            }
            ThreadCmd::SetupDone {
                turn_id,
                item_id,
                ok,
                text,
            } => self.on_setup_done(turn_id, item_id, ok, text),
            ThreadCmd::ShowView {
                title,
                html,
                make,
                reply,
            } => {
                let _ = reply.send(self.show_view(title, html, make));
            }
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
            ThreadCmd::AgentInfo { kind, reply } => {
                // Starts the agent if it isn't running (it answers at once).
                if let Err(err) = self.ensure_session() {
                    let _ = reply.send(Err(err));
                } else if let Some(s) = &self.session {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = s.commands.send(SessionCommand::Query { kind, reply: tx });
                    tokio::spawn(async move {
                        let res = match tokio::time::timeout(std::time::Duration::from_secs(20), rx)
                            .await
                        {
                            Ok(Ok(Ok(info))) => Ok(Response::AgentInfo { info }),
                            Ok(Ok(Err(msg))) => Err(ErrorBody::new(ErrorCode::Harness, msg)),
                            Ok(Err(_)) => Err(ErrorBody::new(
                                ErrorCode::Unsupported,
                                "this agent doesn't report that",
                            )),
                            Err(_) => Err(ErrorBody::new(
                                ErrorCode::Harness,
                                "the agent didn't answer",
                            )),
                        };
                        let _ = reply.send(res);
                    });
                }
            }
            ThreadCmd::StopSubagent { item_id, reply } => {
                let native = self
                    .subagents
                    .iter()
                    .find(|(_, id)| **id == item_id)
                    .map(|(n, _)| n.clone());
                let res = match (&self.session, native) {
                    (Some(s), Some(native)) => {
                        let _ = s.commands.send(SessionCommand::StopSubagent(native));
                        // Mark it stopped (shown as such, never as done).
                        let item = match self.core.live().remove(&item_id) {
                            Some(item) => Some(item),
                            None => self.load_item(&item_id).ok().flatten(),
                        };
                        if let Some(mut item) = item {
                            item.status = ItemStatus::Declined;
                            item.updated_at_ms = now_ms();
                            let _ = self.core.commit(vec![Event::ItemUpserted { item }]);
                        }
                        Ok(Response::Ok)
                    }
                    _ => Err(ErrorBody::new(
                        ErrorCode::Conflict,
                        "that subagent isn't running",
                    )),
                };
                let _ = reply.send(res);
            }
            ThreadCmd::Respond {
                request_id,
                decision,
                answer,
                reply,
            } => {
                let _ = reply.send(self.respond(request_id, decision, answer));
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
                // What the session said it ran no longer holds.
                self.session_model = None;
                self.thread.model = model.clone();
                if let Some(s) = &self.session {
                    let _ = s.commands.send(SessionCommand::SetModel(model));
                }
                let res = self.save_thread().map(|_| Response::Ok);
                let _ = reply.send(res);
            }
            ThreadCmd::SetEffort { effort, reply } => {
                let live = self
                    .core
                    .harness(&self.thread.harness)
                    .is_some_and(|h| h.capabilities().live_effort_switch);
                if self.thread.effort != effort
                    && let Some(s) = &self.session
                {
                    if live {
                        let _ = s.commands.send(SessionCommand::SetEffort(effort.clone()));
                    } else {
                        self.restart_session = true;
                    }
                }
                self.thread.effort = effort;
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
            ThreadCmd::SetPullRequest {
                pull_request,
                reply,
            } => {
                let res = crate::pull_requests::apply(&self.core, &mut self.thread, pull_request);
                let _ = reply.send(res);
            }
            ThreadCmd::SetLanded { landed, reply } => {
                // Not over a turn that has started since the look began.
                let res = if self.thread.landed == landed || (landed && self.turn.is_some()) {
                    Ok(Response::Ok)
                } else {
                    // Without touching `updated_at_ms`: the sidebar keeps
                    // its order.
                    self.thread.landed = landed;
                    self.core
                        .commit(vec![Event::ThreadUpdated {
                            thread: self.thread.clone(),
                        }])
                        .map(|_| Response::Ok)
                };
                let _ = reply.send(res);
            }
            ThreadCmd::Shutdown { .. } => unreachable!("handled in run loop"),
        }
    }

    // -- the queue ----------------------------------------------------------

    /// Tell the store and whoever watches the thread what the queue is now.
    fn save_queue(&self) -> CoreResult<()> {
        self.core
            .commit(vec![Event::QueueUpdated {
                thread_id: self.thread.id.clone(),
                queue: self.queue.clone(),
            }])
            .map(|_| ())
    }

    fn enqueue(&mut self, text: String, attachments: Vec<AttachmentRef>) -> CoreResult<Response> {
        // Nothing to wait for (the turn ended as this was on its way).
        if self.turn.is_none() && self.queue.is_empty() {
            return self.send_message(text, attachments);
        }
        self.queue.push(QueuedMessage {
            id: self.core.next_queued_id(),
            text,
            attachments,
            queued_at_ms: now_ms(),
        });
        self.save_queue().map(|_| Response::Ok)
    }

    /// A queued message, now: into the running turn if there is one (it
    /// stays queued if the agent can't take it), else as the next turn.
    fn send_queued(&mut self, id: u64) -> CoreResult<Response> {
        let Some(index) = self.queue.iter().position(|m| m.id == id) else {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                "that message is no longer queued",
            ));
        };
        let message = self.queue.remove(index);
        let sent = if self.turn.is_some() {
            self.steer(message.text.clone(), message.attachments.clone())
        } else {
            self.send_message(message.text.clone(), message.attachments.clone())
        };
        if sent.is_err() {
            self.queue.insert(index, message);
            return sent;
        }
        self.save_queue()?;
        sent
    }

    /// A turn completed: the first message in line starts the next one.
    /// (After one that failed or was stopped the queue waits: the user
    /// sends the next when they have seen what happened.)
    fn send_next_queued(&mut self) {
        if self.turn.is_some() || self.queue.is_empty() {
            return;
        }
        let next = self.queue.remove(0);
        match self.send_message(next.text.clone(), next.attachments.clone()) {
            Ok(_) => {
                if let Err(err) = self.save_queue() {
                    tracing::error!(thread = %self.thread.id, err = %err.message, "save queue");
                }
            }
            Err(err) => {
                tracing::warn!(thread = %self.thread.id, err = %err.message, "queued message not sent");
                self.queue.insert(0, next);
            }
        }
    }

    // -- usage limits -------------------------------------------------------

    fn resume_at_reset(&mut self, on: bool) -> CoreResult<Response> {
        let Some(limit) = self.thread.limit.as_mut() else {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "this thread isn't stopped by a usage limit",
            ));
        };
        if on && limit.resets_at_ms.is_none() {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "the agent didn't say when the limit resets",
            ));
        }
        if limit.resume != on {
            limit.resume = on;
            let limit = *limit;
            // Not activity: keep `updated_at_ms` (sidebar order).
            self.core.commit(vec![Event::ThreadUpdated {
                thread: self.thread.clone(),
            }])?;
            self.core.limit_changed(&self.thread.id, Some(limit));
        }
        Ok(Response::Ok)
    }

    /// The limit reset and the thread was to carry on: do, unless the user
    /// got there first (a turn since clears the limit).
    fn on_limit_reset(&mut self) {
        if self.turn.is_some() || !self.thread.limit.is_some_and(|l| l.resume) {
            return;
        }
        self.resumed = true;
        if let Err(err) = self.send_message("Continue where you left off.".to_owned(), Vec::new()) {
            self.resumed = false;
            tracing::warn!(thread = %self.thread.id, err = %err.message, "resume after limit reset");
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
        // Worked out before this turn joins the thread's turns. Not for a
        // message that compacts or clears the conversation itself.
        let command = text.split_whitespace().next().unwrap_or_default();
        let compact_first = (!matches!(command, "/compact" | "/clear"))
            .then(|| self.idle_compaction(now_ms()))
            .flatten();
        self.ensure_session()?;
        if self.thread.started_at_ms.is_none() || self.thread.draft.is_some() {
            self.thread.started_at_ms.get_or_insert_with(now_ms);
            self.thread.draft = None;
            self.save_thread()?;
        }

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
            native_turn: None,
            changes: None,
            context: None,
            model: None,
        };
        let user_item = self.new_item(
            Some(turn.id.clone()),
            ItemBody::UserMessage {
                text: text.clone(),
                attachments: attachments.clone(),
            },
            ItemStatus::Completed,
        )?;
        // A new worktree in place of one that was cleaned up: said in the
        // conversation, and to the agent with this prompt.
        let renewed = match self.core.take_pending_note(&self.thread.id) {
            Some((notice, note)) => {
                self.pending_note = Some(match self.pending_note.take() {
                    Some(earlier) => format!("{earlier}\n{note}"),
                    None => note,
                });
                Some(self.new_item(
                    Some(turn.id.clone()),
                    ItemBody::Notice {
                        level: loft_protocol::NoticeLevel::Info,
                        text: notice,
                    },
                    ItemStatus::Completed,
                )?)
            }
            None => None,
        };
        if ordinal == 1 && self.thread.title == "New thread" {
            self.thread.title = title_from(&text);
            self.generate_title(&text, &attachments);
        }
        self.thread.status = ThreadStatus::Running;
        self.thread.updated_at_ms = now;
        // The agent may change the worktree; looked at again when it ends.
        self.thread.landed = false;
        self.limit_hit = None;
        if self.thread.limit.take().is_some() {
            self.core.limit_changed(&self.thread.id, None);
        }
        let mut events = vec![
            Event::TurnStarted { turn: turn.clone() },
            Event::ItemUpserted { item: user_item },
        ];
        events.extend(renewed.map(|item| Event::ItemUpserted { item }));
        events.push(Event::ThreadUpdated {
            thread: self.thread.clone(),
        });
        self.core.commit(events)?;
        self.turn = Some(turn.clone());

        // Compact a conversation that sat idle first; the message follows
        // once that's done.
        let prompt = match compact_first {
            Some(why) => {
                let notice = self.new_item(
                    Some(turn.id.clone()),
                    ItemBody::Notice {
                        level: loft_protocol::NoticeLevel::Info,
                        text: why,
                    },
                    ItemStatus::Completed,
                )?;
                self.core
                    .commit(vec![Event::ItemUpserted { item: notice }])?;
                self.after_compact = Some((text, attachments));
                ("/compact".to_owned(), Vec::new())
            }
            None => (text, attachments),
        };
        // Snapshot the work tree first; the prompt goes out once that's done
        // so the turn's diff covers exactly the agent's changes.
        self.pending_start = Some(prompt);
        // A new worktree's setup runs first, so the turn's checkpoint (and
        // diff) start after it.
        match self.core.take_pending_setup(&self.thread.id) {
            Some(cmd) => self.run_setup(&turn, cmd)?,
            None => self.capture_checkpoint(&turn, true),
        }
        Ok(Response::Ok)
    }

    /// A message into the running turn, without stopping it: it shows in
    /// the turn now, and the agent takes it in at its next step. With no
    /// turn running it's an ordinary message.
    fn steer(&mut self, text: String, attachments: Vec<AttachmentRef>) -> CoreResult<Response> {
        let Some(turn) = self.turn.clone() else {
            return self.send_message(text, attachments);
        };
        let can_steer = self
            .core
            .harness(&self.thread.harness)
            .is_some_and(|h| h.capabilities().steer);
        // Not sent yet (the checkpoint is being taken, or the conversation
        // is being compacted first): it joins the prompt.
        let joins_prompt = self.pending_start.is_some() || self.after_compact.is_some();
        if !can_steer && !joins_prompt {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "this agent can't take a message mid-turn; it will be sent when the turn ends",
            ));
        }
        let item = self.new_item(
            Some(turn.id.clone()),
            ItemBody::UserMessage {
                text: text.clone(),
                attachments: attachments.clone(),
            },
            ItemStatus::Completed,
        )?;
        self.thread.updated_at_ms = now_ms();
        self.core.commit(vec![
            Event::ItemUpserted { item },
            Event::ThreadUpdated {
                thread: self.thread.clone(),
            },
        ])?;
        if let Some((prompt, images)) = self.after_compact.as_mut().or(self.pending_start.as_mut())
        {
            prompt.push_str("\n\n");
            prompt.push_str(&text);
            images.extend(attachments);
            return Ok(Response::Ok);
        }
        let images = attachments
            .into_iter()
            .map(|a| loft_harness::ImageInput {
                path: a.path.into(),
                mime: a.mime,
            })
            .collect();
        let sent = self.session.as_ref().is_some_and(|s| {
            s.commands
                .send(SessionCommand::Steer(TurnInput { text, images }))
                .is_ok()
        });
        if !sent {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "the agent session closed; the message wasn't sent",
            ));
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
        let start = turn.checkpoint_before.clone().filter(|_| !before);
        tokio::spawn(async move {
            let (sha, changes) = tokio::task::spawn_blocking(move || {
                loft_git::repo_root(&dir)?;
                let sha = match loft_git::checkpoint(&dir, &ref_name, &message) {
                    Ok(sha) => sha,
                    Err(err) => {
                        tracing::warn!(%err, "checkpoint failed");
                        return None;
                    }
                };
                // What the turn changed, for its "changed files" card.
                let changes = start.and_then(|start| {
                    loft_git::turn_diff(&dir, &start, &sha, false)
                        .inspect_err(|err| tracing::warn!(%err, "turn diff stat failed"))
                        .ok()
                        .map(|d| d.files.into_iter().map(file_stat).collect())
                });
                Some((Some(sha), changes))
            })
            .await
            .ok()
            .flatten()
            .unwrap_or((None, None));
            if let Some(tx) = tx.upgrade() {
                let _ = tx.send(ThreadCmd::Checkpoint {
                    turn_id,
                    before,
                    sha,
                    changes,
                });
            }
        });
    }

    fn on_checkpoint(
        &mut self,
        turn_id: TurnId,
        before: bool,
        sha: Option<String>,
        changes: Option<Vec<loft_protocol::DiffFileStat>>,
    ) {
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
            // A note waits for the user's message, not `/compact`.
            let note = if self.after_compact.is_some() {
                None
            } else {
                self.pending_note.take()
            };
            self.start_prompt(text, attachments, note);
        } else if let Some(sha) = sha {
            let turn = self
                .core
                .with_store(|s| s.turns(&self.thread.id))
                .ok()
                .and_then(|turns| turns.into_iter().find(|t| t.id == turn_id));
            if let Some(mut turn) = turn {
                turn.checkpoint_after = Some(sha);
                turn.changes = changes;
                let _ = self.core.commit(vec![Event::TurnUpdated { turn }]);
            }
        }
    }

    /// Send `text` to the agent as the running turn's prompt (after `note`).
    fn start_prompt(
        &mut self,
        text: String,
        attachments: Vec<AttachmentRef>,
        note: Option<String>,
    ) {
        let text = match note {
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
        let sent = self.session.as_ref().is_some_and(|s| {
            s.commands
                .send(SessionCommand::StartTurn(TurnInput { text, images }))
                .is_ok()
        });
        if !sent {
            let _ = self.finish_turn(
                TurnStatus::Failed,
                None,
                Some("The agent session closed before the turn started.".into()),
            );
        }
    }

    /// Why a message now should compact the conversation first, if it
    /// should: on (`threads.compact_when_idle`), an agent that can, idle
    /// past the prompt cache, and a context worth shrinking.
    fn idle_compaction(&self, now: i64) -> Option<String> {
        let cfg = self.core.config().threads;
        if !cfg.compact_when_idle
            || !self
                .core
                .harness(&self.thread.harness)
                .is_some_and(|h| h.capabilities().compact)
        {
            return None;
        }
        let turns = self.core.with_store(|s| s.turns(&self.thread.id)).ok()?;
        let idle_ms = now - turns.last()?.ended_at_ms?;
        if idle_ms < i64::from(cfg.compact_idle_minutes) * 60_000 {
            return None;
        }
        let tokens = turns.iter().rev().find_map(|t| t.context)?.tokens;
        if tokens < cfg.compact_min_tokens.max(1) {
            return None;
        }
        Some(format!(
            "Compacting first: idle for {}, {} tokens in context",
            idle_for(idle_ms),
            short_tokens(tokens)
        ))
    }

    /// The compaction before a held message ended: send the message.
    fn after_compaction(
        &mut self,
        status: TurnStatus,
        error: Option<String>,
        text: String,
        attachments: Vec<AttachmentRef>,
    ) -> CoreResult<()> {
        if status != TurnStatus::Completed {
            let why = error.unwrap_or_else(|| "the agent didn't say why".into());
            let item = self.new_item(
                self.turn_id(),
                ItemBody::Notice {
                    level: loft_protocol::NoticeLevel::Warning,
                    text: format!("Couldn't compact ({why}); sending the message as it is"),
                },
                ItemStatus::Completed,
            )?;
            self.core.commit(vec![Event::ItemUpserted { item }])?;
        }
        let note = self.pending_note.take();
        self.start_prompt(text, attachments, note);
        Ok(())
    }

    fn respond(
        &mut self,
        request_id: RequestId,
        decision: DecisionKind,
        answer: Option<loft_protocol::RequestReply>,
    ) -> CoreResult<Response> {
        let native = self
            .requests
            .get(&request_id)
            .cloned()
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "request is not open"))?;
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| ErrorBody::new(ErrorCode::Conflict, "session is not running"))?;
        let plan = self
            .find_open_request(&native)?
            .is_some_and(|r| r.kind == RequestKind::Plan);
        session
            .commands
            .send(SessionCommand::Respond {
                native_request_id: native.clone(),
                decision,
                reply: answer.clone(),
            })
            .map_err(|_| internal("harness session closed"))?;
        self.resolve_request_with(&native, Some(decision), answer)?;
        // An approved plan ends planning: the thread leaves read-only mode
        // (edits asked about, or accepted with "approve & accept edits").
        if plan {
            let mode = match decision {
                DecisionKind::AllowSession => Some(PermissionMode::AcceptEdits),
                DecisionKind::AllowOnce => Some(PermissionMode::AskAll),
                _ => None,
            };
            let current = self.thread.permission_mode;
            let keep = matches!(
                current,
                Some(PermissionMode::Auto | PermissionMode::FullAccess)
            ) || (current == Some(PermissionMode::AcceptEdits)
                && mode == Some(PermissionMode::AskAll));
            if let Some(mode) = mode.filter(|_| !keep) {
                self.thread.permission_mode = Some(mode);
                self.save_thread()?;
            }
            // The agent leaves plan mode for the thread's mode (one it
            // entered planning from on its own, e.g. Full access, stays).
            if mode.is_some()
                && let Some(s) = &self.session
            {
                let _ = s.commands.send(SessionCommand::SetPermissionMode(
                    self.thread.permission_mode,
                ));
            }
        }
        Ok(Response::Ok)
    }

    /// For a native fork's first session: the parent's provider session.
    /// For a native fork's first session: the parent's session and the
    /// turn marker to fork through (`None` = all of it).
    fn native_fork_source(&self) -> (Option<String>, Option<String>) {
        let Some(parent) = self.thread.parent.as_ref() else {
            return (None, None);
        };
        if self.thread.native_session_id.is_some() || parent.kind != ForkKind::Native {
            return (None, None);
        }
        let session = self
            .core
            .with_store(|s| s.thread(&parent.thread_id))
            .ok()
            .flatten()
            .and_then(|p| p.native_session_id);
        let at = parent.turn_id.as_ref().and_then(|turn_id| {
            self.core
                .with_store(|s| s.turns(&parent.thread_id))
                .ok()?
                .into_iter()
                .find(|t| &t.id == turn_id)?
                .native_turn
        });
        (session, at)
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
        let (fork_from, fork_at) = self.native_fork_source();
        let cfg = SessionConfig {
            thread_label: self.thread.id.to_string(),
            cwd: self.thread.cwd.clone().into(),
            model: self.thread.model.clone(),
            effort: self.thread.effort.clone(),
            permission_mode: self.thread.permission_mode,
            resume: self.thread.native_session_id.clone(),
            new_session_id: new_session_id.clone(),
            fork_from,
            fork_at,
            env: std::sync::Arc::new(crate::agent_env(&opts.env, &hcfg)),
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
            self.background_work_gone();
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
            HarnessEvent::ContextUsed(context) => {
                if let Some(turn) = self.turn.as_mut()
                    && turn.context != Some(context)
                {
                    // Keep a window the agent told us before if it doesn't
                    // repeat it every step.
                    let window = context.window.or(turn.context.and_then(|c| c.window));
                    turn.context = Some(loft_protocol::ContextUse { window, ..context });
                    let turn = turn.clone();
                    self.core.commit(vec![Event::TurnUpdated { turn }])?;
                }
            }
            HarnessEvent::TurnMarker { native_turn } => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.native_turn = Some(native_turn);
                }
            }
            HarnessEvent::SessionReady {
                native_session_id,
                model,
                slash_commands,
                models,
            } => {
                if !models.is_empty() {
                    self.core.set_models(&self.thread.harness, models);
                }
                let mut changed = false;
                if !slash_commands.is_empty() && self.thread.slash_commands != slash_commands {
                    self.thread.slash_commands = slash_commands;
                    changed = true;
                }
                if !native_session_id.is_empty()
                    && self.thread.native_session_id.as_deref() != Some(&native_session_id)
                {
                    self.thread.native_session_id = Some(native_session_id);
                    changed = true;
                }
                if model.is_some() {
                    self.session_model.clone_from(&model);
                }
                if self.thread.model.is_none() && model.is_some() {
                    self.thread.model = model;
                    changed = true;
                }
                if changed {
                    self.save_thread()?;
                }
            }
            HarnessEvent::BackgroundTasks(tasks) => {
                if self.thread.waiting_on != tasks {
                    self.thread.waiting_on = tasks;
                    // Not activity: keep `updated_at_ms` (sidebar order).
                    self.core.commit(vec![Event::ThreadUpdated {
                        thread: self.thread.clone(),
                    }])?;
                }
            }
            HarnessEvent::CommandsAvailable(commands) => {
                if self.thread.command_info != commands {
                    self.thread.command_info = commands;
                    // Not activity: keep `updated_at_ms` (sidebar order).
                    self.core.commit(vec![Event::ThreadUpdated {
                        thread: self.thread.clone(),
                    }])?;
                }
            }
            // A turn the agent started itself (Codex working toward a goal):
            // track it like one we started, without a message from the user.
            HarnessEvent::TurnStarted if self.turn.is_none() && self.pending_start.is_none() => {
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
                    native_turn: None,
                    changes: None,
                    context: None,
                    model: None,
                };
                self.thread.status = ThreadStatus::Running;
                self.thread.updated_at_ms = now;
                self.core.commit(vec![
                    Event::TurnStarted { turn: turn.clone() },
                    Event::ThreadUpdated {
                        thread: self.thread.clone(),
                    },
                ])?;
                self.turn = Some(turn);
            }
            HarnessEvent::TurnStarted => {}
            HarnessEvent::ItemStarted {
                native_id,
                body,
                parent,
            } => {
                let mut item = self.new_item(self.turn_id(), body, ItemStatus::InProgress)?;
                item.parent_id = parent.and_then(|p| {
                    self.items
                        .get(&p)
                        .or_else(|| self.subagents.get(&p))
                        .cloned()
                });
                if matches!(&item.body, ItemBody::ToolCall { tool_kind, .. }
                    if *tool_kind == loft_protocol::ToolKind::Subagent)
                {
                    self.subagents.insert(native_id.clone(), item.id.clone());
                }
                self.items.insert(native_id, item.id.clone());
                self.core.live().insert(item.id.clone(), item.clone());
                self.core.commit(vec![Event::ItemUpserted { item }])?;
            }
            HarnessEvent::ItemDelta {
                native_id,
                channel,
                text,
            } => self.on_delta(&native_id, channel, text),
            HarnessEvent::ToolInput {
                native_id,
                mut input,
                partial,
            } => {
                let Some(item_id) = self.items.get(&native_id).cloned() else {
                    return Ok(());
                };
                let Some(mut item) = self.core.live().get(&item_id).cloned() else {
                    return Ok(());
                };
                let ItemBody::ToolCall {
                    tool, input: slot, ..
                } = &mut item.body
                else {
                    return Ok(());
                };
                let view = loft_view::is_tool(tool);
                if partial && let Some(map) = input.as_object_mut() {
                    // Marks an input that isn't all there yet.
                    map.insert(loft_view::PARTIAL.into(), true.into());
                }
                let html = input["html"].as_str().unwrap_or_default().to_owned();
                *slot = input;
                item.updated_at_ms = now_ms();
                self.core.live().insert(item_id.clone(), item.clone());
                // What is half-written stays in memory; the log gets the
                // input once, whole.
                if !partial {
                    self.core.commit(vec![Event::ItemUpserted { item }])?;
                }
                if view {
                    self.core
                        .view_written(&self.thread.id, &item_id, html, !partial);
                }
            }
            HarnessEvent::ToolOutput { native_id, result } => {
                let Some(item_id) = self.items.get(&native_id).cloned() else {
                    return Ok(());
                };
                let Some(mut item) = self.core.live().get(&item_id).cloned() else {
                    return Ok(());
                };
                if let ItemBody::ToolCall {
                    output, is_error, ..
                } = &mut item.body
                {
                    *output = Some(result.output);
                    *is_error = result.is_error;
                }
                item.updated_at_ms = now_ms();
                self.core.live().insert(item_id, item.clone());
                self.core.commit(vec![Event::ItemUpserted { item }])?;
            }
            HarnessEvent::ItemReopened { native_id } => {
                let Some(item_id) = self
                    .items
                    .get(&native_id)
                    .or_else(|| self.subagents.get(&native_id))
                    .cloned()
                else {
                    return Ok(());
                };
                let item = match self.core.live().get(&item_id).cloned() {
                    Some(item) => Some(item),
                    None => self.load_item(&item_id)?,
                };
                let Some(mut item) = item else {
                    return Ok(());
                };
                item.status = ItemStatus::InProgress;
                item.updated_at_ms = now_ms();
                self.items.insert(native_id, item_id.clone());
                self.core.live().insert(item_id, item.clone());
                self.core.commit(vec![Event::ItemUpserted { item }])?;
            }
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
                    reply: None,
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
            // The compaction ahead of a held message is done: on with it.
            HarnessEvent::TurnCompleted { status, error, .. }
                if status != TurnStatus::Interrupted && self.after_compact.is_some() =>
            {
                let (text, attachments) = self.after_compact.take().unwrap_or_default();
                self.after_compaction(status, error, text, attachments)?;
            }
            HarnessEvent::TurnCompleted {
                status,
                usage,
                error,
            } => self.finish_turn(status, usage, error)?,
            HarnessEvent::LimitReached(hit) => self.limit_hit = Some(hit),
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
        self.background_work_gone();
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

    /// The session is gone and its background work with it: nothing is
    /// waited on any more, and subagents it left working (and their steps)
    /// are closed, since nothing more will come from them.
    fn background_work_gone(&mut self) {
        let mut events = Vec::new();
        if self.turn.is_none() {
            let subagents: HashSet<&ItemId> = self.subagents.values().collect();
            for item_id in self.items.values() {
                if let Some(mut item) = self.core.live().remove(item_id) {
                    item.status = if subagents.contains(item_id) {
                        // Shown as stopped, not as done.
                        ItemStatus::Declined
                    } else {
                        ItemStatus::Failed
                    };
                    item.updated_at_ms = now_ms();
                    events.push(Event::ItemUpserted { item });
                }
            }
            self.items.clear();
            self.dirty.clear();
        }
        self.subagents.clear();
        if !self.thread.waiting_on.is_empty() {
            self.thread.waiting_on.clear();
            events.push(Event::ThreadUpdated {
                thread: self.thread.clone(),
            });
        }
        if !events.is_empty()
            && let Err(err) = self.core.commit(events)
        {
            tracing::error!(%err, "failed to close what the session left open");
        }
    }

    fn finish_turn(
        &mut self,
        status: TurnStatus,
        usage: Option<loft_protocol::Usage>,
        error: Option<String>,
    ) -> CoreResult<()> {
        let mut events = Vec::new();
        // A subagent launched in the background works on after the turn,
        // whichever way that ended: it stays open until the agent says it
        // finished. It is the one call that is still open with its result
        // (a receipt, `HarnessEvent::ToolOutput`) already in. Its steps
        // carry on too: keep them (and their ids) unless the turn was
        // stopped or failed.
        let keep: HashMap<String, ItemId> = {
            let live = self.core.live();
            self.items
                .iter()
                .filter(|(native, id)| {
                    live.get(*id).is_some_and(|i| {
                        let launched = self.subagents.contains_key(*native)
                            && matches!(
                                &i.body,
                                ItemBody::ToolCall {
                                    output: Some(_),
                                    ..
                                }
                            );
                        launched || (i.parent_id.is_some() && status == TurnStatus::Completed)
                    })
                })
                .map(|(n, id)| (n.clone(), id.clone()))
                .collect()
        };
        // Close anything else still streaming.
        for item_id in self.items.values() {
            if keep.values().any(|k| k == item_id) {
                continue;
            }
            if let Some(mut item) = self.core.live().remove(item_id) {
                item.status = if status == TurnStatus::Failed {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Completed
                };
                events.push(Event::ItemUpserted { item });
            }
        }
        self.dirty.retain(|id| keep.values().any(|k| k == id));
        self.items = keep;
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
        self.after_compact = None;
        let mut finished = None;
        if let Some(mut turn) = self.turn.take() {
            turn.status = status;
            turn.usage = usage;
            turn.error = error;
            turn.model = self
                .session_model
                .clone()
                .or_else(|| self.thread.model.clone());
            turn.ended_at_ms = Some(now_ms());
            finished = Some(turn.clone());
            events.push(Event::TurnUpdated { turn });
        }
        self.thread.status = ThreadStatus::Idle;
        self.thread.updated_at_ms = now_ms();
        // Stopped by the usage limit: say so on the thread, and carry on at
        // the reset if that is wanted (the setting, or a turn that was
        // itself such a carrying-on and met the next limit).
        let resumed = std::mem::take(&mut self.resumed);
        let limit = self
            .limit_hit
            .take()
            .filter(|_| status == TurnStatus::Failed)
            .map(|hit| LimitStop {
                resets_at_ms: hit.resets_at_ms,
                resume: hit.resets_at_ms.is_some_and(|at| at > now_ms())
                    && (resumed || self.core.config().threads.resume_at_reset),
            });
        if limit != self.thread.limit {
            self.thread.limit = limit;
            self.core.limit_changed(&self.thread.id, limit);
        }
        events.push(Event::ThreadUpdated {
            thread: self.thread.clone(),
        });
        self.core.commit(events)?;
        crate::pull_requests::turn_ended(&self.core, &self.thread);
        crate::cleanup::turn_ended(&self.core, &self.thread);
        if let Some(turn) = finished.filter(|t| t.checkpoint_before.is_some()) {
            self.capture_checkpoint(&turn, false);
        }
        // The run loop sends it, after a session restart that was waiting
        // for the thread to be idle.
        self.send_next = status == TurnStatus::Completed && !self.queue.is_empty();
        Ok(())
    }

    // -- helpers ------------------------------------------------------------

    fn turn_id(&self) -> Option<TurnId> {
        self.turn.as_ref().map(|t| t.id.clone())
    }

    /// The item that holds a `show_ui` call (see [`ThreadCmd::ShowView`]).
    fn show_view(
        &mut self,
        title: Option<String>,
        html: String,
        make: bool,
    ) -> CoreResult<Response> {
        // The call as the harness reported it: still open, same page.
        let reported = self
            .core
            .live()
            .values()
            .filter(|i| i.thread_id == self.thread.id)
            .find(|i| {
                matches!(&i.body, ItemBody::ToolCall { tool, input, .. }
                    if loft_view::is_tool(tool) && input["html"].as_str() == Some(html.as_str()))
            })
            .map(|i| i.id.clone());
        let item_id = match reported {
            Some(id) => Some(id),
            None if make => {
                let tool = format!("mcp__agentloft__{}", loft_view::TOOL);
                let item = self.new_item(
                    self.turn_id(),
                    ItemBody::ToolCall {
                        title: tool.clone(),
                        tool,
                        tool_kind: loft_protocol::ToolKind::Mcp,
                        input: serde_json::json!({ "title": title, "html": html }),
                        output: Some("Shown in the chat.".into()),
                        is_error: false,
                    },
                    ItemStatus::Completed,
                )?;
                let id = item.id.clone();
                self.core.commit(vec![Event::ItemUpserted { item }])?;
                Some(id)
            }
            None => None,
        };
        Ok(Response::Json {
            value: serde_json::json!({ "item_id": item_id.map(|id| id.0) }),
        })
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
            started_at_ms: Some(now_ms()),
        })
    }

    fn load_item(&self, id: &ItemId) -> CoreResult<Option<Item>> {
        self.core.with_store(|s| s.item(&self.thread.id, id))
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
        self.resolve_request_with(native, decision, None)
    }

    fn resolve_request_with(
        &mut self,
        native: &str,
        decision: Option<DecisionKind>,
        reply: Option<loft_protocol::RequestReply>,
    ) -> CoreResult<()> {
        let Some(mut request) = self.find_open_request(native)? else {
            return Ok(());
        };
        self.requests.retain(|_, n| n != native);
        request.status = RequestStatus::Resolved;
        if decision.is_some() {
            request.decision = decision;
        }
        if reply.is_some() {
            request.reply = reply;
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

impl ThreadActor {
    /// Ask an agent for a title in the background (`[titles]`); the
    /// first-line title stays until it answers, and if it can't.
    fn generate_title(&self, text: &str, attachments: &[AttachmentRef]) {
        let cfg = self.core.config().titles;
        let Some((harness, model)) = crate::titles::writer(&cfg, &self.thread.harness) else {
            return;
        };
        let prompt = crate::titles::prompt(text, attachments, &cfg.instructions);
        let placeholder = self.thread.title.clone();
        let (core, thread_id) = (self.core.clone(), self.thread.id.clone());
        tokio::spawn(async move {
            let title = match core.ask_once(prompt, harness.clone(), model).await {
                Ok(Response::Text { text }) => crate::titles::clean(&text),
                Ok(_) => None,
                Err(err) => {
                    tracing::warn!(harness = %harness.as_str(), err = %err.message, "title generation failed");
                    None
                }
            };
            // The thread's actor may have stopped meanwhile; this wakes it.
            if let Some(title) = title
                && let Ok(handle) = core.thread_handle(&thread_id)
            {
                let _ = handle.send(ThreadCmd::GeneratedTitle { placeholder, title });
            }
        });
    }
}

/// "2 h 5 min", "45 min", "3 days".
fn idle_for(ms: i64) -> String {
    let mins = ms / 60_000;
    match mins {
        m if m >= 2 * 1440 => format!("{} days", m / 1440),
        m if m >= 60 && m % 60 == 0 => format!("{} h", m / 60),
        m if m >= 60 => format!("{} h {} min", m / 60, m % 60),
        m => format!("{m} min"),
    }
}

/// "230K", "1.2M".
fn short_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{}K", n / 1_000),
        n => n.to_string(),
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

pub(crate) fn file_stat(f: loft_git::FileStat) -> loft_protocol::DiffFileStat {
    loft_protocol::DiffFileStat {
        path: f.path,
        added: f.added,
        removed: f.removed,
        binary: f.binary,
    }
}

/// Move message `id` to where `to` is: after it when moving down, before
/// it when moving up. Whether anything moved.
fn move_in_line(queue: &mut Vec<QueuedMessage>, id: u64, to: u64) -> bool {
    let (Some(from), Some(at)) = (
        queue.iter().position(|m| m.id == id),
        queue.iter().position(|m| m.id == to),
    ) else {
        return false;
    };
    if from == at {
        return false;
    }
    let message = queue.remove(from);
    queue.insert(at, message);
    true
}
