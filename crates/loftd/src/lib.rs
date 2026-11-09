//! `agentloftd`: owns the store, harness sessions and the Unix socket that
//! clients connect to (ADR-0001, PLAN §1.6).

mod conn;
mod reaper;

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

pub struct DaemonOptions {
    pub paths: Paths,
    pub harnesses: Vec<Arc<dyn Harness>>,
    pub capture_login_env: bool,
}

/// A running daemon. Dropping it does not stop it; call [`Daemon::shutdown`].
pub struct Daemon {
    pub core: Arc<Core>,
    shutdown_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
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
        let task = tokio::spawn(accept_loop(
            listener,
            core.clone(),
            shutdown_tx.clone(),
            shutdown_rx,
            paths.clone(),
        ));
        Ok(Self {
            core,
            shutdown_tx,
            task,
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
        self.core.shutdown().await;
        let _ = self.task.await;
        tracing::info!("agentloftd stopped");
    }
}

async fn accept_loop(
    listener: UnixListener,
    core: Arc<Core>,
    shutdown_tx: watch::Sender<bool>,
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
                    tokio::spawn(conn::serve(stream, core.clone(), shutdown_tx.clone()));
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
