//! Web access end to end (ADR-0016): a real daemon, its real listener on a
//! loopback port, and hand-written HTTP so every header is ours to forge.

use futures::{SinkExt as _, StreamExt as _};
use loft_client::Client;
use loft_config::Paths;
use loft_harness::Harness;
use loft_protocol::*;
use loftd::{Daemon, DaemonOptions};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

struct Fixture {
    daemon: Daemon,
    paths: Paths,
    dir: tempfile::TempDir,
    addr: SocketAddr,
}

/// A daemon with web access on a free loopback port. `extra` is appended
/// to the `[web]` section.
async fn start_with(extra: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    std::fs::create_dir_all(&paths.home).unwrap();
    std::fs::write(
        paths.config_file(),
        format!("[web]\nenabled = true\nport = 0\n{extra}\n"),
    )
    .unwrap();
    let bundle = dir.path().join("bundle");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(bundle.join("index.html"), "<!doctype html><title>t</title>").unwrap();
    std::fs::write(bundle.join("boot.js"), "// boot").unwrap();
    std::fs::write(bundle.join("loft_web.js"), "// the app").unwrap();
    std::fs::write(bundle.join("loft_web_bg.wasm"), b"\0asm-plain").unwrap();
    std::fs::write(bundle.join("loft_web_bg.wasm.gz"), b"gzipped").unwrap();
    std::fs::write(dir.path().join("secret.txt"), "hunter2").unwrap();

    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(loft_harness::fake::FakeHarness)];
    let daemon = Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses,
        capture_login_env: false,
        mcp_bridge: None,
        web_bundle: Some(bundle),
    })
    .await
    .unwrap();
    let addr = daemon
        .web
        .local_addr()
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0)));
    Fixture {
        daemon,
        paths,
        dir,
        addr,
    }
}

async fn start() -> Fixture {
    start_with("").await
}

impl Fixture {
    fn host(&self) -> String {
        self.addr.to_string()
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The pairing token, as the desktop app would show it in the link.
    fn token(&self) -> String {
        let url = self.daemon.web.status().pairing_url.unwrap();
        url.split_once("#token=").unwrap().1.to_owned()
    }

    /// Pair like our page does and return the session cookie (`name=value`).
    async fn pair(&self) -> String {
        let reply = self.pair_with(&self.token()).await;
        assert_eq!(reply.status, 200, "{reply:?}");
        let cookie = reply.header("set-cookie").unwrap();
        cookie.split(';').next().unwrap().to_owned()
    }

    async fn pair_with(&self, token: &str) -> Reply {
        let body = serde_json::json!({ "token": token }).to_string();
        send(
            self.addr,
            &request(
                "POST",
                "/api/pair",
                &[
                    ("Host", &self.host()),
                    ("Origin", &self.origin()),
                    ("Content-Type", "application/json"),
                ],
                &body,
            ),
        )
        .await
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> Reply {
        let host = self.host();
        let mut all = vec![("Host", host.as_str())];
        all.extend_from_slice(headers);
        send(self.addr, &request("GET", path, &all, "")).await
    }

    /// Try to open the WebSocket with these headers.
    async fn ws(
        &self,
        headers: &[(&str, &str)],
    ) -> Result<tokio_tungstenite::WebSocketStream<TcpStream>, u16> {
        let mut req = format!("ws://{}/ws", self.addr)
            .into_client_request()
            .unwrap();
        for (name, value) in headers {
            req.headers_mut().insert(
                tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes())
                    .unwrap(),
                value.parse().unwrap(),
            );
        }
        let stream = TcpStream::connect(self.addr).await.unwrap();
        match tokio_tungstenite::client_async(req, stream).await {
            Ok((socket, _)) => Ok(socket),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                Err(response.status().as_u16())
            }
            Err(other) => panic!("{other:?}"),
        }
    }

    /// An open, greeted WebSocket for a freshly paired browser.
    async fn connected(&self) -> (Socket, String) {
        let (socket, cookie, _) = self.greeted().await;
        (socket, cookie)
    }

    /// The same, with what the daemon said this session may do.
    async fn greeted(&self) -> (Socket, String, RemoteAccess) {
        let cookie = self.pair().await;
        let mut socket = Socket(
            self.ws(&[("Cookie", &cookie), ("Origin", &self.origin())])
                .await
                .unwrap(),
        );
        socket
            .send(&ClientMsg::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                // A browser can claim anything; the listener decides.
                client_kind: ClientKind::LocalUi,
                client_build: "test".into(),
                capabilities: vec!["browser".into()],
            }))
            .await;
        // A browser session is always told it is one, whatever it claimed.
        let access = match socket.recv().await {
            ServerMsg::Welcome(welcome) => welcome.remote.expect("a remote session"),
            other => panic!("{other:?}"),
        };
        (socket, cookie, access)
    }

    /// The desktop app's side: a local client on the Unix socket.
    async fn local(&self) -> Client {
        Client::connect(&self.paths.socket_file(), ClientKind::Test, "test")
            .await
            .unwrap()
            .0
    }
}

