//! The machine the daemon runs on, as the UI reaches it.
//!
//! The desktop app runs on that machine: it reads `config.toml`, themes and
//! its own state files directly, and draws images straight from their
//! paths. In the browser (ADR-0016) there is no such filesystem, so the same
//! calls go through the daemon instead:
//!
//! - the **settings files** are mirrored in memory from `Request::UiFiles`
//!   and re-asked for every few seconds; a write updates the mirror at once
//!   and is sent on (`ChangeSetting`, `SaveTheme`, `DeleteTheme`,
//!   `PutUiState`);
//! - **images** are fetched with `Request::ReadPath` (which only reaches
//!   into projects, thread folders and Agentloft's own attachments and
//!   icons) and kept in a small cache;
//! - the **window layout** is this browser's own, so it lives in the
//!   browser's `localStorage` rather than on the daemon's disk.
//!
//! Views call these functions and never ask which of the two they are on,
//! except to hide what the browser can't do ([`IS_WEB`]).

use loft_config::{Config, Paths, SettingValue};
use loft_protocol::ThreadId;
use std::path::{Path, PathBuf};

/// Running in a browser, against a daemon somewhere else.
pub const IS_WEB: bool = cfg!(target_arch = "wasm32");

/// "Not available in the browser", for panels the web client lacks.
pub const NOT_IN_BROWSER: &str = "Not available in the browser";

pub use imp::*;

