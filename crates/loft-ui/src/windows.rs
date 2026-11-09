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

/// One window in the saved session: its focused thread and its panes.
/// (`group` is from when windows were native tabs: a group's threads now
/// open as panes of one window.)
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedWindow {
    thread: Option<ThreadId>,
    #[serde(default)]
    group: usize,
    #[serde(default)]
    panes: Vec<ThreadId>,
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
    let mut saved = Vec::new();
    for (ix, (_, store)) in open.into_iter().enumerate() {
        let Some(store) = store.upgrade() else {
            continue;
        };
        let store = store.read(cx);
        saved.push(SavedWindow {
            thread: store.selected.clone(),
            group: ix,
            panes: store.panes.clone(),
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
    // Windows that were tabs of one group come back as panes of one window.
    let mut windows: Vec<(usize, Option<ThreadId>, Vec<ThreadId>)> = Vec::new();
    for w in saved {
        let mut panes = w.panes;
        if panes.is_empty() {
            panes.extend(w.thread.clone());
        }
        match windows.iter_mut().find(|(g, ..)| *g == w.group) {
            Some((_, _, existing)) => {
                for id in panes {
                    if !existing.contains(&id) {
                        existing.push(id);
                    }
                }
            }
            None => windows.push((w.group, w.thread, panes)),
        }
    }
    for (_, thread, panes) in windows {
        if let Some(handle) = open(thread.clone(), Placement::Window, cx)
            && let Some(env) = cx.try_global::<AppEnv>()
            && let Some(store) = env
                .open
                .iter()
                .find(|(h, _)| *h == handle)
                .and_then(|(_, s)| s.upgrade())
        {
            store.update(cx, |s, cx| {
                for id in &panes {
                    s.ensure_thread(id, cx);
                }
                s.panes = panes;
                if let Some(t) = &thread
                    && !s.panes.contains(t)
                {
                    s.panes.push(t.clone());
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
            let Placement::Window = placement;
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
