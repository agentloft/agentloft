//! What a kind of device (iOS simulators, Android emulators) gives the
//! manager in `devices/mod.rs`. Each platform is one [`Backend`]; a running
//! device's screen, once attached, is one [`Screen`].

use loft_protocol::{DeviceApp, DeviceInfo, DeviceInput, DevicePlatform, DeviceSetting};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::watch;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;

/// A picture of a screen, encoded once for every viewer.
#[derive(Debug)]
pub struct Picture {
    pub jpeg: Vec<u8>,
    /// The picture's size in pixels (it may be smaller than the screen).
    pub width: u32,
    pub height: u32,
}

/// One platform's devices. `native` is what follows the platform in a
/// [`loft_protocol::DeviceId`]: a simulator's UDID, an AVD's name.
pub trait Backend: Send + Sync + 'static {
    fn platform(&self) -> DevicePlatform;

    /// `Err(why)` when this computer can't run them (no Xcode, no SDK):
    /// words for the user, saying what to install.
    fn support(&self) -> BoxFut<'_, Result<(), String>>;

    /// Every device, stopped or running. `id`, `platform`, `name`, `os`,
    /// `state`, and the screen's size and scale when known; the manager
    /// fills in `error` and `agent`.
    fn list(&self) -> BoxFut<'_, Vec<DeviceInfo>>;

    /// Start a stopped device without a window of its own. Returns once it
    /// is running (booted far enough to show its screen and take input).
    fn boot<'a>(&'a self, native: &'a str) -> BoxFut<'a, ()>;

    fn shutdown<'a>(&'a self, native: &'a str) -> BoxFut<'a, ()>;

    /// Start taking pictures of a running device's screen and accept input
    /// for it. Dropping the [`Screen`] stops both and leaves the device
    /// running.
    fn attach<'a>(&'a self, native: &'a str) -> BoxFut<'a, Arc<dyn Screen>>;

    fn app<'a>(&'a self, native: &'a str, app: DeviceApp) -> BoxFut<'a, ()>;

    fn set<'a>(&'a self, native: &'a str, setting: DeviceSetting) -> BoxFut<'a, ()>;
}

/// A running device's screen and input.
pub trait Screen: Send + Sync + 'static {
    /// The newest picture; it changes as the screen does (only while
    /// [`Screen::watching`] is on, else it may stay as it was). A picture
    /// at most as large as the screen and at most about 30 a second.
    fn pictures(&self) -> watch::Receiver<Option<Arc<Picture>>>;

    /// Someone is looking: keep pictures coming. Off: the backend may stop
    /// taking them (input still works).
    fn watching(&self, on: bool);

    /// Touches, keys, text and buttons, points in fractions of the screen
    /// as the pictures show it.
    fn input(&self, input: DeviceInput) -> BoxFut<'_, ()>;

    /// A PNG of the screen at its full size, taken now.
    fn capture(&self) -> BoxFut<'_, Vec<u8>>;

    /// The screen's size in pixels as it is turned now, and pixels per
    /// point; `None` until known.
    fn size(&self) -> Option<(u32, u32, f32)>;

    /// Set when the device went away or the stream broke: the manager drops
    /// this screen and says why.
    fn ended(&self) -> watch::Receiver<Option<String>>;
}
