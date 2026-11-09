//! Calls from the daemon into connected clients — e.g. an agent driving the
//! browser panel that lives in the user's app window.

use loft_protocol::{ClientCall, ErrorBody, ErrorCode, ThreadId};
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
    /// Thread subscriptions per connection (`conn → sub → thread`), so a
    /// browser call goes to a window that has the thread open.
    watching: Mutex<HashMap<u64, HashMap<u64, ThreadId>>>,
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
        self.watching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&conn_id);
    }

    /// A connection subscribed to a thread.
    pub fn watch(&self, conn_id: u64, sub_id: u64, thread: ThreadId) {
        self.watching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(conn_id)
            .or_default()
            .insert(sub_id, thread);
    }

    pub fn unwatch(&self, conn_id: u64, sub_id: u64) {
        if let Some(subs) = self
            .watching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&conn_id)
        {
            subs.remove(&sub_id);
        }
    }

    fn watches(&self, conn_id: u64, thread: &ThreadId) -> bool {
        self.watching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&conn_id)
            .is_some_and(|subs| subs.values().any(|t| t == thread))
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
        let thread = match &call {
            ClientCall::Browser { thread_id, .. } => thread_id.clone(),
        };
        let host = {
            let hosts = self.browser_hosts.lock().unwrap_or_else(|e| e.into_inner());
            let live = || hosts.iter().rev().filter(|(_, t)| !t.is_closed());
            // Prefer the newest window showing this thread, else the newest.
            live()
                .find(|(id, _)| self.watches(*id, &thread))
                .or_else(|| live().next())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn calls_go_to_a_window_watching_the_thread() {
        let reg = std::sync::Arc::new(ClientRegistry::default());
        let mut a = reg.register_browser_host(1);
        let mut b = reg.register_browser_host(2);
        let t = ThreadId::new();
        reg.watch(1, 7, t.clone());
        let call = |reg: std::sync::Arc<ClientRegistry>, t: ThreadId| {
            tokio::spawn(async move {
                reg.call(
                    ClientCall::Browser {
                        thread_id: t,
                        command: serde_json::json!({}),
                    },
                    Duration::from_millis(200),
                )
                .await
            })
        };
        // Window 1 shows the thread, though window 2 registered last.
        drop(call(reg.clone(), t.clone()));
        let got = a.recv().await.unwrap();
        reg.complete(got.call_id, Ok(serde_json::json!(1)));
        // Another thread falls back to the newest window.
        drop(call(reg.clone(), ThreadId::new()));
        assert!(b.recv().await.is_some());
        reg.unwatch(1, 7);
        drop(call(reg.clone(), t));
        assert!(b.recv().await.is_some(), "no longer watched");
    }
}