struct Socket(tokio_tungstenite::WebSocketStream<TcpStream>);

impl Socket {
    async fn send(&mut self, msg: &ClientMsg) {
        self.0
            .send(Message::text(serde_json::to_string(msg).unwrap()))
            .await
            .unwrap();
    }

    async fn next(&mut self) -> Option<Message> {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), self.0.next())
                .await
                .expect("timed out waiting for the daemon")?;
            match message {
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(message) => return Some(message),
                Err(_) => return None,
            }
        }
    }

    async fn recv(&mut self) -> ServerMsg {
        match self.next().await {
            Some(Message::Text(text)) => serde_json::from_str(&text).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    async fn request(&mut self, id: u64, body: Request) -> Result<Response, ErrorBody> {
        self.send(&ClientMsg::Request { id, body }).await;
        loop {
            if let ServerMsg::Response { id: got, result } = self.recv().await
                && got == id
            {
                return result;
            }
        }
    }

    /// The close code the daemon ends the connection with.
    async fn closed(&mut self) -> Option<u16> {
        loop {
            match self.next().await {
                Some(Message::Close(frame)) => return frame.map(|f| u16::from(f.code)),
                Some(_) => continue,
                None => return None,
            }
        }
    }
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

fn request(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut out = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("Connection: close\r\n\r\n");
    out.push_str(body);
    out.into_bytes()
}

/// Send raw bytes and read the whole reply.
async fn send(addr: SocketAddr, raw: &[u8]) -> Reply {
    let stream = TcpStream::connect(addr).await.unwrap();
    exchange(stream, raw).await
}

async fn exchange<S>(mut stream: S, raw: &[u8]) -> Reply
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // The daemon may answer (and close) before a large request is all
    // written; that is not an error here.
    let _ = stream.write_all(raw).await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut bytes)).await;
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Reply {
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(bytes.len());
    let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: bytes.get(split + 4..).unwrap_or_default().to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Off by default; local by default
// ---------------------------------------------------------------------------

#[tokio::test]
async fn off_unless_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    let daemon = Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses: vec![],
        capture_login_env: false,
        mcp_bridge: None,
        web_bundle: None,
    })
    .await
    .unwrap();
    let status = daemon.web.status();
    assert!(!status.enabled && !status.running);
    assert!(status.url.is_none() && status.pairing_url.is_none());
    assert!(daemon.web.local_addr().is_none());
    // Nothing was created for a feature that is off.
    assert!(!paths.web_dir().exists());
    daemon.shutdown().await;
}

