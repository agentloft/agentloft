//! One client connection: handshake, requests, subscriptions.
//!
//! The same loop serves both transports: the Unix socket (length-prefixed
//! frames, a local client that may do everything) and the web listener's
//! WebSocket (one frame per message, a remote session held to
//! `loft_core::RemotePolicy`).

use bytes::Bytes;
use futures::stream::SplitSink;
use futures::{SinkExt as _, Stream, StreamExt as _};
use loft_core::{BusMsg, Core, RemotePolicy};
use loft_protocol::{
    ClientMsg, ErrorBody, ErrorCode, MAX_FRAME_LEN, PROTOCOL_VERSION, PageButton, PageInput,
    PageMouse, RemoteAccess, Request, Response, ServerMsg, Snapshot, Topic, Welcome,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// What every connection needs from the daemon.
#[derive(Clone)]
pub(crate) struct Hub {
    pub core: Arc<Core>,
    pub shutdown: watch::Sender<bool>,
    /// Web access, for the local clients that manage it.
    pub web: Option<Arc<crate::web::Web>>,
}

/// Who is on the other end.
#[derive(Debug, Clone)]
pub(crate) enum Peer {
    /// The Unix socket: the user who owns the data home.
    Local,
    /// A browser signed in to web access.
    Remote {
        session: String,
        policy: RemotePolicy,
    },
}

/// Requests one remote session may have running at once.
const MAX_REMOTE_IN_FLIGHT: usize = 64;
/// Subscriptions one remote session may hold.
const MAX_REMOTE_SUBSCRIPTIONS: usize = 256;

pub(crate) fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

/// A message for a client, as it goes on the wire.
pub(crate) enum Encoded {
    Ready(Vec<u8>),
    /// It doesn't serialize (a bug, logged): there is nothing to send.
    Skip,
    /// Too large to send, and not an answer that could say so instead. The
    /// connection has to end: the client would carry on without it, never
    /// knowing (a thread that stops updating, answers that never come).
    TooBig,
}

/// Serialize `msg` for a client. An answer too large to send becomes an
/// error answer, so the request that asked for it fails and nothing else.
pub(crate) fn encode(msg: &ServerMsg) -> Encoded {
    encode_within(msg, MAX_FRAME_LEN)
}

fn encode_within(msg: &ServerMsg, limit: usize) -> Encoded {
    let bytes = match serde_json::to_vec(msg) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!(%err, kind = msg_kind(msg), "a message for a client doesn't serialize");
            return Encoded::Skip;
        }
    };
    if bytes.len() <= limit {
        return Encoded::Ready(bytes);
    }
    let len = bytes.len();
    match msg {
        ServerMsg::Response { id, .. } => {
            tracing::error!(len, "an answer is too large to send; it fails instead");
            let failed = ServerMsg::Response {
                id: *id,
                result: Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!(
                        "The answer is too large to send ({} MB).",
                        len.div_ceil(1024 * 1024)
                    ),
                )),
            };
            serde_json::to_vec(&failed).map_or(Encoded::Skip, Encoded::Ready)
        }
        other => {
            tracing::error!(
                len,
                kind = msg_kind(other),
                "a message is too large to send; closing the connection"
            );
            Encoded::TooBig
        }
    }
}

/// What kind of message this is, for logs.
fn msg_kind(msg: &ServerMsg) -> &'static str {
    match msg {
        ServerMsg::Welcome(_) => "welcome",
        ServerMsg::Response { .. } => "response",
        ServerMsg::Snapshot { .. } => "snapshot",
        ServerMsg::Event { .. } => "event",
        ServerMsg::Delta { .. } => "delta",
        ServerMsg::TerminalOutput { .. } => "terminal_output",
        ServerMsg::TerminalExited { .. } => "terminal_exited",
        ServerMsg::PageFrame { .. } => "page_frame",
        ServerMsg::PageState { .. } => "page_state",
        ServerMsg::PageClosed { .. } => "page_closed",
        ServerMsg::PageAction { .. } => "page_action",
        ServerMsg::PageDriven { .. } => "page_driven",
        ServerMsg::PageTabs { .. } => "page_tabs",
        ServerMsg::Fatal(_) => "fatal",
    }
}

