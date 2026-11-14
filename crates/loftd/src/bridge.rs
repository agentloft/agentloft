//! `agentloftd bridge`: the daemon's connection over stdin and stdout.
//!
//! Another computer's Agentloft reaches this one by running
//! `ssh <host> agentloftd bridge`: SSH carries the bytes, encrypts them and
//! has already checked who is asking (the same person who could open a
//! shell here). The bridge only joins its stdin and stdout to the daemon's
//! Unix socket, starting the daemon first if it isn't running. It reads
//! nothing of what passes, so the connection is the same one the desktop
//! app has on this machine.

use anyhow::{Context as _, Result};
use loft_config::Paths;
use std::time::Duration;
use tokio::net::UnixStream;

pub async fn run(paths: Paths) -> Result<()> {
    let socket = connect_or_start(&paths).await?;
    let (mut from_daemon, mut to_daemon) = socket.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    // Whichever side ends first ends the bridge: a daemon that went away,
    // or a client that hung up.
    tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut to_daemon) => {
            tracing::info!(?result, "the client hung up");
        }
        result = async {
            let copied = tokio::io::copy(&mut from_daemon, &mut stdout).await;
            use tokio::io::AsyncWriteExt as _;
            let _ = stdout.flush().await;
            copied
        } => {
            tracing::info!(?result, "the daemon closed the connection");
        }
    }
    // Reading stdin blocks a thread the runtime would wait for.
    std::process::exit(0);
}

/// The daemon's socket, starting the daemon if nothing answers there.
async fn connect_or_start(paths: &Paths) -> Result<UnixStream> {
    let path = paths.socket_file();
    if let Ok(socket) = UnixStream::connect(&path).await {
        return Ok(socket);
    }
    let exe = std::env::current_exe().context("find agentloftd")?;
    start(&exe, paths)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match UnixStream::connect(&path).await {
            Ok(socket) => return Ok(socket),
            Err(err) if tokio::time::Instant::now() >= deadline => {
                return Err(err).context("agentloftd didn't start within 20 s");
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

fn start(exe: &std::path::Path, paths: &Paths) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    let child = Command::new(exe)
        .arg("--home")
        .arg(&paths.home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group, and a session of its own: it outlives the
        // SSH session that started it, like the one the desktop app starts.
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {}", exe.display()))?;
    tracing::info!(pid = child.id(), "started agentloftd");
    Ok(())
}