#[tokio::test]
async fn listens_on_loopback_and_follows_the_config() {
    let f = start().await;
    let status = f.daemon.web.status();
    assert!(status.enabled && status.running, "{status:?}");
    assert!(f.addr.ip().is_loopback());
    assert!(!status.tls && !status.remote && !status.insecure_remote);
    assert_eq!(status.url.as_deref(), Some(f.origin().as_str()));
    assert!(status.bundle);

    // Turning it off in config.toml stops the listener…
    std::fs::write(f.paths.config_file(), "[web]\nenabled = false\n").unwrap();
    let local = f.local().await;
    local.request(Request::ReloadConfig).await.unwrap();
    assert!(!f.daemon.web.status().running);
    assert!(TcpStream::connect(f.addr).await.is_err());
    // …and on again brings it back.
    std::fs::write(f.paths.config_file(), "[web]\nenabled = true\nport = 0\n").unwrap();
    local.request(Request::ReloadConfig).await.unwrap();
    assert!(f.daemon.web.status().running);
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn a_remote_address_without_tls_is_refused() {
    for bind in ["0.0.0.0", "::", "192.0.2.10"] {
        let f = start_with(&format!("bind = \"{bind}\"")).await;
        let status = f.daemon.web.status();
        assert!(status.enabled && !status.running, "{bind}: {status:?}");
        let error = status.error.unwrap();
        assert!(error.contains("TLS"), "{bind}: {error}");
        assert!(status.pairing_url.is_none());
        f.daemon.shutdown().await;
    }
    // Half a TLS setup is an error too, not a silent downgrade.
    let f = start_with("tls_cert = \"/nonexistent/cert.pem\"").await;
    assert!(!f.daemon.web.status().running);
    f.daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_page_shell_is_public_and_the_app_is_not() {
    let f = start().await;
    for path in ["/", "/index.html", "/boot.js"] {
        assert_eq!(f.get(path, &[]).await.status, 200, "{path}");
    }
    for path in ["/loft_web.js", "/loft_web_bg.wasm"] {
        assert_eq!(f.get(path, &[]).await.status, 401, "{path}");
    }
    let session = f.get("/api/session", &[]).await;
    assert_eq!(session.status, 401);
    assert_eq!(session.json()["authenticated"], false);

    let cookie = f.pair().await;
    let app = f.get("/loft_web.js", &[("Cookie", &cookie)]).await;
    assert_eq!(app.status, 200);
    assert_eq!(app.text(), "// the app");
    let session = f.get("/api/session", &[("Cookie", &cookie)]).await;
    assert_eq!(session.json()["authenticated"], true);
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn a_wrong_token_is_rejected() {
    let f = start().await;
    let token = f.token();
    for wrong in ["", "wrong", &token[1..], &format!("{token}0")] {
        let reply = f.pair_with(wrong).await;
        assert_eq!(reply.status, 401, "{wrong:?}");
        assert!(reply.header("set-cookie").is_none());
    }
    assert!(f.daemon.web.status().sessions.is_empty());
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn the_token_only_counts_in_the_request_body() {
    let f = start().await;
    let token = f.token();
    // Not in the query, where it would end up in logs and history…
    let reply = f.get(&format!("/api/session?token={token}"), &[]).await;
    assert_eq!(reply.status, 401);
    let reply = f.get(&format!("/loft_web.js?token={token}"), &[]).await;
    assert_eq!(reply.status, 401);
    // …nor as a cookie or bearer: it buys a session, it isn't one.
    let as_cookie = format!("agentloft_session={token}");
    assert_eq!(
        f.get("/api/session", &[("Cookie", &as_cookie)])
            .await
            .status,
        401
    );
    let as_bearer = format!("Bearer {token}");
    assert_eq!(
        f.get("/api/session", &[("Authorization", &as_bearer)])
            .await
            .status,
        401
    );
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn the_session_cookie_is_httponly_strict_and_not_the_token() {
    let f = start().await;
    let reply = f.pair_with(&f.token()).await;
    let cookie = reply.header("set-cookie").unwrap();
    assert!(cookie.starts_with("agentloft_session="), "{cookie}");
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age="] {
        assert!(cookie.contains(attr), "{attr} in {cookie}");
    }
    assert!(!cookie.contains("Domain"));
    assert!(!cookie.contains(&f.token()));
    // The secret isn't in the body either: page scripts never see it.
    assert!(reply.json().get("session").is_none());
    assert!(
        !reply
            .text()
            .contains(cookie.split(['=', ';']).nth(1).unwrap())
    );

    let sessions = f.daemon.web.status().sessions;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].addr, "127.0.0.1");
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn wrong_tokens_are_rate_limited() {
    let f = start().await;
    for _ in 0..5 {
        assert_eq!(f.pair_with("wrong").await.status, 401);
    }
    // Locked out: even the right token waits.
    let reply = f.pair_with(&f.token()).await;
    assert_eq!(reply.status, 429);
    let wait: u64 = reply.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=60).contains(&wait), "{wait}");
    assert_eq!(reply.json()["error"], "rate_limited");
    assert!(reply.header("set-cookie").is_none());
    assert!(f.daemon.web.status().sessions.is_empty());
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn logout_ends_the_session() {
    let f = start().await;
    let cookie = f.pair().await;
    let host = f.host();
    let origin = f.origin();
    let logout = |headers: Vec<(&'static str, String)>| {
        let headers: Vec<(String, String)> = headers
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        async move {
            let refs: Vec<(&str, &str)> = headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            send(f.addr, &request("POST", "/api/logout", &refs, "")).await
        }
    };
    // Another site can't sign the user out…
    let cross = logout(vec![
        ("Host", host.clone()),
        ("Cookie", cookie.clone()),
        ("Origin", "https://evil.example".into()),
    ])
    .await;
    assert_eq!(cross.status, 403);
    assert_eq!(
        f.get("/api/session", &[("Cookie", &cookie)]).await.status,
        200
    );
    // …our own page can.
    let own = logout(vec![
        ("Host", host),
        ("Cookie", cookie.clone()),
        ("Origin", origin),
    ])
    .await;
    assert_eq!(own.status, 200);
    assert!(own.header("set-cookie").unwrap().contains("Max-Age=0"));
    assert_eq!(
        f.get("/api/session", &[("Cookie", &cookie)]).await.status,
        401
    );
    f.daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Browser attacks: DNS rebinding, cross-site requests, framing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_host_is_refused() {
    let f = start().await;
    let port = f.addr.port();
    for host in [
        "evil.example".to_owned(),
        format!("evil.example:{port}"),
        format!("127.0.0.1.evil.example:{port}"),
        format!("localhost.evil.example:{port}"),
        String::new(),
    ] {
        let reply = send(f.addr, &request("GET", "/", &[("Host", &host)], "")).await;
        assert_eq!(reply.status, 403, "{host:?}");
        // Not even the public shell, and the refusal is still hardened.
        assert!(!reply.text().contains("doctype"));
        assert!(reply.header("content-security-policy").is_some());
    }
    // No Host at all, or two of them.
    let none = send(f.addr, &request("GET", "/", &[], "")).await;
    assert!(matches!(none.status, 400 | 403), "{none:?}");
    let two = send(
        f.addr,
        &request(
            "GET",
            "/",
            &[("Host", &f.host()), ("Host", "evil.example")],
            "",
        ),
    )
    .await;
    assert!(matches!(two.status, 400 | 403), "{two:?}");

    // Pairing through a rebound name fails even with the right token.
    let body = serde_json::json!({ "token": f.token() }).to_string();
    let rebound = send(
        f.addr,
        &request(
            "POST",
            "/api/pair",
            &[
                ("Host", "evil.example"),
                ("Origin", "http://evil.example"),
                ("Content-Type", "application/json"),
            ],
            &body,
        ),
    )
    .await;
    assert_eq!(rebound.status, 403);

    // The names a browser legitimately uses.
    for host in [format!("localhost:{port}"), f.host()] {
        let reply = send(f.addr, &request("GET", "/", &[("Host", &host)], "")).await;
        assert_eq!(reply.status, 200, "{host}");
    }
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn extra_host_names_come_from_the_config() {
    let f = start_with("allowed_hosts = [\"desk.tailnet.example\"]").await;
    let ok = send(
        f.addr,
        &request("GET", "/", &[("Host", "desk.tailnet.example")], ""),
    )
    .await;
    assert_eq!(ok.status, 200);
    let other = send(
        f.addr,
        &request("GET", "/", &[("Host", "other.tailnet.example")], ""),
    )
    .await;
    assert_eq!(other.status, 403);

    // Behind a TLS-terminating proxy the browser's origin is https: the
    // cookie is then Secure and host-locked.
    let body = serde_json::json!({ "token": f.token() }).to_string();
    let paired = send(
        f.addr,
        &request(
            "POST",
            "/api/pair",
            &[
                ("Host", "desk.tailnet.example"),
                ("Origin", "https://desk.tailnet.example"),
                ("Content-Type", "application/json"),
            ],
            &body,
        ),
    )
    .await;
    assert_eq!(paired.status, 200);
    let cookie = paired.header("set-cookie").unwrap();
    assert!(cookie.starts_with("__Host-agentloft_session="), "{cookie}");
    assert!(cookie.contains("Secure"));
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn the_websocket_needs_a_session() {
    let f = start().await;
    let origin = f.origin();
    assert_eq!(f.ws(&[("Origin", &origin)]).await.err(), Some(401));
    assert_eq!(
        f.ws(&[
            ("Origin", &origin),
            ("Cookie", "agentloft_session=not-a-session")
        ])
        .await
        .err(),
        Some(401)
    );
    let token_as_cookie = format!("agentloft_session={}", f.token());
    assert_eq!(
        f.ws(&[("Origin", &origin), ("Cookie", &token_as_cookie)])
            .await
            .err(),
        Some(401)
    );
    assert_eq!(
        f.ws(&[("Origin", &origin), ("Authorization", "Bearer nope")])
            .await
            .err(),
        Some(401)
    );
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn the_websocket_refuses_other_origins() {
    let f = start().await;
    let cookie = f.pair().await;
    let port = f.addr.port();
    for origin in [
        "https://evil.example".to_owned(),
        // Another port on this machine: a dev server, an agent's preview.
        "http://127.0.0.1:3000".to_owned(),
        format!("http://localhost:{port}"),
        format!("http://127.0.0.1:{port}.evil.example"),
        "null".to_owned(),
    ] {
        assert_eq!(
            f.ws(&[("Cookie", &cookie), ("Origin", &origin)])
                .await
                .err(),
            Some(403),
            "{origin}"
        );
    }
    // A cookie with no Origin at all isn't a browser we can vouch for.
    assert_eq!(f.ws(&[("Cookie", &cookie)]).await.err(), Some(403));
    // A browser that says another site is asking.
    assert_eq!(
        f.ws(&[
            ("Cookie", &cookie),
            ("Origin", &f.origin()),
            ("Sec-Fetch-Site", "cross-site")
        ])
        .await
        .err(),
        Some(403)
    );
    // A rebound host name, even with everything else right.
    assert_eq!(
        f.ws(&[
            ("Cookie", &cookie),
            ("Host", "evil.example"),
            ("Origin", "http://evil.example")
        ])
        .await
        .err(),
        Some(403)
    );
    // Our own page.
    assert!(
        f.ws(&[("Cookie", &cookie), ("Origin", &f.origin())])
            .await
            .is_ok()
    );
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn cross_site_pairing_is_refused() {
    let f = start().await;
    let body = serde_json::json!({ "token": f.token() }).to_string();
    let host = f.host();
    let post = |headers: Vec<(&'static str, &'static str)>, body: String| {
        let host = host.clone();
        async move {
            let mut all = vec![("Host", host.as_str())];
            all.extend(headers);
            send(f.addr, &request("POST", "/api/pair", &all, &body)).await
        }
    };
    // From another origin, with the right token.
    let cross = post(
        vec![
            ("Origin", "https://evil.example"),
            ("Content-Type", "application/json"),
        ],
        body.clone(),
    )
    .await;
    assert_eq!(cross.status, 403);
    // A form post (the one cross-site POST that needs no preflight).
    let form = post(
        vec![("Content-Type", "application/x-www-form-urlencoded")],
        format!("token={}", f.token()),
    )
    .await;
    assert_eq!(form.status, 415);
    let plain = post(vec![("Content-Type", "text/plain")], body.clone()).await;
    assert_eq!(plain.status, 415);
    assert!(f.daemon.web.status().sessions.is_empty());
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn responses_are_hardened_and_never_cors() {
    let f = start().await;
    let cookie = f.pair().await;
    for (path, headers) in [
        ("/", vec![]),
        ("/nope", vec![]),
        ("/api/session", vec![("Cookie", cookie.as_str())]),
        ("/loft_web.js", vec![("Cookie", cookie.as_str())]),
        ("/api/session", vec![("Origin", "https://evil.example")]),
    ] {
        let reply = f.get(path, &headers).await;
        let csp = reply.header("content-security-policy").unwrap();
        for part in [
            "default-src 'none'",
            "script-src 'self' 'wasm-unsafe-eval'",
            "frame-ancestors 'none'",
            "base-uri 'none'",
            "form-action 'none'",
        ] {
            assert!(csp.contains(part), "{path}: {part} in {csp}");
        }
        // No inline or evaluated script, ever; inline styles only as
        // attributes (GPUI's canvas), never as style sheets.
        for directive in csp.split(';').map(str::trim) {
            let relaxed =
                directive.contains("'unsafe-inline'") || directive.contains("'unsafe-eval'");
            assert!(
                !relaxed || directive.starts_with("style-src-attr "),
                "{directive}"
            );
        }
        assert!(csp.contains("style-src 'self';"));
        assert_eq!(reply.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(reply.header("x-frame-options"), Some("DENY"));
        assert_eq!(reply.header("referrer-policy"), Some("no-referrer"));
        assert_eq!(
            reply.header("cross-origin-resource-policy"),
            Some("same-origin")
        );
        assert!(
            !reply
                .headers
                .iter()
                .any(|(k, _)| k.to_ascii_lowercase().starts_with("access-control-")),
            "{path}: {:?}",
            reply.headers
        );
        // Plain HTTP on loopback: no HSTS to poison other local servers.
        assert!(reply.header("strict-transport-security").is_none());
    }
    // A CORS preflight gets no permission.
    let preflight = send(
        f.addr,
        &request(
            "OPTIONS",
            "/api/pair",
            &[
                ("Host", &f.host()),
                ("Origin", "https://evil.example"),
                ("Access-Control-Request-Method", "POST"),
                ("Access-Control-Request-Headers", "content-type"),
            ],
            "",
        ),
    )
    .await;
    assert_eq!(preflight.status, 405);
    assert!(preflight.header("access-control-allow-origin").is_none());
    for method in ["PUT", "DELETE", "PATCH", "TRACE"] {
        let reply = send(f.addr, &request(method, "/", &[("Host", &f.host())], "")).await;
        assert_eq!(reply.status, 405, "{method}");
    }
    f.daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn path_escapes_find_nothing() {
    let f = start().await;
    let cookie = f.pair().await;
    for path in [
        "/../secret.txt",
        "/..%2fsecret.txt",
        "/%2e%2e/secret.txt",
        "/%2e%2e%2fsecret.txt",
        "/..%5csecret.txt",
        "/./../secret.txt",
        "/boot.js/../../secret.txt",
        "//secret.txt",
        "/..;/secret.txt",
        "/index.html%00.txt",
        "/.env",
        "/etc/passwd",
    ] {
        let reply = f.get(path, &[("Cookie", &cookie)]).await;
        assert!(matches!(reply.status, 400 | 404), "{path}: {reply:?}");
        assert!(!reply.text().contains("hunter2"), "{path}");
    }
    #[cfg(unix)]
    {
        // A symlink planted in the bundle isn't followed.
        let secret = f.dir.path().join("secret.txt");
        std::os::unix::fs::symlink(&secret, f.dir.path().join("bundle/leak.txt")).unwrap();
        tokio::time::sleep(Duration::from_millis(2100)).await;
        let reply = f.get("/leak.txt", &[("Cookie", &cookie)]).await;
        assert_eq!(reply.status, 404);
    }
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn assets_revalidate_and_compress() {
    let f = start().await;
    let cookie = f.pair().await;
    let wasm = f.get("/loft_web_bg.wasm", &[("Cookie", &cookie)]).await;
    assert_eq!(wasm.status, 200);
    assert_eq!(wasm.header("content-type"), Some("application/wasm"));
    assert_eq!(wasm.body, b"\0asm-plain");
    assert_eq!(wasm.header("cache-control"), Some("no-cache"));
    let etag = wasm.header("etag").unwrap().to_owned();

    let cached = f
        .get(
            "/loft_web_bg.wasm",
            &[("Cookie", &cookie), ("If-None-Match", &etag)],
        )
        .await;
    assert_eq!(cached.status, 304);
    assert!(cached.body.is_empty());

    let gz = f
        .get(
            "/loft_web_bg.wasm",
            &[("Cookie", &cookie), ("Accept-Encoding", "gzip, br")],
        )
        .await;
    assert_eq!(gz.header("content-encoding"), Some("gzip"));
    assert_eq!(gz.body, b"gzipped");
    // The compressed copy isn't a file of its own.
    assert_eq!(
        f.get("/loft_web_bg.wasm.gz", &[("Cookie", &cookie)])
            .await
            .status,
        404
    );
    f.daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Size and connection limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversized_requests_are_cut_off() {
    let f = start().await;
    // A pairing body far over its 4 KiB.
    let big = serde_json::json!({ "token": "x".repeat(64 * 1024) }).to_string();
    let reply = f.pair_with_body(&big).await;
    assert_eq!(reply.status, 413, "{reply:?}");
    // …which wasn't counted as a wrong guess.
    assert_eq!(f.pair_with(&f.token()).await.status, 200);

    // A request head over its 32 KiB.
    let huge = "a".repeat(64 * 1024);
    let reply = f.get("/", &[("X-Padding", &huge)]).await;
    assert!(matches!(reply.status, 400 | 431 | 0), "{}", reply.status);
    assert!(!reply.text().contains("doctype"));
    f.daemon.shutdown().await;
}

impl Fixture {
    async fn pair_with_body(&self, body: &str) -> Reply {
        send(
            self.addr,
            &request(
                "POST",
                "/api/pair",
                &[
                    ("Host", &self.host()),
                    ("Origin", &self.origin()),
                    ("Content-Type", "application/json"),
                ],
                body,
            ),
        )
        .await
    }
}

#[tokio::test]
async fn an_oversized_websocket_message_ends_the_connection() {
    let f = start().await;
    let (mut socket, _) = f.connected().await;
    // One byte over the protocol's frame limit.
    let big = "x".repeat(MAX_FRAME_LEN + 1);
    let _ = socket.0.send(Message::text(big)).await;
    let code = socket.closed().await;
    assert!(
        matches!(code, Some(1009) | None),
        "closed with {code:?} (1009 = message too big)"
    );
    // The daemon is fine: a new connection works.
    let (mut again, _) = f.connected().await;
    assert_eq!(again.request(1, Request::Ping).await, Ok(Response::Pong));
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn connections_are_capped() {
    let f = start().await;
    // Hold every slot open with connections that say nothing.
    let mut held = Vec::new();
    for _ in 0..128 {
        held.push(TcpStream::connect(f.addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    // One more gets no answer.
    let stream = TcpStream::connect(f.addr).await.unwrap();
    let reply = exchange(stream, &request("GET", "/", &[("Host", &f.host())], "")).await;
    assert_eq!(reply.status, 0, "{reply:?}");
    // Slots come back when connections go.
    drop(held);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(f.get("/", &[]).await.status, 200);
    f.daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// The protocol over the WebSocket, and what a browser session may do
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_websocket_carries_the_protocol() {
    let f = start().await;
    let (mut socket, _) = f.connected().await;
    assert_eq!(socket.request(1, Request::Ping).await, Ok(Response::Pong));

    socket
        .send(&ClientMsg::Subscribe {
            sub_id: 7,
            topic: Topic::Workspace,
        })
        .await;
    match socket.recv().await {
        ServerMsg::Snapshot {
            sub_id: 7,
            snapshot: Snapshot::Workspace { .. },
        } => {}
        other => panic!("{other:?}"),
    }
    // Binary frames work too.
    let ping = serde_json::to_vec(&ClientMsg::Request {
        id: 2,
        body: Request::Ping,
    })
    .unwrap();
    socket.0.send(Message::binary(ping)).await.unwrap();
    loop {
        if let ServerMsg::Response { id: 2, result } = socket.recv().await {
            assert_eq!(result, Ok(Response::Pong));
            break;
        }
    }
    assert_eq!(f.daemon.web.status().sessions[0].connections, 1);
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn a_browser_session_is_held_to_its_limits() {
    let f = start_with("allow_terminal = false").await;
    let (mut socket, _, access) = f.greeted().await;
    assert!(!access.terminal);
    let forbidden = |r: Result<Response, ErrorBody>| {
        matches!(&r, Err(e) if e.code == ErrorCode::Forbidden) || panic!("{r:?}")
    };
    // Can't stop the daemon, manage web access, or see the pairing link.
    forbidden(
        socket
            .request(
                1,
                Request::Shutdown {
                    reason: ShutdownReason::User,
                },
            )
            .await,
    );
    forbidden(socket.request(2, Request::WebStatus).await);
    forbidden(socket.request(3, Request::WebRotateToken).await);
    // Can't point the daemon at arbitrary folders or files.
    forbidden(
        socket
            .request(
                4,
                Request::AddProject {
                    path: "/".into(),
                    name: None,
                },
            )
            .await,
    );
    let secret = f.dir.path().join("secret.txt");
    let read = socket
        .request(
            5,
            Request::ReadPath {
                path: secret.to_string_lossy().into_owned(),
                thread_id: None,
            },
        )
        .await;
    assert!(read.is_err(), "{read:?}");
    // Can't change what agents run.
    forbidden(
        socket
            .request(
                6,
                Request::ChangeSetting {
                    key: "harness.claude.path".into(),
                    value: serde_json::json!("/tmp/evil"),
                },
            )
            .await,
    );
    // No terminal once the config says so: not its requests, not its output.
    forbidden(
        socket
            .request(
                7,
                Request::ListTerminals {
                    key_prefix: String::new(),
                },
            )
            .await,
    );
    socket
        .send(&ClientMsg::Subscribe {
            sub_id: 9,
            topic: Topic::Terminal(TerminalId::new()),
        })
        .await;
    assert!(matches!(socket.recv().await, ServerMsg::Fatal(e) if e.code == ErrorCode::Forbidden));
    // The daemon is still up, and ordinary work goes through.
    assert_eq!(socket.request(10, Request::Ping).await, Ok(Response::Pong));
    assert!(f.daemon.web.status().running);
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn terminals_follow_who_can_reach_the_listener() {
    // Only this machine can reach it: a browser gets a shell, and is told.
    let f = start().await;
    let (mut socket, _, access) = f.greeted().await;
    assert!(access.terminal);
    let list = Request::ListTerminals {
        key_prefix: String::new(),
    };
    assert!(
        matches!(
            socket.request(1, list.clone()).await,
            Ok(Response::Terminals { .. })
        ),
        "terminal requests go through"
    );
    f.daemon.shutdown().await;

    // A proxy's or tunnel's name in front of it: not unless asked for.
    let f = start_with("allowed_hosts = [\"desk.example\"]").await;
    let (mut socket, _, access) = f.greeted().await;
    assert!(!access.terminal);
    let refused = socket.request(1, list.clone()).await;
    assert!(
        matches!(&refused, Err(e) if e.code == ErrorCode::Forbidden),
        "{refused:?}"
    );
    f.daemon.shutdown().await;

    // Asked for, it is given there too.
    let f = start_with("allowed_hosts = [\"desk.example\"]\nallow_terminal = true").await;
    let (_, _, access) = f.greeted().await;
    assert!(access.terminal);
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn only_local_clients_manage_web_access() {
    let f = start().await;
    let local = f.local().await;
    let Response::WebStatus { status } = local.request(Request::WebStatus).await.unwrap() else {
        panic!()
    };
    assert!(status.running);
    let link = status.pairing_url.unwrap();
    // The token rides in the fragment, never the query.
    assert!(link.contains("/#token="), "{link}");
    assert!(!link.contains('?'));
    // A local client isn't a remote session.
    assert!(local.welcome().remote.is_none());
    f.daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Revocation and rotation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoking_a_session_closes_its_connection_at_once() {
    let f = start().await;
    let (mut socket, cookie) = f.connected().await;
    let local = f.local().await;
    let Response::WebStatus { status } = local.request(Request::WebStatus).await.unwrap() else {
        panic!()
    };
    assert_eq!(status.sessions.len(), 1);
    let ours = status.sessions[0].id.clone();
    let (mut other, other_cookie) = f.connected().await;
    assert_eq!(f.daemon.web.status().sessions.len(), 2);

    let started = std::time::Instant::now();
    let Response::WebStatus { status } = local
        .request(Request::WebRevokeSession { id: ours })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(status.sessions.len(), 1);
    assert_eq!(socket.closed().await, Some(4401));
    assert!(started.elapsed() < Duration::from_secs(1));
    // Its cookie is dead for new requests and reconnects.
    assert_eq!(
        f.get("/api/session", &[("Cookie", &cookie)]).await.status,
        401
    );
    assert_eq!(
        f.ws(&[("Cookie", &cookie), ("Origin", &f.origin())])
            .await
            .err(),
        Some(401)
    );
    // The other browser is untouched…
    assert_eq!(other.request(1, Request::Ping).await, Ok(Response::Pong));
    // …until everyone is signed out.
    local.request(Request::WebRevokeAllSessions).await.unwrap();
    assert_eq!(other.closed().await, Some(4401));
    assert_eq!(
        f.get("/api/session", &[("Cookie", &other_cookie)])
            .await
            .status,
        401
    );
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn rotating_the_token_ends_the_old_link_and_every_session() {
    let f = start().await;
    let old = f.token();
    let (mut socket, cookie) = f.connected().await;
    let local = f.local().await;
    let Response::WebStatus { status } = local.request(Request::WebRotateToken).await.unwrap()
    else {
        panic!()
    };
    assert!(status.sessions.is_empty());
    let new = f.token();
    assert_ne!(old, new);
    assert_eq!(socket.closed().await, Some(4401));
    assert_eq!(
        f.get("/api/session", &[("Cookie", &cookie)]).await.status,
        401
    );
    assert_eq!(f.pair_with(&old).await.status, 401);
    assert_eq!(f.pair_with(&new).await.status, 200);
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn turning_web_access_off_drops_connections() {
    let f = start().await;
    let (mut socket, _) = f.connected().await;
    std::fs::write(f.paths.config_file(), "[web]\nenabled = false\n").unwrap();
    f.local()
        .await
        .request(Request::ReloadConfig)
        .await
        .unwrap();
    // 1001 = going away: the page should retry, not ask to pair again.
    let code = socket.closed().await;
    assert!(matches!(code, Some(1001) | None), "{code:?}");
    assert_eq!(u16::from(CloseCode::Away), 1001);
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn sessions_survive_a_daemon_restart() {
    let f = start().await;
    let cookie = f.pair().await;
    let Fixture {
        daemon, paths, dir, ..
    } = f;
    daemon.shutdown().await;

    let daemon = Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses: vec![],
        capture_login_env: false,
        mcp_bridge: None,
        web_bundle: Some(dir.path().join("bundle")),
    })
    .await
    .unwrap();
    let f = Fixture {
        addr: daemon.web.local_addr().unwrap(),
        daemon,
        paths,
        dir,
    };
    assert_eq!(
        f.get("/api/session", &[("Cookie", &cookie)]).await.status,
        200
    );
    f.daemon.shutdown().await;
}

// ---------------------------------------------------------------------------
// Non-browser clients and TLS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_native_client_can_pair_for_a_bearer_session() {
    let f = start().await;
    let body = serde_json::json!({ "token": f.token(), "bearer": true }).to_string();
    // No Origin: not a browser.
    let reply = send(
        f.addr,
        &request(
            "POST",
            "/api/pair",
            &[("Host", &f.host()), ("Content-Type", "application/json")],
            &body,
        ),
    )
    .await;
    assert_eq!(reply.status, 200);
    assert!(reply.header("set-cookie").is_none());
    let session = reply.json()["session"].as_str().unwrap().to_owned();
    let bearer = format!("Bearer {session}");
    // A bearer session needs no Origin (nothing sends it automatically)…
    assert!(f.ws(&[("Authorization", &bearer)]).await.is_ok());
    // …but a wrong one is still refused.
    assert_eq!(
        f.ws(&[
            ("Authorization", &bearer),
            ("Origin", "https://evil.example")
        ])
        .await
        .err(),
        Some(403)
    );
    f.daemon.shutdown().await;
}

#[tokio::test]
async fn tls_serves_https_with_secure_cookies() {
    use tokio_rustls::rustls;
    let certs = tempfile::tempdir().unwrap();
    let generated =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .unwrap();
    let cert_file = certs.path().join("cert.pem");
    let key_file = certs.path().join("key.pem");
    std::fs::write(&cert_file, generated.cert.pem()).unwrap();
    std::fs::write(&key_file, generated.signing_key.serialize_pem()).unwrap();
    let f = start_with(&format!(
        "tls_cert = \"{}\"\ntls_key = \"{}\"",
        cert_file.display(),
        key_file.display()
    ))
    .await;
    let status = f.daemon.web.status();
    assert!(status.running && status.tls, "{status:?}");
    assert!(status.url.as_deref().unwrap().starts_with("https://"));

    let mut roots = rustls::RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tls = |raw: Vec<u8>| {
        let connector = connector.clone();
        async move {
            let tcp = TcpStream::connect(f.addr).await.unwrap();
            let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let stream = connector.connect(name, tcp).await.unwrap();
            exchange(stream, &raw).await
        }
    };

    let host = format!("localhost:{}", f.addr.port());
    let page = tls(request("GET", "/", &[("Host", &host)], "")).await;
    assert_eq!(page.status, 200);
    assert!(
        page.header("strict-transport-security")
            .unwrap()
            .contains("max-age=")
    );

    let body = serde_json::json!({ "token": f.token() }).to_string();
    let origin = format!("https://{host}");
    let paired = tls(request(
        "POST",
        "/api/pair",
        &[
            ("Host", &host),
            ("Origin", &origin),
            ("Content-Type", "application/json"),
        ],
        &body,
    ))
    .await;
    assert_eq!(paired.status, 200);
    let cookie = paired.header("set-cookie").unwrap();
    assert!(cookie.starts_with("__Host-agentloft_session="), "{cookie}");
    assert!(cookie.contains("Secure") && cookie.contains("HttpOnly"));

    // Plain HTTP to the TLS port gets nothing.
    let plain = send(f.addr, &request("GET", "/", &[("Host", &host)], "")).await;
    assert_eq!(plain.status, 0, "{plain:?}");
    f.daemon.shutdown().await;
}