/// `config.toml`, parsed.
pub fn load_config(paths: &Paths) -> anyhow::Result<Config> {
    match read_to_string(&paths.config_file()) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

/// An image's format from its first bytes, else from its file name.
pub fn image_format(path: &Path, bytes: &[u8]) -> Option<gpui_kit::ImageFormat> {
    use gpui_kit::ImageFormat as F;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    Some(match bytes {
        [0x89, b'P', b'N', b'G', ..] => F::Png,
        [0xff, 0xd8, 0xff, ..] => F::Jpeg,
        [b'G', b'I', b'F', b'8', ..] => F::Gif,
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => F::Webp,
        [b'B', b'M', ..] => F::Bmp,
        [0, 0, 1, 0, ..] => F::Ico,
        _ => match ext.as_deref() {
            Some("svg") => F::Svg,
            Some("png") => F::Png,
            Some("jpg" | "jpeg") => F::Jpeg,
            Some("gif") => F::Gif,
            Some("webp") => F::Webp,
            Some("bmp") => F::Bmp,
            Some("ico") => F::Ico,
            Some("tif" | "tiff") => F::Tiff,
            _ if bytes
                .iter()
                .take(256)
                .skip_while(|b| b.is_ascii_whitespace())
                .take(5)
                .eq(b"<svg ".iter())
                || bytes.starts_with(b"<?xml") =>
            {
                F::Svg
            }
            _ => return None,
        },
    })
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;
    use loft_client::{Client, ClientEvent};
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Where connection I/O runs: a small tokio runtime (ADR-0010).
    pub type Runtime = tokio::runtime::Handle;

    /// The user's home folder.
    pub fn user_home() -> PathBuf {
        std::env::home_dir().unwrap_or_else(|| "/".into())
    }

    /// Whether shortcuts use ⌘ and Apple's key symbols.
    pub fn is_mac() -> bool {
        cfg!(target_os = "macos")
    }

    pub fn read(path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    pub fn read_to_string(path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    /// Write a settings or state file, creating its folder.
    pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, bytes)
    }

    pub fn remove_file(path: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }

    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    /// The files in `dir` (not its folders).
    pub fn list_files(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect()
    }

    /// Changes when the file does.
    pub fn modified(path: &Path) -> Option<std::time::SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    /// Set one key in `config.toml`.
    pub fn write_setting(paths: &Paths, key: &[&str], value: SettingValue) -> anyhow::Result<()> {
        loft_config::write_setting(paths, key, value)
    }

    /// Connect to the daemon, starting it if it isn't running.
    pub async fn connect(
        runtime: Runtime,
        paths: Paths,
        build: &'static str,
    ) -> anyhow::Result<(Client, UnboundedReceiver<ClientEvent>)> {
        runtime
            .spawn(async move {
                loft_client::connect_or_spawn(
                    &paths,
                    &loft_client::daemon_binary(),
                    loft_protocol::ClientKind::LocalUi,
                    build,
                    &["browser"],
                )
                .await
            })
            .await
            .map_err(|e| anyhow::anyhow!(e))?
    }

    /// The store's connection came up or went away (the browser build
    /// routes its own requests through it).
    pub fn set_client(_client: Option<Client>) {}

    /// Something to draw the image at `path` with. Always ready here: GPUI
    /// loads local files itself. (`thread`: whose folder a relative path is
    /// in, for the browser build.)
    pub fn image(path: &Path, _thread: Option<&ThreadId>) -> Option<gpui_kit::ImageSource> {
        Some(path.to_path_buf().into())
    }

    /// Something to draw a project's icon with, if it has one.
    pub fn project_icon(project: &loft_protocol::Project) -> Option<gpui_kit::ImageSource> {
        crate::project_icon::image(project).map(Into::into)
    }

    /// Icons may have changed: look again.
    pub fn forget_project_icons() {
        crate::project_icon::forget();
    }

    /// The window layout to restore (`ui-session.json`).
    pub fn load_session(paths: &Paths) -> Option<Vec<u8>> {
        std::fs::read(paths.home.join("ui-session.json")).ok()
    }

    pub fn save_session(paths: &Paths, json: &[u8]) {
        let _ = std::fs::write(paths.home.join("ui-session.json"), json);
    }

    /// Signing out is the browser build's (there is no session here).
    pub fn sign_out() {}
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use loft_client::{Client, ClientEvent};
    use loft_protocol::{Request, Response, UiFiles};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Nothing to run I/O on: the browser's event loop does it. (`Clone`
    /// but not `Copy`, like the handle it stands in for.)
    #[derive(Debug, Clone, Default)]
    pub struct Runtime;

    /// Images kept at once, in bytes, before the cache starts over.
    const MAX_IMAGE_BYTES: usize = 96 * 1024 * 1024;
    /// How often the daemon is asked whether the settings files changed.
    const POLL: Duration = Duration::from_secs(3);
    const SESSION_KEY: &str = "agentloft.session";

    enum Picture {
        Loading,
        Ready(Arc<gpui_kit::Image>, usize),
        Missing,
    }

    #[derive(Default)]
    struct Host {
        /// Agentloft's home and the user's home on the daemon's machine.
        home: PathBuf,
        user_home: PathBuf,
        /// The settings files, by path there. The counter stands in for a
        /// modification time: it goes up whenever the text changes.
        files: HashMap<PathBuf, (Vec<u8>, u64)>,
        revision: u64,
        /// The stamp of the last `UiFiles` answer.
        stamp: Option<String>,
        client: Option<Client>,
        app: Option<gpui_kit::AsyncApp>,
        images: HashMap<String, Picture>,
        image_bytes: usize,
    }

    thread_local! {
        static HOST: RefCell<Host> = RefCell::new(Host::default());
    }

    /// The browser is on an Apple device (set at start-up).
    static APPLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Whether shortcuts use ⌘ and Apple's key symbols: decided by the
    /// device the browser runs on, not the one the daemon does.
    pub fn is_mac() -> bool {
        APPLE.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn with<T>(f: impl FnOnce(&mut Host) -> T) -> T {
        HOST.with(|h| f(&mut h.borrow_mut()))
    }

    fn unsupported() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::Unsupported, NOT_IN_BROWSER)
    }

    fn not_found() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no such file")
    }

    /// Remember the app (to redraw when an image arrives). Call once at
    /// startup.
    pub fn init(cx: &mut gpui_kit::App) {
        let apple = matches!(
            cx.platform_info().os,
            gpui_kit::OperatingSystem::Mac | gpui_kit::OperatingSystem::Ios
        );
        APPLE.store(apple, std::sync::atomic::Ordering::Relaxed);
        let app = cx.to_async();
        with(|h| h.app = Some(app));
    }

    /// Take the daemon's copy of the settings files as the truth.
    pub fn install(files: UiFiles) -> Paths {
        with(|h| {
            h.home = PathBuf::from(&files.home);
            h.user_home = PathBuf::from(&files.user_home);
            let paths = Paths::new(&h.home);
            let mut fresh: HashMap<PathBuf, Vec<u8>> = HashMap::new();
            if !files.config_toml.is_empty() {
                fresh.insert(paths.config_file(), files.config_toml.into_bytes());
            }
            for theme in files.themes {
                fresh.insert(
                    paths.themes_dir().join(format!("{}.toml", theme.name)),
                    theme.text.into_bytes(),
                );
            }
            if let Some(sidebar) = files.sidebar {
                fresh.insert(h.home.join("ui-sidebar.json"), sidebar.into_bytes());
            }
            // Keep the counter of what didn't change, so only real changes
            // look modified.
            let mut next = HashMap::new();
            for (path, bytes) in fresh {
                let revision = match h.files.get(&path) {
                    Some((old, revision)) if *old == bytes => *revision,
                    _ => {
                        h.revision += 1;
                        h.revision
                    }
                };
                next.insert(path, (bytes, revision));
            }
            if next.len() != h.files.len() {
                h.revision += 1;
            }
            h.files = next;
            h.stamp = Some(files.stamp);
            paths
        })
    }

    /// The daemon's paths, once [`install`] has run.
    pub fn paths() -> Paths {
        with(|h| Paths::new(&h.home))
    }

    pub fn user_home() -> PathBuf {
        with(|h| h.user_home.clone())
    }

    pub fn read(path: &Path) -> std::io::Result<Vec<u8>> {
        with(|h| h.files.get(path).map(|(bytes, _)| bytes.clone())).ok_or_else(not_found)
    }

    pub fn read_to_string(path: &Path) -> std::io::Result<String> {
        String::from_utf8(read(path)?)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub fn exists(path: &Path) -> bool {
        with(|h| h.files.contains_key(path))
    }

    pub fn list_files(dir: &Path) -> Vec<PathBuf> {
        with(|h| {
            h.files
                .keys()
                .filter(|p| p.parent() == Some(dir))
                .cloned()
                .collect()
        })
    }

    pub fn modified(path: &Path) -> Option<SystemTime> {
        with(|h| h.files.get(path).map(|(_, revision)| *revision))
            .map(|revision| SystemTime::UNIX_EPOCH + Duration::from_secs(revision))
    }

    fn put(path: &Path, bytes: Vec<u8>) {
        with(|h| {
            h.revision += 1;
            let revision = h.revision;
            h.files.insert(path.to_path_buf(), (bytes, revision));
        });
    }

    /// Send a request for its effect; a refusal is logged (the next poll
    /// puts the mirror right).
    fn send(request: Request) {
        let Some(client) = with(|h| h.client.clone()) else {
            tracing::warn!("not connected; a settings change wasn't saved");
            return;
        };
        let pending = client.request(request);
        let Some(app) = with(|h| h.app.clone()) else {
            return;
        };
        app.spawn(async move |_| {
            if let Err(err) = pending.await {
                tracing::warn!(%err, "the daemon refused a settings change");
                // Ask again now rather than at the next poll.
                with(|h| h.stamp = None);
            }
        })
        .detach();
    }

    /// The custom theme a path under `<home>/themes` names.
    fn theme_id(path: &Path) -> Option<String> {
        let themes = with(|h| Paths::new(&h.home).themes_dir());
        (path.parent() == Some(themes.as_path()) && path.extension().is_some_and(|x| x == "toml"))
            .then(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .flatten()
            .map(|stem| format!("custom:{stem}"))
    }

    pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            .to_owned();
        let sidebar = with(|h| h.home.join("ui-sidebar.json"));
        if let Some(id) = theme_id(path) {
            send(Request::SaveTheme {
                toml: text,
                select: false,
                id: Some(id),
            });
        } else if path == sidebar {
            send(Request::PutUiState {
                name: "sidebar".into(),
                json: text,
            });
        } else {
            return Err(unsupported());
        }
        put(path, bytes.to_vec());
        Ok(())
    }

    pub fn remove_file(path: &Path) -> std::io::Result<()> {
        let id = theme_id(path).ok_or_else(unsupported)?;
        send(Request::DeleteTheme { id });
        with(|h| {
            h.revision += 1;
            h.files.remove(path);
        });
        Ok(())
    }

    pub fn write_setting(paths: &Paths, key: &[&str], value: SettingValue) -> anyhow::Result<()> {
        let file = paths.config_file();
        let text = read_to_string(&file).unwrap_or_default();
        let json = match &value {
            SettingValue::Str(s) => serde_json::json!(s),
            SettingValue::Float(f) => serde_json::json!(f),
            SettingValue::Int(n) => serde_json::json!(n),
            SettingValue::Bool(b) => serde_json::json!(b),
            SettingValue::List(items) => serde_json::json!(items),
            SettingValue::Map(pairs) => serde_json::Value::Object(
                pairs
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                    .collect(),
            ),
            SettingValue::Unset => serde_json::Value::Null,
        };
        let edited = loft_config::edit_setting(&text, key, value)?;
        put(&file, edited.into_bytes());
        send(Request::ChangeSetting {
            key: key.join("."),
            value: json,
        });
        Ok(())
    }

    pub async fn connect(
        _runtime: Runtime,
        _paths: Paths,
        build: &'static str,
    ) -> anyhow::Result<(Client, UnboundedReceiver<ClientEvent>)> {
        loft_client::web::connect(loft_protocol::ClientKind::RemoteUi, build).await
    }

    /// Ask the daemon for the settings files (all of them the first time,
    /// then only when they changed).
    pub async fn fetch_ui_files(client: &Client) -> anyhow::Result<bool> {
        let known = with(|h| h.stamp.clone());
        match client.request(Request::UiFiles { known }).await {
            Ok(Response::UiFiles { files: Some(files) }) => {
                install(*files);
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(err) => Err(anyhow::anyhow!(err)),
        }
    }

    pub fn set_client(client: Option<Client>) {
        let connected = client.is_some();
        let already = with(|h| std::mem::replace(&mut h.client, client).is_some());
        if !connected || already {
            return;
        }
        // Keep the mirror fresh for as long as this connection lasts.
        let Some(app) = with(|h| h.app.clone()) else {
            return;
        };
        app.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let Some(client) = with(|h| h.client.clone()) else {
                    break;
                };
                let _ = fetch_ui_files(&client).await;
            }
        })
        .detach();
    }

    /// The picture cached under `key`; if there is none yet, `request` is
    /// sent for it and the windows redraw when it arrives. `name` helps
    /// tell its format.
    fn picture(
        key: String,
        name: PathBuf,
        request: impl FnOnce() -> Request,
    ) -> Option<gpui_kit::ImageSource> {
        let cached = with(|h| match h.images.get(&key) {
            Some(Picture::Ready(image, _)) => Some(Some(image.clone())),
            Some(Picture::Loading | Picture::Missing) => Some(None),
            None => None,
        });
        if let Some(found) = cached {
            return found.map(Into::into);
        }
        let (Some(client), Some(app)) = with(|h| (h.client.clone(), h.app.clone())) else {
            return None;
        };
        with(|h| h.images.insert(key.clone(), Picture::Loading));
        let pending = client.request(request());
        app.spawn(async move |cx| {
            let picture = match pending.await {
                Ok(Response::File { file }) if !file.truncated => {
                    // The daemon says where it found it (a project's icon).
                    let name = if name.as_os_str().is_empty() {
                        PathBuf::from(&file.abs_path)
                    } else {
                        name
                    };
                    match image_format(&name, &file.bytes.0) {
                        Some(format) => {
                            let len = file.bytes.0.len();
                            Picture::Ready(
                                Arc::new(gpui_kit::Image::from_bytes(format, file.bytes.0)),
                                len,
                            )
                        }
                        None => Picture::Missing,
                    }
                }
                _ => Picture::Missing,
            };
            with(|h| {
                if let Picture::Ready(_, len) = &picture {
                    if h.image_bytes + len > MAX_IMAGE_BYTES {
                        h.images.retain(|_, p| !matches!(p, Picture::Ready(..)));
                        h.image_bytes = 0;
                    }
                    h.image_bytes += len;
                }
                h.images.insert(key, picture);
            });
            cx.update(|cx| cx.refresh_windows());
        })
        .detach();
        None
    }

    pub fn image(path: &Path, thread: Option<&ThreadId>) -> Option<gpui_kit::ImageSource> {
        let key = match (path.is_absolute(), thread) {
            (true, _) | (false, None) => path.to_string_lossy().into_owned(),
            (false, Some(thread)) => format!("{}\u{0}{}", thread.as_str(), path.display()),
        };
        picture(key, path.to_path_buf(), || Request::ReadPath {
            path: path.to_string_lossy().into_owned(),
            thread_id: thread.cloned(),
        })
    }

    const PROJECT_ICON: &str = "project-icon\u{0}";

    /// A project's icon, found and read by the daemon.
    pub fn project_icon(project: &loft_protocol::Project) -> Option<gpui_kit::ImageSource> {
        let key = format!(
            "{PROJECT_ICON}{}\u{0}{}",
            project.id.as_str(),
            project.icon.as_deref().unwrap_or("")
        );
        picture(key, PathBuf::new(), || Request::ProjectIcon {
            project_id: project.id.clone(),
        })
    }

    pub fn forget_project_icons() {
        with(|h| {
            let mut freed = 0;
            h.images.retain(|key, picture| {
                if !key.starts_with(PROJECT_ICON) {
                    return true;
                }
                if let Picture::Ready(_, len) = picture {
                    freed += *len;
                }
                false
            });
            h.image_bytes = h.image_bytes.saturating_sub(freed);
        });
    }

    /// Forget what was fetched for `path` (it changed).
    pub fn forget_image(path: &Path) {
        let key = path.to_string_lossy().into_owned();
        with(|h| {
            if let Some(Picture::Ready(_, len)) = h.images.remove(&key) {
                h.image_bytes = h.image_bytes.saturating_sub(len);
            }
        });
    }

    /// End this browser's session: the page signs out and goes back to
    /// pairing (the HTTP side of the session is the page's business).
    pub fn sign_out() {
        if let Some(window) = web_sys::window()
            && let Ok(event) = web_sys::Event::new("agentloft-sign-out")
        {
            let _ = window.dispatch_event(&event);
        }
    }

    fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub fn load_session(_paths: &Paths) -> Option<Vec<u8>> {
        storage()?
            .get_item(SESSION_KEY)
            .ok()
            .flatten()
            .map(String::into_bytes)
    }

    pub fn save_session(_paths: &Paths, json: &[u8]) {
        if let (Some(storage), Ok(text)) = (storage(), std::str::from_utf8(json)) {
            let _ = storage.set_item(SESSION_KEY, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_read_from_the_bytes_first() {
        use gpui_kit::ImageFormat as F;
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a];
        // A PNG named .jpg is still a PNG.
        assert_eq!(image_format(Path::new("shot.jpg"), &png), Some(F::Png));
        assert_eq!(
            image_format(Path::new("x"), &[0xff, 0xd8, 0xff, 0xe0]),
            Some(F::Jpeg)
        );
        assert_eq!(
            image_format(Path::new("logo.svg"), b"<svg xmlns='x'/>"),
            Some(F::Svg)
        );
        assert_eq!(
            image_format(Path::new("noext"), b"  <svg xmlns='x'/>"),
            Some(F::Svg)
        );
        assert_eq!(image_format(Path::new("notes.txt"), b"hello"), None);
    }
}
