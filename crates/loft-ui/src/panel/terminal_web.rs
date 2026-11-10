//! The terminal tab in the browser build: a placeholder. The terminal
//! emulator (libghostty-vt) is native code, and a browser session isn't
//! given a shell on the daemon's machine unless `[web] allow_terminal` is
//! set (ADR-0016). Same shape as the desktop pane, so the panel code is
//! shared.

use crate::store::AppStore;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window, div,
};
use loft_protocol::{FsRoot, TerminalId};

/// Stands in for the emulator view (something to focus).
pub struct TerminalView {
    focus: FocusHandle,
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub struct TerminalPane {
    pub view: Entity<TerminalView>,
    pub id: Option<TerminalId>,
}

impl TerminalPane {
    pub fn new(
        _store: Entity<AppStore>,
        _root: FsRoot,
        _key: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| TerminalView {
            focus: cx.focus_handle(),
        });
        Self { view, id: None }
    }

    pub fn restart(&mut self, _cx: &mut Context<Self>) {}

    pub fn label(&self) -> String {
        "Terminal".into()
    }

    pub fn close(&mut self, _cx: &mut Context<Self>) {}

    pub fn on_replay(&mut self, _bytes: &[u8], _exited: Option<i32>, _cx: &mut Context<Self>) {}

    pub fn on_output(&mut self, _bytes: &[u8], _cx: &mut Context<Self>) {}

    pub fn on_exit(&mut self, _code: Option<i32>, _cx: &mut Context<Self>) {}

    pub fn on_lost(&mut self, _cx: &mut Context<Self>) {}
}

impl Render for TerminalPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::host_ui::not_in_browser(IconName::SquareTerminal, "Terminal", &Palette::of(cx))
    }
}
