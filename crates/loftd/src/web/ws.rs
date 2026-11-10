//! The WebSocket a browser session talks over: the same `ClientMsg` /
//! `ServerMsg` JSON as the Unix socket, one message per WebSocket message.
//!
//! The upgrade needs a session and, for a cookie, our own origin (no other
//! page may open it in the user's name). The connection ends when the
//! session is revoked or expires, the listener stops, or the browser stops
//! answering pings.

use super::auth::{Authed, Revoked};
use super::http::{Body, Ctx};
use crate::conn::{self, Peer};
use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use loft_core::RemotePolicy;
use loft_protocol::{MAX_FRAME_LEN, ServerMsg};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Message, handshake::derive_accept_key};

const PING_EVERY: Duration = Duration::from_secs(25);
/// No message or pong for this long: the browser is gone.
const DEAD_AFTER: Duration = Duration::from_secs(75);
/// One write stuck for this long: the browser isn't reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Messages waiting to be written before a slow browser is dropped.
const MAX_BACKLOG: usize = 20_000;

/// Close code telling the page its session is over (pair again), as
/// opposed to a dropped connection (reconnect).
pub(crate) const CLOSE_SIGNED_OUT: u16 = 4401;

fn refuse(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut response = Response::new(
        http_body_util::Full::new(Bytes::from_static(message.as_bytes()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response
}

fn header_has(request: &Request<Incoming>, name: header::HeaderName, token: &str) -> bool {
    request
        .headers()
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// `GET /ws`: check the caller, then switch protocols.
pub(crate) fn upgrade(mut request: Request<Incoming>, ctx: &Ctx, host: &str) -> Response<Body> {
    let Some((authed, cookie)) = super::http::ws_session(ctx, request.headers(), host) else {
        return refuse(StatusCode::UNAUTHORIZED, "Sign in first.\n");
    };
    if !cookie.origin_ok {
        tracing::warn!(peer = %ctx.peer, "web access: refused a cross-site WebSocket");
        return refuse(StatusCode::FORBIDDEN, "Cross-site request refused.\n");
    }
    let key = request
        .headers()
        .get(header::SEC_WEBSOCKET_KEY)
        .map(|k| derive_accept_key(k.as_bytes()));
    let valid = header_has(&request, header::UPGRADE, "websocket")
        && header_has(&request, header::CONNECTION, "upgrade")
        && header_has(&request, header::SEC_WEBSOCKET_VERSION, "13");
    let (Some(accept), true) = (key, valid) else {
        return refuse(StatusCode::BAD_REQUEST, "Expected a WebSocket upgrade.\n");
    };

    let ctx = ctx.clone();
    let on_upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let mut config = WebSocketConfig::default();
                config.max_message_size = Some(MAX_FRAME_LEN);
                config.max_frame_size = Some(MAX_FRAME_LEN);
                let socket = WebSocketStream::from_raw_socket(
                    TokioIo::new(upgraded),
                    Role::Server,
                    Some(config),
                )
                .await;
                session(socket, ctx, authed).await;
            }
            Err(err) => tracing::debug!(%err, "web access: upgrade failed"),
        }
    });

    let mut response = Response::new(Empty::new().map_err(|never| match never {}).boxed());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Ok(accept) = HeaderValue::from_str(&accept) {
        headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    }
    response
}

fn close(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    }))
}

