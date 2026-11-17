//! The daemon's own browser (ADR-0017): tabs it runs in a headless Chrome
//! and streams to whoever is looking. A viewer subscribes to
//! [`crate::Topic::Page`], gets pictures of the page ([`PageFrame`]) and its
//! state ([`PageInfo`]), and sends input back ([`PageInput`]).

use crate::ids::{PageId, ThreadId};
use crate::workbench::Bytes;
use serde::{Deserialize, Serialize};

/// One tab of the daemon's browser.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageInfo {
    pub id: PageId,
    pub thread_id: ThreadId,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    /// The page's viewport in CSS pixels.
    pub width: u32,
    pub height: u32,
    /// Device pixels per CSS pixel the page renders at.
    pub scale: f32,
    /// The page draws a view an agent made (ADR-0018), not a browser tab:
    /// it is as tall as its content, and has no address to show.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub view: bool,
    /// A view with more content than its tallest size: it scrolls inside.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overflow: bool,
    /// The pointer a view wants where the mouse is, as CSS names it
    /// ("text", "pointer", "grab"…); empty for the ordinary arrow. The
    /// viewer sees a picture, so its own pointer follows this.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cursor: String,
    /// The page's zoom: 1 is 100%. The viewport above is in the page's CSS
    /// pixels, so a zoomed page has a smaller one.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub zoom: f32,
    /// The screen the page is shown as, if not its viewers' own size. Its
    /// viewers then show the page scaled to fit, rather than resizing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<PageDevice>,
    /// Notes are being left on the page ([`PageAction::Note`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub annotating: bool,
    /// Something the user types into has the page's focus (a text field,
    /// an editable area): a touch device shows its keyboard for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub editing: bool,
    /// Where the page's visible text fields are, in the viewport as viewers
    /// see it (CSS px): a tap there is for typing, anywhere else it isn't.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<PageRect>,
    /// What the thread's agent has to do with the tab (ADR-0017 §9): the
    /// name it gave it, the limits it opened it with, and whether it waits
    /// for a person there. Empty for a person's tab.
    #[serde(default, skip_serializing_if = "PageAgent::is_empty")]
    pub agent: PageAgent,
}

/// A tab as the thread's agent uses it (ADR-0017 §9).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PageAgent {
    /// The name the agent gave it (`browser_tabs` `new` with a `name`):
    /// it never closes for going unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The agent opened it (or a page of one of its tabs did), not a
    /// person. A window shows it beside the person's tab rather than
    /// instead of it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opened: bool,
    /// The agent may only send it to these origins (patterns like
    /// `example.com`, `*.example.com`, `http://localhost:3000`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// The agent may read the tab but not click, type or navigate in it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// The agent handed the tab to a person and waits for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<PageHandoff>,
}

impl PageAgent {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The agent waits for a person in a tab: a card on the tab says why, and
/// what the person presses when they are done
/// ([`crate::Request::PageHandoffDone`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageHandoff {
    pub kind: HandoffKind,
    /// The agent's words: what to do (`Sign in to GitHub`).
    pub reason: String,
    /// The site the tab was at when it was handed over
    /// (`https://github.com`).
    #[serde(default)]
    pub origin: String,
    pub since_ms: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HandoffKind {
    /// Something only a person can do (a sign-in, a captcha, a payment):
    /// **Continue** when done.
    Handoff,
    /// The person shows the agent how something is done while their steps
    /// are recorded: **Done** when done.
    Demonstrate,
}

/// What an agent's browser command answered (`Request::BrowserCall`), and
/// the tab it ran in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserReply {
    /// The command's own result.
    pub value: serde_json::Value,
    /// The tab it ran in (none for commands about all of them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<BrowserTab>,
    /// Tabs pages opened meanwhile (popups, `target=_blank` links).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_tabs: Vec<BrowserTab>,
    /// What the agent should know besides (a document the tab's allowed
    /// origins refused, a popup closed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A tab, as an agent refers to it: by `name` if it has one, else `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserTab {
    pub id: PageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
}

