//! A DevTools-protocol connection to a browser we started, over a pair of
//! pipes (`--remote-debugging-pipe`): JSON messages, each ended by a NUL.
//! Nothing listens on a port, so only this process can drive the browser.

use command_fds::{CommandFdExt as _, FdMapping};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot};

/// How long a command may take before the browser counts as stuck.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
/// No single message is this big; a browser that sends one is broken.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// Something the browser told us without being asked.
#[derive(Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// The page session it belongs to; `None` for the browser itself.
    pub session: Option<String>,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, String>>>;

pub struct Cdp {
    writer: tokio::sync::Mutex<tokio::fs::File>,
    pending: Arc<Mutex<Pending>>,
    next: AtomicU64,
    child: Mutex<Option<tokio::process::Child>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Cdp {
    /// Start `exe` with `args` and connect to it. Events arrive on the
    /// receiver, which closes when the browser exits.
    pub fn launch(
        exe: &Path,
        args: &[String],
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Event>), String> {
        let io = |e: std::io::Error| format!("couldn't start the browser: {e}");
        // The browser reads commands on its fd 3 and writes on its fd 4.
        let (browser_reads, we_write) = std::io::pipe().map_err(io)?;
        let (we_read, browser_writes) = std::io::pipe().map_err(io)?;
        let mut command = tokio::process::Command::new(exe);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
            .fd_mappings(vec![
                FdMapping {
                    parent_fd: OwnedFd::from(browser_reads),
                    child_fd: 3,
                },
                FdMapping {
                    parent_fd: OwnedFd::from(browser_writes),
                    child_fd: 4,
                },
            ])
            .map_err(|_| "couldn't start the browser: pipe setup".to_owned())?;
        let child = command.spawn().map_err(io)?;
        // Our copies of the browser's ends go with the command, so its
        // exit reads as end-of-file here.
        drop(command);

        let writer = tokio::fs::File::from_std(std::fs::File::from(OwnedFd::from(we_write)));
        let mut reader = tokio::fs::File::from_std(std::fs::File::from(OwnedFd::from(we_read)));
        let pending: Arc<Mutex<Pending>> = Arc::default();
        let (events, events_rx) = mpsc::unbounded_channel();
        let waiting = pending.clone();
        tokio::spawn(async move {
            let mut buffer: Vec<u8> = Vec::new();
            let mut chunk = vec![0u8; 256 * 1024];
            'read: loop {
                let n = match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buffer.extend_from_slice(&chunk[..n]);
                while let Some(end) = buffer.iter().position(|b| *b == 0) {
                    let message: Vec<u8> = buffer.drain(..=end).collect();
                    match parse(&message[..end]) {
                        Some(Incoming::Reply { id, result }) => {
                            if let Some(tx) = lock(&waiting).remove(&id) {
                                let _ = tx.send(result);
                            }
                        }
                        Some(Incoming::Event(event)) => {
                            if events.send(event).is_err() {
                                break 'read;
                            }
                        }
                        None => {}
                    }
                }
                if buffer.len() > MAX_MESSAGE {
                    tracing::error!("the browser sent an oversized message; dropping it");
                    break;
                }
            }
            // Whoever is still waiting learns the browser is gone.
            for (_, tx) in lock(&waiting).drain() {
                let _ = tx.send(Err("the browser stopped".into()));
            }
        });
        Ok((
            Arc::new(Self {
                writer: tokio::sync::Mutex::new(writer),
                pending,
                next: AtomicU64::new(1),
                child: Mutex::new(Some(child)),
            }),
            events_rx,
        ))
    }

    /// Send a command and wait for its result.
    pub async fn send(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        let mut bytes = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        bytes.push(0);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        let written = {
            let mut writer = self.writer.lock().await;
            match writer.write_all(&bytes).await {
                Ok(()) => writer.flush().await,
                Err(e) => Err(e),
            }
        };
        if written.is_err() {
            lock(&self.pending).remove(&id);
            return Err("the browser stopped".into());
        }
        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the browser stopped".into()),
            Err(_) => {
                lock(&self.pending).remove(&id);
                Err(format!("the browser didn't answer `{method}`"))
            }
        }
    }

    /// Stop the browser.
    pub fn kill(&self) {
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.start_kill();
            // Reap it off to the side.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

enum Incoming {
    Reply {
        id: u64,
        result: Result<Value, String>,
    },
    Event(Event),
}

fn parse(bytes: &[u8]) -> Option<Incoming> {
    let mut value: Value = serde_json::from_slice(bytes).ok()?;
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        let result = match value.get("error") {
            Some(error) => Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the browser refused")
                .to_owned()),
            None => Ok(value.get_mut("result").map(Value::take).unwrap_or_default()),
        };
        return Some(Incoming::Reply { id, result });
    }
    let method = value.get("method")?.as_str()?.to_owned();
    Some(Incoming::Event(Event {
        method,
        session: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        params: value.get_mut("params").map(Value::take).unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_and_events_parse() {
        let Some(Incoming::Reply { id, result }) = parse(br#"{"id":7,"result":{"targetId":"t"}}"#)
        else {
            panic!()
        };
        assert_eq!((id, result.unwrap()["targetId"].as_str()), (7, Some("t")));
        let Some(Incoming::Reply { result, .. }) =
            parse(br#"{"id":8,"error":{"code":-32000,"message":"No target"}}"#)
        else {
            panic!()
        };
        assert_eq!(result.unwrap_err(), "No target");
        let Some(Incoming::Event(event)) =
            parse(br#"{"method":"Page.frameNavigated","params":{"frame":{}},"sessionId":"s1"}"#)
        else {
            panic!()
        };
        assert_eq!(event.method, "Page.frameNavigated");
        assert_eq!(event.session.as_deref(), Some("s1"));
        assert!(parse(b"not json").is_none());
    }
}
