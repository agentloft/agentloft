//! `loft-terminal`: a GPUI terminal widget backed by Ghostty's VT engine
//! (libghostty-vt).
//!
//! The widget only turns bytes into pixels and keystrokes into bytes. It
//! never spawns processes: in Agentloft the daemon owns the PTY (ADR-0007),
//! streams its output into [`TerminalView::feed`], and writes
//! [`TerminalEvent::Input`] back to the PTY.
//!
//! ```ignore
//! let terminal = cx.new(|cx| TerminalView::new(window, cx));
//! cx.subscribe(&terminal, |_, _, event, _| match event {
//!     TerminalEvent::Input(bytes) => pty.write_all(bytes),
//!     TerminalEvent::Resize { cols, rows, pixel_width, pixel_height } => pty.resize(..),
//!     TerminalEvent::TitleChanged(title) => { /* tab title */ }
//!     TerminalEvent::Bell => { /* attention */ }
//! });
//! terminal.update(cx, |t, cx| t.feed(&output, cx));
//! ```
//!
//! The VT engine sits behind an internal `TerminalBackend` trait, so it can
//! be swapped (for example for `alacritty_terminal`) without touching
//! rendering or input.

mod backend;
mod element;
mod input;
mod theme;
mod view;

pub use view::{Copy, KEY_CONTEXT, Paste, SelectAll, TerminalEvent, TerminalView, init};