/// Serve a local client on the daemon's socket (`loft_client::local`).
pub(crate) async fn serve_local(stream: loft_client::local::ServerStream, hub: Hub) {
    let (mut sink, source): (SplitSink<_, Bytes>, _) = Framed::new(stream, codec()).split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();

    let mut writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let bytes = match encode(&msg) {
                Encoded::Ready(bytes) => bytes,
                Encoded::Skip => continue,
                Encoded::TooBig => return,
            };
            if let Err(err) = sink.send(Bytes::from(bytes)).await {
                tracing::warn!(%err, "couldn't write to a client; closing its connection");
                return;
            }
        }
    });
    // A read error (oversized frame, reset) ends the connection.
    let frames = source
        .take_while(|frame| futures::future::ready(frame.is_ok()))
        .filter_map(|frame| futures::future::ready(frame.ok().map(|f| f.freeze())));
    tokio::select! {
        () = run(frames, out_tx, hub, Peer::Local) => {
            let _ = (&mut writer).await;
        }
        // Nothing more reaches the client: end the connection rather than
        // keep it half alive (requests heard, never answered). Dropping the
        // handler drops its subscriptions and the socket, and the client
        // reconnects and catches up from fresh snapshots.
        _ = &mut writer => {}
    }
    tracing::debug!("client disconnected");
}

/// The `op` of a request frame, without decoding the rest (audit log).
fn peek_op(frame: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Frame<'a> {
        #[serde(borrow)]
        body: Option<Body<'a>>,
    }
    #[derive(serde::Deserialize)]
    struct Body<'a> {
        op: Option<&'a str>,
    }
    serde_json::from_slice::<Frame<'_>>(frame)
        .ok()?
        .body?
        .op
        .map(str::to_owned)
}

/// Requests too frequent or too plain to be worth a log line each.
fn quiet_op(op: &str) -> bool {
    matches!(
        op,
        "ping"
            | "set_draft"
            | "find_files"
            | "list_dir"
            | "read_file"
            | "read_path"
            | "get_diff"
            | "get_settings"
            | "list_models"
            | "list_commands"
            | "list_branches"
            | "probe_harness"
            | "search_threads"
            | "git_hub_login"
            | "agent_info"
            // Every few seconds from each open tab, and once per key press,
            // pointer move or window resize: opening the terminal or the
            // page they go to is what gets the line.
            | "ui_files"
            | "terminal_input"
            | "terminal_resize"
            | "page_input"
            | "page_resize"
    )
}

/// Undoes a connection's registrations however its task ends.
struct Registered {
    subs: HashMap<u64, tokio::task::AbortHandle>,
    /// For subscriptions to a page: the last picture its viewer has shown.
    page_acks: HashMap<u64, watch::Sender<u64>>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        for (_, task) in self.subs.drain() {
            task.abort();
        }
    }
}

