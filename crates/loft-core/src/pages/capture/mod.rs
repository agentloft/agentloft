//! Agents' capture tools (ADR-0017 §8): screenshots of any region (with
//! numbered elements, saved or kept as a baseline), screenshot diffs, a
//! tab's network traffic with credentials redacted, its console, a
//! recording of it, and emulation (size, colour scheme, time zone, network,
//! CPU, …).
//!
//! The set of tools was inspired by OpenCode Browse's capture tools; the
//! code is our own, written from a description of what each tool does. The
//! diff counts changed pixels as pixelmatch does, and a recording is MJPEG
//! in Matroska as that specification describes. What differs from Browse is
//! the browser: ours is the daemon's, its pages are streamed to viewers,
//! and an agent shares a page with them.
//!
//! - Nothing a page could notice runs unless an agent asks for it. Network
//!   capture turns the page's `Network` domain on while it runs (pages
//!   can't see that); console capture needs `Runtime`, which pages can
//!   detect, so it is on only between `console_start` and `console_stop`.
//! - A recording is one more watcher of the page's screencast: viewers'
//!   pictures are the same with or without one, and a page nobody else
//!   watches is pictured for it.
//! - Emulating a size is the viewers' responsive mode (`PageEmulate`), so
//!   every viewer shows the page as the agent set it.
//! - Files (recordings, saved screenshots and captures) go to a folder of
//!   the thread's in the temporary directory, the user's only. The daemon
//!   writes nowhere an agent names: an agent copies a file on with its own
//!   tools, under its own permissions.
//! - Everything kept is bounded: entries, bodies, images, baselines.

mod console;
mod cross_site;
mod emulate;
mod labels;
mod mkv;
mod network;
mod network_model;
mod pixeldiff;
mod recording;
mod redaction;
mod shot;

use super::automation::Agent;
use super::cdp::Event;
use super::{Browser, Page, lock};
use loft_page_script::{BrowserCommand, BrowserError};
use loft_protocol::PageId;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// What agents' captures keep, per page.
#[derive(Default)]
pub(super) struct Captures {
    pages: Mutex<HashMap<PageId, Arc<PageCapture>>>,
    /// The pages' frames of other sites, followed over their own sessions
    /// (`cross_site.rs`).
    frames: Arc<cross_site::FrameSessions>,
}

impl Captures {
    fn get(&self, id: &PageId) -> Option<Arc<PageCapture>> {
        lock(&self.pages).get(id).cloned()
    }

    /// The page's, made if it has none (forgetting closed pages').
    fn of(&self, browser: &Browser, id: &PageId) -> Arc<PageCapture> {
        let mut captures = lock(&self.pages);
        if let Some(capture) = captures.get(id) {
            return capture.clone();
        }
        let open = lock(&browser.pages);
        captures.retain(|id, _| open.contains_key(id));
        captures.entry(id.clone()).or_default().clone()
    }
}

/// One page's captures.
#[derive(Default)]
pub(super) struct PageCapture {
    network: Mutex<network::NetworkState>,
    console: Mutex<console::ConsoleState>,
    recording: tokio::sync::Mutex<recording::RecordingState>,
    /// The running recording's frames go here (looked at for every picture).
    frames: Mutex<Option<Arc<Mutex<recording::Sink>>>>,
    shots: Mutex<shot::Baselines>,
    emulation: Mutex<emulate::Emulation>,
    /// Who needs the page's `Network` domain on: a capture, or emulated
    /// network conditions (which apply only while it is).
    network_domain: Mutex<NetworkUse>,
}

#[derive(Default, Clone, Copy)]
struct NetworkUse {
    capture: bool,
    conditions: bool,
}

impl Browser {
    /// What the page says, as far as captures care: its traffic, its
    /// console, and its pictures; and what its frames of other sites say
    /// over their own sessions (`cross_site.rs`), which `run` brings here
    /// too.
    pub(super) fn observe_for_capture(&self, page: &Arc<Page>, event: &Event) {
        let method = event.method.as_str();
        if matches!(
            method,
            "Target.attachedToTarget" | "Target.detachedFromTarget"
        ) {
            cross_site::observe(&self.cdp, &self.captures, page, event);
            return;
        }
        let Some(capture) = self.captures.get(&page.id) else {
            return;
        };
        if method.starts_with("Network.") {
            let frame = cross_site::frame_of(&self.captures, page, event);
            network::observe(&capture, event, frame.as_deref());
        } else if method.starts_with("Runtime.") || method.starts_with("Log.") {
            let frame = cross_site::frame_of(&self.captures, page, event);
            console::observe(&capture, event, frame.as_deref());
        } else if method == "Page.screencastFrame"
            && event.session.as_deref() == Some(&page.session)
        {
            recording::observe(&capture, event);
        }
    }
}

