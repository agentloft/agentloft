//! Web access (ADR-0016): an HTTP + WebSocket listener that serves the web
//! client and carries the daemon's protocol to browsers that have paired.
//!
//! Off unless `[web] enabled`. It follows `config.toml`: a change to the
//! section restarts the listener, and a section that breaks the safety
//! rules (a non-loopback address without TLS) leaves it off with the reason
//! in [`Web::status`].

mod auth;
mod bundle;
mod http;
mod ws;

use crate::conn::Hub;
use anyhow::{Context as _, Result};
use hyper_util::rt::{TokioIo, TokioTimer};
use loft_config::{Paths, WebConfig, WebPlan};
use loft_core::Core;
use loft_protocol::WebStatus;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio_rustls::TlsAcceptor;

/// TCP connections served at once (pages, assets, WebSockets together).
const MAX_CONNECTIONS: usize = 128;
/// WebSocket sessions at once.
const MAX_WEBSOCKETS: usize = 32;
/// A request's head must arrive within this long; so must a TLS handshake.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest request head (request line and headers).
const MAX_HEAD: usize = 32 * 1024;
/// Largest body `POST /api/pair` reads.
pub(crate) const MAX_PAIR_BODY: usize = 4 * 1024;

/// Resolves once `flag` is set (or its sender is gone).
pub(crate) async fn raised(flag: &mut watch::Receiver<bool>) {
    let _ = flag.wait_for(|set| *set).await;
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A running listener.
struct Running {
    plan: Arc<WebPlan>,
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct State {
    enabled: bool,
    error: Option<String>,
    running: Option<Running>,
}

pub(crate) struct Inner {
    core: Arc<Core>,
    shutdown: watch::Sender<bool>,
    pub(crate) auth: Arc<auth::Auth>,
    pub(crate) bundle: bundle::Bundle,
    pub(crate) ws_slots: Arc<Semaphore>,
    /// Back-reference for connections (they answer management requests
    /// only for local clients, but share one [`Hub`] type).
    me: std::sync::Weak<Web>,
    state: Mutex<State>,
    /// Serializes start/stop.
    applying: tokio::sync::Mutex<()>,
}

impl Inner {
    pub(crate) fn hub(&self) -> Hub {
        Hub {
            core: self.core.clone(),
            shutdown: self.shutdown.clone(),
            web: self.me.upgrade(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Web access for one daemon.
pub struct Web {
    inner: Arc<Inner>,
}

impl Web {
    /// `bundle`: where the web client's files are, instead of the usual
    /// places (tests).
    pub fn new(
        paths: &Paths,
        core: Arc<Core>,
        shutdown: watch::Sender<bool>,
        bundle: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: Arc::new(Inner {
                core,
                shutdown,
                auth: auth::Auth::new(paths.web_dir()),
                bundle: match bundle {
                    Some(dir) => bundle::Bundle::at(vec![dir]),
                    None => bundle::Bundle::locate(),
                },
                ws_slots: Arc::new(Semaphore::new(MAX_WEBSOCKETS)),
                me: me.clone(),
                state: Mutex::new(State::default()),
                applying: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Start, restart or stop the listener to match `config`.
    pub async fn apply(&self, config: &WebConfig) {
        let _serial = self.inner.applying.lock().await;
        let wanted = config.enabled.then(|| config.plan());
        let current = self.inner.state().running.as_ref().map(|r| r.plan.clone());
        if let (Some(Ok(plan)), Some(current)) = (&wanted, &current)
            && **current == *plan
        {
            return;
        }
        // Anything else means the old listener goes.
        let old = self.inner.state().running.take();
        if let Some(old) = old {
            let _ = old.stop.send(true);
            let _ = old.task.await;
            tracing::info!(addr = %old.addr, "web access: stopped");
        }
        let (running, error) = match wanted {
            None => (None, None),
            Some(Err(reason)) => (None, Some(reason)),
            Some(Ok(plan)) => match self.start(plan).await {
                Ok(running) => (Some(running), None),
                Err(err) => (None, Some(format!("{err:#}"))),
            },
        };
        if let Some(reason) = &error {
            tracing::error!(%reason, "web access: not started");
        }
        let mut state = self.inner.state();
        state.enabled = config.enabled;
        state.error = error;
        state.running = running;
    }

    async fn start(&self, plan: WebPlan) -> Result<Running> {
        let tls = match &plan.tls {
            Some((cert, key)) => Some(load_tls(cert, key)?),
            None => None,
        };
        // The pairing token exists (0600) before anything can ask for it.
        self.inner
            .auth
            .token()
            .context("create the pairing token")?;
        let listener = TcpListener::bind(plan.addr).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                anyhow::anyhow!("port {} is already in use", plan.addr.port())
            } else {
                anyhow::anyhow!("can't listen on {}: {e}", plan.addr)
            }
        })?;
        let addr = listener.local_addr()?;
        let plan = Arc::new(plan);
        if plan.insecure_remote {
            tracing::warn!(%addr, "web access: plain HTTP on a non-loopback address (allow_insecure_remote)");
        }
        tracing::info!(%addr, tls = tls.is_some(), "web access: listening");
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(accept_loop(
            listener,
            self.inner.clone(),
            plan.clone(),
            tls,
            stop_rx,
        ));
        Ok(Running {
            plan,
            addr,
            stop,
            task,
        })
    }

    /// Follow the config until the daemon shuts down.
    pub(crate) fn follow_config(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let web = self.clone();
        let mut changes = web.inner.core.config_changes();
        let mut shutdown = web.inner.shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                web.apply(&web.inner.core.config().web).await;
                tokio::select! {
                    changed = changes.changed() => if changed.is_err() { break },
                    _ = raised(&mut shutdown) => break,
                }
            }
            web.apply(&WebConfig::default()).await;
            web.inner.auth.save();
        })
    }

    pub fn status(&self) -> WebStatus {
        let state = self.inner.state();
        let mut status = WebStatus {
            enabled: state.enabled,
            running: state.running.is_some(),
            error: state.error.clone(),
            bundle: self.inner.bundle.found(),
            sessions: self.inner.auth.sessions(),
            ..WebStatus::default()
        };
        if let Some(running) = &state.running {
            let scheme = if running.plan.tls.is_some() {
                "https"
            } else {
                "http"
            };
            let url = format!(
                "{scheme}://{}",
                display_authority(&running.plan, running.addr)
            );
            status.tls = running.plan.tls.is_some();
            status.remote = !running.addr.ip().is_loopback();
            status.insecure_remote = running.plan.insecure_remote;
            // In the fragment: never sent to a server, kept out of logs.
            status.pairing_url = self
                .inner
                .auth
                .token()
                .ok()
                .map(|token| format!("{url}/#token={token}"));
            status.url = Some(url);
        }
        status
    }

    /// The address the listener is bound to, when it's up.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.inner.state().running.as_ref().map(|r| r.addr)
    }

    pub fn rotate_token(&self) -> Result<()> {
        self.inner.auth.rotate_token().context("rotate the token")
    }

    pub fn revoke_session(&self, id: &str) -> bool {
        self.inner.auth.revoke(id)
    }

    pub fn revoke_all_sessions(&self) {
        self.inner.auth.revoke_all();
    }
}

/// `host:port` to show and to put in the pairing link: the first configured
/// name if there is one, else the address itself (for "every interface",
/// this machine's address on its default route).
fn display_authority(plan: &WebPlan, addr: SocketAddr) -> String {
    if let Some(name) = plan.allowed_hosts.first()
        && !addr.ip().is_loopback()
    {
        return if name.contains(':') && !name.starts_with('[') || name.ends_with(']') {
            name.clone()
        } else {
            format!("{name}:{}", addr.port())
        };
    }
    let ip = if addr.ip().is_unspecified() {
        outbound_ip(addr.is_ipv6()).unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
    } else {
        addr.ip()
    };
    SocketAddr::new(ip, addr.port()).to_string()
}

/// This machine's address on its default route. Connecting a UDP socket
/// sends nothing; it only makes the OS pick a source address.
fn outbound_ip(v6: bool) -> Option<IpAddr> {
    let (bind, probe) = if v6 {
        ("[::]:0", "[2001:db8::1]:9")
    } else {
        ("0.0.0.0:0", "192.0.2.1:9")
    };
    let socket = std::net::UdpSocket::bind(bind).ok()?;
    socket.connect(probe).ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

fn load_tls(cert: &Path, key: &Path) -> Result<TlsAcceptor> {
    use tokio_rustls::rustls::pki_types::pem::PemObject as _;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("read the certificate {}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("read the certificate {}", cert.display()))?;
    if certs.is_empty() {
        anyhow::bail!("{} holds no certificate", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("read the private key {}", key.display()))?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut config = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the certificate and key don't go together")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

async fn accept_loop(
    listener: TcpListener,
    inner: Arc<Inner>,
    plan: Arc<WebPlan>,
    tls: Option<TlsAcceptor>,
    mut stop: watch::Receiver<bool>,
) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    // Full: this connection is dropped unanswered.
                    let Ok(slot) = slots.clone().try_acquire_owned() else {
                        tracing::warn!(%peer, "web access: too many connections");
                        continue;
                    };
                    let ctx = http::Ctx {
                        web: inner.clone(),
                        plan: plan.clone(),
                        peer,
                        tls: tls.is_some(),
                        stop: stop.clone(),
                    };
                    let tls = tls.clone();
                    connections.spawn(async move {
                        serve_connection(stream, tls, ctx).await;
                        drop(slot);
                    });
                }
                Err(err) => {
                    tracing::error!(%err, "web access: accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            // Reap finished connections so the set doesn't grow.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
        }
    }
    drop(listener);
    // Connections watch `stop` themselves; give them a moment, then cut.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.shutdown().await;
}

async fn serve_connection(stream: tokio::net::TcpStream, tls: Option<TlsAcceptor>, ctx: http::Ctx) {
    let _ = stream.set_nodelay(true);
    let io: Box<dyn Io> = match tls {
        Some(acceptor) => match tokio::time::timeout(HEAD_TIMEOUT, acceptor.accept(stream)).await {
            Ok(Ok(stream)) => Box::new(stream),
            // Not TLS, a failed handshake, or too slow.
            _ => return,
        },
        None => Box::new(stream),
    };
    let mut stop = ctx.stop.clone();
    let service = hyper::service::service_fn(move |request| {
        let ctx = ctx.clone();
        async move { Ok::<_, std::convert::Infallible>(http::handle(request, ctx).await) }
    });
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEAD_TIMEOUT)
        .max_buf_size(MAX_HEAD)
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades();
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => {}
        _ = raised(&mut stop) => {
            connection.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(Duration::from_secs(2), connection).await;
        }
    }
}