/// Run one connection: `frames` are the client's messages (ending when it
/// goes away), `out_tx` takes ours. Returns when the client is gone.
pub(crate) async fn run<S>(
    mut frames: S,
    out_tx: mpsc::UnboundedSender<ServerMsg>,
    hub: Hub,
    peer: Peer,
) where
    S: Stream<Item = Bytes> + Unpin,
{
    let core = hub.core.clone();
    let remote = match &peer {
        Peer::Local => None,
        Peer::Remote { session, policy } => Some((session.clone(), *policy)),
    };

    // Handshake.
    let hello = match frames.next().await {
        Some(frame) => serde_json::from_slice::<ClientMsg>(&frame).ok(),
        None => None,
    };
    // An app on this computer whose pages may play sound here; never a
    // browser session, whatever it says.
    let plays_sound: bool;
    match hello {
        Some(ClientMsg::Hello(h))
            if h.protocol_version == PROTOCOL_VERSION
                || h.protocol_version + 1 == PROTOCOL_VERSION =>
        {
            plays_sound =
                remote.is_none() && h.capabilities.iter().any(|c| c == loft_protocol::SOUND);
            tracing::debug!(kind = ?h.client_kind, build = %h.client_build, remote = remote.is_some(), "client connected");
            let _ = out_tx.send(ServerMsg::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_build: crate::BUILD.into(),
                daemon_pid: std::process::id(),
                daemon_exe_mtime_ms: *crate::EXE_MTIME,
                remote: remote.as_ref().map(|(_, policy)| RemoteAccess {
                    terminal: policy.allow_terminal,
                    browser: policy.allow_browser,
                }),
            }));
        }
        Some(ClientMsg::Hello(h)) => {
            let _ = out_tx.send(ServerMsg::Fatal(ErrorBody::new(
                ErrorCode::IncompatibleVersion,
                format!(
                    "client protocol {} is not supported (daemon speaks {PROTOCOL_VERSION})",
                    h.protocol_version
                ),
            )));
            return;
        }
        _ => {
            let _ = out_tx.send(ServerMsg::Fatal(ErrorBody::new(
                ErrorCode::BadRequest,
                "expected hello",
            )));
            return;
        }
    }

    static NEXT_CONN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let conn_id = NEXT_CONN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut registered = Registered {
        subs: HashMap::new(),
        page_acks: HashMap::new(),
    };
    let in_flight = Arc::new(Semaphore::new(MAX_REMOTE_IN_FLIGHT));

    while let Some(frame) = frames.next().await {
        let msg = match serde_json::from_slice::<ClientMsg>(&frame) {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(%err, "bad client frame");
                // A newer app asking for something this daemon predates:
                // answer instead of leaving the request hanging.
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&frame)
                    && v["type"] == "request"
                    && let Some(id) = v["id"].as_u64()
                {
                    let op = v["body"]["op"].as_str().unwrap_or("?");
                    let _ = out_tx.send(ServerMsg::Response {
                        id,
                        result: Err(ErrorBody::new(
                            ErrorCode::Unsupported,
                            format!(
                                "agentloftd {} doesn't understand `{op}`; it's older than the app. Restart it (quit Agentloft with no agents running) to update.",
                                crate::BUILD
                            ),
                        )),
                    });
                }
                continue;
            }
        };
        match msg {
            ClientMsg::Hello(_) => {}
            ClientMsg::Request { id, body } => {
                // Whoever clicks in a page is who that page answers. A
                // pointer only passing over it counts from the app here (the
                // person is at this computer: let it be heard) but not from
                // elsewhere: the same mouse crossing a web client's window
                // on its way back to the app would mute the app's tab
                // (`pages/sound.rs`).
                if let Request::PageInput { id, input } = &body {
                    let hover = matches!(
                        input,
                        PageInput::Mouse {
                            action: PageMouse::Move,
                            button: PageButton::None,
                            ..
                        }
                    );
                    if plays_sound || !hover {
                        core.page_touched(id, conn_id, plays_sound);
                    }
                }
                if let Some((session, policy)) = &remote {
                    // What remote sessions asked for, by name only.
                    if let Some(op) = peek_op(&frame)
                        && !quiet_op(&op)
                    {
                        tracing::info!(target: "agentloft::web::audit", session = %session, op = %op, "remote request");
                    }
                    if let Some(result) = core.handle_inline_remote(&body, policy) {
                        let _ = out_tx.send(ServerMsg::Response { id, result });
                        continue;
                    }
                    let Ok(permit) = in_flight.clone().try_acquire_owned() else {
                        let _ = out_tx.send(ServerMsg::Response {
                            id,
                            result: Err(ErrorBody::new(
                                ErrorCode::Conflict,
                                "too many requests at once",
                            )),
                        });
                        continue;
                    };
                    let (core, out, policy) = (core.clone(), out_tx.clone(), *policy);
                    tokio::spawn(async move {
                        let result = core.handle_remote(body, &policy).await;
                        let _ = out.send(ServerMsg::Response { id, result });
                        drop(permit);
                    });
                    continue;
                }
                if let Some(result) = core.handle_inline(&body) {
                    let _ = out_tx.send(ServerMsg::Response { id, result });
                    continue;
                }
                let hub = hub.clone();
                let out = out_tx.clone();
                tokio::spawn(async move {
                    let result = handle_local(&hub, body).await;
                    let _ = out.send(ServerMsg::Response { id, result });
                });
            }
            ClientMsg::Subscribe { sub_id, topic } => {
                if let Some((_, policy)) = &remote {
                    let allowed = core.check_remote_topic(&topic, policy).and_then(|()| {
                        if registered.subs.len() >= MAX_REMOTE_SUBSCRIPTIONS
                            && !registered.subs.contains_key(&sub_id)
                        {
                            Err(ErrorBody::new(
                                ErrorCode::Conflict,
                                "too many subscriptions",
                            ))
                        } else {
                            Ok(())
                        }
                    });
                    if let Err(err) = allowed {
                        let _ = out_tx.send(ServerMsg::Fatal(err));
                        continue;
                    }
                }
                if let Some(old) = registered.subs.remove(&sub_id) {
                    old.abort();
                }
                registered.page_acks.remove(&sub_id);
                let task = match topic {
                    Topic::Page(page) => {
                        let (acked, acks) = watch::channel(0);
                        registered.page_acks.insert(sub_id, acked);
                        tokio::spawn(forward_page(
                            core.clone(),
                            (conn_id, plays_sound),
                            sub_id,
                            page,
                            out_tx.clone(),
                            acks,
                        ))
                    }
                    topic => {
                        // A browser session that may not use the browser isn't
                        // told about pages.
                        let pages = remote.as_ref().is_none_or(|(_, p)| p.allow_browser);
                        tokio::spawn(forward(core.clone(), sub_id, topic, out_tx.clone(), pages))
                    }
                };
                registered.subs.insert(sub_id, task.abort_handle());
            }
            ClientMsg::Unsubscribe { sub_id } => {
                registered.page_acks.remove(&sub_id);
                if let Some(task) = registered.subs.remove(&sub_id) {
                    task.abort();
                }
            }
            ClientMsg::PageFrameAck { sub_id, seq } => {
                if let Some(acked) = registered.page_acks.get(&sub_id) {
                    acked.send_if_modified(|last| {
                        let newer = seq > *last;
                        *last = (*last).max(seq);
                        newer
                    });
                }
            }
        }
    }
}

