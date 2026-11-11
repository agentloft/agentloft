//! No native pages off macOS: [`NativeHost::new`] fails, and whoever asked
//! shows the page another way.

use crate::host::{Corners, HostEvent, HostPicture};
use futures::channel::{mpsc::UnboundedSender, oneshot};
use std::cell::Cell;
use std::rc::Rc;

pub(crate) struct NativeHost {
    pub(crate) paint_token: Cell<u64>,
}

impl NativeHost {
    pub fn new(
        _window: &gpui_kit::Window,
        _origin: &str,
        _allowed_hosts: &[&str],
        _events: UnboundedSender<HostEvent>,
    ) -> anyhow::Result<Rc<Self>> {
        Err(
            crate::BrowserError::Unsupported("a native page is only implemented on macOS".into())
                .into(),
        )
    }

    pub fn run(&self, _body: &str, _bridge: bool) -> oneshot::Receiver<Result<(), String>> {
        oneshot::channel().1
    }

    pub fn snapshot(&self) -> oneshot::Receiver<Result<HostPicture, String>> {
        oneshot::channel().1
    }

    pub fn set_size(&self, _width: f64, _height: f64) {}

    pub fn set_scrolls(&self, _scrolls: bool) {}

    pub fn park(&self) {}

    pub fn place(&self, _bounds: [f64; 4], _visible: [f64; 4], _radius: f64, _corners: Corners) {}

    pub fn is_shown(&self) -> bool {
        false
    }

    pub fn is_focused(&self) -> bool {
        false
    }

    pub fn release_focus(&self) {}

    pub fn take_press(&self) {}

    pub fn take_release(&self) {}

    pub fn pointer_left(&self) {}

    pub fn click_at(&self, _x: f64, _y: f64) {}

    pub fn wheel_at(&self, _x: f64, _y: f64, _lines: i32) {}

    pub fn click_in_window(&self, _x: f64, _y: f64) {}
}
