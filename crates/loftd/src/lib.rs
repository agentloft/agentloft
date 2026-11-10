//! `agentloftd`: owns the store, harness sessions and the Unix socket that
//! clients connect to (ADR-0001, PLAN §1.6).

mod conn;
pub mod mcp;
mod reaper;
pub mod web;

use anyhow::{Context as _, Result};
use loft_config::Paths;
use loft_core::{Core, CoreOptions};
use loft_harness::Harness;
use std::fs::File;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::sync::watch;

pub const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("CARGO_PKG_NAME"));

/// Our executable's mtime at startup (see `Welcome::daemon_exe_mtime_ms`).
pub static EXE_MTIME: std::sync::LazyLock<Option<i64>> = std::sync::LazyLock::new(|| {
    std::env::current_exe()
        .ok()
        .and_then(|exe| loft_protocol::file_mtime_ms(&exe))
});

pub struct DaemonOptions {
    pub paths: Paths,
    pub harnesses: Vec<Arc<dyn Harness>>,
    pub capture_login_env: bool,
    /// Attach `agentloftd mcp` to agent sessions (None in tests).
    pub mcp_bridge: Option<loft_core::McpBridge>,
    /// The web client's files, instead of the usual places (tests).
    pub web_bundle: Option<std::path::PathBuf>,
}

/// A running daemon. Dropping it does not stop it; call [`Daemon::shutdown`].
pub struct Daemon {
    pub core: Arc<Core>,
    /// Web access (ADR-0016); off unless `[web] enabled`.
    pub web: Arc<web::Web>,
    shutdown_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    web_task: tokio::task::JoinHandle<()>,
    _lock: File,
}

impl Daemon {
    /// Acquire the single-instance lock, recover state and start listening.
    pub async fn start(opts: DaemonOptions) -> Result<Self> {
        let paths = opts.paths;
        paths.ensure_dirs()?;
        let lock = acquire_lock(&paths)?;

        let store = loft_store::Store::open(&paths.db_file())?;
        reaper::reap_orphans(&store);

        let env = if opts.capture_login_env {
            let env = tokio::task::spawn_blocking(|| {
                loft_config::capture_login_env(Duration::from_secs(5))
            })
            .await?;
            Arc::new(env)
        } else {
            Arc::new(std::env::vars().collect())
        };
        let config = loft_config::Config::load(&paths).unwrap_or_else(|err| {
            tracing::warn!(%err, "invalid config.toml; using defaults");
            loft_config::Config::default()
        });
        let core = Core::new(
            store,
            opts.harnesses,
            CoreOptions {
                env,
                config,
                mcp_bridge: opts.mcp_bridge,
                worktrees_dir: paths.worktrees_dir(),
                chats_dir: paths.chats_dir(),
                projects_dir: paths.projects_dir(),
                config_file: Some(paths.config_file()),
                attachments_dir: paths.attachments_dir(),
                ..CoreOptions::default()
            },
        );
        let recovered = core.recover().map_err(|e| anyhow::anyhow!(e))?;
        if recovered > 0 {
            tracing::warn!(recovered, "marked turns interrupted after unclean shutdown");
        }

        let socket = paths.socket_file();
        let _ = std::fs::remove_file(&socket);
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
        set_socket_mode(&socket)?;
        tracing::info!(socket = %socket.display(), pid = std::process::id(), "agentloftd listening");

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let web = web::Web::new(&paths, core.clone(), shutdown_tx.clone(), opts.web_bundle);
        // Up (or refused, with the reason recorded) before we return, so a
        // status asked at once is true; then it follows the config.
        web.apply(&core.config().web).await;
        let web_task = web.follow_config();
        let hub = conn::Hub {
            core: core.clone(),
            shutdown: shutdown_tx.clone(),
            web: Some(web.clone()),
        };
        let task = tokio::spawn(accept_loop(listener, hub, shutdown_rx, paths.clone()));
        Ok(Self {
            core,
            web,
            shutdown_tx,
            task,
            web_task,
            _lock: lock,
        })
    }

    /// Resolves when a client requested shutdown.
    pub async fn wait_for_shutdown_request(&self) {
        let mut rx = self.shutdown_tx.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.web_task.await;
        self.core.shutdown().await;
        let _ = self.task.await;
        tracing::info!("agentloftd stopped");
    }
}

async fn accept_loop(
    listener: UnixListener,
    hub: conn::Hub,
    mut shutdown_rx: watch::Receiver<bool>,
    paths: Paths,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    if !conn::peer_allowed(&stream, &paths) {
                        tracing::warn!("rejected connection from another user");
                        continue;
                    }
                    tokio::spawn(conn::serve_unix(stream, hub.clone()));
                }
                Err(err) => {
                    tracing::error!(%err, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
    let _ = std::fs::remove_file(paths.socket_file());
}

/// Wait up to `timeout` for another daemon on this home to release its
/// lock (an outdated daemon still stopping its agents during an update).
pub async fn wait_for_lock(paths: &Paths, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match acquire_lock(paths) {
            // Free: release it again; `Daemon::start` takes it for real.
            Ok(file) => {
                drop(file);
                return;
            }
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(_) => return,
        }
    }
}

fn acquire_lock(paths: &Paths) -> Result<File> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.lock_file())
        .context("open lock file")?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!(
                "another agentloftd is already running for {}",
                paths.home.display()
            )
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("lock file"),
    }
}

#[cfg(unix)]
fn set_socket_mode(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_socket_mode(_: &std::path::Path) -> Result<()> {
    Ok(())
}
