//! Start-up in the browser: fonts, the settings mirror, then the window.

use gpui_kit::{App, AsyncApp};
use loft_ui::{host, theme, windows};
use std::borrow::Cow;
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen::prelude::*;

const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), "+web");

/// The browser has no system fonts, so the client carries its own (all
/// OFL-1.1, see `fonts/`): Inter for the interface, JetBrains Mono for
/// code, and IBM Plex Sans because GPUI's web platform falls back to that
/// family by name when a font is missing.
fn fonts() -> Vec<Cow<'static, [u8]>> {
    macro_rules! font {
        ($file:literal) => {
            Cow::Borrowed(include_bytes!(concat!("../fonts/", $file)).as_slice())
        };
    }
    vec![
        font!("IBMPlexSans-Regular.ttf"),
        font!("Inter-Regular.ttf"),
        font!("Inter-Medium.ttf"),
        font!("Inter-SemiBold.ttf"),
        font!("Inter-Bold.ttf"),
        font!("Inter-Italic.ttf"),
        font!("JetBrainsMono-Regular.ttf"),
        font!("JetBrainsMono-Bold.ttf"),
        font!("JetBrainsMono-Italic.ttf"),
    ]
}

/// Tell the page how start-up is going (it shows this until the app draws).
fn status(message: &str) {
    if let Some(element) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("boot"))
    {
        element.set_text_content(Some(message));
    }
}

/// The app has drawn: the page can drop its loading note.
fn ready() {
    if let Some(window) = web_sys::window()
        && let Ok(event) = web_sys::Event::new("agentloft-ready")
    {
        let _ = window.dispatch_event(&event);
    }
}

/// Fetch the settings files the UI reads at start-up (config, themes,
/// sidebar state) through a short-lived connection.
async fn settings() -> anyhow::Result<loft_config::Paths> {
    let (client, _events) =
        loft_client::web::connect(loft_protocol::ClientKind::RemoteUi, BUILD).await?;
    host::fetch_ui_files(&client).await?;
    Ok(host::paths())
}

fn open(paths: loft_config::Paths, cx: &mut App) {
    theme::init(paths.clone(), cx);
    loft_ui::bind_keys(cx);
    windows::init(host::Runtime, paths, cx);
    windows::restore(cx);
    // There is no "quit" in a browser: keep the layout saved as it changes.
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            cx.update(windows::save_session);
        }
    })
    .detach();
    ready();
}

#[wasm_bindgen(start)]
pub fn start() {
    gpui_kit::platform::web_init();
    // One thread (no SharedArrayBuffer, so no cross-origin isolation to set
    // up); CJK and emoji fall back to the browser's own fonts.
    let platform = Rc::new(
        gpui_kit::web::WebPlatform::new_with_backend_and_font_fallback(
            false,
            gpui_kit::web::WebBackendPreference::Auto,
            gpui_kit::web::CanvasFontFallback::EmojiAndCjk,
        ),
    );
    gpui_kit::Application::with_platform(platform)
        .with_assets(loft_ui::assets::Assets)
        .run(|cx| {
            if let Err(err) = cx.text_system().add_fonts(fonts()) {
                status(&format!("Agentloft could not load its fonts: {err:#}"));
                return;
            }
            gpui_kit::init(cx);
            host::init(cx);
            cx.spawn(async move |cx: &mut AsyncApp| {
                let paths = loop {
                    match settings().await {
                        Ok(paths) => break paths,
                        Err(err) => {
                            log::warn!("start-up: {err:#}");
                            status("Connecting to Agentloft…");
                            cx.background_executor().timer(Duration::from_secs(2)).await;
                        }
                    }
                };
                cx.update(|cx| open(paths, cx));
            })
            .detach();
        });
}
