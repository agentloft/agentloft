//! Native pieces for GPUI windows that show web content: the pages of views
//! agents make ([`ViewHost`], ADR-0018) and the app's own menus ([`menu`]).
//!
//! The browser tab itself is no longer a native web view: it is a page of the
//! daemon's own browser, streamed (ADR-0017), so the desktop app, the web
//! client and agents all see the same one.
//!
//! # Platform
//!
//! macOS hosts a view's page in a WKWebView (through `wry`) added as a child
//! of GPUI's content NSView. Elsewhere [`ViewHost::new`] fails and the app
//! shows the daemon's streamed page instead.

mod host;
pub mod menu;
mod native;

/// The icon for leaving notes on things for the agent, wherever the app
/// offers it (browser tabs, documents in the viewer).
pub const ANNOTATE_ICON: gpui_kit::assets::IconName =
    gpui_kit::assets::IconName::SquareDashedMousePointer;

pub use host::{Corners, HostEvent, HostPicture, HostSurface, ViewHost, without_band};
pub use loft_page_script::address::{DEFAULT_SEARCH_TEMPLATE, normalize_address, search_url};