async fn session<S>(socket: WebSocketStream<S>, ctx: Ctx, authed: Authed)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let _connected = ctx.web.auth.connected(&authed.id);
    let _slot = ctx.web.ws_slots.clone().try_acquire_owned();
    let (mut sink, mut source) = socket.split();
    if _slot.is_err() {
        let _ = sink.send(close(1013, "too many connections")).await;
        return;
    }
    tracing::info!(peer = %ctx.peer, session = %authed.id, "web access: connected");

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();
    // Bounded: a browser that floods us waits on its own socket.
    let (frames_tx, frames_rx) = mpsc::channel::<Bytes>(64);
    let hub = ctx.web.hub();
    let peer = Peer::Remote {
        session: authed.id.clone(),
        policy: RemotePolicy {
            allow_terminal: ctx.plan.allow_terminal,
        },
    };
    let serving = tokio::spawn(conn::run(
        tokio_stream_from(frames_rx),
        out_tx,
        hub.clone(),
        peer,
    ));

    let mut revocations = ctx.web.auth.revocations();
    let mut stop = ctx.stop.clone();
    let mut shutdown = hub.shutdown.subscribe();
    let expires = {
        let left = authed.expires_ms.saturating_sub(super::now_ms()).max(0);
        Instant::now() + Duration::from_millis(u64::try_from(left).unwrap_or(0))
    };
    let mut ping = tokio::time::interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    let mut heard = Instant::now();

    let farewell = loop {
        tokio::select! {
            incoming = source.next() => match incoming {
                Some(Ok(message)) => {
                    heard = Instant::now();
                    let frame = match message {
                        Message::Text(text) => Bytes::from(text),
                        Message::Binary(bytes) => bytes,
                        Message::Close(_) => break None,
                        // Pings are answered by the library; pongs only
                        // prove the browser is there.
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                    };
                    if frames_tx.send(frame).await.is_err() {
                        break Some(close(1011, "closed"));
                    }
                }
                // An oversized or malformed frame, or the socket dropped.
                Some(Err(err)) => {
                    tracing::debug!(%err, session = %authed.id, "web access: socket error");
                    break Some(close(1009, "message too big or malformed"));
                }
                None => break None,
            },
            outgoing = out_rx.recv() => match outgoing {
                Some(message) => {
                    if out_rx.len() > MAX_BACKLOG {
                        tracing::warn!(session = %authed.id, "web access: browser too slow; dropping");
                        break Some(close(1013, "too slow"));
                    }
                    let Ok(json) = serde_json::to_string(&message) else { continue };
                    if json.len() > MAX_FRAME_LEN {
                        tracing::warn!(len = json.len(), "web access: message over the frame limit");
                        break Some(close(1009, "message too big"));
                    }
                    match tokio::time::timeout(WRITE_TIMEOUT, sink.send(Message::text(json))).await {
                        Ok(Ok(())) => {}
                        _ => break None,
                    }
                }
                // The connection handler finished (a fatal handshake error
                // was already sent as a message).
                None => break Some(close(1000, "bye")),
            },
            _ = ping.tick() => {
                if heard.elapsed() > DEAD_AFTER {
                    break None;
                }
                if tokio::time::timeout(WRITE_TIMEOUT, sink.send(Message::Ping(Bytes::new())))
                    .await
                    .is_err()
                {
                    break None;
                }
            }
            revoked = revocations.recv() => {
                let ended = match revoked {
                    Ok(Revoked::All) => true,
                    Ok(Revoked::One(id)) => id == authed.id,
                    // Missed some: ask directly.
                    Err(broadcast::error::RecvError::Lagged(_)) => !ctx.web.auth.is_live(&authed.id),
                    Err(broadcast::error::RecvError::Closed) => true,
                };
                if ended {
                    break Some(close(CLOSE_SIGNED_OUT, "signed out"));
                }
            }
            _ = tokio::time::sleep_until(expires) => {
                break Some(close(CLOSE_SIGNED_OUT, "session expired"));
            }
            _ = super::raised(&mut stop) => break Some(close(1001, "web access turned off")),
            _ = super::raised(&mut shutdown) => break Some(close(1001, "restarting")),
        }
    };
    if let Some(farewell) = farewell {
        let _ = tokio::time::timeout(Duration::from_secs(2), sink.send(farewell)).await;
    }
    // The handler ends once its input does (cleaning up after itself).
    drop(frames_tx);
    let abort = serving.abort_handle();
    if tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .is_err()
    {
        abort.abort();
    }
    tracing::info!(session = %authed.id, "web access: disconnected");
}

/// An mpsc receiver as a `Stream` (without pulling in `tokio-stream`).
fn tokio_stream_from(
    mut rx: mpsc::Receiver<Bytes>,
) -> impl futures::Stream<Item = Bytes> + Unpin + Send {
    Box::pin(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)))
}
