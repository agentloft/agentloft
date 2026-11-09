//! Windows and window groups (PLAN M5). Every window is a full workspace
//! with its own daemon connection and selection, so a native macOS tab
//! group can hold threads from any project side by side — groups aren't
//! tied to projects.

use crate::store::AppStore;
use crate::workspace::WorkspaceView;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Global, TitlebarOptions, WindowBounds,
    WindowOptions, point, px, size,
};
use loft_config::Paths;
use loft_protocol::ThreadId;

/// Process-wide context every window needs to start its store.
pub struct AppEnv {
    pub runtime: tokio::runtime::Handle,
    pub paths: Paths,
}
impl Global for AppEnv {}

pub fn init(runtime: tokio::runtime::Handle, paths: Paths, cx: &mut App) {
    cx.set_global(AppEnv { runtime, paths });
}

/// Windows with this identifier can merge into one tabbed window.
const TAB_GROUP: &str = "agentloft-workspace";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement {
    /// A separate window.
    Window,
    /// A new tab in this window's group.
    TabOf(AnyWindowHandle),
}

/// Open a workspace window, optionally showing `thread`.
pub fn open(thread: Option<ThreadId>, placement: Placement, cx: &mut App) {
    let Some(env) = cx.try_global::<AppEnv>() else {
        tracing::error!("windows::init was not called");
        return;
    };
    let (runtime, paths) = (env.runtime.clone(), env.paths.clone());
    let store = cx.new(|cx| AppStore::new(runtime, paths, cx));
    if let Some(thread) = thread {
        store.update(cx, |s, cx| s.select_thread(thread, cx));
    }
    let bounds = Bounds::centered(None, size(px(1240.), px(820.)), cx);
    let opened = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(720.), px(480.))),
            titlebar: Some(TitlebarOptions {
                title: Some("Agentloft".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.), px(18.))),
            }),
            tabbing_identifier: Some(TAB_GROUP.into()),
            ..Default::default()
        },
        cx,
        move |window, cx| cx.new(|cx| WorkspaceView::new(store.clone(), window, cx)),
    );
    match opened {
        Ok((handle, _)) => {
            // The parent may be mid-update (we're often called from its
            // event handlers), so merge the tab once that's done.
            if let Placement::TabOf(parent) = placement {
                cx.defer(move |cx| add_as_tab(parent, handle, cx));
            }
        }
        Err(err) => tracing::error!("open window: {err:#}"),
    }
}

/// Put `child` into `parent`'s native tab group (⌘T semantics), whatever
/// the user's "prefer tabs" setting is.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn add_as_tab(parent: AnyWindowHandle, child: AnyWindowHandle, cx: &mut App) {
    use objc2_app_kit::{NSView, NSWindowOrderingMode};
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};

    fn ns_window(
        handle: AnyWindowHandle,
        cx: &mut App,
    ) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
        handle
            .update(cx, |_, window, _| {
                let raw = window.window_handle().ok()?.as_raw();
                let RawWindowHandle::AppKit(h) = raw else {
                    return None;
                };
                // SAFETY: GPUI's AppKit handle is its live content NSView.
                let view: &NSView = unsafe { h.ns_view.cast().as_ref() };
                view.window()
            })
            .ok()
            .flatten()
    }
    let (Some(p), Some(c)) = (ns_window(parent, cx), ns_window(child, cx)) else {
        return;
    };
    if p != c {
        p.addTabbedWindow_ordered(&c, NSWindowOrderingMode::Above);
        c.makeKeyAndOrderFront(None);
    }
}

#[cfg(not(target_os = "macos"))]
fn add_as_tab(_: AnyWindowHandle, _: AnyWindowHandle, _: &mut App) {}
