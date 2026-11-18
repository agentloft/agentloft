//! The automation surface: commands an agent sends, and the typed errors
//! running one can fail with (`loft-browser`'s `BrowserView::execute` wraps
//! them in an `anyhow::Error`, so callers can `downcast_ref::<BrowserError>()`).

use crate::capture::{
    ConsoleReadOptions, DiffOptions, EmulateOptions, NetworkOptions, NetworkStopOptions,
    RecordingOptions, ScreenshotOptions,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One automation step. The serde shape (`{"action": "click", "ref_id": 3}`)
/// is the wire format the MCP layer exposes, so keep it stable: fields
/// added since are optional, and left out when they have their default.
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
    Snapshot {
        /// What to outline: a CSS selector, or `"page"` for all of it.
        /// Unset: the page's main area when it has exactly one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        within: Option<String>,
    },
    /// Click an element (scrolled into view, at its centre) or a point:
    /// real mouse input where the element is the one hit there.
    Click {
        #[serde(flatten)]
        target: Target,
        #[serde(default, skip_serializing_if = "MouseButton::is_left")]
        button: MouseButton,
        /// 2 for a double click.
        #[serde(default = "one", skip_serializing_if = "is_one")]
        count: u32,
        /// Keys held during the click: `Shift`, `Control`, `Alt`, `Meta`,
        /// `ControlOrMeta`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modifiers: Vec<String>,
    },
    /// Type text key by key (trusted key events) into the target, focused
    /// first with the caret at its end, or into the focused element; each
    /// key waits for the page to have dealt with the one before. `submit`
    /// then presses Enter. `insert`: no key events, the text put in as an
    /// input method commits it, a line at a time with Enter between (a
    /// script's `keyboard.insertText`).
    Type {
        #[serde(flatten)]
        target: Target,
        text: String,
        #[serde(default)]
        submit: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        insert: bool,
    },
    /// Paste text, and optionally HTML, into the target (focused first,
    /// the caret at its end) or the focused element: a real paste, through
    /// the browser's own clipboard (not this computer's) and paste
    /// command, so the page gets one trusted `paste` event and a rich
    /// editor makes its own paragraphs and formatting. `text` is the plain
    /// form, also what is checked for afterwards.
    Paste {
        #[serde(flatten)]
        target: Target,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        html: Option<String>,
    },
    /// Press a key or a shortcut on the focused element (or the target,
    /// focused first): `"Enter"`, `"Tab"`, `"Escape"`, `"ArrowDown"`,
    /// `"Backspace"`, `"a"`, `"Shift+Tab"`, `"ControlOrMeta+A"`, …
    Press {
        key: String,
        #[serde(flatten)]
        target: Target,
        /// Press it this many times.
        #[serde(default = "one", skip_serializing_if = "is_one")]
        count: u32,
    },
    /// Scroll with the mouse wheel by `dx`/`dy` CSS pixels (negative: up or
    /// left) over the target or the page; with a target and no delta,
    /// scroll it into view; `to` jumps to an edge (of the target when one
    /// is given).
    Scroll {
        #[serde(default)]
        dy: f64,
        #[serde(default, skip_serializing_if = "is_zero")]
        dx: f64,
        #[serde(flatten)]
        target: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<ScrollEdge>,
    },
    /// Run JavaScript in the page and return its JSON-serialized result.
    /// The script is an expression (`document.title`) or, when it does not
    /// parse as one, an async function body that uses `return`.
    Eval {
        script: String,
    },
    /// A PNG of the visible viewport at the display's backing scale (the
    /// page's device scale in the daemon's browser), base64-encoded. The
    /// options (all optional) pick another region, scale or format, number
    /// the elements on it, save it, or keep it as a diff baseline.
    Screenshot(ScreenshotOptions),
    // Page interaction (A)
    /// Set a field's value as if typed (trusted text input), replacing what
    /// is there; a checkbox or radio takes `checked`, a `<select>` the
    /// option with that value or label. `submit` then presses Enter.
    Fill {
        #[serde(flatten)]
        target: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checked: Option<bool>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        submit: bool,
    },
    /// Pick options of a `<select>` by value or label (several for a
    /// multiple select).
    Select {
        #[serde(flatten)]
        target: Target,
        #[serde(deserialize_with = "one_or_many")]
        values: Vec<String>,
    },
    /// Move the pointer over an element or a point (menus, tooltips).
    Hover {
        #[serde(flatten)]
        target: Target,
    },
    /// Press the mouse on one element or point, move to another and
    /// release there (HTML5 drag and drop included).
    Drag {
        from: Target,
        to: Target,
        /// Pointer moves on the way (default 10).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steps: Option<u32>,
    },
    /// Wait until every condition given holds: `text` is on the page,
    /// `text_gone` isn't, the target reaches `state` (`visible`, `hidden`,
    /// `attached`, `detached`), the address matches `url` (a substring, a
    /// glob with `*`, or `/regex/flags`). At most [`MAX_WAIT`].
    WaitFor {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_gone: Option<String>,
        #[serde(flatten)]
        target: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Milliseconds; default [`DEFAULT_WAIT`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Answer the JavaScript dialog (alert, confirm, prompt) the page is
    /// showing: accept (with `prompt_text` for a prompt) or dismiss.
    Dialog {
        accept: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_text: Option<String>,
    },
    /// Set the files of a file input (or the label of one), without a file
    /// chooser. Absolute paths of files the daemon may hand out.
    Upload {
        #[serde(flatten)]
        target: Target,
        #[serde(deserialize_with = "one_or_many")]
        files: Vec<String>,
    },
    /// Tools the page offers through WebMCP (`navigator.modelContext`): the
    /// list, or (`call`) run the one named with `input`.
    #[serde(rename = "webmcp")]
    WebMcp {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
    // Scripts (D)
    /// Run a script (`browser_execute`): JavaScript in a sandboxed engine
    /// in the daemon that drives the page with these same commands
    /// (`loft-page-exec`). Only the daemon's core runs it; a page can't.
    Execute {
        script: String,
        /// Overall time limit; [`EXECUTE_TIMEOUT`] when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Find elements by a chain of steps (what a script's `locator()`,
    /// `getByRole()`, `getByText()` … stand for) and give them refs, for
    /// the commands that take one. Each step is an object: `{css}`,
    /// `{text, exact?, regex?}`, `{role, name?, exact?, regex?, level?,
    /// checked?}`, `{label|placeholder|alt|title|testid, exact?, regex?}`
    /// search inside what the previous steps found; `{nth}` (negative
    /// counts from the end), `{has_text}`, `{has_not_text}` and
    /// `{visible}` narrow it. Answers `{count, refs, elements}` for the
    /// first `limit` (20) matches, in document order.
    Locate {
        steps: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Run `function` (JavaScript source, `(el, arg) => …`) in the page's
    /// own world on the element `refs[0]` stands for, or (`all`) on the
    /// array of them, with `arg`; answers its JSON result as `eval` does.
    /// What a script's `locator.textContent()`, `evaluate()` … stand for:
    /// refs live in the helpers' world, so the page never sees them.
    EvalOn {
        refs: Vec<u32>,
        function: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        arg: serde_json::Value,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
    },
    // Capture (B): ADR-0017 §8, the daemon's browser only.
    /// Compare a fresh screenshot with a baseline (the tab's last
    /// screenshot, or a named one): what changed, where, and a diff image.
    ScreenshotDiff(DiffOptions),
    /// Start capturing the tab's network traffic.
    NetworkStart(NetworkOptions),
    /// How the capture is going (counts only).
    NetworkStatus,
    /// End the capture and return its exchanges, credentials redacted.
    NetworkStop(NetworkStopOptions),
    /// End the capture and forget it.
    NetworkCancel,
    /// Start keeping the tab's console messages and uncaught errors.
    ConsoleStart,
    /// The messages kept so far.
    ConsoleRead(ConsoleReadOptions),
    ConsoleStop,
    /// Start recording the tab to a video file.
    RecordingStart(RecordingOptions),
    RecordingStatus,
    /// Finish the file and say where it is.
    RecordingStop,
    /// Stop and delete the file.
    RecordingCancel,
    /// Change how the tab presents itself (size, colour scheme, time zone,
    /// network, CPU, …).
    Emulate(Box<EmulateOptions>),
    // Tabs and handoff (C)
    /// List the thread's tabs, open one, or close some (ADR-0017 §9). The
    /// call's `tab` is the one to close.
    Tabs {
        #[serde(default)]
        op: TabsOp,
        /// `new`: a name for the tab (`docs`): opening that name again
        /// returns the same tab, and it never closes for going unused.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// `new`: where it opens (else a blank tab).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// `new`: agents may only send it to these origins.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        allowed_origins: Vec<String>,
        /// `new`: agents may read it but not click, type or navigate in it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        read_only: bool,
        /// `close`: every tab the agent opened.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
    },
    /// Hand the tab to a person (a sign-in, 2FA, a captcha, a payment) and
    /// wait until they press Continue, up to `timeout_secs`.
    Handoff {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// Ask a person to show how something is done in the tab, recording
    /// their steps until they press Done (or `timeout_secs` passes).
    Demonstrate {
        what: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// Whether the browser can run, the thread's tabs, and what waits for
    /// a person.
    Status,
}

/// What an element command acts on: a ref from the latest snapshot, a
/// selector, or a point in the viewport (CSS pixels). A ref wins over a
/// selector, and either over a point.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Target {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<u32>,
    /// CSS (shadow roots included), or `text=…`, `label=…`,
    /// `placeholder=…`, `role=button[name="…"]`, `testid=…`; `>>` chains
    /// them, `>> frame >>` enters an iframe, `>> nth=N` picks one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
}

impl Target {
    pub fn by_ref(ref_id: u32) -> Self {
        Self {
            ref_id: Some(ref_id),
            ..Self::default()
        }
    }

    pub fn by_selector(selector: impl Into<String>) -> Self {
        Self {
            selector: Some(selector.into()),
            ..Self::default()
        }
    }

    /// An element (a ref or a selector) rather than a point or nothing.
    pub fn is_element(&self) -> bool {
        self.ref_id.is_some()
            || self
                .selector
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    }

    /// The point, when that is what this is.
    pub fn point(&self) -> Option<(f64, f64)> {
        if self.is_element() {
            return None;
        }
        Some((self.x?, self.y?))
    }

    pub fn is_empty(&self) -> bool {
        !self.is_element() && self.point().is_none()
    }

    /// How it reads in a message: `[ref=3]`, `selector "#go"`, `(10, 20)`.
    pub fn label(&self) -> String {
        if let Some(id) = self.ref_id {
            format!("[ref={id}]")
        } else if let Some(selector) = self.selector.as_deref().filter(|s| !s.trim().is_empty()) {
            format!("selector {selector:?}")
        } else if let Some((x, y)) = self.point() {
            format!("({x}, {y})")
        } else {
            "nothing".to_owned()
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn is_left(&self) -> bool {
        *self == Self::Left
    }

    /// The DevTools protocol's name for it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollEdge {
    Top,
    Bottom,
    Left,
    Right,
}

impl ScrollEdge {
    pub fn name(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

fn one() -> u32 {
    1
}

fn is_one(n: &u32) -> bool {
    *n == 1
}

fn is_zero(n: &f64) -> bool {
    *n == 0.0
}

/// `"a"` or `["a", "b"]`.
fn one_or_many<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(de)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

/// What `BrowserCommand::Tabs` does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabsOp {
    #[default]
    List,
    New,
    Close,
}

impl BrowserCommand {
    /// The `action` tag, for logs and error messages.
    pub fn action(&self) -> &'static str {
        match self {
            Self::Navigate { .. } => "navigate",
            Self::Back => "back",
            Self::Forward => "forward",
            Self::Reload => "reload",
            Self::Snapshot { .. } => "snapshot",
            Self::Click { .. } => "click",
            Self::Type { .. } => "type",
            Self::Paste { .. } => "paste",
            Self::Press { .. } => "press",
            Self::Scroll { .. } => "scroll",
            Self::Eval { .. } => "eval",
            Self::Screenshot(_) => "screenshot",
            // Page interaction (A)
            Self::Fill { .. } => "fill",
            Self::Select { .. } => "select",
            Self::Hover { .. } => "hover",
            Self::Drag { .. } => "drag",
            Self::WaitFor { .. } => "wait_for",
            Self::Dialog { .. } => "dialog",
            Self::Upload { .. } => "upload",
            Self::WebMcp { .. } => "webmcp",
            // Scripts (D)
            Self::Execute { .. } => "execute",
            Self::Locate { .. } => "locate",
            Self::EvalOn { .. } => "eval_on",
            // Capture (B)
            Self::ScreenshotDiff(_) => "screenshot_diff",
            Self::NetworkStart(_) => "network_start",
            Self::NetworkStatus => "network_status",
            Self::NetworkStop(_) => "network_stop",
            Self::NetworkCancel => "network_cancel",
            Self::ConsoleStart => "console_start",
            Self::ConsoleRead(_) => "console_read",
            Self::ConsoleStop => "console_stop",
            Self::RecordingStart(_) => "recording_start",
            Self::RecordingStatus => "recording_status",
            Self::RecordingStop => "recording_stop",
            Self::RecordingCancel => "recording_cancel",
            Self::Emulate(_) => "emulate",
            // Tabs and handoff (C)
            Self::Tabs { .. } => "tabs",
            Self::Handoff { .. } => "handoff",
            Self::Demonstrate { .. } => "demonstrate",
            Self::Status => "status",
        }
    }

    /// Whether a read-only tab (ADR-0017 §9) still takes it: it reads the
    /// page, or is about tabs or people rather than the page. Everything
    /// else (clicks, typing, keys, navigation, anything new) is refused
    /// there. `Eval` still runs: it can change the page, so
    /// read-only keeps an agent from wandering, it doesn't fence it in.
    pub fn allowed_read_only(&self) -> bool {
        matches!(
            self,
            Self::Snapshot { .. }
                | Self::Eval { .. }
                | Self::Screenshot(_)
                | Self::Scroll { .. }
                // Reads (A, D): waiting, and the page's own tools, which
                // are its JavaScript as `eval` is.
                | Self::WaitFor { .. }
                | Self::WebMcp { .. }
                | Self::Locate { .. }
                | Self::EvalOn { .. }
                // A script's own steps are checked one by one.
                | Self::Execute { .. }
                // Looking closer (B): none of these click, type or go
                // anywhere.
                | Self::ScreenshotDiff(_)
                | Self::NetworkStart(_)
                | Self::NetworkStatus
                | Self::NetworkStop(_)
                | Self::NetworkCancel
                | Self::ConsoleStart
                | Self::ConsoleRead(_)
                | Self::ConsoleStop
                | Self::RecordingStart(_)
                | Self::RecordingStatus
                | Self::RecordingStop
                | Self::RecordingCancel
                | Self::Emulate(_)
                | Self::Tabs { .. }
                | Self::Handoff { .. }
                | Self::Demonstrate { .. }
                | Self::Status
        )
    }

    /// Whether it acts on the thread's tabs as a whole rather than on one
    /// page (no tab is opened for it).
    pub fn about_tabs(&self) -> bool {
        matches!(self, Self::Tabs { .. } | Self::Status)
    }

    /// Whether it sends the tab somewhere by itself (allowed even while the
    /// tab is outside its allowed origins: it is how to get back).
    pub fn goes_somewhere(&self) -> bool {
        matches!(self, Self::Navigate { .. } | Self::Back | Self::Forward)
    }
}

/// How long `Navigate`, `Back`, `Forward` and `Reload` wait for the load.
pub const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(20);
/// How long helper scripts (snapshot, click, …) may run.
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `Eval` may run (it may await promises).
pub const EVAL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `WaitFor` waits unless told otherwise, and at most.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(10);
pub const MAX_WAIT: Duration = Duration::from_secs(60);
// Scripts (D)
/// How long an `Execute` may run when it doesn't say.
pub const EXECUTE_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest an `Execute` may ask for.
pub const MAX_EXECUTE_TIMEOUT: Duration = Duration::from_secs(600);

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
    /// The page shows a JavaScript dialog and is paused until it is
    /// answered (`Dialog`).
    #[error("{0}")]
    DialogOpen(String),
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
            (
                BrowserCommand::Snapshot { within: None },
                json!({"action": "snapshot"}),
            ),
            (
                BrowserCommand::Click {
                    target: Target::by_ref(7),
                    button: MouseButton::Left,
                    count: 1,
                    modifiers: vec![],
                },
                json!({"action": "click", "ref_id": 7}),
            ),
            (
                BrowserCommand::Type {
                    target: Target::by_ref(2),
                    text: "hi".into(),
                    submit: true,
                    insert: false,
                },
                json!({"action": "type", "ref_id": 2, "text": "hi", "submit": true}),
            ),
            (
                BrowserCommand::Type {
                    target: Target::default(),
                    text: "a\nb".into(),
                    submit: false,
                    insert: true,
                },
                json!({"action": "type", "text": "a\nb", "submit": false, "insert": true}),
            ),
            (
                BrowserCommand::Paste {
                    target: Target::by_selector("#body"),
                    text: "One\nTwo".into(),
                    html: None,
                },
                json!({"action": "paste", "selector": "#body", "text": "One\nTwo"}),
            ),
            (
                BrowserCommand::Paste {
                    target: Target::default(),
                    text: "Bold".into(),
                    html: Some("<p><b>Bold</b></p>".into()),
                },
                json!({"action": "paste", "text": "Bold", "html": "<p><b>Bold</b></p>"}),
            ),
            (
                BrowserCommand::Press {
                    key: "Enter".into(),
                    target: Target::default(),
                    count: 1,
                },
                json!({"action": "press", "key": "Enter"}),
            ),
            (
                BrowserCommand::Scroll {
                    dy: -120.5,
                    dx: 0.0,
                    target: Target::default(),
                    to: None,
                },
                json!({"action": "scroll", "dy": -120.5}),
            ),
            // Page interaction (A)
            (
                BrowserCommand::Click {
                    target: Target::by_selector("#go"),
                    button: MouseButton::Right,
                    count: 2,
                    modifiers: vec!["Shift".into()],
                },
                json!({"action": "click", "selector": "#go", "button": "right", "count": 2,
                       "modifiers": ["Shift"]}),
            ),
            (
                BrowserCommand::Fill {
                    target: Target::by_ref(4),
                    value: Some("a@b.c".into()),
                    checked: None,
                    submit: false,
                },
                json!({"action": "fill", "ref_id": 4, "value": "a@b.c"}),
            ),
            (
                BrowserCommand::Drag {
                    from: Target::by_ref(1),
                    to: Target {
                        x: Some(10.0),
                        y: Some(20.0),
                        ..Target::default()
                    },
                    steps: None,
                },
                json!({"action": "drag", "from": {"ref_id": 1}, "to": {"x": 10.0, "y": 20.0}}),
            ),
            (
                BrowserCommand::Dialog {
                    accept: true,
                    prompt_text: Some("yes".into()),
                },
                json!({"action": "dialog", "accept": true, "prompt_text": "yes"}),
            ),
            (
                BrowserCommand::WebMcp {
                    call: None,
                    input: None,
                },
                json!({"action": "webmcp"}),
            ),
            (
                BrowserCommand::Eval {
                    script: "1+1".into(),
                },
                json!({"action": "eval", "script": "1+1"}),
            ),
            (
                BrowserCommand::Screenshot(Default::default()),
                json!({"action": "screenshot"}),
            ),
        ];
        for (cmd, wire) in cases {
            assert_eq!(serde_json::to_value(&cmd).unwrap(), wire);
            assert_eq!(serde_json::from_value::<BrowserCommand>(wire).unwrap(), cmd);
        }
    }

    #[test]
    fn tab_commands_wire_shape() {
        let cases = [
            (
                BrowserCommand::Tabs {
                    op: TabsOp::List,
                    name: None,
                    url: None,
                    allowed_origins: Vec::new(),
                    read_only: false,
                    all: false,
                },
                json!({"action": "tabs", "op": "list"}),
            ),
            (
                BrowserCommand::Tabs {
                    op: TabsOp::New,
                    name: Some("docs".into()),
                    url: Some("https://example.com".into()),
                    allowed_origins: vec!["example.com".into()],
                    read_only: true,
                    all: false,
                },
                json!({"action": "tabs", "op": "new", "name": "docs",
                       "url": "https://example.com", "allowed_origins": ["example.com"],
                       "read_only": true}),
            ),
            (
                BrowserCommand::Handoff {
                    reason: "Sign in".into(),
                    timeout_secs: Some(60),
                },
                json!({"action": "handoff", "reason": "Sign in", "timeout_secs": 60}),
            ),
            (
                BrowserCommand::Demonstrate {
                    what: "Export".into(),
                    timeout_secs: None,
                },
                json!({"action": "demonstrate", "what": "Export"}),
            ),
            (BrowserCommand::Status, json!({"action": "status"})),
        ];
        for (cmd, wire) in cases {
            assert_eq!(serde_json::to_value(&cmd).unwrap(), wire);
            assert_eq!(serde_json::from_value::<BrowserCommand>(wire).unwrap(), cmd);
        }
        // A bare `tabs` lists.
        assert!(matches!(
            serde_json::from_value::<BrowserCommand>(json!({"action": "tabs"})).unwrap(),
            BrowserCommand::Tabs {
                op: TabsOp::List,
                ..
            }
        ));
    }

    #[test]
    fn read_only_tabs_take_reads_only() {
        let command =
            |value: serde_json::Value| -> BrowserCommand { serde_json::from_value(value).unwrap() };
        assert!(command(json!({"action": "snapshot"})).allowed_read_only());
        assert!(command(json!({"action": "screenshot"})).allowed_read_only());
        assert!(command(json!({"action": "console_start"})).allowed_read_only());
        assert!(!command(json!({"action": "click", "ref_id": 1})).allowed_read_only());
        assert!(!command(json!({"action": "fill", "ref_id": 1, "value": "x"})).allowed_read_only());
        assert!(!command(json!({"action": "hover", "x": 1, "y": 1})).allowed_read_only());
        assert!(
            !BrowserCommand::Navigate {
                url: "x.com".into()
            }
            .allowed_read_only()
        );
        assert!(!command(json!({"action": "press", "key": "Enter"})).allowed_read_only());
        assert!(!command(json!({"action": "paste", "text": "x"})).allowed_read_only());
        assert!(
            !command(json!({"action": "type", "text": "x", "insert": true})).allowed_read_only()
        );
    }

    #[test]
    fn submit_defaults_to_false() {
        let cmd: BrowserCommand =
            serde_json::from_value(json!({"action": "type", "ref_id": 1, "text": "x"})).unwrap();
        assert_eq!(
            cmd,
            BrowserCommand::Type {
                target: Target::by_ref(1),
                text: "x".into(),
                submit: false,
                insert: false,
            }
        );
    }

    #[test]
    fn values_and_files_take_one_or_many() {
        let one: BrowserCommand =
            serde_json::from_value(json!({"action": "select", "ref_id": 3, "values": "M"}))
                .unwrap();
        let many: BrowserCommand = serde_json::from_value(
            json!({"action": "upload", "selector": "input[type=file]", "files": ["/a", "/b"]}),
        )
        .unwrap();
        assert!(matches!(one, BrowserCommand::Select { values, .. } if values == ["M"]));
        assert!(matches!(many, BrowserCommand::Upload { files, .. } if files.len() == 2));
        // A point is a target too; a ref wins over it.
        let point = Target {
            x: Some(1.0),
            y: Some(2.0),
            ..Target::default()
        };
        assert_eq!(point.point(), Some((1.0, 2.0)));
        assert_eq!(
            Target {
                ref_id: Some(3),
                ..point
            }
            .point(),
            None
        );
        assert!(Target::default().is_empty());
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
