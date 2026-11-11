//! Non-macOS placeholder: construction fails with `Unsupported`, so no other
//! method can ever be reached.

use super::{JsWorld, LoadMark, LoadProgress, NativeEvent, PageState, Screenshot};
use crate::BrowserError;
use futures::channel::{mpsc::UnboundedSender, oneshot};
use std::cell::Cell;

enum Never {}

pub(crate) struct NativeWebView {
    never: Never,
    pub(crate) paint_token: Cell<u64>,
}

impl NativeWebView {
    pub fn new(
        _window: &gpui_kit::Window,
        _events: UnboundedSender<NativeEvent>,
    ) -> anyhow::Result<Self> {
        Err(
            BrowserError::Unsupported("the embedded browser is only implemented on macOS".into())
                .into(),
        )
    }

    pub fn load(&self, _url: &str) -> anyhow::Result<()> {
        match self.never {}
    }
    pub fn go_back(&self) {
        match self.never {}
    }
    pub fn go_forward(&self) {
        match self.never {}
    }
    pub fn reload(&self) {
        match self.never {}
    }
    pub fn hard_reload(&self) {
        match self.never {}
    }

    pub fn open_devtools(&self) {
        match self.never {}
    }

    pub fn detach_devtools(&self) -> bool {
        match self.never {}
    }

    pub fn close_devtools(&self) {
        match self.never {}
    }

    pub fn devtools_open(&self) -> bool {
        match self.never {}
    }

    pub fn clear_data(&self, _what: super::SiteData) {
        match self.never {}
    }

    pub fn stop(&self) {
        match self.never {}
    }
    pub fn set_muted(&self, _muted: bool) {
        match self.never {}
    }
    pub fn state(&self) -> PageState {
        match self.never {}
    }
    pub fn load_mark(&self) -> LoadMark {
        match self.never {}
    }
    pub fn load_progress(&self, _mark: LoadMark) -> LoadProgress {
        match self.never {}
    }
    pub fn set_page_zoom(&self, _zoom: f64) {
        match self.never {}
    }
    pub fn set_user_agent(&self, _agent: Option<&str>) {
        match self.never {}
    }
    pub fn set_frame(&self, _x: f64, _y: f64, _w: f64, _h: f64) {
        match self.never {}
    }
    pub fn set_hidden(&self, _hidden: bool) {
        match self.never {}
    }
    pub fn is_focused(&self) -> bool {
        match self.never {}
    }
    pub fn focus(&self) {
        match self.never {}
    }
    pub fn release_focus(&self) {
        match self.never {}
    }
    pub fn call_js(
        &self,
        _body: &str,
        _world: JsWorld,
    ) -> oneshot::Receiver<Result<Option<String>, String>> {
        match self.never {}
    }
    pub fn screenshot(&self) -> oneshot::Receiver<Result<Screenshot, String>> {
        match self.never {}
    }
}