/// A local client's request: the daemon's own (shutdown, web access), else
/// the core's.
async fn handle_local(hub: &Hub, body: Request) -> Result<Response, ErrorBody> {
    let web = || {
        hub.web
            .clone()
            .ok_or_else(|| ErrorBody::new(ErrorCode::Unsupported, "web access isn't available"))
    };
    match body {
        // An upgrade waits for running turns; a user quit doesn't.
        Request::Shutdown {
            reason: loft_protocol::ShutdownReason::Upgrade,
        } if hub.core.is_busy() => Err(ErrorBody::new(
            ErrorCode::Conflict,
            "agents are running; the update applies once they're done",
        )),
        Request::Shutdown { reason } => {
            tracing::info!(?reason, "shutdown requested by client");
            let _ = hub.shutdown.send(true);
            Ok(Response::Ok)
        }
        Request::WebStatus => Ok(Response::WebStatus {
            status: web()?.status(),
        }),
        Request::WebRotateToken => {
            let web = web()?;
            web.rotate_token()
                .map_err(|e| ErrorBody::new(ErrorCode::Internal, format!("{e:#}")))?;
            Ok(Response::WebStatus {
                status: web.status(),
            })
        }
        Request::WebRevokeSession { id } => {
            let web = web()?;
            web.revoke_session(&id);
            Ok(Response::WebStatus {
                status: web.status(),
            })
        }
        Request::WebRevokeAllSessions => {
            let web = web()?;
            web.revoke_all_sessions();
            Ok(Response::WebStatus {
                status: web.status(),
            })
        }
        Request::WebLinkStart => {
            let web = web()?;
            // A failure is part of the status (`website.error`).
            web.link_start().await;
            Ok(Response::WebStatus {
                status: web.status(),
            })
        }
        Request::WebLinkCancel => {
            let web = web()?;
            web.link_cancel();
            Ok(Response::WebStatus {
                status: web.status(),
            })
        }
        Request::WebUnlink => {
            let web = web()?;
            web.unlink().await;
            Ok(Response::WebStatus {
                status: web.status(),
            })
        }
        Request::WebSync => {
            let web = web()?;
            web.sync_now();
            Ok(Response::WebStatus {
                status: web.status(),
            })
        }
        // The listener follows the config; answer once it has caught up, so
        // a status asked right after is true.
        Request::ReloadConfig => {
            let result = hub.core.handle(Request::ReloadConfig).await;
            if let Some(web) = &hub.web {
                web.apply(&hub.core.config().web).await;
            }
            result
        }
        // This computer's user: may also show local files in the browser.
        other => hub.core.handle_local(other).await,
    }
}

