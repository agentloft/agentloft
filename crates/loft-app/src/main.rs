//! `agentloft`: the GPUI client. Connects to (or starts) `agentloftd` and
//! renders its state.

use loft_config::Paths;
use loft_ui::{theme, windows};
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

    let app = gpui_kit::application().with_assets(loft_ui::assets::Assets);
    // Clicking the Dock icon with every window closed opens one.
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            windows::open(None, windows::Placement::Window, cx);
        }
    });
    app.run(move |cx| {
        gpui_kit::init(cx);
        theme::init(paths.clone(), cx);
        loft_ui::bind_keys(cx);
        cx.set_menus(loft_ui::app_menus());
        dock_icon::set();

        // Live-reload appearance settings from config.toml.
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                cx.update(theme::reload_if_changed);
            }
        })
        .detach();

        windows::init(handle.clone(), paths.clone(), cx);
        windows::open(None, windows::Placement::Window, cx);
        cx.activate(true);
    });
    drop(runtime);
    Ok(())
}

/// In a bundle the Dock icon comes from Info.plist (`CFBundleIconFile`);
/// dev runs (`cargo run`) set it at runtime so the Dock shows the real icon.
mod dock_icon {
    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    pub fn set() {
        use objc2::AllocAnyThread as _;
        use objc2_app_kit::{NSApplication, NSImage};
        use objc2_foundation::{MainThreadMarker, NSData};

        const PNG: &[u8] = include_bytes!("../../../assets/icon/png/agentloft-512.png");
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let data = NSData::with_bytes(PNG);
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: called on the main thread with a valid, retained NSImage.
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }

    #[cfg(not(target_os = "macos"))]
    pub fn set() {}
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
