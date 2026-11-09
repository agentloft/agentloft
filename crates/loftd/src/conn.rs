//! One client connection: handshake, requests, subscriptions.

use bytes::Bytes;
use futures::stream::SplitSink;
use futures::{SinkExt as _, StreamExt as _};
use loft_config::Paths;
use loft_core::{BusMsg, Core};
use loft_protocol::{
    ClientMsg, ErrorBody, ErrorCode, MAX_FRAME_LEN, PROTOCOL_VERSION, Request, Response, ServerMsg,
    Snapshot, Topic, Welcome,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

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

pub(crate) async fn serve(stream: UnixStream, core: Arc<Core>, shutdown: watch::Sender<bool>) {
    let (mut sink, mut source): (SplitSink<_, Bytes>, _) = Framed::new(stream, codec()).split();
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

    // Handshake.
    let browser_host;
    let hello = match source.next().await {
        Some(Ok(frame)) => serde_json::from_slice::<ClientMsg>(&frame).ok(),
        _ => None,
    };
    match hello {
        Some(ClientMsg::Hello(h))
            if h.protocol_version == PROTOCOL_VERSION
                || h.protocol_version + 1 == PROTOCOL_VERSION =>
        {
            tracing::debug!(kind = ?h.client_kind, build = %h.client_build, "client connected");
            browser_host = h.capabilities.iter().any(|c| c == "browser");
            let _ = out_tx.send(ServerMsg::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_build: crate::BUILD.into(),
                daemon_pid: std::process::id(),
                daemon_exe_mtime_ms: *crate::EXE_MTIME,
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
            drop(out_tx);
            let _ = writer.await;
            return;
        }
        _ => {
            let _ = out_tx.send(ServerMsg::Fatal(ErrorBody::new(
                ErrorCode::BadRequest,
                "expected hello",
            )));
            drop(out_tx);
            let _ = writer.await;
            return;
        }
    }

    static NEXT_CONN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let conn_id = NEXT_CONN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let call_relay = browser_host.then(|| {
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

    let mut subs: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    while let Some(frame) = source.next().await {
        let Ok(frame) = frame else { break };
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
                if let Some(result) = core.handle_inline(&body) {
                    let _ = out_tx.send(ServerMsg::Response { id, result });
                    continue;
                }
                let core = core.clone();
                let out = out_tx.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let result = match body {
                        // An upgrade waits for running turns; a user quit doesn't.
                        Request::Shutdown {
                            reason: loft_protocol::ShutdownReason::Upgrade,
                        } if core.is_busy() => Err(ErrorBody::new(
                            ErrorCode::Conflict,
                            "agents are running; the update applies once they're done",
                        )),
                        Request::Shutdown { reason } => {
                            tracing::info!(?reason, "shutdown requested by client");
                            let _ = shutdown.send(true);
                            Ok(Response::Ok)
                        }
                        other => core.handle(other).await,
                    };
                    let _ = out.send(ServerMsg::Response { id, result });
                });
            }
            ClientMsg::Subscribe { sub_id, topic } => {
                if let Some(old) = subs.remove(&sub_id) {
                    old.abort();
                }
                if let Topic::Thread(thread) = &topic {
                    core.clients().watch(conn_id, sub_id, thread.clone());
                }
                let task = tokio::spawn(forward(core.clone(), sub_id, topic, out_tx.clone()));
                subs.insert(sub_id, task.abort_handle());
            }
            ClientMsg::Unsubscribe { sub_id } => {
                core.clients().unwatch(conn_id, sub_id);
                if let Some(task) = subs.remove(&sub_id) {
                    task.abort();
                }
            }
            ClientMsg::ClientCallResult { call_id, result } => {
                core.clients().complete(call_id, result);
            }
        }
    }
    for (_, task) in subs {
        task.abort();
    }
    core.clients().unregister(conn_id);
    if let Some(relay) = call_relay {
        relay.abort();
    }
    drop(out_tx);
    let _ = writer.await;
    tracing::debug!("client disconnected");
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
                Ok(BusMsg::TerminalOutput { id, offset, data }) => {
                    if topic != Topic::Terminal(id.clone()) {
                        continue;
                    }
                    // `seq` is the stream offset already delivered; trim overlap.
                    let end = offset + data.len() as u64;
                    if end <= seq {
                        continue;
                    }
                    let skip = seq.saturating_sub(offset) as usize;
                    seq = end;
                    let chunk = data[skip..].to_vec();
                    if out
                        .send(ServerMsg::TerminalOutput {
                            sub_id,
                            id,
                            data: chunk.into(),
                        })
                        .is_err()
                    {
                        return;
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
