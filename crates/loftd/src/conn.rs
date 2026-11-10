//! One client connection: handshake, requests, subscriptions.
//!
//! The same loop serves both transports: the Unix socket (length-prefixed
//! frames, a local client that may do everything) and the web listener's
//! WebSocket (one frame per message, a remote session held to
//! `loft_core::RemotePolicy`).

use bytes::Bytes;
use futures::stream::SplitSink;
use futures::{SinkExt as _, Stream, StreamExt as _};
use loft_config::Paths;
use loft_core::{BusMsg, Core, RemotePolicy};
use loft_protocol::{
    ClientMsg, ErrorBody, ErrorCode, MAX_FRAME_LEN, PROTOCOL_VERSION, RemoteAccess, Request,
    Response, ServerMsg, Snapshot, Topic, Welcome,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::UnixStream;
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

/// Only the user who owns the data home may connect.
pub(crate) fn peer_allowed(stream: &UnixStream, paths: &Paths) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(cred) = stream.peer_cred() else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(paths.run_dir()) else {
            return false;
        };
        cred.uid() == meta.uid()
    }
    #[cfg(not(unix))]
    {
        let _ = (stream, paths);
        true
    }
}

/// Serve a local client on the Unix socket.
pub(crate) async fn serve_unix(stream: UnixStream, hub: Hub) {
    let (mut sink, source): (SplitSink<_, Bytes>, _) = Framed::new(stream, codec()).split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let Ok(bytes) = serde_json::to_vec(&msg) else {
                continue;
            };
            if sink.send(Bytes::from(bytes)).await.is_err() {
                break;
            }
        }
    });
    // A read error (oversized frame, reset) ends the connection.
    let frames = source
        .take_while(|frame| futures::future::ready(frame.is_ok()))
        .filter_map(|frame| futures::future::ready(frame.ok().map(|f| f.freeze())));
    run(frames, out_tx, hub, Peer::Local).await;
    let _ = writer.await;
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
    )
}

/// Undoes a connection's registrations however its task ends.
struct Registered {
    core: Arc<Core>,
    conn_id: u64,
    subs: HashMap<u64, tokio::task::AbortHandle>,
    relay: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        for (_, task) in self.subs.drain() {
            task.abort();
        }
        self.core.clients().unregister(self.conn_id);
        if let Some(relay) = self.relay.take() {
            relay.abort();
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
    let browser_host;
    let hello = match frames.next().await {
        Some(frame) => serde_json::from_slice::<ClientMsg>(&frame).ok(),
        None => None,
    };
    match hello {
        Some(ClientMsg::Hello(h))
            if h.protocol_version == PROTOCOL_VERSION
                || h.protocol_version + 1 == PROTOCOL_VERSION =>
        {
            tracing::debug!(kind = ?h.client_kind, build = %h.client_build, remote = remote.is_some(), "client connected");
            // Only a local window hosts the browser panel agents drive.
            browser_host = remote.is_none() && h.capabilities.iter().any(|c| c == "browser");
            let _ = out_tx.send(ServerMsg::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_build: crate::BUILD.into(),
                daemon_pid: std::process::id(),
                daemon_exe_mtime_ms: *crate::EXE_MTIME,
                remote: remote.as_ref().map(|(_, policy)| RemoteAccess {
                    terminal: policy.allow_terminal,
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
    let relay = browser_host.then(|| {
        let mut calls = core.clients().register_browser_host(conn_id);
        let out = out_tx.clone();
        tokio::spawn(async move {
            while let Some(c) = calls.recv().await {
                if out
                    .send(ServerMsg::ClientCall {
                        call_id: c.call_id,
                        call: c.call,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    });
    let mut registered = Registered {
        core: core.clone(),
        conn_id,
        subs: HashMap::new(),
        relay,
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
                if let Topic::Thread(thread) = &topic {
                    core.clients().watch(conn_id, sub_id, thread.clone());
                }
                let task = tokio::spawn(forward(core.clone(), sub_id, topic, out_tx.clone()));
                registered.subs.insert(sub_id, task.abort_handle());
            }
            ClientMsg::Unsubscribe { sub_id } => {
                core.clients().unwatch(conn_id, sub_id);
                if let Some(task) = registered.subs.remove(&sub_id) {
                    task.abort();
                }
            }
            ClientMsg::ClientCallResult { call_id, result } => {
                // Only a browser host is ever asked; a remote session isn't one.
                if remote.is_none() {
                    core.clients().complete(call_id, result);
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
        // The listener follows the config; answer once it has caught up, so
        // a status asked right after is true.
        Request::ReloadConfig => {
            let result = hub.core.handle(Request::ReloadConfig).await;
            if let Some(web) = &hub.web {
                web.apply(&hub.core.config().web).await;
            }
            result
        }
        other => hub.core.handle(other).await,
    }
}

/// Send a snapshot, then every later event/delta for the topic.
async fn forward(
    core: Arc<Core>,
    sub_id: u64,
    topic: Topic,
    out: mpsc::UnboundedSender<ServerMsg>,
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
            | Snapshot::Terminal { seq, .. } => *seq,
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
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, sub_id, "subscriber lagged; resending snapshot");
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

fn matches_topic(topic: &Topic, event: &loft_protocol::Event) -> bool {
    match topic {
        Topic::Workspace => event.is_workspace_event(),
        Topic::Thread(id) => event.thread_id() == Some(id),
        Topic::Terminal(_) => false,
    }
}
