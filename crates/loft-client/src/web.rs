//! The browser transport (ADR-0016): a WebSocket to the daemon's web
//! listener, on the page's own origin. Each WebSocket message is one
//! `ClientMsg` / `ServerMsg` as JSON.
//!
//! The browser attaches the session cookie to the upgrade request itself;
//! this code never sees it (it is HttpOnly).

use crate::{Client, ClientEvent, Pending, disconnect, dispatch};
use anyhow::{Result, anyhow, bail};
use loft_protocol::{ClientKind, ClientMsg, Hello, PROTOCOL_VERSION, ServerMsg, Welcome};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

/// The daemon closes with this code when the session is over (revoked or
/// expired): pair again rather than reconnect.
pub const CLOSE_SIGNED_OUT: u16 = 4401;

/// `ws(s)://<this page's host>/ws`.
fn socket_url() -> Result<String> {
    let location = web_sys::window()
        .ok_or_else(|| anyhow!("not running in a page"))?
        .location();
    let secure = location.protocol().map_err(js_error)? == "https:";
    let host = location.host().map_err(js_error)?;
    Ok(format!("{}://{host}/ws", if secure { "wss" } else { "ws" }))
}

fn js_error(value: wasm_bindgen::JsValue) -> anyhow::Error {
    anyhow!(
        "{}",
        value
            .as_string()
            .or_else(|| js_sys::JSON::stringify(&value)
                .ok()
                .and_then(|s| s.as_string()))
            .unwrap_or_else(|| "browser error".into())
    )
}

/// The page's event handlers, kept alive for as long as the socket is.
#[derive(Default)]
struct Handlers {
    _open: Option<Closure<dyn FnMut()>>,
    _message: Option<Closure<dyn FnMut(MessageEvent)>>,
    _close: Option<Closure<dyn FnMut(CloseEvent)>>,
}

/// Connect to the daemon this page was served by and perform the hello
/// handshake.
pub async fn connect(
    kind: ClientKind,
    build: &str,
) -> Result<(Client, mpsc::UnboundedReceiver<ClientEvent>)> {
    let socket = WebSocket::new(&socket_url()?).map_err(js_error)?;
    socket.set_binary_type(web_sys::BinaryType::Arraybuffer);

    let pending: Pending = Arc::default();
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (welcome_tx, welcome_rx) = oneshot::channel::<Result<Welcome, String>>();
    let welcome_tx = Rc::new(RefCell::new(Some(welcome_tx)));
    let handlers = Rc::new(RefCell::new(Handlers::default()));

    let hello = serde_json::to_string(&ClientMsg::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_kind: kind,
        client_build: build.to_owned(),
        capabilities: Vec::new(),
    }))?;
    let on_open = {
        let socket = socket.clone();
        Closure::<dyn FnMut()>::new(move || {
            let _ = socket.send_with_str(&hello);
        })
    };
    socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));

    let on_message = {
        let (pending, events_tx, welcome_tx) =
            (pending.clone(), events_tx.clone(), welcome_tx.clone());
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let data = event.data();
            let parsed = if let Some(text) = data.as_string() {
                serde_json::from_str::<ServerMsg>(&text)
            } else if let Ok(buffer) = data.dyn_into::<js_sys::ArrayBuffer>() {
                serde_json::from_slice::<ServerMsg>(&js_sys::Uint8Array::new(&buffer).to_vec())
            } else {
                return;
            };
            let msg = match parsed {
                Ok(msg) => msg,
                Err(err) => {
                    tracing::warn!(%err, "bad daemon message");
                    return;
                }
            };
            // The first message settles the handshake.
            if let Some(waiting) = welcome_tx.borrow_mut().take() {
                let _ = waiting.send(match msg {
                    ServerMsg::Welcome(welcome) => Ok(welcome),
                    ServerMsg::Fatal(e) => Err(format!("daemon refused connection: {e}")),
                    other => Err(format!("unexpected first message: {other:?}")),
                });
                return;
            }
            dispatch(msg, &pending, &events_tx);
        })
    };
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

    let on_close = {
        let (pending, events_tx, welcome_tx, handlers) = (
            pending.clone(),
            events_tx.clone(),
            welcome_tx.clone(),
            handlers.clone(),
        );
        Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
            if let Some(waiting) = welcome_tx.borrow_mut().take() {
                let _ = waiting.send(Err(format!(
                    "the connection closed during the handshake ({})",
                    event.code()
                )));
            }
            disconnect(&pending, &events_tx);
            if event.code() == CLOSE_SIGNED_OUT {
                // The session is over: back to the pairing screen.
                if let Some(window) = web_sys::window() {
                    let _ = window.location().reload();
                }
            }
            // Free the handlers, but not from inside one of them.
            let handlers = handlers.clone();
            wasm_bindgen_futures::spawn_local(async move {
                *handlers.borrow_mut() = Handlers::default();
            });
        })
    };
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    *handlers.borrow_mut() = Handlers {
        _open: Some(on_open),
        _message: Some(on_message),
        _close: Some(on_close),
    };

    let welcome = match welcome_rx.await {
        Ok(Ok(welcome)) => welcome,
        Ok(Err(reason)) => {
            let _ = socket.close();
            bail!(reason)
        }
        Err(_) => bail!("the connection closed during the handshake"),
    };

    // Outgoing messages; ends (and closes the socket) once every `Client`
    // clone is gone.
    let (tx, mut out_rx) = mpsc::unbounded_channel::<ClientMsg>();
    wasm_bindgen_futures::spawn_local(async move {
        while let Some(msg) = out_rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };
            if socket.send_with_str(&json).is_err() {
                break;
            }
        }
        let _ = socket.close();
    });

    Ok((Client::new(tx, pending, welcome), events_rx))
}
