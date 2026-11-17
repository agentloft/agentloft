//! `agentloftd`: owns the store, harness sessions and the Unix socket that
//! clients connect to (ADR-0001, PLAN §1.6).

pub mod bridge;
mod conn;
mod http;
pub mod mcp;
mod mcp_automations;
mod mcp_tabs;
mod prices;
mod reaper;
pub mod web;

use anyhow::{Context as _, Result};
use loft_client::local::Listener;
use loft_config::Paths;
use loft_core::{Core, CoreOptions};
use loft_harness::Harness;
use std::fs::File;
use std::sync::Arc;
use std::time::Duration;
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
    /// Ask npm and Homebrew for the agents' latest releases (off in tests).
    pub check_agent_updates: bool,
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
    /// Deletes archived threads past `threads.delete_archived_after_days`.
    cleanup_task: tokio::task::JoinHandle<()>,
    /// Keeps the model prices for cost estimates a day fresh at most.
    prices_task: tokio::task::JoinHandle<()>,
    /// Starts automations' runs when they are due.
    automations_task: tokio::task::JoinHandle<()>,
    limits_task: tokio::task::JoinHandle<()>,
    /// Keeps threads' open pull requests (state, checks, review) current.
    pull_requests_task: tokio::task::JoinHandle<()>,
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
            Arc::new(loft_config::process_env())
        };
        let config = loft_config::Config::load(&paths).unwrap_or_else(|err| {
            tracing::warn!(%err, "invalid config.toml; using defaults");
            loft_config::Config::default()
        });
        let device_backends = device_backends(&env);
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
                device_backends,
                fetch: opts.check_agent_updates.then(|| {
                    std::sync::Arc::new(|url: &str| http::get_text(url, 1 << 20))
                        as loft_core::Fetcher
                }),
                ..CoreOptions::default()
            },
        );
        let recovered = core.recover().map_err(|e| anyhow::anyhow!(e))?;
        if recovered > 0 {
            tracing::warn!(recovered, "marked turns interrupted after unclean shutdown");
        }

        let socket = paths.socket_file();
        let listener = Listener::bind(&socket)
            .await
            .with_context(|| format!("bind {}", socket.display()))?;
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
        let cleanup_task = core.spawn_archive_cleanup();
        let prices_task = prices::spawn(core.clone(), paths.clone());
        let automations_task = core.spawn_automations();
        let limits_task = core.spawn_limit_resumes();
        let pull_requests_task = core.spawn_pull_requests();
        Ok(Self {
            core,
            web,
            shutdown_tx,
            task,
            web_task,
            cleanup_task,
            prices_task,
            automations_task,
            limits_task,
            pull_requests_task,
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
        self.cleanup_task.abort();
        self.prices_task.abort();
        self.automations_task.abort();
        self.limits_task.abort();
        self.pull_requests_task.abort();
        let _ = self.web_task.await;
        self.core.shutdown().await;
        let _ = self.task.await;
        tracing::info!("agentloftd stopped");
    }
}

async fn accept_loop(
    mut listener: Listener,
    hub: conn::Hub,
    mut shutdown_rx: watch::Receiver<bool>,
    paths: Paths,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    if !loft_client::local::peer_allowed(&stream, &paths.run_dir()) {
                        tracing::warn!("rejected connection from another user");
                        continue;
                    }
                    tokio::spawn(conn::serve_local(stream, hub.clone()));
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
    loft_client::local::unbind(&paths.socket_file());
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

/// Simulators and emulators (ADR-0020): this computer's, or with
/// `AGENTLOFT_FAKE_DEVICES=1` a fake iPhone and a fake Android phone that
/// need neither Xcode nor the Android SDK (for trying the panel).
fn device_backends(
    env: &std::collections::HashMap<String, String>,
) -> Vec<Arc<dyn loft_core::devices::Backend>> {
    use loft_core::devices::fake::FakeBackend;
    use loft_protocol::DevicePlatform;
    if std::env::var_os("AGENTLOFT_FAKE_DEVICES").is_some_and(|v| v == "1") {
        return vec![
            Arc::new(FakeBackend::new(
                DevicePlatform::Ios,
                Duration::from_secs(2),
            )),
            Arc::new(FakeBackend::new(
                DevicePlatform::Android,
                Duration::from_secs(3),
            )),
        ];
    }
    loft_core::devices::system_backends(env)
}