/// Send a snapshot, then every later event/delta for the topic (and, for a
/// thread, word that an agent is using its page, when `pages`).
async fn forward(
    core: Arc<Core>,
    sub_id: u64,
    topic: Topic,
    out: mpsc::UnboundedSender<ServerMsg>,
    pages: bool,
) {
    loop {
        // Subscribe before snapshotting so nothing falls in between.
        let mut bus = core.subscribe();
        let snapshot = match core.snapshot(&topic) {
            Ok(s) => s,
            Err(err) => {
                let _ = out.send(ServerMsg::Fatal(err));
                return;
            }
        };
        let mut seq = match &snapshot {
            Snapshot::Workspace { seq, .. }
            | Snapshot::Thread { seq, .. }
            | Snapshot::Terminal { seq, .. }
            | Snapshot::Page { seq, .. } => *seq,
        };
        if out.send(ServerMsg::Snapshot { sub_id, snapshot }).is_err() {
            return;
        }
        loop {
            match bus.recv().await {
                Ok(BusMsg::Event(env)) => {
                    if env.seq <= seq || !matches_topic(&topic, &env.event) {
                        continue;
                    }
                    let event = Arc::unwrap_or_clone(env);
                    if out.send(ServerMsg::Event { sub_id, event }).is_err() {
                        return;
                    }
                }
                Ok(BusMsg::Delta(delta)) => {
                    if topic == Topic::Thread(delta.thread_id.clone())
                        && out.send(ServerMsg::Delta { sub_id, delta }).is_err()
                    {
                        return;
                    }
                }
                Ok(BusMsg::TerminalOutput { id, end }) => {
                    // `seq` is the stream offset already delivered.
                    if topic != Topic::Terminal(id.clone()) || end <= seq {
                        continue;
                    }
                    match core.terminal_read_since(&id, seq) {
                        Ok(Some((data, new_end))) => {
                            seq = new_end;
                            if !data.is_empty()
                                && out
                                    .send(ServerMsg::TerminalOutput {
                                        sub_id,
                                        id,
                                        data: data.into(),
                                    })
                                    .is_err()
                            {
                                return;
                            }
                        }
                        // Fell behind the ring (or the terminal closed): resend
                        // the replay snapshot.
                        Ok(None) | Err(_) => break,
                    }
                }
                Ok(BusMsg::TerminalExited { id, code }) => {
                    if topic == Topic::Terminal(id.clone())
                        && out
                            .send(ServerMsg::TerminalExited { sub_id, id, code })
                            .is_err()
                    {
                        return;
                    }
                }
                Ok(BusMsg::PageDriven { thread_id, page }) => {
                    if pages
                        && topic == Topic::Thread(thread_id.clone())
                        && out.send(ServerMsg::PageDriven { thread_id, page }).is_err()
                    {
                        return;
                    }
                }
                Ok(BusMsg::PageTabs {
                    thread_id,
                    pages: tabs,
                }) => {
                    if pages
                        && topic == Topic::Thread(thread_id.clone())
                        && out
                            .send(ServerMsg::PageTabs {
                                thread_id,
                                pages: tabs,
                            })
                            .is_err()
                    {
                        return;
                    }
                }
                // Pages have a forwarder of their own (`forward_page`).
                Ok(
                    BusMsg::PageFrame { .. }
                    | BusMsg::PageState { .. }
                    | BusMsg::PageClosed { .. }
                    | BusMsg::PageAction { .. },
                ) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, sub_id, "subscriber lagged; resending snapshot");
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

/// How long a viewer may sit on a picture before the next is sent anyway.
const PAGE_ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// Counts one viewer of a page for as long as it lives, and one listener if
/// it plays sound here (`hearer`: its connection).
struct Watching {
    core: Arc<Core>,
    page: loft_protocol::PageId,
    hearer: Option<u64>,
}

impl Drop for Watching {
    fn drop(&mut self) {
        let (core, page, hearer) = (self.core.clone(), self.page.clone(), self.hearer);
        tokio::spawn(async move {
            if let Some(conn) = hearer {
                core.page_hearing(&page, conn, false).await;
            }
            core.page_watching(&page, false).await;
        });
    }
}

/// How many pictures one viewer may have on the way at once. One at a
/// time, a link with a long round trip (a phone, through the tunnel) gets
/// one picture per round trip: about two a second at 500 ms. So more may be
/// on the way, as many as keep about one waiting in the link's queue
/// (judged by how much slower acknowledgements come back than the quickest
/// seen): more would only make the pictures shown older.
struct PictureWindow {
    /// Sent and not yet acknowledged, oldest first: (seq, when sent).
    in_flight: std::collections::VecDeque<(u64, tokio::time::Instant)>,
    size: usize,
    /// The quickest acknowledgement seen: the link's round trip, nothing
    /// queued.
    quickest: Option<Duration>,
}

impl PictureWindow {
    const MAX: usize = 12;

    fn new() -> Self {
        Self {
            in_flight: std::collections::VecDeque::new(),
            size: 2,
            quickest: None,
        }
    }

    fn ready(&self) -> bool {
        self.in_flight.len() < self.size
    }

    fn sent(&mut self, seq: u64, at: tokio::time::Instant) {
        self.in_flight.push_back((seq, at));
    }

    /// The viewer has shown everything up to `seq`.
    fn acked(&mut self, seq: u64, now: tokio::time::Instant) {
        let mut sent_at = None;
        while let Some(&(s, at)) = self.in_flight.front() {
            if s > seq {
                break;
            }
            sent_at = Some(at);
            self.in_flight.pop_front();
        }
        let Some(sent_at) = sent_at else {
            return;
        };
        let took = now.saturating_duration_since(sent_at);
        let quickest = *self
            .quickest
            .insert(self.quickest.map_or(took, |q| q.min(took)));
        // Of the pictures on the way, how many waited in a queue rather
        // than travelling: with `size` sent per `took` where `quickest`
        // would do, `size × (1 − quickest/took)` (TCP Vegas's estimate).
        // Under one keeps the link busy without making pictures stale.
        let queued =
            self.size as f64 * (1.0 - quickest.as_secs_f64() / took.as_secs_f64().max(1e-6));
        if queued < 0.4 {
            self.size = (self.size + 1).min(Self::MAX);
        } else if queued > 1.0 {
            self.size = (self.size - 1).max(1);
        }
    }

    /// Forget pictures the viewer never acknowledged in time (it may have
    /// dropped them); the link is struggling, so fewer go out.
    fn expire(&mut self, now: tokio::time::Instant) {
        let before = self.in_flight.len();
        self.in_flight
            .retain(|(_, at)| now.saturating_duration_since(*at) < PAGE_ACK_TIMEOUT);
        if self.in_flight.len() < before {
            self.size = (self.size / 2).max(1);
        }
    }

    /// When the oldest picture on the way counts as lost.
    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.in_flight.front().map(|(_, at)| *at + PAGE_ACK_TIMEOUT)
    }
}

/// One viewer of a page of the daemon's browser: its state as it changes,
/// and its pictures, as many on the way at once as its link carries
/// ([`PictureWindow`]), always the newest.
/// `(conn_id, plays_sound)`: the viewer's connection, and whether it plays
/// sound on this computer ([`loft_protocol::SOUND`]).
async fn forward_page(
    core: Arc<Core>,
    (conn_id, plays_sound): (u64, bool),
    sub_id: u64,
    page: loft_protocol::PageId,
    out: mpsc::UnboundedSender<ServerMsg>,
    mut acks: watch::Receiver<u64>,
) {
    // Subscribe before snapshotting so nothing falls in between.
    let mut bus = core.subscribe();
    let snapshot = match core.snapshot(&Topic::Page(page.clone())) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            let _ = out.send(ServerMsg::PageClosed {
                sub_id,
                id: page,
                reason: err.message,
            });
            return;
        }
    };
    let mut sent = match &snapshot {
        Snapshot::Page {
            seq,
            frame: Some(_),
            ..
        } => *seq,
        _ => 0,
    };
    let mut window = PictureWindow::new();
    if sent > 0 {
        window.sent(sent, tokio::time::Instant::now());
    }
    if out.send(ServerMsg::Snapshot { sub_id, snapshot }).is_err() {
        return;
    }
    core.page_watching(&page, true).await;
    let hearer = plays_sound.then_some(conn_id);
    let _watching = Watching {
        core: core.clone(),
        page: page.clone(),
        hearer,
    };
    if let Some(conn) = hearer {
        core.page_hearing(&page, conn, true).await;
    }
    let mut newer = false;
    loop {
        let now = tokio::time::Instant::now();
        window.acked(*acks.borrow(), now);
        window.expire(now);
        if newer && window.ready() {
            newer = false;
            if let Some(frame) = core.page_frame_after(&page, sent) {
                sent = frame.seq;
                window.sent(sent, now);
                let frame = ServerMsg::PageFrame {
                    sub_id,
                    id: page.clone(),
                    frame,
                };
                if out.send(frame).is_err() {
                    return;
                }
            }
        }
        let waiting = newer.then(|| window.deadline()).flatten();
        tokio::select! {
            msg = bus.recv() => match msg {
                Ok(BusMsg::PageFrame { id, seq }) if id == page && seq > sent => newer = true,
                Ok(BusMsg::PageState { id }) if id == page => {
                    if let Some(info) = core.page_info(&page)
                        && out.send(ServerMsg::PageState { sub_id, page: info }).is_err()
                    {
                        return;
                    }
                }
                Ok(BusMsg::PageClosed { id, reason }) if id == page => {
                    let _ = out.send(ServerMsg::PageClosed { sub_id, id, reason });
                    return;
                }
                // What a view asked for goes to whoever clicked in it (to
                // every viewer if nobody is known to have).
                Ok(BusMsg::PageAction { id, action, to })
                    if id == page && to.is_none_or(|conn| conn == conn_id) =>
                {
                    if out.send(ServerMsg::PageAction { sub_id, id, action }).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                // Missed some: the newest picture and the current state
                // are all that matter.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    newer = true;
                    match core.page_info(&page) {
                        Some(info) => {
                            if out.send(ServerMsg::PageState { sub_id, page: info }).is_err() {
                                return;
                            }
                        }
                        None => {
                            let _ = out.send(ServerMsg::PageClosed {
                                sub_id,
                                id: page,
                                reason: "The tab was closed.".into(),
                            });
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            changed = acks.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = async { tokio::time::sleep_until(waiting.unwrap_or_else(tokio::time::Instant::now)).await }, if waiting.is_some() => {}
        }
    }
}

fn matches_topic(topic: &Topic, event: &loft_protocol::Event) -> bool {
    match topic {
        Topic::Workspace => event.is_workspace_event(),
        Topic::Thread(id) => event.thread_id() == Some(id),
        Topic::Terminal(_) | Topic::Page(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::{Delta, DeltaChannel, ItemId, ThreadId};

    /// A far link that keeps up: more pictures go out at once, up to the
    /// most. One that queues them: fewer. One that loses them: fewer still.
    #[test]
    fn pictures_on_the_way_follow_the_link() {
        let ms = Duration::from_millis;
        let start = tokio::time::Instant::now();
        let mut window = PictureWindow::new();
        let mut seq = 0;
        // 500 ms there and back, whatever is on the way: nothing queues.
        for round in 0..20u32 {
            let at = start + ms(500) * round;
            while window.ready() {
                seq += 1;
                window.sent(seq, at);
            }
            window.acked(seq, at + ms(500));
        }
        assert_eq!(window.size, PictureWindow::MAX);

        // Now each picture takes as long again as the last to come back:
        // they are waiting in a queue.
        let mut at = start + ms(20_000);
        let mut took = ms(600);
        while window.size > 1 {
            while window.ready() {
                seq += 1;
                window.sent(seq, at);
            }
            window.acked(seq, at + took);
            at += took;
            took += ms(300);
        }
        assert_eq!(window.size, 1);

        // Never acknowledged: forgotten after a while, and the window
        // stays shut no longer.
        window.size = 4;
        seq += 1;
        window.sent(seq, at);
        window.expire(at + PAGE_ACK_TIMEOUT);
        assert!(window.in_flight.is_empty());
        assert_eq!(window.size, 2);
        assert!(window.ready());
    }

    fn decoded(encoded: Encoded) -> ServerMsg {
        match encoded {
            Encoded::Ready(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Encoded::Skip => panic!("skipped"),
            Encoded::TooBig => panic!("too big"),
        }
    }

    /// An answer too large to send fails that request, and the connection
    /// carries on; anything else too large ends the connection, so the
    /// client reconnects instead of silently missing it.
    #[test]
    fn a_message_too_large_fails_its_request_or_ends_the_connection() {
        let big = "x".repeat(4096);
        let answer = ServerMsg::Response {
            id: 7,
            result: Ok(Response::Text { text: big.clone() }),
        };
        match decoded(encode_within(&answer, 1024)) {
            ServerMsg::Response {
                id: 7,
                result: Err(err),
            } => {
                assert!(err.message.contains("too large"), "{}", err.message);
            }
            other => panic!("expected an error answer, got {other:?}"),
        }
        // Within the limit it goes as it is.
        assert!(matches!(
            decoded(encode_within(&answer, 1 << 20)),
            ServerMsg::Response {
                id: 7,
                result: Ok(_)
            }
        ));

        let delta = ServerMsg::Delta {
            sub_id: 1,
            delta: Delta {
                thread_id: ThreadId::new(),
                item_id: ItemId::new(),
                channel: DeltaChannel::Text,
                offset: 0,
                text: big,
            },
        };
        assert!(matches!(encode_within(&delta, 1024), Encoded::TooBig));
    }
}
