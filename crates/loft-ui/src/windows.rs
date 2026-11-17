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
    pub runtime: crate::host::Runtime,
    pub paths: Paths,
    /// Open workspace windows and their stores (for session restore).
    open: Vec<(AnyWindowHandle, gpui_kit::WeakEntity<AppStore>)>,
    /// A connection to this computer's daemon for Settings, when every
    /// window shows another computer.
    local: Option<gpui_kit::Entity<AppStore>>,
}
impl Global for AppEnv {}

pub fn init(runtime: crate::host::Runtime, paths: Paths, cx: &mut App) {
    #[cfg(not(target_arch = "wasm32"))]
    crate::host::init(cx);
    cx.set_global(AppEnv {
        runtime,
        paths,
        open: Vec::new(),
        local: None,
    });
    // Remember the windows and tab groups for next launch.
    cx.on_app_quit(|cx| {
        save_session(cx);
        async {}
    })
    .detach();
}

/// The store of the workspace in `window`.
pub(crate) fn store_of(window: &gpui_kit::Window, cx: &App) -> Option<gpui_kit::Entity<AppStore>> {
    let handle = window.window_handle();
    cx.try_global::<AppEnv>()?
        .open
        .iter()
        .find(|(open, _)| *open == handle)
        .and_then(|(_, store)| store.upgrade())
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

/// One window in the saved session: its focused thread and its panes.
/// (`group` is from when windows were native tabs: a group's threads now
/// open as panes of one window.)
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedWindow {
    thread: Option<ThreadId>,
    #[serde(default)]
    group: usize,
    /// Before columns: the panes, one per column.
    #[serde(default)]
    panes: Vec<ThreadId>,
    /// Columns of stacked panes.
    #[serde(default)]
    layout: Vec<Vec<ThreadId>>,
    /// The SSH host whose Agentloft the window shows (none: this
    /// computer's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
}

/// Remember each window's threads and panes for next time.
pub fn save_session(cx: &mut App) {
    let Some(env) = cx.try_global::<AppEnv>() else {
        return;
    };
    let paths = env.paths.clone();
    let open = env.open.clone();
    let mut saved = Vec::new();
    for (ix, (_, store)) in open.into_iter().enumerate() {
        let Some(store) = store.upgrade() else {
            continue;
        };
        let store = store.read(cx);
        saved.push(SavedWindow {
            thread: store.selected.clone(),
            group: ix,
            panes: Vec::new(),
            layout: store.layout.clone(),
            remote: store.remote().map(str::to_owned),
        });
    }
    if let Ok(json) = serde_json::to_vec_pretty(&saved) {
        crate::host::save_session(&paths, &json);
    }
}

/// Reopen last session's windows and tabs; a single window if none.
pub fn restore(cx: &mut App) {
    // A dev run on another computer only: `AGENTLOFT_DEV_REMOTE=<ssh host>`.
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(host) = std::env::var("AGENTLOFT_DEV_REMOTE")
        .ok()
        .filter(|h| loft_client::valid_destination(h))
    {
        open_on(None, Some(host), cx);
        return;
    }
    let saved: Vec<SavedWindow> = cx
        .try_global::<AppEnv>()
        .and_then(|env| crate::host::load_session(&env.paths))
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if saved.is_empty() {
        open(None, Placement::Window, cx);
        return;
    }
    // Windows that were tabs of one group come back as panes of one window.
    #[allow(clippy::type_complexity)]
    let mut windows: Vec<(usize, Option<ThreadId>, Vec<Vec<ThreadId>>, Option<String>)> =
        Vec::new();
    for w in saved {
        let mut layout = w.layout;
        if layout.is_empty() {
            layout = w.panes.into_iter().map(|t| vec![t]).collect();
        }
        if layout.is_empty() {
            layout.extend(w.thread.clone().map(|t| vec![t]));
        }
        match windows.iter_mut().find(|(g, ..)| *g == w.group) {
            Some((_, _, existing, _)) => {
                for col in layout {
                    let col: Vec<ThreadId> = col
                        .into_iter()
                        .filter(|id| !existing.iter().flatten().any(|e| e == id))
                        .collect();
                    if !col.is_empty() {
                        existing.push(col);
                    }
                }
            }
            None => windows.push((w.group, w.thread, layout, w.remote)),
        }
    }
    for (_, thread, layout, remote) in windows {
        if let Some(handle) = open_on(thread.clone(), remote, cx)
            && let Some(env) = cx.try_global::<AppEnv>()
            && let Some(store) = env
                .open
                .iter()
                .find(|(h, _)| *h == handle)
                .and_then(|(_, s)| s.upgrade())
        {
            store.update(cx, |s, cx| {
                for id in layout.iter().flatten() {
                    s.ensure_thread(id, cx);
                }
                s.layout = layout;
                if let Some(t) = &thread
                    && s.pane_at(t).is_none()
                {
                    s.layout.push(vec![t.clone()]);
                }
                cx.notify();
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement {
    /// A separate window.
    Window,
}

/// Open a workspace window, optionally showing `thread`.
pub fn open(
    thread: Option<ThreadId>,
    placement: Placement,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    let Placement::Window = placement;
    open_on(thread, None, cx)
}

/// A store connected to this computer's daemon: an open window's, else one
/// of its own.
pub fn local_store(cx: &mut App) -> Option<gpui_kit::Entity<AppStore>> {
    let env = cx.try_global::<AppEnv>()?;
    let open = env
        .open
        .iter()
        .filter_map(|(_, s)| s.upgrade())
        .find(|s| s.read(cx).remote().is_none());
    if let Some(store) = open.or_else(|| env.local.clone()) {
        return Some(store);
    }
    let (runtime, paths) = (env.runtime.clone(), env.paths.clone());
    let store = cx.new(|cx| AppStore::new(runtime, paths, None, cx));
    cx.global_mut::<AppEnv>().local = Some(store.clone());
    Some(store)
}

/// Whether a window is open on SSH host `host`'s Agentloft.
pub fn is_open_on(host: &str, cx: &App) -> bool {
    cx.try_global::<AppEnv>().is_some_and(|env| {
        env.open.iter().any(|(_, store)| {
            store
                .upgrade()
                .is_some_and(|s| s.read(cx).remote() == Some(host))
        })
    })
}

/// Open a window on the Agentloft of SSH host `remote`, or bring the one
/// already open on it to the front.
pub fn open_remote(remote: String, cx: &mut App) -> Option<AnyWindowHandle> {
    let existing = cx.try_global::<AppEnv>().and_then(|env| {
        env.open.iter().find_map(|(handle, store)| {
            let store = store.upgrade()?;
            (store.read(cx).remote() == Some(remote.as_str())).then_some(*handle)
        })
    });
    if let Some(handle) = existing {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        return Some(handle);
    }
    crate::remote::remember(&remote, cx);
    open_on(None, Some(remote), cx)
}

/// Open a workspace window on this computer's Agentloft or (`remote`) an
/// SSH host's, optionally showing `thread`.
fn open_on(
    thread: Option<ThreadId>,
    remote: Option<String>,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    let Some(env) = cx.try_global::<AppEnv>() else {
        tracing::error!("windows::init was not called");
        return None;
    };
    let (runtime, paths) = (env.runtime.clone(), env.paths.clone());
    let title: gpui_kit::SharedString = match &remote {
        Some(host) => format!("Agentloft — {host}").into(),
        None => "Agentloft".into(),
    };
    let store = cx.new(|cx| AppStore::new(runtime, paths, remote, cx));
    if let Some(thread) = thread {
        store.update(cx, |s, cx| s.select_thread(thread, cx));
    }
    let bounds = Bounds::centered(None, size(px(1240.), px(820.)), cx);
    let opened = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(720.), px(480.))),
            titlebar: Some(TitlebarOptions {
                title: Some(title),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.), px(18.))),
            }),
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