impl BrowserTab {
    pub fn of(info: &PageInfo) -> Self {
        Self {
            id: info.id.clone(),
            name: info.agent.name.clone(),
            url: info.url.clone(),
            title: info.title.clone(),
        }
    }

    /// What the agent passes back as `tab`.
    pub fn handle(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id.0)
    }
}

fn one() -> f32 {
    1.0
}

fn is_one(zoom: &f32) -> bool {
    *zoom == 1.0
}

/// A screen a page can be shown as (Responsive is none).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageDevice {
    pub name: String,
    /// CSS pixels, as the device is held.
    pub width: u32,
    pub height: u32,
    /// A phone or a tablet: touch, a mobile viewport and its user agent.
    #[serde(default)]
    pub mobile: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

/// A note the user pinned to something on a page, for the agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageNote {
    /// A CSS selector for the element (empty for an area).
    #[serde(default)]
    pub selector: String,
    pub note: String,
    /// The element's visible text (trimmed), to recognise it by.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub tag: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// The pin number shown on the page.
    #[serde(default)]
    pub n: u32,
    /// For a dragged area: where it is on the page (CSS px).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<PageRect>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct PageRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The app's look, as a view is told it (CSS variables and
/// `agentloft.theme`).
pub use loft_view::Theme as ViewTheme;

/// Something a view asked for on the user's click (`agentloft.send`,
/// `.copy`, `.open`, or a link), or a note left on a page. It goes to the
/// viewer who clicked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum PageAction {
    /// Say this to the agent, as the user.
    Send { text: String },
    /// Put this on the viewer's clipboard.
    Copy { text: String },
    /// Open a link, or a file of the thread's folder.
    Open { target: String },
    /// A note was left on the page in annotation mode: it goes to the
    /// composer of whoever left it.
    Note { note: PageNote },
}

/// A picture of the page's viewport. Only the newest matters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageFrame {
    /// Counts up per page; a viewer acknowledges it to get the next one.
    pub seq: u64,
    pub jpeg: Bytes,
}

/// Where a page goes next.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "to", rename_all = "snake_case")]
pub enum PageNav {
    /// An `http`, `https` or `about:blank` address.
    Url {
        url: String,
    },
    /// A page whose server is only now starting (a project action's
    /// preview): asked for again while nothing answers there, for up to
    /// 45 seconds or until the tab goes somewhere else.
    Preview {
        url: String,
    },
    Back,
    Forward,
    Reload,
    /// Reload without the cache.
    HardReload,
    Stop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PageMouse {
    Move,
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PageButton {
    #[default]
    None,
    Left,
    Middle,
    Right,
}

/// Modifier bits in [`PageInput`], as the DevTools protocol numbers them.
pub mod page_modifiers {
    pub const ALT: u8 = 1;
    pub const CONTROL: u8 = 2;
    /// ⌘ on a Mac, the Windows key elsewhere.
    pub const META: u8 = 4;
    pub const SHIFT: u8 = 8;
}

/// What a viewer did. Positions are CSS pixels in the page's viewport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PageInput {
    Mouse {
        action: PageMouse,
        x: f32,
        y: f32,
        #[serde(default)]
        button: PageButton,
        /// 1 for a click, 2 for a double click…
        #[serde(default)]
        click_count: u8,
        #[serde(default)]
        modifiers: u8,
    },
    /// Positive `dy` scrolls down, positive `dx` right.
    Wheel {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        #[serde(default)]
        modifiers: u8,
    },
    /// A key going down or up: `key` and `code` as in a DOM `KeyboardEvent`
    /// ("a" and "KeyA", "Enter" and "Enter"), and the text it types, if any.
    Key {
        down: bool,
        key: String,
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default)]
        modifiers: u8,
        #[serde(default)]
        repeat: bool,
    },
    /// Text that arrives whole: a paste, or what an input method committed.
    Text { text: String },
}
