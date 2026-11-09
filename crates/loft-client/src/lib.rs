//! GPUI-free client for `agentloftd` (ADR-0010): connects over the Unix
//! socket, issues requests, manages subscriptions, and can start the daemon.
//! [`model`] holds mirrored projections that apply snapshots, events, deltas.

pub mod model;

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use futures::stream::SplitSink;
use futures::{SinkExt as _, StreamExt as _};
use loft_config::Paths;
use loft_protocol::{
    ClientKind, ClientMsg, Delta, ErrorBody, ErrorCode, EventEnvelope, Hello, MAX_FRAME_LEN,
    PROTOCOL_VERSION, Request, Response, ServerMsg, Snapshot, Topic, Welcome,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

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

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Response, ErrorBody>>>>>;

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

impl Client {
    /// Connect and perform the hello handshake. Must run inside a tokio
    /// runtime (the I/O tasks are spawned on it).
    pub async fn connect(
        socket: &Path,
        kind: ClientKind,
        build: &str,
    ) -> Result<(Client, mpsc::UnboundedReceiver<ClientEvent>)> {
        Self::connect_with(socket, kind, build, &[]).await
    }

    /// Connect advertising `capabilities` (e.g. `"browser"`).
    pub async fn connect_with(
        socket: &Path,
        kind: ClientKind,
        build: &str,
        capabilities: &[&str],
    ) -> Result<(Client, mpsc::UnboundedReceiver<ClientEvent>)> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connect {}", socket.display()))?;
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(MAX_FRAME_LEN)
            .new_codec();
        let (mut sink, mut source): (SplitSink<_, Bytes>, _) = Framed::new(stream, codec).split();

        let hello = ClientMsg::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_kind: kind,
            client_build: build.to_owned(),
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
        });
        sink.send(Bytes::from(serde_json::to_vec(&hello)?)).await?;
        let welcome = match source.next().await {
            Some(Ok(frame)) => match serde_json::from_slice::<ServerMsg>(&frame)? {
                ServerMsg::Welcome(w) => w,
                ServerMsg::Fatal(e) => bail!("daemon refused connection: {e}"),
                other => bail!("unexpected first message: {other:?}"),
            },
            Some(Err(e)) => return Err(e.into()),
            None => bail!("daemon closed the connection during handshake"),
        };

        let (tx, mut out_rx) = mpsc::unbounded_channel::<ClientMsg>();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let Ok(bytes) = serde_json::to_vec(&msg) else {
                    continue;
                };
                if sink.send(Bytes::from(bytes)).await.is_err() {
                    break;
                }
            }
        });

        let pending: Pending = Arc::default();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = source.next().await {
                let msg = match serde_json::from_slice::<ServerMsg>(&frame) {
                    Ok(m) => m,
                    Err(err) => {
                        tracing::warn!(%err, "bad daemon frame");
                        continue;
                    }
                };
                let event = match msg {
                    ServerMsg::Response { id, result } => {
                        let waiter = reader_pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&id);
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(result);
                        }
                        continue;
                    }
                    ServerMsg::Snapshot { sub_id, snapshot } => {
                        ClientEvent::Snapshot { sub_id, snapshot }
                    }
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
                    ServerMsg::ClientCall { call_id, call } => {
                        ClientEvent::ClientCall { call_id, call }
                    }
                    ServerMsg::Fatal(e) => ClientEvent::Fatal(e),
                    ServerMsg::Welcome(_) => continue,
                };
                if events_tx.send(event).is_err() {
                    break;
                }
            }
            // Fail all in-flight requests.
            for (_, waiter) in reader_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain()
            {
                let _ = waiter.send(Err(disconnected()));
            }
            let _ = events_tx.send(ClientEvent::Disconnected);
        });

        Ok((
            Client {
                inner: Arc::new(Inner {
                    tx,
                    pending,
                    next_id: AtomicU64::new(1),
                    welcome,
                }),
            },
            events_rx,
        ))
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

fn disconnected() -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, "disconnected from agentloftd")
}

/// Where the daemon binary lives: `$AGENTLOFT_DAEMON_BIN`, else next to the
/// current executable (both ship in the same bundle / target dir).
pub fn daemon_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("AGENTLOFT_DAEMON_BIN") {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("agentloftd")))
        .unwrap_or_else(|| PathBuf::from("agentloftd"))
}

/// Connect, starting the daemon (detached) if nothing is listening.
pub async fn connect_or_spawn(
    paths: &Paths,
    daemon_bin: &Path,
    kind: ClientKind,
    build: &str,
    capabilities: &[&str],
) -> Result<(Client, mpsc::UnboundedReceiver<ClientEvent>)> {
    let socket = paths.socket_file();
    if let Ok(conn) = Client::connect_with(&socket, kind, build, capabilities).await {
        if !is_stale(conn.0.welcome(), daemon_bin) {
            return Ok(conn);
        }
        // The daemon binary changed since it started (rebuild or app update).
        // Swap it if it's idle; with agents running, keep it for now.
        let pid = conn.0.welcome().daemon_pid;
        let asked = conn
            .0
            .request(Request::Shutdown {
                reason: loft_protocol::ShutdownReason::Upgrade,
            })
            .await;
        if asked.is_err() {
            tracing::info!(pid, "daemon is outdated but busy; using it until idle");
            return Ok(conn);
        }
        tracing::info!(pid, "restarting outdated agentloftd");
        drop(conn);
        // Wait for the old process itself to exit: it holds the home's lock
        // until it has stopped its agents, and the new daemon needs it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while process_alive(pid) {
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("the outdated agentloftd (pid {pid}) didn't exit");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    spawn_daemon(paths, daemon_bin)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match Client::connect_with(&socket, kind, build, capabilities).await {
            Ok(conn) => return Ok(conn),
            Err(err) if tokio::time::Instant::now() >= deadline => {
                return Err(err).context("agentloftd did not come up within 10s");
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Whether `pid` is still running (signal 0 probes without signalling).
#[cfg(unix)]
#[allow(unsafe_code)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 only checks existence and permission.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// The running daemon's executable differs from the one we'd start.
fn is_stale(welcome: &loft_protocol::Welcome, daemon_bin: &Path) -> bool {
    match (
        welcome.daemon_exe_mtime_ms,
        loft_protocol::file_mtime_ms(daemon_bin),
    ) {
        (Some(running), Some(on_disk)) => running != on_disk,
        // A daemon from before this check existed is outdated by definition.
        (None, Some(_)) => true,
        // No binary to compare against (e.g. tests): keep it.
        (_, None) => false,
    }
}

fn spawn_daemon(paths: &Paths, daemon_bin: &Path) -> Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(daemon_bin);
    cmd.arg("--home")
        .arg(&paths.home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: terminal signals to the UI don't reach it.
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("start {}", daemon_bin.display()))?;
    tracing::info!(pid = child.id(), "started agentloftd");
    // Reap it if it exits while we're alive (no zombie).
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
