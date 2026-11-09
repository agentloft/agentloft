//! An embedded web browser panel for GPUI that a coding agent can drive.
//!
//! [`BrowserView`] is a GPUI view: a gpui-kit toolbar (back, forward,
//! reload/stop, an address field where bare words search, a progress line)
//! over a native web view. The app shows it in a side panel next to the chat;
//! agents drive it through [`BrowserView::execute`] with [`BrowserCommand`]s
//! while the user watches.
//!
//! # Platform
//!
//! macOS only for now: a WKWebView (through `wry`) added as a child of
//! GPUI's content NSView, which GPUI's `Window` exposes through
//! `raw_window_handle::HasWindowHandle`. Elsewhere the view renders a notice
//! and `execute` fails with [`BrowserError::Unsupported`].
//!
//! Native views draw above GPUI, so the surface element (see `surface.rs`)
//! re-applies its bounds on every paint and hides the WKWebView on the first
//! frame it is not painted. Known consequence: GPUI overlays (menus,
//! tooltips, dialogs) that overlap the page are drawn *under* it; call
//! [`BrowserView::set_visible`]`(false)` while such an overlay is open.
//!
//! # Commands and results
//!
//! | Command | Result |
//! |---|---|
//! | `navigate {url}` | `{url, title}` after the load finishes (20 s timeout) |
//! | `back` / `forward` / `reload` | `{url, title}` |
//! | `snapshot` | `{url, title, snapshot, refs, truncated}`; `snapshot` is the text outline |
//! | `click {ref_id}` | `{clicked, navigated, url, title, warning?, navigation_error?}` |
//! | `type {ref_id, text, submit}` | `{element, value, submit?, navigated, url, title}` |
//! | `press {key}` | `{key, target, default_action?, navigated, url, title}` |
//! | `scroll {dy}` | `{target, scroll_y, scroll_height, viewport_height, at_top, at_bottom}` |
//! | `eval {script}` | the script's value as JSON |
//! | `screenshot` | `{mime_type: "image/png", width, height, scale, data}`: the viewport as a base64 PNG at the display's backing scale (`scale` image px per CSS px, 2 on Retina) |
//!
//! Failures are `anyhow::Error`s wrapping a [`BrowserError`].
//!
//! # Snapshots and refs
//!
//! `snapshot` returns the rendered page as compact text: title, URL, scroll
//! position, then one line per block (headings as `#`, list items as `-`)
//! with interactive elements inline, e.g.
//! `[ref=12] link "Show HN: …" -> https://…` or
//! `[ref=3] input "Email" type=email placeholder="you@x.com"`. Refs are
//! stored on the elements as `data-loft-ref` and stay stable across
//! snapshots of the same document. The text is capped at about 25,000
//! characters with a truncation note.
//!
//! Automation scripts run in an isolated WebKit content world, so page
//! scripts cannot see or interfere with them; `eval` runs in the page's own
//! world so it can read page globals. Click, type and press dispatch the
//! events a user would cause (pointer/mouse events and a click; editing
//! commands or the native value setter plus `input`/`change`). They are
//! synthetic (`isTrusted` is false for mouse and key events), so the few
//! sites that check for trusted events will ignore them.
//!
//! # Safety policy
//!
//! Only `http`, `https` and `about` URLs may load, plus `file` URLs the host
//! navigates to itself (address bar, `navigate`, history); pages cannot
//! navigate to `file:` or any other scheme. Pop-ups open in place. Downloads
//! are cancelled. Web inspector is enabled in debug builds only.

mod address;
mod command;
mod menu;
mod native;
mod script;
mod snapshot;
mod surface;
mod view;

pub use address::{DEFAULT_SEARCH_TEMPLATE, normalize_address, search_url};
pub use command::{BrowserCommand, BrowserError, EVAL_TIMEOUT, NAVIGATION_TIMEOUT, SCRIPT_TIMEOUT};
pub use snapshot::{
    Block, BlockKind, ElementInfo, FormattedSnapshot, MAX_SNAPSHOT_CHARS, PageSnapshot, Part,
    Viewport, format_snapshot,
};
pub use view::{
    Annotation, AreaRect, BrowserEvent, BrowserView, DEVICES, Device, DeviceKind, FocusAddressBar,
    GoBack, GoForward, ReloadPage, StopLoading, bind_keys,
};
