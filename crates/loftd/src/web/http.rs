//! One HTTP request to the web listener (ADR-0016): which names it may be
//! addressed to, where it may come from, who it is, and what it gets.
//!
//! Every request passes, in order:
//! 1. **Host**: only an IP literal, `localhost`, or a configured name. A
//!    page on an attacker's domain that re-points its DNS at this machine
//!    (DNS rebinding) still sends its own name here, and is refused.
//! 2. **Origin** (state-changing requests and the WebSocket): must be this
//!    very origin. Another site, or another port on this machine, can make a
//!    browser send our cookie, but not forge where the request came from.
//! 3. **Session**: everything except the page shell and pairing needs one.
//!
//! Responses never carry CORS headers, so no other origin can read them,
//! and always carry a strict Content-Security-Policy.

use super::auth::{Authed, PairError};
use super::{Inner, MAX_PAIR_BODY, bundle};
use bytes::Bytes;
use futures::TryStreamExt as _;
use http_body_util::{BodyExt as _, Full, Limited, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use loft_config::WebPlan;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::watch;

pub(crate) type Body = BoxBody<Bytes, std::io::Error>;

/// What a request handler knows about its connection.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub web: Arc<Inner>,
    pub plan: Arc<WebPlan>,
    pub peer: SocketAddr,
    /// This listener serves HTTPS itself.
    pub tls: bool,
    /// Flips when the listener is stopped.
    pub stop: watch::Receiver<bool>,
}

const COOKIE: &str = "agentloft_session";
/// The `__Host-` prefix makes browsers refuse the cookie unless it is
/// Secure, has `Path=/` and no `Domain`: it can't be set from a subdomain
/// or over plain HTTP.
const SECURE_COOKIE: &str = "__Host-agentloft_session";

fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

