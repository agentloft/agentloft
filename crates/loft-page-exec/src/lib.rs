//! `browser_execute`: agents' browser scripts (ADR-0017 §10).
//!
//! An agent's JavaScript runs here, in an embedded QuickJS of its own, and
//! drives one of its thread's tabs: every step it takes is a
//! [`BrowserCommand`], sent through [`Tabs`] exactly as the plain
//! `browser_*` tools send theirs. Nothing else is in reach: no network, no
//! files, no processes, only the commands and timers.
//!
//! How a run is laid out:
//!
//! - `engine.rs`: the JavaScript, on a thread of its own (a runaway script
//!   must never hold up the daemon's async runtime), under a heap, stack and
//!   CPU-slice limit. It asks for commands, sleeps and tab facts over a
//!   channel and blocks while it has nothing to run.
//! - `host.rs`: the async side. It runs the commands the engine asks for,
//!   keeps the clock (paused while a person has the tab), the console, the
//!   pictures, and writes the [`Report`].
//! - `prelude.js`: everything a script sees: `page`, locators, `browse`,
//!   `snapshot`, `ref`, `console`, timers, `state`, and the rendering of the
//!   result.
//! - `mapping.rs`: `browse.<name>(args)` → a `BrowserCommand`.

mod engine;
mod host;
mod mapping;
#[cfg(test)]
mod tests;

use std::pin::Pin;
use std::time::Duration;

use loft_page_script::BrowserCommand;
use serde_json::{Value, json};

/// How long synchronous JavaScript may run without giving control back.
pub const CPU_SLICE: Duration = Duration::from_secs(2);
/// A script's heap.
pub const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// The result text (the returned value) is cut beyond this many characters.
pub const MAX_RESULT_CHARS: usize = 100_000;
/// The largest `state` kept between runs, in bytes of JSON.
pub const MAX_STATE: usize = 1024 * 1024;
/// Pictures attached to one result.
pub const MAX_IMAGES: usize = 8;
/// Base64 bytes of all the pictures attached to one result.
pub const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;

/// The thread's tabs, as a script reaches them.
pub trait Tabs: Send + Sync {
    /// Run one command on the tab with that id. `Err` carries a message
    /// meant for the agent.
    fn call<'a>(
        &'a self,
        tab: &'a str,
        command: BrowserCommand,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

    /// The tab's last known address and title, without asking the page;
    /// `None` once it is closed (or isn't the thread's).
    fn info(&self, tab: &str) -> Option<TabInfo>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub url: String,
    pub title: String,
}

/// One run.
pub struct Script {
    /// The JavaScript: an async function body, or a single expression.
    pub code: String,
    /// The tab `page` stands for.
    pub tab: String,
    /// That tab's `state` from earlier runs, as JSON (`"{}"` when none).
    pub state: String,
    /// The whole run's time limit.
    pub timeout: Duration,
}

/// A picture the script returned.
pub struct Image {
    pub mime_type: String,
    /// Base64.
    pub data: String,
}

/// How a run went.
pub struct Report {
    /// The tab `page` ended on: the agent passes it back as `tab`.
    pub tab: String,
    /// The whole text result: `tab:` line, value or error, warnings, what
    /// happened to the page, console.
    pub text: String,
    pub is_error: bool,
    pub images: Vec<Image>,
    /// `state` to keep for [`Report::tab`], as JSON; `None` leaves the
    /// stored one as it was.
    pub state: Option<String>,
}

impl Report {
    /// `{"tab", "text", "is_error", "images": [{"mime_type", "data"}]}`.
    pub fn to_json(&self) -> Value {
        json!({
            "tab": self.tab,
            "text": self.text,
            "is_error": self.is_error,
            "images": self
                .images
                .iter()
                .map(|image| json!({ "mime_type": image.mime_type, "data": image.data }))
                .collect::<Vec<_>>(),
        })
    }
}

/// Run a script to its end, its time limit or its first uncaught error.
/// Every outcome is a [`Report`]; dropping the future stops the script.
pub async fn run(script: Script, tabs: &dyn Tabs) -> Report {
    host::run(script, tabs).await
}

/// The first `max` characters of `text` and, when that cuts it, a line
/// saying how much was left out.
pub(crate) fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((at, _)) => {
            let left = text[at..].chars().count();
            format!("{}\n… ({left} more characters cut)", &text[..at])
        }
    }
}
