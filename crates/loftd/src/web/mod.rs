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
    pub(crate) core: Arc<Core>,
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
            status.addresses = addresses(&running.plan, running.addr, scheme);
            let url = status
                .addresses
                .iter()
                .find(|a| a.default)
                .map(|a| a.url.clone())
                .unwrap_or_else(|| {
                    format!(
                        "{scheme}://{}",
                        display_authority(&running.plan, running.addr)
                    )
                });
            status.tls = running.plan.tls.is_some();
            status.remote = !running.addr.ip().is_loopback();
            status.insecure_remote = running.plan.insecure_remote;
            status.network = running.plan.private_peers_only;
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

/// Where the listener can be reached: on every interface, this computer's
/// loopback, local network and tailnet addresses (public ones only when
/// they're allowed to connect), and the names in `allowed_hosts`. The one
/// `link_host` names is the default; else the local network's (the address
/// on the default route), else the first.
fn addresses(plan: &WebPlan, addr: SocketAddr, scheme: &str) -> Vec<loft_protocol::WebAddress> {
    let port = addr.port();
    let url = |host: &str| {
        let authority = match host.parse::<IpAddr>() {
            Ok(IpAddr::V6(_)) => format!("[{host}]:{port}"),
            _ => format!("{host}:{port}"),
        };
        format!("{scheme}://{authority}")
    };
    let mut found: Vec<loft_protocol::WebAddress> = Vec::new();
    let mut add = |kind: &str, host: String| {
        if !found.iter().any(|a| a.host == host) {
            found.push(loft_protocol::WebAddress {
                kind: kind.into(),
                url: url(&host),
                host,
                default: false,
            });
        }
    };
    if addr.ip().is_unspecified() {
        add("local", "127.0.0.1".into());
        let mut ips = interface_addresses();
        // The default route's first: the address others most likely see.
        if let Some(main) = outbound_ip(false) {
            ips.retain(|ip| *ip != main);
            ips.insert(0, main);
        }
        for ip in ips {
            if ip.is_loopback() || ip.is_unspecified() || !ip.is_ipv4() {
                continue;
            }
            if loft_config::is_tailnet_address(ip) {
                add("tailnet", ip.to_string());
            } else if loft_config::is_private_address(ip) || !plan.private_peers_only {
                // A public address only where public peers may connect.
                add("lan", ip.to_string());
            }
        }
    } else if addr.ip().is_loopback() {
        add("local", addr.ip().to_string());
    } else {
        let kind = if loft_config::is_tailnet_address(addr.ip()) {
            "tailnet"
        } else {
            "lan"
        };
        add(kind, addr.ip().to_string());
    }
    for name in &plan.allowed_hosts {
        add("name", name.clone());
    }
    let chosen = plan
        .link_host
        .as_deref()
        .and_then(|host| found.iter().position(|a| a.host == host));
    let fallback = || {
        // A named proxy in front of a loopback listener, as before.
        if addr.ip().is_loopback() {
            found.iter().position(|a| a.kind == "name")
        } else {
            found
                .iter()
                .position(|a| a.kind == "lan" || a.kind == "tailnet")
        }
        .or(if found.is_empty() { None } else { Some(0) })
    };
    if let Some(ix) = chosen.or_else(fallback) {
        found[ix].default = true;
    }
    found
}

/// Whether a connection from `peer` that arrived at `local` may be served.
fn admits(plan: &WebPlan, peer: IpAddr, local: Option<IpAddr>) -> bool {
    !plan.private_peers_only
        || loft_config::is_private_address(peer)
            && local.is_some_and(loft_config::is_private_address)
}

/// This computer's interface addresses.
fn interface_addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list we only read and
    // then free with freeifaddrs.
    #[allow(unsafe_code)]
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut at = list;
        while !at.is_null() {
            let entry = &*at;
            let up = entry.ifa_flags & (libc::IFF_UP as libc::c_uint) != 0;
            if up && !entry.ifa_addr.is_null() {
                match i32::from((*entry.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let v4 = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        out.push(IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(
                            v4.sin_addr.s_addr,
                        ))));
                    }
                    libc::AF_INET6 => {
                        let v6 = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                        out.push(IpAddr::V6(std::net::Ipv6Addr::from(v6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            at = entry.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
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
                    // Network access takes private networks only: both
                    // the peer's address and the one it reached must be
                    // private (a cloud VM's public address may be NATed
                    // to a private interface; its peers still aren't).
                    if !admits(&plan, peer.ip(), stream.local_addr().ok().map(|a| a.ip())) {
                        tracing::warn!(%peer, "web access: refused a connection from outside private networks");
                        continue;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(toml: &str) -> WebPlan {
        let config: WebConfig = toml::from_str(toml).unwrap();
        config.plan().unwrap()
    }

    #[test]
    fn network_access_serves_private_networks_only() {
        let network = plan("enabled = true\nnetwork = true\n");
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let home = Some(ip("192.168.1.20"));
        assert!(admits(&network, ip("192.168.1.30"), home));
        assert!(admits(&network, ip("100.100.1.2"), Some(ip("100.64.0.7"))));
        assert!(admits(&network, ip("127.0.0.1"), Some(ip("127.0.0.1"))));
        assert!(!admits(&network, ip("203.0.113.9"), home), "a public peer");
        assert!(
            !admits(&network, ip("192.168.1.30"), Some(ip("203.0.113.1"))),
            "reached at a public address"
        );
        assert!(!admits(&network, ip("192.168.1.30"), None));
        // Without network access, who may connect is the address's business.
        let tls = plan("enabled = true\nbind = \"0.0.0.0\"\nallow_insecure_remote = true\n");
        assert!(admits(&tls, ip("203.0.113.9"), home));
    }

    #[test]
    fn the_pairing_link_uses_the_chosen_address() {
        let local = plan("enabled = true\n");
        let found = addresses(&local, "127.0.0.1:4780".parse().unwrap(), "http");
        assert_eq!(found.len(), 1);
        assert!(found[0].default && found[0].kind == "local");
        assert_eq!(found[0].url, "http://127.0.0.1:4780");

        // A name in front of a loopback listener is the default, unless
        // another is chosen.
        let named = plan("enabled = true\nallowed_hosts = [\"desk.example\"]\n");
        let found = addresses(&named, "127.0.0.1:4780".parse().unwrap(), "https");
        let default: Vec<_> = found
            .iter()
            .filter(|a| a.default)
            .map(|a| &a.host)
            .collect();
        assert_eq!(default, ["desk.example"]);
        let chosen =
            plan("enabled = true\nallowed_hosts = [\"desk.example\"]\nlink_host = \"127.0.0.1\"\n");
        let found = addresses(&chosen, "127.0.0.1:4780".parse().unwrap(), "https");
        let default: Vec<_> = found
            .iter()
            .filter(|a| a.default)
            .map(|a| &a.host)
            .collect();
        assert_eq!(default, ["127.0.0.1"]);

        // Every interface: this computer's loopback is always one of them.
        let network = plan("enabled = true\nnetwork = true\n");
        let found = addresses(&network, "0.0.0.0:4780".parse().unwrap(), "http");
        assert!(found.iter().any(|a| a.kind == "local"));
        assert_eq!(found.iter().filter(|a| a.default).count(), 1);
        assert!(
            found.iter().all(
                |a| a.kind != "lan" || loft_config::is_private_address(a.host.parse().unwrap())
            )
        );
    }
}