fn text(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut response = Response::new(full(message));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn json(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    let mut response = Response::new(full(value.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// A `Host` or `Origin` authority as (name, port). The name is lowercased,
/// IPv6 keeps its brackets. Anything with characters that don't belong in a
/// host name is refused outright.
pub(crate) fn split_authority(authority: &str) -> Option<(String, Option<u16>)> {
    let ok = !authority.is_empty()
        && authority.len() <= 255
        && authority
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
    if !ok {
        return None;
    }
    let (name, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']')?;
        addr.parse::<std::net::Ipv6Addr>().ok()?;
        let port = match tail {
            "" => None,
            tail => Some(tail.strip_prefix(':')?.parse().ok()?),
        };
        (format!("[{}]", addr.to_ascii_lowercase()), port)
    } else {
        match authority.rsplit_once(':') {
            Some((name, port)) => (name.to_ascii_lowercase(), Some(port.parse().ok()?)),
            None => (authority.to_ascii_lowercase(), None),
        }
    };
    let name = name.trim_end_matches('.').to_owned();
    let valid = name.starts_with('[') || (!name.is_empty() && !name.contains([':', '[', ']']));
    valid.then_some((name, port))
}

/// Whether a browser may address the listener as `host` (the `Host`
/// header). IP literals and `localhost` can't be re-pointed by someone
/// else's DNS; any other name has to be listed in `allowed_hosts`.
pub(crate) fn host_allowed(host: &str, allowed_hosts: &[String]) -> bool {
    let Some((name, port)) = split_authority(host) else {
        return false;
    };
    if name.starts_with('[') || name.parse::<std::net::Ipv4Addr>().is_ok() || name == "localhost" {
        return true;
    }
    allowed_hosts.iter().any(|entry| {
        split_authority(entry).is_some_and(|(allowed, allowed_port)| {
            allowed == name && (allowed_port.is_none() || allowed_port == port)
        })
    })
}

/// Whether `origin` (the `Origin` header) is the origin the request was
/// addressed to (`host`). The scheme is not compared: a reverse proxy may
/// terminate TLS in front of us.
pub(crate) fn same_origin(origin: &str, host: &str) -> bool {
    let (default_port, authority) = if let Some(rest) = origin.strip_prefix("https://") {
        (443, rest)
    } else if let Some(rest) = origin.strip_prefix("http://") {
        (80, rest)
    } else {
        // `null`, `file://`, extensions…
        return false;
    };
    let (Some((o_name, o_port)), Some((h_name, h_port))) =
        (split_authority(authority), split_authority(host))
    else {
        return false;
    };
    o_name == h_name
        && match (o_port, h_port) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            (Some(port), None) | (None, Some(port)) => port == default_port,
        }
}

/// The one value of a header that must not repeat.
fn single(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// How a request proved its session.
enum Proof {
    /// A browser's cookie: sent automatically, so the request's origin
    /// matters.
    Cookie,
    /// `Authorization: Bearer`, set deliberately by a non-browser client.
    Bearer,
}

fn session(ctx: &Ctx, headers: &HeaderMap) -> Option<(Authed, Proof)> {
    if let Some(bearer) = single(headers, header::AUTHORIZATION)
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
    {
        return ctx
            .web
            .auth
            .authenticate(bearer)
            .map(|a| (a, Proof::Bearer));
    }
    [SECURE_COOKIE, COOKIE]
        .iter()
        .filter_map(|name| cookie(headers, name))
        .find_map(|secret| ctx.web.auth.authenticate(secret))
        .map(|a| (a, Proof::Cookie))
}

/// Who is asking to open the WebSocket.
pub(crate) struct WsCaller {
    /// It comes from our own page (or from a non-browser client, which
    /// proves itself with a bearer session rather than a cookie).
    pub origin_ok: bool,
}

pub(crate) fn ws_session(ctx: &Ctx, headers: &HeaderMap, host: &str) -> Option<(Authed, WsCaller)> {
    let (authed, proof) = session(ctx, headers)?;
    let origin_ok = from_our_origin(headers, host, matches!(proof, Proof::Cookie));
    Some((authed, WsCaller { origin_ok }))
}

/// A request that changes something (or opens the WebSocket) must come from
/// our own page. `require` is set when the session came from a cookie.
fn from_our_origin(headers: &HeaderMap, host: &str, require: bool) -> bool {
    // Browsers say outright when another site is asking.
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && !matches!(site, "same-origin" | "none")
    {
        return false;
    }
    match single(headers, header::ORIGIN) {
        Some(origin) => same_origin(origin, host),
        None => !require && headers.get(header::ORIGIN).is_none(),
    }
}

fn set_cookie(secret: &str, max_age_secs: u64, secure: bool) -> Option<HeaderValue> {
    let value = if secure {
        format!(
            "{SECURE_COOKIE}={secret}; Path=/; Max-Age={max_age_secs}; HttpOnly; Secure; SameSite=Strict"
        )
    } else {
        format!("{COOKIE}={secret}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Strict")
    };
    HeaderValue::from_str(&value).ok()
}

/// Headers every response carries.
fn harden(response: &mut Response<Body>, host: Option<&str>, tls: bool) {
    let headers = response.headers_mut();
    // `connect-src 'self'` covers the WebSocket in current browsers; older
    // Safari wants the ws: origin spelled out. `host` was validated above:
    // it holds nothing but host-name characters.
    let ws = host
        .map(|h| format!(" ws://{h} wss://{h}"))
        .unwrap_or_default();
    // Style *attributes* are allowed because GPUI sizes its canvas and its
    // hidden text input with them; scripts and stylesheets are ours only.
    let csp = format!(
        "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; \
         style-src-attr 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; \
         connect-src 'self'{ws}; worker-src 'self' blob:; manifest-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    if let Ok(csp) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    for (name, value) in [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            header::HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (
            header::HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
        (
            header::HeaderName::from_static("permissions-policy"),
            "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    if tls {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
}

pub(crate) async fn handle(request: Request<Incoming>, ctx: Ctx) -> Response<Body> {
    // 1. Host.
    let host = single(request.headers(), header::HOST)
        .filter(|h| host_allowed(h, &ctx.plan.allowed_hosts))
        .map(str::to_owned);
    let mut response = match &host {
        Some(host) => route(request, &ctx, host).await,
        None => {
            tracing::warn!(peer = %ctx.peer, "web access: refused a request for an unknown host");
            text(StatusCode::FORBIDDEN, "This host name isn't allowed.\n")
        }
    };
    harden(&mut response, host.as_deref(), ctx.tls);
    response
}

async fn route(request: Request<Incoming>, ctx: &Ctx, host: &str) -> Response<Body> {
    let path = request.uri().path().to_owned();
    match (request.method().clone(), path.as_str()) {
        (Method::GET, "/api/session") => match session(ctx, request.headers()) {
            Some((authed, _)) => json(
                StatusCode::OK,
                serde_json::json!({
                    "authenticated": true,
                    "expires_ms": authed.expires_ms,
                }),
            ),
            None => json(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({ "authenticated": false }),
            ),
        },
        (Method::POST, "/api/pair") => pair(request, ctx, host).await,
        (Method::POST, "/api/logout") => logout(request, ctx, host),
        (Method::GET, "/ws") => super::ws::upgrade(request, ctx, host),
        (Method::GET | Method::HEAD, _) => asset(request, ctx, &path).await,
        // No CORS preflight is ever answered, and nothing else is offered.
        _ => text(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.\n"),
    }
}

/// `POST /api/pair` `{"token": "…"}`: trade the pairing token for a session
/// cookie.
async fn pair(request: Request<Incoming>, ctx: &Ctx, host: &str) -> Response<Body> {
    let headers = request.headers();
    if !from_our_origin(headers, host, false) {
        return text(StatusCode::FORBIDDEN, "Cross-site request refused.\n");
    }
    // JSON only: a cross-site form can't send it without a preflight, and
    // preflights go unanswered.
    let is_json = single(headers, header::CONTENT_TYPE)
        .is_some_and(|t| t.split(';').next().unwrap_or("").trim() == "application/json");
    if !is_json {
        return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send JSON.\n");
    }
    let secure = ctx.tls
        || single(headers, header::ORIGIN).is_some_and(|origin| origin.starts_with("https://"));
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let wants_bearer;
    let token = {
        let body = match Limited::new(request.into_body(), MAX_PAIR_BODY)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(_) => return text(StatusCode::PAYLOAD_TOO_LARGE, "Request too large.\n"),
        };
        #[derive(serde::Deserialize)]
        struct Pair {
            token: String,
            /// A non-browser client asks for the session in the reply
            /// instead of a cookie.
            #[serde(default)]
            bearer: bool,
        }
        match serde_json::from_slice::<Pair>(&body) {
            Ok(pair) => {
                wants_bearer = pair.bearer;
                pair.token
            }
            Err(_) => return text(StatusCode::BAD_REQUEST, "Bad request.\n"),
        }
    };
    match ctx
        .web
        .auth
        .pair(&token, ctx.peer.ip(), &user_agent, ctx.plan.session_ttl)
    {
        Ok((secret, authed)) => {
            let mut body = serde_json::json!({ "ok": true, "expires_ms": authed.expires_ms });
            let mut cookie = None;
            if wants_bearer {
                body["session"] = serde_json::Value::String(secret);
            } else {
                cookie = set_cookie(&secret, ctx.plan.session_ttl.as_secs(), secure);
            }
            let mut response = json(StatusCode::OK, body);
            if let Some(cookie) = cookie {
                response.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            response
        }
        Err(PairError::Invalid) => json(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({ "ok": false, "error": "invalid" }),
        ),
        Err(PairError::RateLimited(wait)) => {
            let secs = wait.as_secs().max(1);
            let mut response = json(
                StatusCode::TOO_MANY_REQUESTS,
                serde_json::json!({ "ok": false, "error": "rate_limited", "retry_after": secs }),
            );
            if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
        Err(PairError::Internal(err)) => {
            tracing::error!(%err, "web access: pairing failed");
            text(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.\n")
        }
    }
}

/// `POST /api/logout`: end this browser's session.
fn logout(request: Request<Incoming>, ctx: &Ctx, host: &str) -> Response<Body> {
    let headers = request.headers();
    let Some((authed, proof)) = session(ctx, headers) else {
        return json(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({ "authenticated": false }),
        );
    };
    if !from_our_origin(headers, host, matches!(proof, Proof::Cookie)) {
        return text(StatusCode::FORBIDDEN, "Cross-site request refused.\n");
    }
    ctx.web.auth.revoke(&authed.id);
    let mut response = json(StatusCode::OK, serde_json::json!({ "ok": true }));
    for name in [SECURE_COOKIE, COOKIE] {
        let secure = if name == SECURE_COOKIE {
            "; Secure"
        } else {
            ""
        };
        if let Ok(value) = HeaderValue::from_str(&format!(
            "{name}=; Path=/; Max-Age=0; HttpOnly{secure}; SameSite=Strict"
        )) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

/// A file of the web client.
async fn asset(request: Request<Incoming>, ctx: &Ctx, path: &str) -> Response<Body> {
    let Some(asset) = ctx.web.bundle.get(path) else {
        if path == "/" {
            let mut response = Response::new(full(bundle::MISSING_PAGE));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            return response;
        }
        return text(StatusCode::NOT_FOUND, "Not found.\n");
    };
    let headers = request.headers();
    if !asset.public && session(ctx, headers).is_none() {
        return text(StatusCode::UNAUTHORIZED, "Sign in first.\n");
    }

    let mut response = Response::new(full(Bytes::new()));
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    // Always revalidate: a rebuilt client is picked up at once, and an
    // unchanged file costs one small 304.
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(etag) = HeaderValue::from_str(&asset.etag) {
        out.insert(header::ETAG, etag);
    }
    out.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|tags| tags.split(',').any(|t| t.trim() == asset.etag));
    if unchanged {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        return response;
    }

    let accepts_gzip = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|e| e.split(';').next().unwrap_or("").trim() == "gzip")
        });
    let (file, len) = match &asset.gzip {
        Some((gz, len)) if accepts_gzip => {
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            (gz.clone(), *len)
        }
        _ => (asset.path.clone(), asset.len),
    };
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if request.method() == Method::HEAD {
        return response;
    }
    match tokio::fs::File::open(&file).await {
        Ok(file) => {
            // Streamed from disk: the app is tens of megabytes.
            let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);
            *response.body_mut() = StreamBody::new(stream.map_ok(Frame::data)).boxed();
            response
        }
        Err(_) => text(StatusCode::NOT_FOUND, "Not found.\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn host_must_be_an_address_localhost_or_a_listed_name() {
        for host in [
            "127.0.0.1:4780",
            "127.0.0.1",
            "localhost:4780",
            "LOCALHOST:4780",
            "localhost.:4780",
            "[::1]:4780",
            "192.168.1.20:4780",
            "100.64.0.7",
        ] {
            assert!(host_allowed(host, &[]), "{host}");
        }
        // A rebinding page still announces its own name.
        for host in [
            "evil.example:4780",
            "evil.example",
            "127.0.0.1.evil.example:4780",
            "localhost.evil.example",
            "sub.localhost",
            "",
            "127.0.0.1:4780@evil.example",
            "evil.example/127.0.0.1",
            "127.0.0.1:notaport",
            "[::1",
            "[nonsense]:80",
            "local host",
            "localhost:4780\r\nX-Injected: 1",
        ] {
            assert!(!host_allowed(host, &[]), "{host:?}");
        }
    }

    #[test]
    fn listed_names_are_allowed_with_or_without_a_port() {
        let any_port = allowed(&["desk.tailnet.ts.net"]);
        assert!(host_allowed("desk.tailnet.ts.net", &any_port));
        assert!(host_allowed("Desk.Tailnet.ts.net:8443", &any_port));
        assert!(!host_allowed("other.tailnet.ts.net", &any_port));
        assert!(!host_allowed("desk.tailnet.ts.net.evil.example", &any_port));

        let one_port = allowed(&["desk.example:8443"]);
        assert!(host_allowed("desk.example:8443", &one_port));
        assert!(!host_allowed("desk.example:9000", &one_port));
        assert!(!host_allowed("desk.example", &one_port));
    }

    #[test]
    fn origin_must_be_the_addressed_origin() {
        assert!(same_origin("http://127.0.0.1:4780", "127.0.0.1:4780"));
        assert!(same_origin("http://localhost:4780", "LocalHost:4780"));
        assert!(same_origin("http://[::1]:4780", "[::1]:4780"));
        // TLS ended by a proxy in front: the scheme differs, the origin's
        // name and port still match.
        assert!(same_origin("https://desk.example", "desk.example"));
        assert!(same_origin("https://desk.example", "desk.example:443"));
        assert!(same_origin("https://desk.example:443", "desk.example"));
        assert!(same_origin("http://desk.example", "desk.example:80"));

        for (origin, host) in [
            // Another site.
            ("https://evil.example", "127.0.0.1:4780"),
            // Another port on this machine (a dev server, an agent's preview).
            ("http://127.0.0.1:3000", "127.0.0.1:4780"),
            ("http://localhost:4780", "127.0.0.1:4780"),
            // Look-alikes.
            ("http://127.0.0.1:4780.evil.example", "127.0.0.1:4780"),
            ("http://127.0.0.1:4780@evil.example", "127.0.0.1:4780"),
            ("http://evil.example/127.0.0.1:4780", "127.0.0.1:4780"),
            ("https://desk.example:8443", "desk.example"),
            ("http://desk.example", "desk.example:443"),
            // Sandboxed frames, files, extensions.
            ("null", "127.0.0.1:4780"),
            ("file://", "127.0.0.1:4780"),
            ("chrome-extension://abcdef", "127.0.0.1:4780"),
            ("", "127.0.0.1:4780"),
        ] {
            assert!(!same_origin(origin, host), "{origin:?} vs {host:?}");
        }
    }

    #[test]
    fn cookies_are_read_by_exact_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; agentloft_session=abc; xagentloft_session=no"),
        );
        assert_eq!(cookie(&headers, COOKIE), Some("abc"));
        assert_eq!(cookie(&headers, SECURE_COOKIE), None);
        assert_eq!(cookie(&headers, "session"), None);
    }

    #[test]
    fn session_cookies_are_httponly_and_strict() {
        let plain = set_cookie("s3cret", 60, false).unwrap();
        let plain = plain.to_str().unwrap();
        assert!(plain.starts_with("agentloft_session=s3cret;"));
        for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=60"] {
            assert!(plain.contains(attr), "{attr}");
        }
        assert!(!plain.contains("Secure"));
        assert!(!plain.contains("Domain"));

        let secure = set_cookie("s3cret", 60, true).unwrap();
        let secure = secure.to_str().unwrap();
        assert!(secure.starts_with("__Host-agentloft_session=s3cret;"));
        assert!(secure.contains("; Secure"));
        assert!(secure.contains("HttpOnly") && secure.contains("SameSite=Strict"));
    }
}
