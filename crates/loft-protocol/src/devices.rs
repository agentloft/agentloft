//! iOS simulators and Android emulators on the daemon's computer (ADR-0020).
//! The daemon lists them, boots them headless and streams their screens to
//! whoever is looking, as it does the pages of its browser (ADR-0017): a
//! viewer subscribes to [`crate::Topic::Device`], gets pictures of the
//! screen ([`DeviceFrame`], acknowledged with
//! [`crate::ClientMsg::PageFrameAck`] like a page's) and the device's state
//! ([`DeviceInfo`]), and sends touches, keys and buttons back
//! ([`DeviceInput`]). Agents use the same devices through `device_*` tools.

use crate::ids::ThreadId;
use crate::workbench::Bytes;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A device, as the daemon names it: `ios:<simulator UDID>` or
/// `android:<AVD name>`. Stable across boots, so a window can remember
/// which device it showed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub String);

impl DeviceId {
    pub fn ios(udid: &str) -> Self {
        Self(format!("ios:{udid}"))
    }

    pub fn android(avd: &str) -> Self {
        Self(format!("android:{avd}"))
    }

    /// A physical Android phone, by its adb serial: `android:@<serial>`
    /// (an AVD's name never has an `@`).
    pub fn android_phone(serial: &str) -> Self {
        Self(format!("android:@{serial}"))
    }

    /// The adb serial of a physical Android phone.
    pub fn phone_serial(&self) -> Option<&str> {
        self.0.strip_prefix("android:@")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn platform(&self) -> Option<DevicePlatform> {
        match self.0.split_once(':')?.0 {
            "ios" => Some(DevicePlatform::Ios),
            "android" => Some(DevicePlatform::Android),
            _ => None,
        }
    }

    /// What follows the platform: the simulator's UDID or the AVD's name.
    pub fn native(&self) -> &str {
        self.0.split_once(':').map_or(&self.0, |(_, rest)| rest)
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DevicePlatform {
    Ios,
    Android,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    Stopped,
    /// Starting; its screen may not show anything yet.
    Booting,
    Running,
    ShuttingDown,
}

/// One simulator or emulator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub platform: DevicePlatform,
    /// As the user named it: "iPhone 17 Pro", "Pixel 8".
    pub name: String,
    /// The system it runs: "iOS 26.1", "Android 16".
    pub os: String,
    pub state: DeviceState,
    /// The screen in pixels, as it is turned now (0 until known).
    pub width: u32,
    pub height: u32,
    /// Pixels per point (iOS) or per density-independent pixel (Android).
    pub scale: f32,
    /// Why it couldn't boot or stream, when it couldn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The thread whose agent used it last, if one has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<ThreadId>,
    /// A real phone plugged in (or paired over Wi-Fi), not an emulator. It
    /// can't be booted or shut down from here.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub physical: bool,
    /// A device that folds: how, and how it is folded now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fold: Option<DeviceFold>,
}

/// How a foldable device folds, and its posture now (ADR-0020 §5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceFold {
    /// Where the hinge runs: `vertical` folds like a book (Pixel Fold),
    /// `horizontal` like a flip phone.
    pub axis: FoldAxis,
    /// The main screen is inside when folded shut (fold-in), not outside.
    pub inward: bool,
    /// It has a cover screen, used while folded shut.
    pub cover_screen: bool,
    /// The postures it can be put in.
    pub postures: Vec<DevicePosture>,
    pub posture: DevicePosture,
    /// The hinge's angle in degrees: 0 folded shut, 180 flat (up to 360
    /// for a fold-out).
    pub hinge: f32,
    pub hinge_min: f32,
    pub hinge_max: f32,
    /// The pictures show the cover screen now (the device is folded shut),
    /// not the main one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_cover: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FoldAxis {
    Vertical,
    Horizontal,
}

/// A foldable's posture, as Android names them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DevicePosture {
    /// Folded shut.
    Closed,
    /// Half open: a laptop on a table (tabletop) or an open book.
    HalfOpened,
    /// Open flat.
    Opened,
    /// Folded back on itself, screen outward (fold-out).
    Flipped,
    /// Stood on its edges like a tent (fold-out).
    Tent,
}

/// One finger of several (`DeviceInput::Touches`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct TouchPoint {
    pub x: f32,
    pub y: f32,
}

/// What the daemon can run, and why not when it can't, for a panel with no
/// devices to say what to install.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeviceSupport {
    /// Xcode's simulators are usable (macOS with Xcode and a runtime).
    pub ios: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ios_problem: Option<String>,
    /// The Android SDK's emulator and adb were found.
    pub android: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android_problem: Option<String>,
}

