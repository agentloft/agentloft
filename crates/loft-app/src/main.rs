//! `agentloft`: the GPUI client. Connects to (or starts) `agentloftd` and
//! renders its state.

use gpui_kit::{
    AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use loft_config::Paths;
use loft_ui::{AppStore, WorkspaceView, theme};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    paths.ensure_dirs()?;
    init_tracing(&paths);

    // Client I/O runs on a small dedicated tokio runtime (ADR-0010).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("agentloft-io")
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();

    gpui_kit::application()
        .with_assets(loft_ui::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::init(paths.clone(), cx);
            loft_ui::bind_keys(cx);

            // Live-reload appearance settings from config.toml.
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    cx.update(|cx| theme::reload_if_changed(cx));
                }
            })
            .detach();

            let store = cx.new(|cx| AppStore::new(handle.clone(), paths.clone(), cx));
            let bounds = Bounds::centered(None, size(px(1240.), px(820.)), cx);
            gpui_kit::open_window(
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
                move |window, cx| cx.new(|cx| WorkspaceView::new(store.clone(), window, cx)),
            )
            .expect("failed to open window");
            cx.activate(true);
        });
    drop(runtime);
    Ok(())
}

fn init_tracing(paths: &Paths) {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter =
        EnvFilter::try_from_env("AGENTLOFT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir().join("agentloft.log"));
    match file {
        Ok(file) => fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        Err(_) => fmt().with_env_filter(filter).init(),
    }
}
