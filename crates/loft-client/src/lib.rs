//! GPUI-free client for `agentloftd` (ADR-0010): issues requests, manages
//! subscriptions, and — on native targets — connects over the Unix socket
//! and can start the daemon. [`model`] holds mirrored projections that apply
//! snapshots, events, deltas.
//!
//! Two transports carry the same `ClientMsg` / `ServerMsg` JSON:
//! - native: length-prefixed frames on the daemon's Unix socket ([`native`]);
//! - wasm: one message per WebSocket message to the daemon's web listener
//!   ([`web`], ADR-0016).

pub mod model;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{connect_or_spawn, daemon_binary};

#[cfg(target_arch = "wasm32")]
pub mod web;

use loft_protocol::{
    ClientMsg, Delta, ErrorBody, ErrorCode, EventEnvelope, Request, Response, ServerMsg, Snapshot,
    Topic, Welcome,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub enum ClientEvent {
    Snapshot {
        sub_id: u64,
        snapshot: Snapshot,
    },
    Event {
        sub_id: u64,
        event: EventEnvelope,
    },
    Delta {
        sub_id: u64,
        delta: Delta,
    },
    TerminalOutput {
        sub_id: u64,
        id: loft_protocol::TerminalId,
        data: Vec<u8>,
    },
    TerminalExited {
        sub_id: u64,
        id: loft_protocol::TerminalId,
        code: Option<i32>,
    },
    /// A new picture of a page of the daemon's browser (acknowledge it
    /// with [`Client::ack_page_frame`] once shown).
    PageFrame {
        sub_id: u64,
        id: loft_protocol::PageId,
        seq: u64,
        jpeg: Vec<u8>,
    },
    PageState {
        sub_id: u64,
        page: loft_protocol::PageInfo,
    },
    PageClosed {
        sub_id: u64,
        id: loft_protocol::PageId,
        reason: String,
    },
    /// The daemon asks this client to do something (answer with
    /// [`Client::respond_call`]).
    ClientCall {
        call_id: u64,
        call: loft_protocol::ClientCall,
    },
    /// Connection-level error from the daemon.
    Fatal(ErrorBody),
    Disconnected,
}

pub(crate) type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Response, ErrorBody>>>>>;

struct Inner {
    tx: mpsc::UnboundedSender<ClientMsg>,
    pending: Pending,
    next_id: AtomicU64,
    welcome: Welcome,
}

/// Cheap to clone; all clones share one connection.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

/// Route one message from the daemon: answers go to their waiting request,
/// everything else becomes a [`ClientEvent`]. Returns `false` once nobody
/// is listening for events any more.
pub(crate) fn dispatch(
    msg: ServerMsg,
    pending: &Pending,
    events: &mpsc::UnboundedSender<ClientEvent>,
) -> bool {
    let event = match msg {
        ServerMsg::Response { id, result } => {
            let waiter = pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(result);
            }
            return true;
        }
        ServerMsg::Snapshot { sub_id, snapshot } => ClientEvent::Snapshot { sub_id, snapshot },
        ServerMsg::Event { sub_id, event } => ClientEvent::Event { sub_id, event },
        ServerMsg::Delta { sub_id, delta } => ClientEvent::Delta { sub_id, delta },
        ServerMsg::TerminalOutput { sub_id, id, data } => ClientEvent::TerminalOutput {
            sub_id,
            id,
            data: data.0,
        },
        ServerMsg::TerminalExited { sub_id, id, code } => {
            ClientEvent::TerminalExited { sub_id, id, code }
        }
        ServerMsg::PageFrame { sub_id, id, frame } => ClientEvent::PageFrame {
            sub_id,
            id,
            seq: frame.seq,
            jpeg: frame.jpeg.0,
        },
        ServerMsg::PageState { sub_id, page } => ClientEvent::PageState { sub_id, page },
        ServerMsg::PageClosed { sub_id, id, reason } => {
            ClientEvent::PageClosed { sub_id, id, reason }
        }
        ServerMsg::ClientCall { call_id, call } => ClientEvent::ClientCall { call_id, call },
        ServerMsg::Fatal(e) => ClientEvent::Fatal(e),
        ServerMsg::Welcome(_) => return true,
    };
    events.send(event).is_ok()
}

/// The connection is gone: fail what was waiting and say so.
pub(crate) fn disconnect(pending: &Pending, events: &mpsc::UnboundedSender<ClientEvent>) {
    for (_, waiter) in pending.lock().unwrap_or_else(|e| e.into_inner()).drain() {
        let _ = waiter.send(Err(disconnected()));
    }
    let _ = events.send(ClientEvent::Disconnected);
}

impl Client {
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<ClientMsg>,
        pending: Pending,
        welcome: Welcome,
    ) -> Self {
        Client {
            inner: Arc::new(Inner {
                tx,
                pending,
                next_id: AtomicU64::new(1),
                welcome,
            }),
        }
    }

    pub fn welcome(&self) -> &Welcome {
        &self.inner.welcome
    }

    /// Send a request; the returned future works on any executor.
    pub fn request(
        &self,
        body: Request,
    ) -> impl Future<Output = Result<Response, ErrorBody>> + Send + 'static {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let sent = self.inner.tx.send(ClientMsg::Request { id, body }).is_ok();
        async move {
            if !sent {
                return Err(disconnected());
            }
            rx.await.unwrap_or_else(|_| Err(disconnected()))
        }
    }

    /// Subscribe to a topic; returns the subscription id.
    pub fn subscribe(&self, topic: Topic) -> u64 {
        let sub_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.inner.tx.send(ClientMsg::Subscribe { sub_id, topic });
        sub_id
    }

    /// This viewer has shown picture `seq` of the page it watches on
    /// `sub_id`, and can take the next.
    pub fn ack_page_frame(&self, sub_id: u64, seq: u64) {
        let _ = self.inner.tx.send(ClientMsg::PageFrameAck { sub_id, seq });
    }

    /// Answer a [`ClientEvent::ClientCall`].
    pub fn respond_call(&self, call_id: u64, result: Result<serde_json::Value, ErrorBody>) {
        let _ = self
            .inner
            .tx
            .send(ClientMsg::ClientCallResult { call_id, result });
    }

    pub fn unsubscribe(&self, sub_id: u64) {
        let _ = self.inner.tx.send(ClientMsg::Unsubscribe { sub_id });
    }

    pub fn is_connected(&self) -> bool {
        !self.inner.tx.is_closed()
    }
}

pub(crate) fn disconnected() -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, "disconnected from agentloftd")
}