impl Agent<'_> {
    fn captures(&self) -> Arc<PageCapture> {
        self.browser.captures.of(self.browser, &self.page.id)
    }

    /// A capture command (all but `screenshot`, which is [`Agent::screenshot`]).
    pub(super) async fn capture(&self, command: BrowserCommand) -> Result<Value, BrowserError> {
        let state = self.captures();
        match command {
            BrowserCommand::ScreenshotDiff(options) => self.screenshot_diff(&state, options).await,
            BrowserCommand::NetworkStart(options) => network::start(self, &state, options).await,
            BrowserCommand::NetworkStatus => Ok(network::status(&state)),
            BrowserCommand::NetworkStop(options) => network::stop(self, &state, options).await,
            BrowserCommand::NetworkCancel => network::cancel(self, &state).await,
            BrowserCommand::ConsoleStart => console::start(self, &state).await,
            BrowserCommand::ConsoleRead(options) => console::read(&state, &options),
            BrowserCommand::ConsoleStop => console::stop(self, &state).await,
            BrowserCommand::RecordingStart(options) => {
                recording::start(self, &state, options).await
            }
            BrowserCommand::RecordingStatus => Ok(recording::status(&state).await),
            BrowserCommand::RecordingStop => recording::stop(self, &state).await,
            BrowserCommand::RecordingCancel => Ok(recording::cancel(self, &state).await),
            BrowserCommand::Emulate(options) => emulate::emulate(self, &state, *options).await,
            other => Err(BrowserError::Unsupported(format!(
                "{} isn't a capture command",
                other.action()
            ))),
        }
    }

    /// Turn the page's `Network` domain on for `who` (it may already be).
    async fn network_on(
        &self,
        state: &PageCapture,
        who: fn(&mut NetworkUse) -> &mut bool,
    ) -> Result<(), BrowserError> {
        // Request bodies up to 64 KiB arrive with their event; bigger ones
        // are asked for (`Network.getRequestPostData`).
        self.send(
            "Network.enable",
            serde_json::json!({ "maxPostDataSize": 65_536 }),
            loft_page_script::SCRIPT_TIMEOUT,
            "turning on the page's network events",
        )
        .await?;
        *who(&mut lock(&state.network_domain)) = true;
        Ok(())
    }
}

/// `who` no longer needs the page's `Network` domain: off, if nobody else
/// does.
async fn network_off(
    cdp: &super::cdp::Cdp,
    session: &str,
    state: &PageCapture,
    who: fn(&mut NetworkUse) -> &mut bool,
) {
    let off = {
        let mut domain = lock(&state.network_domain);
        *who(&mut domain) = false;
        !domain.capture && !domain.conditions
    };
    if off {
        let _ = cdp
            .send("Network.disable", serde_json::json!({}), Some(session))
            .await;
    }
}

/// Where the thread's captures are saved: a folder of its own in the
/// temporary directory, the user's only.
fn folder(page: &Page) -> Result<PathBuf, BrowserError> {
    let thread = lock(&page.state).info.thread_id.clone();
    let failed = |dir: &std::path::Path, e: std::io::Error| {
        BrowserError::Action(format!(
            "couldn't make the folder for captures ({}): {e}",
            dir.display()
        ))
    };
    // Both levels the user's own (on Linux the temporary folder is shared).
    let base = loft_config::private_temp_dir("agentloft-browser")
        .map_err(|e| failed(&std::env::temp_dir().join("agentloft-browser"), e))?;
    let dir = base.join(thread.as_str());
    loft_config::private_dir(&dir).map_err(|e| failed(&dir, e))?;
    Ok(dir)
}

/// A file name an agent gave (or `stem-<time>.ext`): a plain name, in the
/// thread's folder, not yet taken.
fn file_in(
    page: &Page,
    name: Option<&str>,
    stem: &str,
    ext: &str,
) -> Result<PathBuf, BrowserError> {
    let dir = folder(page)?;
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => {
            let plain = name.len() <= 120
                && !name.starts_with('.')
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
            if !plain {
                return Err(BrowserError::Action(format!(
                    "{name:?} isn't a plain file name (letters, digits, '-', '_', '.' and spaces; no folders)"
                )));
            }
            name.to_owned()
        }
        None => format!("{stem}-{}.{ext}", stamp()),
    };
    let path = dir.join(&name);
    if !path.exists() {
        return Ok(path);
    }
    let (base, extension) = match name.rsplit_once('.') {
        Some((base, extension)) => (base.to_owned(), format!(".{extension}")),
        None => (name.clone(), String::new()),
    };
    (2..1000)
        .map(|n| dir.join(format!("{base}-{n}{extension}")))
        .find(|p| !p.exists())
        .ok_or_else(|| BrowserError::Action(format!("too many files named {name}")))
}

/// Write `bytes` to a new file in the thread's folder, the user's only.
fn save(page: &Page, stem: &str, ext: &str, bytes: &[u8]) -> Result<PathBuf, BrowserError> {
    let path = file_in(page, None, stem, ext)?;
    write_private(&path, bytes)
        .map_err(|e| BrowserError::Action(format!("couldn't save {}: {e}", path.display())))?;
    Ok(path)
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// The time now, as JavaScript's `toISOString` writes it.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// The time now, for a file name.
fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S-%3f").to_string()
}

/// Milliseconds since 1970, as the browser's timestamps count.
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests;
