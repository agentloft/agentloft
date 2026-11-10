//! The native transport: length-prefixed JSON frames on the daemon's Unix
//! socket, and starting the daemon when nothing is listening.

use crate::{Client, ClientEvent, Pending, disconnect, dispatch};
use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use futures::stream::SplitSink;
use futures::{SinkExt as _, StreamExt as _};
use loft_config::Paths;
use loft_protocol::{
    ClientKind, ClientMsg, Hello, MAX_FRAME_LEN, PROTOCOL_VERSION, Request, ServerMsg,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

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
                match serde_json::from_slice::<ServerMsg>(&frame) {
                    Ok(msg) => {
                        if !dispatch(msg, &reader_pending, &events_tx) {
                            break;
                        }
                    }
                    Err(err) => tracing::warn!(%err, "bad daemon frame"),
                }
            }
            disconnect(&reader_pending, &events_tx);
        });

        Ok((Client::new(tx, pending, welcome), events_rx))
    }
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
