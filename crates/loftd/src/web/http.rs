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
//!    On the public address (Cloudflare Tunnel, ADR-0019) there is no
//!    pairing, and only sessions made from a ticket from the website count:
//!    signing in with Google there is the only way in.
//!
//! Responses never carry CORS headers, so no other origin can read them,
//! and always carry a strict Content-Security-Policy. No page may frame
//! them, except that the linked website may frame the public address: it
//! shows a computer in place, so the browser's address stays the website's.

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

/// Whether `host` (the `Host` header) is one of the names in `list`, and
/// only those: unlike [`host_allowed`], IP literals and `localhost` don't
/// match by themselves. No port means 443 on both sides.
pub(crate) fn host_listed(host: &str, list: &[String]) -> bool {
    let Some((name, port)) = split_authority(host) else {
        return false;
    };
    list.iter().any(|entry| {
        split_authority(entry).is_some_and(|(listed, listed_port)| {
            listed == name && listed_port.unwrap_or(443) == port.unwrap_or(443)
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

/// Every value of the cookie `name`: a browser can hold two of the same
/// name (one partitioned, for a frame, and one not).
fn cookies<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(move |(k, _)| *k == name)
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

/// Whether the request is addressed to the public name (Cloudflare
/// Tunnel's). What decides is the address, not who is asking.
pub(crate) fn is_public(ctx: &Ctx, host: &str) -> bool {
    host_listed(host, &ctx.web.tunnel.public_hosts())
}

fn session(ctx: &Ctx, headers: &HeaderMap, host: &str) -> Option<(Authed, Proof)> {
    let found = if let Some(bearer) = single(headers, header::AUTHORIZATION)
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
    {
        ctx.web
            .auth
            .authenticate(bearer)
            .map(|a| (a, Proof::Bearer))
    } else {
        [SECURE_COOKIE, COOKIE]
            .iter()
            .flat_map(|name| cookies(headers, name))
            .find_map(|secret| ctx.web.auth.authenticate(secret))
            .map(|a| (a, Proof::Cookie))
    };
    // A session from pairing doesn't count on the public address.
    found.filter(|(authed, _)| authed.ticket || !is_public(ctx, host))
}

/// Who is asking to open the WebSocket.
pub(crate) struct WsCaller {
    /// It comes from our own page (or from a non-browser client, which
    /// proves itself with a bearer session rather than a cookie).
    pub origin_ok: bool,
}

pub(crate) fn ws_session(ctx: &Ctx, headers: &HeaderMap, host: &str) -> Option<(Authed, WsCaller)> {
    let (authed, proof) = session(ctx, headers, host)?;
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

/// How the session cookie is set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CookieKind {
    /// Plain HTTP (a local network address).
    Plain,
    /// HTTPS: `__Host-`, `Secure`.
    Secure,
    /// HTTPS, for the page inside the website's frame. A frame is another
    /// site's, so a `SameSite=Strict` cookie is never sent there; this one
    /// is `SameSite=None` but `Partitioned` (CHIPS): the browser keeps it
    /// for frames under the website alone, and sends it nowhere else.
    Framed,
}

fn set_cookie(secret: &str, max_age_secs: u64, kind: CookieKind) -> Option<HeaderValue> {
    let value = match kind {
        CookieKind::Secure => format!(
            "{SECURE_COOKIE}={secret}; Path=/; Max-Age={max_age_secs}; HttpOnly; Secure; SameSite=Strict"
        ),
        CookieKind::Framed => format!(
            "{SECURE_COOKIE}={secret}; Path=/; Max-Age={max_age_secs}; HttpOnly; Secure; SameSite=None; Partitioned"
        ),
        CookieKind::Plain => {
            format!("{COOKIE}={secret}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Strict")
        }
    };
    HeaderValue::from_str(&value).ok()
}

/// Headers every response carries. `framer` is the one origin allowed to
/// show the response in a frame (the linked website, on the public
/// address); no other page may.
fn harden(response: &mut Response<Body>, host: Option<&str>, tls: bool, framer: Option<&str>) {
    let headers = response.headers_mut();
    // `connect-src 'self'` covers the WebSocket in current browsers; older
    // Safari wants the ws: origin spelled out. `host` was validated above:
    // it holds nothing but host-name characters.
    let ws = host
        .map(|h| format!(" ws://{h} wss://{h}"))
        .unwrap_or_default();
    let ancestors = framer.unwrap_or("'none'");
    // Style *attributes* are allowed because GPUI sizes its canvas and its
    // hidden text input with them; scripts and stylesheets are ours only.
    let csp = format!(
        "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; \
         style-src-attr 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self'; \
         font-src 'self'; \
         connect-src 'self'{ws}; worker-src 'self' blob:; manifest-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-ancestors {ancestors}"
    );
    if let Ok(csp) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    if framer.is_none() {
        // For browsers that predate `frame-ancestors`.
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    for (name, value) in [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
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
    // 1. Host: an IP, localhost, a configured name, or the public name
    // while Cloudflare Tunnel fronts us.
    let host = single(request.headers(), header::HOST)
        .filter(|h| {
            host_allowed(h, &ctx.plan.allowed_hosts)
                || host_listed(h, &ctx.web.tunnel.public_hosts())
        })
        .map(str::to_owned);
    let mut response = match &host {
        Some(host) => route(request, &ctx, host).await,
        None => {
            tracing::warn!(peer = %ctx.peer, "web access: refused a request for an unknown host");
            text(StatusCode::FORBIDDEN, "This host name isn't allowed.\n")
        }
    };
    // The website shows the public address in a frame; nothing else may.
    let framer = host
        .as_deref()
        .filter(|h| is_public(&ctx, h))
        .and_then(|_| ctx.web.website.frame_ancestor());
    harden(&mut response, host.as_deref(), ctx.tls, framer.as_deref());
    response
}

async fn route(request: Request<Incoming>, ctx: &Ctx, host: &str) -> Response<Body> {
    let path = request.uri().path().to_owned();
    match (request.method().clone(), path.as_str()) {
        (Method::GET, "/api/session") => match session(ctx, request.headers(), host) {
            Some((authed, _)) => json(
                StatusCode::OK,
                serde_json::json!({
                    "authenticated": true,
                    "expires_ms": authed.expires_ms,
                }),
            ),
            // On the public address the page offers the website, not a
            // pairing box.
            None if is_public(ctx, host) => json(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "authenticated": false,
                    "pairing": false,
                    "website": ctx.web.core.config().web.website().ok(),
                }),
            ),
            None => json(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({ "authenticated": false, "pairing": true }),
            ),
        },
        (Method::POST, "/api/pair") => pair(request, ctx, host).await,
        (Method::POST, "/api/ticket") => ticket(request, ctx, host).await,
        (Method::POST, "/api/logout") => logout(request, ctx, host),
        (Method::GET, "/ws") => super::ws::upgrade(request, ctx, host),
        (Method::GET | Method::HEAD, "/api/file") => file(request, ctx, host).await,
        (Method::GET | Method::HEAD, _) => asset(request, ctx, host, &path).await,
        // No CORS preflight is ever answered, and nothing else is offered.
        _ => text(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.\n"),
    }
}

/// `POST /api/pair` `{"token": "…"}`: trade the pairing token for a session
/// cookie.
async fn pair(request: Request<Incoming>, ctx: &Ctx, host: &str) -> Response<Body> {
    let headers = request.headers();
    // Not on the public address: Google sign-in through the website is the
    // only way in there.
    if is_public(ctx, host) {
        return json(
            StatusCode::FORBIDDEN,
            serde_json::json!({ "ok": false, "error": "pairing_off_here" }),
        );
    }
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
    let addr = client_ip(ctx, headers, host);
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
        .pair(&token, addr, &user_agent, ctx.plan.session_ttl)
    {
        Ok((secret, authed)) => {
            let mut body = serde_json::json!({ "ok": true, "expires_ms": authed.expires_ms });
            let mut cookie = None;
            if wants_bearer {
                body["session"] = serde_json::Value::String(secret);
            } else {
                cookie = set_cookie(
                    &secret,
                    ctx.plan.session_ttl.as_secs(),
                    if secure {
                        CookieKind::Secure
                    } else {
                        CookieKind::Plain
                    },
                );
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
        Err(PairError::RateLimited(wait)) => too_many(wait),
        Err(PairError::Internal(err)) => {
            tracing::error!(%err, "web access: pairing failed");
            text(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.\n")
        }
    }
}

/// The address a request comes from: the peer's, or, through Cloudflare
/// Tunnel (a loopback peer asking for the public name), the browser's as
/// Cloudflare's edge saw it. Used for rate limits and the sessions list,
/// never to grant anything.
fn client_ip(ctx: &Ctx, headers: &HeaderMap, host: &str) -> std::net::IpAddr {
    let peer = ctx.peer.ip();
    // Through Cloudflare Tunnel: Cloudflare's edge says who connected to it
    // (`cloudflared` passes the header on and the client can't set it).
    if peer.is_loopback()
        && is_public(ctx, host)
        && let Some(client) = single(headers, header::HeaderName::from_static("cf-connecting-ip"))
            .and_then(|v| v.trim().parse::<std::net::IpAddr>().ok())
    {
        return client;
    }
    peer
}

fn too_many(wait: std::time::Duration) -> Response<Body> {
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

/// `POST /api/ticket` `{"ticket": "…"}`: a browser the website sent here
/// (ADR-0019) trades its ticket for a session cookie, as a pairing link
/// would. The ticket must be signed with this computer's key, for the
/// account it is linked to and for this very origin, current, and unused.
async fn ticket(request: Request<Incoming>, ctx: &Ctx, host: &str) -> Response<Body> {
    let headers = request.headers();
    // A browser always says where it is; the ticket is for that origin.
    let Some(origin) = single(headers, header::ORIGIN).map(str::to_owned) else {
        return text(StatusCode::FORBIDDEN, "Cross-site request refused.\n");
    };
    if !from_our_origin(headers, host, true) {
        return text(StatusCode::FORBIDDEN, "Cross-site request refused.\n");
    }
    let is_json = single(headers, header::CONTENT_TYPE)
        .is_some_and(|t| t.split(';').next().unwrap_or("").trim() == "application/json");
    if !is_json {
        return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send JSON.\n");
    }
    let addr = client_ip(ctx, headers, host);
    if let Some(wait) = ctx.web.auth.locked_out(addr) {
        return too_many(wait);
    }
    let secure = ctx.tls || origin.starts_with("https://");
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = match Limited::new(request.into_body(), MAX_PAIR_BODY)
        .collect()
        .await
    {
        Ok(body) => body.to_bytes(),
        Err(_) => return text(StatusCode::PAYLOAD_TOO_LARGE, "Request too large.\n"),
    };
    #[derive(serde::Deserialize)]
    struct Body {
        ticket: String,
        /// The page is inside the website's frame (only the public address
        /// may be framed, and only over HTTPS).
        #[serde(default)]
        framed: bool,
    }
    let Ok(Body { ticket, framed }) = serde_json::from_slice::<Body>(&body) else {
        return text(StatusCode::BAD_REQUEST, "Bad request.\n");
    };
    let kind = match (secure, framed && is_public(ctx, host)) {
        (true, true) => CookieKind::Framed,
        (true, false) => CookieKind::Secure,
        (false, _) => CookieKind::Plain,
    };
    let checked = match ctx.web.website.check_ticket(&ticket, &origin) {
        Ok(checked) => checked,
        Err(why) => {
            ctx.web.auth.failed(addr);
            tracing::warn!(%addr, ?why, "web access: refused a ticket");
            let error = match why {
                super::website::TicketError::NotLinked => "not_linked",
                super::website::TicketError::Invalid => "invalid",
            };
            return json(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };
    let mut label = super::auth::device_label(&user_agent);
    if !checked.email.is_empty() {
        label = format!("{label} · {}", checked.email);
    }
    match ctx
        .web
        .auth
        .open_session(addr, label, ctx.plan.session_ttl, true)
    {
        Ok((secret, authed)) => {
            tracing::info!(%addr, session = %authed.id, "web access: signed in from the website");
            let mut response = json(
                StatusCode::OK,
                serde_json::json!({ "ok": true, "expires_ms": authed.expires_ms }),
            );
            if let Some(cookie) = set_cookie(&secret, ctx.plan.session_ttl.as_secs(), kind) {
                response.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            response
        }
        Err(err) => {
            tracing::error!(?err, "web access: couldn't sign in from the website");
            text(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.\n")
        }
    }
}

/// `POST /api/logout`: end this browser's session.
fn logout(request: Request<Incoming>, ctx: &Ctx, host: &str) -> Response<Body> {
    let headers = request.headers();
    let Some((authed, proof)) = session(ctx, headers, host) else {
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
    // Clear every kind: a partitioned cookie is only cleared by a
    // partitioned `Set-Cookie`.
    for kind in [CookieKind::Secure, CookieKind::Framed, CookieKind::Plain] {
        if let Some(value) = set_cookie("", 0, kind) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

/// What a browser is told a file is (for `<video>`, which needs to know).
fn media_type(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

/// A `Range: bytes=…` header as the span it asks for in a file of `len`
/// bytes (one range only; what a `<video>` sends to seek). `None`: the
/// whole file. `Err`: a range past the end.
fn byte_range(header: Option<&str>, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return Ok(None);
    };
    if spec.contains(',') {
        // Several ranges: the whole file is a fine answer.
        return Ok(None);
    }
    let (start, end) = spec.split_once('-').ok_or(())?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", "") => return Err(()),
        // The last `n` bytes.
        ("", n) => {
            let n: u64 = n.parse().map_err(|_| ())?;
            (len.saturating_sub(n), len.checked_sub(1).ok_or(())?)
        }
        (s, "") => (s.parse().map_err(|_| ())?, len.checked_sub(1).ok_or(())?),
        (s, e) => {
            let (s, e): (u64, u64) = (s.parse().map_err(|_| ())?, e.parse().map_err(|_| ())?);
            (s, e.min(len.saturating_sub(1)))
        }
    };
    if start >= len || start > end {
        return Err(());
    }
    Ok(Some((start, end)))
}

/// `GET /api/file?path=…`: a file of the daemon's machine, streamed (and
/// in ranges, so a `<video>` can seek): a video in the web client. Only
/// what a browser session may read (`Core::file_for_remote`), only for our
/// own page.
async fn file(request: Request<Incoming>, ctx: &Ctx, host: &str) -> Response<Body> {
    let headers = request.headers();
    if session(ctx, headers, host).is_none() {
        return text(StatusCode::UNAUTHORIZED, "Sign in first.\n");
    }
    // Another site can't make a browser fetch it with our cookie (it is
    // SameSite=Strict), but say no outright when the browser says who asks.
    if !from_our_origin(headers, host, false) {
        return text(StatusCode::FORBIDDEN, "Cross-site request refused.\n");
    }
    let Some(path) = request.uri().query().and_then(|query| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == "path").then(|| percent_decode(value))?
        })
    }) else {
        return text(StatusCode::BAD_REQUEST, "Which file?\n");
    };
    let core = ctx.web.core.clone();
    let found = tokio::task::spawn_blocking(move || core.file_for_remote(&path))
        .await
        .ok()
        .flatten();
    let Some(path) = found else {
        return text(StatusCode::NOT_FOUND, "Not found.\n");
    };
    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return text(StatusCode::NOT_FOUND, "Not found.\n");
    };
    let Ok(len) = file.metadata().await.map(|m| m.len()) else {
        return text(StatusCode::NOT_FOUND, "Not found.\n");
    };
    let range = byte_range(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        len,
    );
    let mut response = Response::new(full(Bytes::new()));
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(media_type(&path)),
    );
    out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let (start, count) = match range {
        Err(()) => {
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            return response;
        }
        Ok(None) => (0, len),
        Ok(Some((start, end))) => {
            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            (start, end - start + 1)
        }
    };
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    if request.method() == Method::HEAD {
        return response;
    }
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
    if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return text(StatusCode::NOT_FOUND, "Not found.\n");
    }
    let stream = tokio_util::io::ReaderStream::with_capacity(file.take(count), 256 * 1024);
    *response.body_mut() = StreamBody::new(stream.map_ok(Frame::data)).boxed();
    response
}

/// `%XX` escapes (and `+` for a space) undone; `None` if what they spell
/// isn't UTF-8.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// A file of the web client.
async fn asset(request: Request<Incoming>, ctx: &Ctx, host: &str, path: &str) -> Response<Body> {
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
    if !asset.public && session(ctx, headers, host).is_none() {
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
    // The size once decoded, so the page can show how far a download has
    // got (it reads the decoded bytes; Content-Length counts compressed).
    out.insert(
        header::HeaderName::from_static("x-agentloft-size"),
        HeaderValue::from(asset.len),
    );
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|tags| tags.split(',').any(|t| t.trim() == asset.etag));
    if unchanged {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        return response;
    }

    let accepts = |coding: &str| {
        headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(',').any(|e| {
                    let mut parts = e.split(';');
                    parts.next().unwrap_or("").trim() == coding
                        && !parts
                            .any(|q| matches!(q.trim(), "q=0" | "q=0.0" | "q=0.00" | "q=0.000"))
                })
            })
    };
    let (file, len) = match (&asset.brotli, &asset.gzip) {
        (Some((br, len)), _) if accepts("br") => {
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
            (br.clone(), *len)
        }
        (_, Some((gz, len))) if accepts("gzip") => {
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
            "10.0.0.7",
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
    fn listed_names_match_only_themselves() {
        let list = ["agentloft-desk.example.com".to_owned()];
        assert!(host_listed("agentloft-desk.example.com", &list));
        assert!(host_listed("AGENTLOFT-desk.example.com:443", &list));
        assert!(!host_listed("agentloft-desk.example.com:8443", &list));
        // IPs and localhost are allowed hosts, but never "the public
        // name": that would trust CF-Connecting-IP, or refuse pairing, on
        // this computer's own address.
        for host in ["127.0.0.1:4780", "localhost", "[::1]:4780", "10.0.0.1"] {
            assert!(!host_listed(host, &list), "{host}");
            assert!(!host_listed(host, &[]), "{host}");
        }
        let on_8443 = ["agentloft-desk.example.com:8443".to_owned()];
        assert!(host_listed("agentloft-desk.example.com:8443", &on_8443));
        assert!(!host_listed("agentloft-desk.example.com", &on_8443));
    }

    #[test]
    fn listed_names_are_allowed_with_or_without_a_port() {
        let any_port = allowed(&["desk.vpn.example"]);
        assert!(host_allowed("desk.vpn.example", &any_port));
        assert!(host_allowed("Desk.VPN.example:8443", &any_port));
        assert!(!host_allowed("other.vpn.example", &any_port));
        assert!(!host_allowed("desk.vpn.example.evil.example", &any_port));

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
    fn ranges_are_read_the_way_a_video_asks() {
        assert_eq!(byte_range(None, 100), Ok(None));
        assert_eq!(byte_range(Some("bytes=0-"), 100), Ok(Some((0, 99))));
        assert_eq!(byte_range(Some("bytes=10-19"), 100), Ok(Some((10, 19))));
        assert_eq!(byte_range(Some("bytes=90-500"), 100), Ok(Some((90, 99))));
        assert_eq!(byte_range(Some("bytes=-10"), 100), Ok(Some((90, 99))));
        assert_eq!(byte_range(Some("bytes=0-1,5-6"), 100), Ok(None));
        assert_eq!(byte_range(Some("bytes=100-"), 100), Err(()));
        assert_eq!(byte_range(Some("bytes=5-2"), 100), Err(()));
        assert_eq!(byte_range(Some("bytes=x-"), 100), Err(()));
        assert_eq!(byte_range(Some("bytes=0-"), 0), Err(()));
        assert_eq!(
            percent_decode("%2FUsers%2Fme%2Fmy%20reel.mp4").as_deref(),
            Some("/Users/me/my reel.mp4")
        );
        assert_eq!(percent_decode("a+b").as_deref(), Some("a b"));
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%2"), Some("%2".into()));
    }

    #[test]
    fn cookies_are_read_by_exact_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; agentloft_session=abc; xagentloft_session=no"),
        );
        let first = |name| cookies(&headers, name).next();
        assert_eq!(first(COOKIE), Some("abc"));
        assert_eq!(first(SECURE_COOKIE), None);
        assert_eq!(first("session"), None);

        // A partitioned and an ordinary cookie of the same name: both read.
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("__Host-agentloft_session=old; __Host-agentloft_session=new"),
        );
        assert_eq!(
            cookies(&headers, SECURE_COOKIE).collect::<Vec<_>>(),
            ["old", "new"]
        );
    }

    #[test]
    fn session_cookies_are_httponly_and_strict() {
        let plain = set_cookie("s3cret", 60, CookieKind::Plain).unwrap();
        let plain = plain.to_str().unwrap();
        assert!(plain.starts_with("agentloft_session=s3cret;"));
        for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=60"] {
            assert!(plain.contains(attr), "{attr}");
        }
        assert!(!plain.contains("Secure"));
        assert!(!plain.contains("Domain"));

        let secure = set_cookie("s3cret", 60, CookieKind::Secure).unwrap();
        let secure = secure.to_str().unwrap();
        assert!(secure.starts_with("__Host-agentloft_session=s3cret;"));
        assert!(secure.contains("; Secure"));
        assert!(secure.contains("HttpOnly") && secure.contains("SameSite=Strict"));
        assert!(!secure.contains("Partitioned"));

        // In the website's frame: sent there only, never cross-site
        // elsewhere (CHIPS keys it to the top-level site).
        let framed = set_cookie("s3cret", 60, CookieKind::Framed).unwrap();
        let framed = framed.to_str().unwrap();
        assert!(framed.starts_with("__Host-agentloft_session=s3cret;"));
        for attr in [
            "HttpOnly",
            "; Secure",
            "SameSite=None",
            "Partitioned",
            "Path=/",
        ] {
            assert!(framed.contains(attr), "{attr}");
        }
    }
}
