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
}

/// The app's look, as a view is told it (CSS variables and
/// `agentloft.theme`).
pub use loft_view::Theme as ViewTheme;

/// Something a view asked for on the user's click (`agentloft.send`,
/// `.copy`, `.open`, or a link). It goes to the viewer who clicked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum PageAction {
    /// Say this to the agent, as the user.
    Send { text: String },
    /// Put this on the viewer's clipboard.
    Copy { text: String },
    /// Open a link, or a file of the thread's folder.
    Open { target: String },
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
    Back,
    Forward,
    Reload,
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
