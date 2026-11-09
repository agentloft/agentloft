//! The automation surface: commands an agent sends, and the typed errors
//! [`crate::BrowserView::execute`] can fail with (inside its `anyhow::Error`,
//! so callers can `downcast_ref::<BrowserError>()`).

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One automation step. The serde shape (`{"action": "click", "ref_id": 3}`)
/// is the wire format the MCP layer exposes, so keep it stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BrowserCommand {
    /// Load a URL (or address-bar style input) and wait for it to finish
    /// loading, up to [`NAVIGATION_TIMEOUT`]. Returns the final URL and title.
    Navigate {
        url: String,
    },
    Back,
    Forward,
    Reload,
    /// An agent-readable outline of the page with `[ref=N]` handles.
    Snapshot,
    /// Scroll an element into view and click its center.
    Click {
        ref_id: u32,
    },
    /// Replace an input's value as if typed; `submit` then submits its form
    /// (or presses Enter when it has none).
    Type {
        ref_id: u32,
        text: String,
        #[serde(default)]
        submit: bool,
    },
    /// Press a key on the focused element: `"Enter"`, `"Tab"`, `"Escape"`,
    /// `"ArrowDown"`, `"Backspace"`, `"a"`, `"Shift+Tab"`, …
    Press {
        key: String,
    },
    /// Scroll the page (or the scrollable area under the viewport center)
    /// by `dy` CSS pixels; negative scrolls up.
    Scroll {
        dy: f64,
    },
    /// Run JavaScript in the page and return its JSON-serialized result.
    /// The script is an expression (`document.title`) or, when it does not
    /// parse as one, an async function body that uses `return`.
    Eval {
        script: String,
    },
    /// A PNG of the visible viewport at the display's backing scale,
    /// base64-encoded.
    Screenshot,
}

impl BrowserCommand {
    /// The `action` tag, for logs and error messages.
    pub fn action(&self) -> &'static str {
        match self {
            Self::Navigate { .. } => "navigate",
            Self::Back => "back",
            Self::Forward => "forward",
            Self::Reload => "reload",
            Self::Snapshot => "snapshot",
            Self::Click { .. } => "click",
            Self::Type { .. } => "type",
            Self::Press { .. } => "press",
            Self::Scroll { .. } => "scroll",
            Self::Eval { .. } => "eval",
            Self::Screenshot => "screenshot",
        }
    }
}

/// How long `Navigate`, `Back`, `Forward` and `Reload` wait for the load.
pub const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(20);
/// How long helper scripts (snapshot, click, …) may run.
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `Eval` may run (it may await promises).
pub const EVAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Typed failures. Messages are written for an agent to read and act on.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BrowserError {
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("blocked: {0}")]
    Blocked(String),
    #[error("timed out after {}s {what}", .after.as_secs())]
    Timeout { what: String, after: Duration },
    #[error("navigation failed: {0}")]
    NavigationFailed(String),
    #[error("no element with [ref={0}] on the current page; take a new snapshot")]
    RefNotFound(u32),
    #[error("{0}")]
    Action(String),
    #[error("JavaScript error: {0}")]
    Script(String),
    #[error("the browser view is gone")]
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_shape_is_stable() {
        let cases = [
            (
                BrowserCommand::Navigate {
                    url: "example.com".into(),
                },
                json!({"action": "navigate", "url": "example.com"}),
            ),
            (BrowserCommand::Back, json!({"action": "back"})),
            (BrowserCommand::Snapshot, json!({"action": "snapshot"})),
            (
                BrowserCommand::Click { ref_id: 7 },
                json!({"action": "click", "ref_id": 7}),
            ),
            (
                BrowserCommand::Type {
                    ref_id: 2,
                    text: "hi".into(),
                    submit: true,
                },
                json!({"action": "type", "ref_id": 2, "text": "hi", "submit": true}),
            ),
            (
                BrowserCommand::Press {
                    key: "Enter".into(),
                },
                json!({"action": "press", "key": "Enter"}),
            ),
            (
                BrowserCommand::Scroll { dy: -120.5 },
                json!({"action": "scroll", "dy": -120.5}),
            ),
            (
                BrowserCommand::Eval {
                    script: "1+1".into(),
                },
                json!({"action": "eval", "script": "1+1"}),
            ),
            (BrowserCommand::Screenshot, json!({"action": "screenshot"})),
        ];
        for (cmd, wire) in cases {
            assert_eq!(serde_json::to_value(&cmd).unwrap(), wire);
            assert_eq!(serde_json::from_value::<BrowserCommand>(wire).unwrap(), cmd);
        }
    }

    #[test]
    fn submit_defaults_to_false() {
        let cmd: BrowserCommand =
            serde_json::from_value(json!({"action": "type", "ref_id": 1, "text": "x"})).unwrap();
        assert_eq!(
            cmd,
            BrowserCommand::Type {
                ref_id: 1,
                text: "x".into(),
                submit: false
            }
        );
    }

    #[test]
    fn errors_read_well() {
        assert_eq!(
            BrowserError::RefNotFound(4).to_string(),
            "no element with [ref=4] on the current page; take a new snapshot"
        );
        assert_eq!(
            BrowserError::Timeout {
                what: "waiting for https://x.y/ to load".into(),
                after: NAVIGATION_TIMEOUT
            }
            .to_string(),
            "timed out after 20s waiting for https://x.y/ to load"
        );
    }
}
