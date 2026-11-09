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
    /// Open workspace windows and their stores (for session restore).
    open: Vec<(AnyWindowHandle, gpui_kit::WeakEntity<AppStore>)>,
}
impl Global for AppEnv {}

pub fn init(runtime: tokio::runtime::Handle, paths: Paths, cx: &mut App) {
    cx.set_global(AppEnv {
        runtime,
        paths,
        open: Vec::new(),
    });
    // Remember the windows and tab groups for next launch.
    cx.on_app_quit(|cx| {
        save_session(cx);
        async {}
    })
    .detach();
}

/// Re-render every workspace window (e.g. a tab's title changed).
pub fn refresh_all(cx: &mut App) {
    let handles: Vec<AnyWindowHandle> = cx
        .try_global::<AppEnv>()
        .map(|env| env.open.iter().map(|(h, _)| *h).collect())
        .unwrap_or_default();
    for handle in handles {
        let _ = handle.update(cx, |_, window, _| window.refresh());
    }
}

/// One window in the saved session: its thread and native tab group.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedWindow {
    thread: Option<ThreadId>,
    group: usize,
}

fn session_file(paths: &Paths) -> std::path::PathBuf {
    paths.home.join("ui-session.json")
}

fn save_session(cx: &mut App) {
    let Some(env) = cx.try_global::<AppEnv>() else {
        return;
    };
    let file = session_file(&env.paths);
    let open = env.open.clone();
    let mut groups: Vec<Vec<gpui_kit::WindowId>> = Vec::new();
    let mut saved = Vec::new();
    for (handle, store) in open {
        let Some(store) = store.upgrade() else {
            continue;
        };
        // Windows sharing a native tab group share a group number.
        let tabs = handle
            .update(cx, |_, window, _| window.tabbed_windows())
            .ok()
            .flatten()
            .map(|t| t.iter().map(|t| t.id).collect::<Vec<_>>())
            .unwrap_or_else(|| vec![handle.window_id()]);
        let group = match groups.iter().position(|g| g.contains(&handle.window_id())) {
            Some(ix) => ix,
            None => {
                groups.push(tabs);
                groups.len() - 1
            }
        };
        saved.push(SavedWindow {
            thread: store.read(cx).selected.clone(),
            group,
        });
    }
    if let Ok(json) = serde_json::to_vec_pretty(&saved) {
        let _ = std::fs::write(file, json);
    }
}

/// Reopen last session's windows and tabs; a single window if none.
pub fn restore(cx: &mut App) {
    let saved: Vec<SavedWindow> = cx
        .try_global::<AppEnv>()
        .and_then(|env| std::fs::read(session_file(&env.paths)).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if saved.is_empty() {
        open(None, Placement::Window, cx);
        return;
    }
    let mut leaders: std::collections::HashMap<usize, AnyWindowHandle> = Default::default();
    for w in saved {
        let placement = match leaders.get(&w.group) {
            Some(leader) => Placement::TabOf(*leader),
            None => Placement::Window,
        };
        if let Some(handle) = open(w.thread, placement, cx) {
            leaders.entry(w.group).or_insert(handle);
        }
    }
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
pub fn open(
    thread: Option<ThreadId>,
    placement: Placement,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    let Some(env) = cx.try_global::<AppEnv>() else {
        tracing::error!("windows::init was not called");
        return None;
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
        {
            let store = store.clone();
            move |window, cx| cx.new(|cx| WorkspaceView::new(store.clone(), window, cx))
        },
    );
    match opened {
        Ok((handle, _)) => {
            // The parent may be mid-update (we're often called from its
            // event handlers), so merge the tab once that's done.
            if let Placement::TabOf(parent) = placement {
                cx.defer(move |cx| add_as_tab(parent, handle, cx));
            }
            let weak = store.downgrade();
            let env = cx.global_mut::<AppEnv>();
            env.open.retain(|(_, s)| s.upgrade().is_some());
            env.open.push((handle, weak));
            Some(handle)
        }
        Err(err) => {
            tracing::error!("open window: {err:#}");
            None
        }
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
