//! Calls from the daemon into connected clients — e.g. an agent driving the
//! browser panel that lives in the user's app window.

use loft_protocol::{ClientCall, ErrorBody, ErrorCode};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub struct OutgoingCall {
    pub call_id: u64,
    pub call: ClientCall,
}

type Waiter = oneshot::Sender<Result<serde_json::Value, ErrorBody>>;

#[derive(Default)]
pub struct ClientRegistry {
    next: AtomicU64,
    /// Connections that can run browser calls, most recent last.
    browser_hosts: Mutex<Vec<(u64, mpsc::UnboundedSender<OutgoingCall>)>>,
    pending: Mutex<HashMap<u64, Waiter>>,
}

impl ClientRegistry {
    /// Register a connection as a browser host; calls arrive on the receiver.
    pub fn register_browser_host(&self, conn_id: u64) -> mpsc::UnboundedReceiver<OutgoingCall> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.browser_hosts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((conn_id, tx));
        rx
    }

    pub fn unregister(&self, conn_id: u64) {
        self.browser_hosts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _)| *id != conn_id);
    }

    pub fn has_browser_host(&self) -> bool {
        self.browser_hosts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(_, tx)| !tx.is_closed())
    }

    /// Send a call to the most recent capable client and await its answer.
    pub async fn call(
        &self,
        call: ClientCall,
        timeout: Duration,
    ) -> Result<serde_json::Value, ErrorBody> {
        let call_id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        let host = {
            let hosts = self.browser_hosts.lock().unwrap_or_else(|e| e.into_inner());
            hosts
                .iter()
                .rev()
                .find(|(_, t)| !t.is_closed())
                .map(|(_, t)| t.clone())
        };
        let Some(host) = host else {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "No Agentloft window is open to host the browser. Open the app and try again.",
            ));
        };
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(call_id, tx);
        if host.send(OutgoingCall { call_id, call }).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&call_id);
            return Err(ErrorBody::new(
                ErrorCode::Internal,
                "browser host disconnected",
            ));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ErrorBody::new(
                ErrorCode::Internal,
                "browser host dropped the call",
            )),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&call_id);
                Err(ErrorBody::new(
                    ErrorCode::Internal,
                    "browser call timed out",
                ))
            }
        }
    }

    pub fn complete(&self, call_id: u64, result: Result<serde_json::Value, ErrorBody>) {
        if let Some(w) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&call_id)
        {
            let _ = w.send(result);
        }
    }
}