/// A picture of a device's screen. Its size is the picture's own (it may be
/// smaller than the screen); input is in fractions of it, not pixels.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceFrame {
    /// Counts up per device; a viewer acknowledges it to get the next one.
    pub seq: u64,
    pub jpeg: Bytes,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// The device's own buttons, and turning it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceButton {
    Home,
    /// Android only.
    Back,
    /// The app switcher (Android's Recents; a double Home on iOS).
    Recents,
    /// The side (power/lock) button.
    Power,
    VolumeUp,
    VolumeDown,
    RotateLeft,
    RotateRight,
}

/// Input for a device. Points are fractions of the screen as its pictures
/// show it (0,0 top left, 1,1 bottom right), whichever way it is turned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceInput {
    /// One finger. A press is `down`, any `move`s, then `up`.
    Touch {
        phase: TouchPhase,
        x: f32,
        y: f32,
    },
    /// A wheel or trackpad over the screen: the content moves by `dx`, `dy`
    /// (fractions of the screen; positive scrolls down/right). A device
    /// without a wheel gets a short drag.
    Scroll {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
    },
    /// Text typed into whatever has the device's focus.
    Text {
        text: String,
    },
    /// A key without text, named as a DOM `KeyboardEvent.key`: "Enter",
    /// "Backspace", "Tab", "Escape", "ArrowLeft"…
    Key {
        key: String,
        #[serde(default)]
        modifiers: u8,
    },
    Button {
        button: DeviceButton,
    },
    /// Several fingers at once (a pinch, a two-finger turn): they go down,
    /// move and lift together, `touches` in the same order each time.
    Touches {
        phase: TouchPhase,
        touches: Vec<TouchPoint>,
    },
}

/// Apps on a device (for agents, and for a project's run actions).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum DeviceApp {
    /// Install a built app: a `.app` folder (iOS) or an `.apk` (Android),
    /// a path on the daemon's computer.
    Install {
        path: String,
    },
    /// Start an app by bundle id (iOS) or package name (Android).
    Launch {
        app: String,
    },
    Terminate {
        app: String,
    },
    Uninstall {
        app: String,
    },
    /// Open an address (a web page or an app's own link) on the device.
    OpenUrl {
        url: String,
    },
}

/// Light or dark appearance, and the like, for the panel's tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "set", rename_all = "snake_case")]
pub enum DeviceSetting {
    Appearance {
        dark: bool,
    },
    /// A fake location (degrees).
    Location {
        latitude: f64,
        longitude: f64,
    },
    /// A foldable's posture (its hinge goes where the posture puts it).
    Posture {
        posture: DevicePosture,
    },
    /// A foldable's hinge, in degrees.
    Hinge {
        degrees: f32,
    },
    TextSize {
        size: DeviceTextSize,
    },
    /// Higher contrast (iOS Increase Contrast, Android high-contrast text).
    Contrast {
        high: bool,
    },
    ReduceMotion {
        on: bool,
    },
    /// VoiceOver (iOS) or TalkBack (Android).
    ScreenReader {
        on: bool,
    },
    /// Outline what accessibility sees: the frame of every element it
    /// knows, drawn over the screen (Android's layout bounds; iOS: the
    /// accessibility inspector's frames where available).
    AccessibilityFrames {
        on: bool,
    },
    /// Grant or take back one of an app's permissions. `permission` is the
    /// platform's own name: iOS `simctl privacy` services (`photos`,
    /// `camera`, `location`, `contacts`, `microphone`, `all`…) or Android
    /// permissions (`android.permission.CAMERA`, or just `CAMERA`).
    Permission {
        app: String,
        permission: String,
        grant: bool,
    },
    /// A test push notification for `app` (an APNs payload on iOS; on
    /// Android the payload's `aps.alert` title and body are posted as a
    /// notification).
    Push {
        app: String,
        payload: serde_json::Value,
    },
    /// The network on or off (Android: Wi-Fi and mobile data).
    Network {
        on: bool,
    },
    /// The interface's direction: what the rotate buttons turn to, set
    /// directly.
    Orientation {
        orientation: DeviceOrientation,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceTextSize {
    ExtraSmall,
    Small,
    /// The system's own default.
    Default,
    Large,
    ExtraLarge,
    /// The accessibility sizes' largest.
    Largest,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceOrientation {
    Portrait,
    LandscapeLeft,
    LandscapeRight,
    PortraitUpsideDown,
}

/// What the device's settings are now, read back from it after each change
/// (`Request::DeviceSettings`). `None`: not known, or not something this
/// device has. `supported` names the [`DeviceSetting`]s it takes
/// (`"appearance"`, `"text_size"`…), so the panel shows only those.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeviceSettings {
    #[serde(default)]
    pub supported: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dark: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_size: Option<DeviceTextSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contrast: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce_motion: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_reader: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility_frames: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orientation: Option<DeviceOrientation>,
    /// The app in front: bundle id or package name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_app: Option<String>,
    /// Apps installed by the user (not the system's), for the permission
    /// and push tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
}
