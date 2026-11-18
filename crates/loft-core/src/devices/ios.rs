//! iOS and iPadOS simulators, through Xcode (ADR-0020).
//!
//! Listing, booting, apps and settings go through `xcrun simctl`. A running
//! simulator's screen and input go through Xcode's private CoreSimulator and
//! SimulatorKit frameworks, in a helper process (`agentloftd simulator`,
//! the `loft-sim` crate) so that a crash there can't take the daemon down:
//! the helper sends JPEG pictures of the screen and carries out touches,
//! keys and buttons ([`loft_sim::wire`]).
//!
//! Settings ([`Backend::set`], read back by [`Backend::settings`]):
//! appearance, text size and Increase Contrast through `simctl ui`;
//! Reduce Motion through the device's accessibility preferences
//! (`com.apple.Accessibility`'s `ReduceMotionEnabled`, then the Darwin
//! notifications its readers listen for); permissions through `simctl
//! privacy`; pushes through `simctl push`; the orientation through the
//! helper, as the rotate buttons; accessibility frames are outlined by the
//! helper in its pictures, from the device's accessibility tree
//! (`loft-sim`'s `helper/ax.rs`). The app in front is the one RunningBoard
//! gave the focal UI role (simulator apps are this Mac's processes, so their
//! role reads with `getpriority(PRIO_DARWIN_ROLE)`). What a simulator can't
//! do: VoiceOver (not in the simulator's runtime), turning its network off
//! (it is this Mac's).
//!
//! Folding (the iPhone Duo, Xcode 27.1 on): a device type with a second
//! built-in screen folds like a book, its main screen (screen ID 1) being
//! the cover. It is folded by SpringBoard's own display tool service, from
//! a small program run inside the device (`ios_fold.m`, compiled here with
//! this Mac's Xcode the first time it's needed); the helper notices which
//! screen is in use (the other is black) and shows that one.

use super::{Backend, BoxFut, Picture, Screen};
use anyhow::{Context, Result, anyhow, bail};
use loft_protocol::{
    DeviceApp, DeviceFold, DeviceId, DeviceInfo, DeviceInput, DeviceOrientation, DevicePlatform,
    DevicePosture, DeviceSetting, DeviceSettings, DeviceState, DeviceTextSize, FoldAxis,
};
use loft_sim::wire::{self, Command, Event, Request};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

/// How long a good `support()` answer is kept.
const SUPPORT_KEPT: Duration = Duration::from_secs(60);
/// How long a simulator may take to boot.
const BOOT_TIME: Duration = Duration::from_secs(300);
/// How long the helper may take to attach.
const ATTACH_TIME: Duration = Duration::from_secs(20);

/// How to start the simulator helper: `agentloftd simulator` for the
/// daemon.
#[derive(Debug, Clone)]
pub struct Helper {
    pub exe: PathBuf,
    /// What comes before the helper's own arguments.
    pub args: Vec<String>,
}

impl Helper {
    /// The daemon's own executable, `agentloftd`.
    pub fn agentloftd(exe: PathBuf) -> Self {
        Self {
            exe,
            args: vec!["simulator".into()],
        }
    }
}

pub struct IosBackend {
    helper: Helper,
    env: HashMap<String, String>,
    /// A device set other than the user's (tests).
    set: Option<PathBuf>,
    /// The Xcode in use, once support was confirmed, and when.
    supported: Mutex<Option<(PathBuf, Instant)>>,
    /// Screen sizes by device type (its profile, read once).
    screens: Mutex<HashMap<String, Option<ScreenOf>>>,
    /// The helpers attached now, by UDID, for settings that go through one
    /// (the orientation).
    attached: Mutex<HashMap<String, Weak<IosScreen>>>,
    /// The devices whose accessibility frames are to be outlined (a
    /// helper attached later outlines them too).
    outlined: Mutex<std::collections::HashSet<String>>,
    /// The foldable devices (by UDID), as last listed.
    /// The foldable devices (by UDID), with their inner screen's ID.
    foldables: Mutex<HashMap<String, u32>>,
    /// The hinge each foldable was last folded to, in degrees, while it runs.
    hinges: Mutex<HashMap<String, f32>>,
    /// The fold program, once compiled (see `ios_fold.m`).
    fold_tool: tokio::sync::Mutex<Option<PathBuf>>,
    /// Held while a device folds: SpringBoard drops a fold asked for while
    /// another is going on.
    folding: tokio::sync::Mutex<()>,
    /// The newest fold asked for, per device (a ticket from `fold_tickets`):
    /// a fold still waiting when a newer one arrives is dropped, so a drag
    /// on the hinge folds to where it ends, not through every step of it
    /// (each fold takes over a second).
    fold_newest: Mutex<HashMap<String, u64>>,
    /// The fold program kept running in each foldable (`fold --serve`):
    /// starting one takes over a second, a fold under half of one.
    fold_servers: tokio::sync::Mutex<HashMap<String, FoldServer>>,
    fold_tickets: std::sync::atomic::AtomicU64,
}

impl IosBackend {
    /// `env` is the environment `xcrun` runs in (the daemon's login
    /// environment: it may name another Xcode in `DEVELOPER_DIR`).
    pub fn new(helper: Helper, env: HashMap<String, String>) -> Self {
        Self {
            helper,
            env,
            set: None,
            supported: Mutex::new(None),
            screens: Mutex::default(),
            attached: Mutex::default(),
            outlined: Mutex::default(),
            foldables: Mutex::default(),
            hinges: Mutex::default(),
            fold_tool: tokio::sync::Mutex::new(None),
            folding: tokio::sync::Mutex::new(()),
            fold_newest: Mutex::default(),
            fold_servers: tokio::sync::Mutex::default(),
            fold_tickets: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Use the simulators in `set` (a folder `simctl --set` takes) instead
    /// of the user's.
    pub fn with_device_set(mut self, set: PathBuf) -> Self {
        self.set = Some(set);
        self
    }

    fn xcrun(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("/usr/bin/xcrun");
        if !self.env.is_empty() {
            command.env_clear().envs(&self.env);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Run `simctl` with `args` and return its stdout.
    async fn simctl(&self, args: &[&str]) -> Result<String> {
        self.simctl_with(args, None).await
    }

    /// Run `simctl` with `args`, `input` on its stdin, and return its stdout.
    async fn simctl_with(&self, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        let mut command = self.xcrun();
        command.arg("simctl");
        if let Some(set) = &self.set {
            command.arg("--set").arg(set);
        }
        command.args(args);
        let out = match input {
            None => command.output().await.context("couldn't run xcrun")?,
            Some(input) => {
                let mut child = command
                    .stdin(Stdio::piped())
                    .spawn()
                    .context("couldn't run xcrun")?;
                let mut stdin = child.stdin.take().context("no stdin")?;
                stdin.write_all(input).await?;
                drop(stdin);
                child.wait_with_output().await?
            }
        };
        if !out.status.success() {
            bail!("{}", simctl_error(&String::from_utf8_lossy(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The Xcode to use, or why there is none.
    async fn developer_dir(&self) -> std::result::Result<PathBuf, String> {
        if let Some(dir) = self.env.get("DEVELOPER_DIR").filter(|d| !d.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        let mut command = tokio::process::Command::new("/usr/bin/xcode-select");
        if !self.env.is_empty() {
            command.env_clear().envs(&self.env);
        }
        let out = command
            .arg("-p")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|_| NO_XCODE.to_string())?;
        if !out.status.success() {
            return Err(NO_XCODE.into());
        }
        Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
    }

    async fn check_support(&self) -> std::result::Result<PathBuf, String> {
        if !cfg!(target_os = "macos") {
            return Err("iOS simulators need a Mac with Xcode.".into());
        }
        if let Some((dir, at)) = self.supported.lock().unwrap().clone()
            && at.elapsed() < SUPPORT_KEPT
        {
            return Ok(dir);
        }
        let dir = self.developer_dir().await?;
        if dir.to_string_lossy().contains("CommandLineTools") {
            return Err(ONLY_TOOLS.into());
        }
        let runtimes = self
            .simctl(&["list", "-j", "runtimes"])
            .await
            .map_err(|e| format!("Xcode's simulators aren't ready ({e:#}). Open Xcode once to finish setting it up."))?;
        if !has_ios_runtime(&runtimes).map_err(|e| format!("{e:#}"))? {
            return Err(NO_RUNTIME.into());
        }
        *self.supported.lock().unwrap() = Some((dir.clone(), Instant::now()));
        Ok(dir)
    }

    /// The screen size of each device type in `list`, read from its profile
    /// the first time it's seen.
    async fn screen_sizes(&self, list: &SimctlList) -> HashMap<String, ScreenOf> {
        let mut sizes = HashMap::new();
        for device_type in &list.devicetypes {
            let known = self
                .screens
                .lock()
                .unwrap()
                .get(&device_type.identifier)
                .cloned();
            let size = match known {
                Some(size) => size,
                None => {
                    let size = self.read_screen(&device_type.bundle_path).await;
                    self.screens
                        .lock()
                        .unwrap()
                        .insert(device_type.identifier.clone(), size);
                    size
                }
            };
            if let Some(size) = size {
                sizes.insert(device_type.identifier.clone(), size);
            }
        }
        sizes
    }

    /// The main screen's size from the device type's bundle: its
    /// `profile.plist` up to Xcode 26, its `capabilities.plist` from
    /// Xcode 27 (whose profiles no longer have it); and a foldable's inner
    /// screen, from the displays its capabilities list.
    async fn read_screen(&self, bundle: &str) -> Option<ScreenOf> {
        if bundle.is_empty() {
            return None;
        }
        let resources = Path::new(bundle).join("Contents/Resources");
        let capabilities = plist_file_json(&resources.join("capabilities.plist")).await;
        let main = match plist_file_json(&resources.join("profile.plist"))
            .await
            .and_then(|json| screen_of_profile(&json))
        {
            Some(size) => size,
            None => capabilities.as_deref().and_then(screen_of_capabilities)?,
        };
        Some(ScreenOf {
            width: main.0,
            height: main.1,
            scale: main.2,
            inner: capabilities.as_deref().and_then(inner_screen),
        })
    }
}

/// The program that folds a foldable simulator, run inside it.
const FOLD_SOURCE: &str = include_str!("ios_fold.m");
/// The entitlement SpringBoard's display tool service asks of its clients.
const FOLD_ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.springboard.sbdisplay.service</key><true/></dict></plist>
"#;

/// FNV-1a: a hash that stays the same from one build to the next.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A property list file as JSON.
async fn plist_file_json(path: &Path) -> Option<Vec<u8>> {
    let plist = tokio::fs::read(path).await.ok()?;
    plist_json(&plist).await.ok()
}

/// A device type's main screen: its width and height in pixels, and pixels
/// per point; and a foldable's inner screen, as it is shown (its width and
/// height in pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScreenOf {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    /// A foldable's inner screen: its screen ID, and its width and height.
    pub inner: Option<(u32, u32, u32)>,
}

const NO_XCODE: &str = "Xcode isn't installed. Install it from the App Store and open it once to finish setting it up.";
const ONLY_TOOLS: &str = "Only Xcode's Command Line Tools are selected, and iOS simulators need Xcode itself. Install Xcode, then run `sudo xcode-select -s /Applications/Xcode.app`.";
const NO_RUNTIME: &str = "No iOS simulator is installed. Add one in Xcode ▸ Settings ▸ Components, or run `xcodebuild -downloadPlatform iOS`.";

/// simctl's error output, without its boilerplate first line.
fn simctl_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty() && !l.starts_with("An error was encountered processing the command")
        })
        .collect();
    if lines.is_empty() {
        "simctl failed".into()
    } else {
        lines.join(" ")
    }
}

/// simctl refused because the device is already as asked (booted when
/// booting, shut down when shutting down).
fn already(error: &anyhow::Error, state: &str) -> bool {
    let text = format!("{error:#}");
    text.contains("current state") && text.contains(state)
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct SimctlList {
    #[serde(default)]
    devicetypes: Vec<SimctlDeviceType>,
    #[serde(default)]
    runtimes: Vec<SimctlRuntime>,
    #[serde(default)]
    devices: BTreeMap<String, Vec<SimctlDevice>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimctlDeviceType {
    identifier: String,
    #[serde(default)]
    product_family: String,
    #[serde(default)]
    bundle_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimctlRuntime {
    identifier: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    is_available: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimctlDevice {
    udid: String,
    name: String,
    state: String,
    #[serde(default)]
    is_available: bool,
    #[serde(default)]
    device_type_identifier: String,
}

impl SimctlRuntime {
    fn is_ios(&self) -> bool {
        self.is_available
            && (self.platform == "iOS"
                || (self.platform.is_empty() && self.name.starts_with("iOS")))
    }
}

fn has_ios_runtime(json: &str) -> Result<bool> {
    let list: SimctlList = serde_json::from_str(json).context("simctl's list isn't JSON")?;
    Ok(list.runtimes.iter().any(SimctlRuntime::is_ios))
}

/// `(width, height, scale)` from a device type's `profile.plist` as JSON.
fn screen_of_profile(json: &[u8]) -> Option<(u32, u32, f32)> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Profile {
        main_screen_width: u32,
        main_screen_height: u32,
        main_screen_scale: f32,
    }
    let p: Profile = serde_json::from_slice(json).ok()?;
    Some((
        p.main_screen_width,
        p.main_screen_height,
        p.main_screen_scale,
    ))
}

/// `(width, height, scale)` from a device type's `capabilities.plist` as
/// JSON (Xcode 27).
fn screen_of_capabilities(json: &[u8]) -> Option<(u32, u32, f32)> {
    #[derive(Deserialize)]
    struct File {
        capabilities: Capabilities,
    }
    #[derive(Deserialize)]
    struct Capabilities {
        #[serde(rename = "ScreenDimensionsCapability")]
        screen: Screen,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "kebab-case")]
    struct Screen {
        main_screen_width: u32,
        main_screen_height: u32,
        main_screen_scale: f32,
    }
    let f: File = serde_json::from_slice(json).ok()?;
    let s = f.capabilities.screen;
    Some((
        s.main_screen_width,
        s.main_screen_height,
        s.main_screen_scale,
    ))
}

/// A foldable's inner screen from a device type's `capabilities.plist` as
/// JSON: a second built-in display (screen ID other than 1), as it is shown
/// (turned by its `nativeRotation`).
fn inner_screen(json: &[u8]) -> Option<(u32, u32, u32)> {
    #[derive(Deserialize)]
    struct File {
        capabilities: Capabilities,
    }
    #[derive(Deserialize)]
    struct Capabilities {
        #[serde(default)]
        displays: Vec<Display>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Display {
        #[serde(default)]
        display_type: String,
        #[serde(rename = "screenID", default)]
        screen_id: u32,
        #[serde(default)]
        width: u32,
        #[serde(default)]
        height: u32,
        #[serde(default)]
        native_rotation: u32,
    }
    let f: File = serde_json::from_slice(json).ok()?;
    let d = f
        .capabilities
        .displays
        .into_iter()
        .find(|d| d.display_type == "integrated" && d.screen_id != 1 && d.width > 0)?;
    Some(if d.native_rotation % 180 == 90 {
        (d.screen_id, d.height, d.width)
    } else {
        (d.screen_id, d.width, d.height)
    })
}

/// How a running foldable is folded: its hinge, and whether it shows its
/// cover screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FoldNow {
    pub hinge: f32,
    pub on_cover: bool,
}

impl FoldNow {
    /// As it boots: closed.
    const CLOSED: Self = Self {
        hinge: 0.0,
        on_cover: true,
    };

    /// As the protocol says it, the inner (main) screen being `main` and
    /// the cover `cover` (pixels, upright).
    fn fold(self, main: Option<(u32, u32)>, cover: Option<(u32, u32)>) -> DeviceFold {
        let posture = match self.hinge {
            h if h <= 0.0 => DevicePosture::Closed,
            h if h >= 170.0 => DevicePosture::Opened,
            _ => DevicePosture::HalfOpened,
        };
        DeviceFold {
            axis: FoldAxis::Vertical,
            inward: true,
            cover_screen: true,
            postures: vec![
                DevicePosture::Closed,
                DevicePosture::HalfOpened,
                DevicePosture::Opened,
            ],
            posture,
            hinge: self.hinge,
            hinge_min: 0.0,
            hinge_max: 180.0,
            on_cover: self.on_cover,
            main_size: main,
            cover_size: cover,
        }
    }
}

/// The hinge of SpringBoard's "book" position (about 100 degrees).
const BOOK_HINGE: f32 = 100.0;

/// The fold program's hinge position for a hinge in degrees: 0 closed, 1
/// open flat.
fn fold_position(degrees: f32) -> String {
    format!("{:.4}", (degrees / 180.0).clamp(0.0, 1.0))
}

fn device_state(state: &str) -> DeviceState {
    match state {
        "Booted" => DeviceState::Running,
        "Booting" => DeviceState::Booting,
        "Shutting Down" => DeviceState::ShuttingDown,
        _ => DeviceState::Stopped,
    }
}

/// The iPhones and iPads in `simctl list -j`, with their screens' sizes
/// from `sizes` (by device type); a running foldable folded as `fold_of`
/// says (by UDID), else closed.
pub(crate) fn devices_of(
    list: &SimctlList,
    sizes: &HashMap<String, ScreenOf>,
    fold_of: &dyn Fn(&str) -> Option<FoldNow>,
) -> Vec<DeviceInfo> {
    let families: HashMap<&str, &str> = list
        .devicetypes
        .iter()
        .map(|t| (t.identifier.as_str(), t.product_family.as_str()))
        .collect();
    let mut out = Vec::new();
    for (runtime_id, devices) in &list.devices {
        let Some(runtime) = list.runtimes.iter().find(|r| &r.identifier == runtime_id) else {
            continue;
        };
        if !runtime.is_ios() {
            continue;
        }
        for device in devices.iter().filter(|d| d.is_available) {
            let family = families
                .get(device.device_type_identifier.as_str())
                .copied()
                .unwrap_or_default();
            if family != "iPhone" && family != "iPad" {
                continue;
            }
            let os = if family == "iPad" && !runtime.version.is_empty() {
                format!("iPadOS {}", runtime.version)
            } else {
                runtime.name.clone()
            };
            let screen = sizes.get(&device.device_type_identifier).copied();
            let (mut width, mut height, scale) =
                screen.map_or((0, 0, 0.0), |s| (s.width, s.height, s.scale));
            let state = device_state(&device.state);
            // A foldable shows the size of the screen it's using.
            let cover = screen.map(|s| (s.width, s.height));
            let fold = screen.and_then(|s| s.inner).map(|(_, iw, ih)| {
                let inner = (iw, ih);
                let now = Some(&state)
                    .filter(|s| **s == DeviceState::Running)
                    .and_then(|_| fold_of(&device.udid))
                    .unwrap_or(FoldNow::CLOSED);
                if !now.on_cover {
                    (width, height) = inner;
                }
                now.fold(Some(inner), cover)
            });
            out.push(DeviceInfo {
                id: DeviceId::ios(&device.udid),
                platform: DevicePlatform::Ios,
                name: device.name.clone(),
                os,
                state,
                width,
                height,
                scale,
                error: None,
                agent: None,
                physical: false,
                fold,
            });
        }
    }
    out
}

/// The settings an iOS simulator takes, as `DeviceSettings::supported`
/// names them.
const SETTINGS: &[&str] = &[
    "appearance",
    "location",
    "text_size",
    "contrast",
    "reduce_motion",
    "permission",
    "push",
    "orientation",
    "accessibility_frames",
];

/// The accessibility preferences' domain, and the Darwin notifications its
/// readers (UIKit's cache, Settings) take as "Reduce Motion changed".
const ACCESSIBILITY: &str = "com.apple.Accessibility";
const REDUCE_MOTION: &str = "ReduceMotionEnabled";
const REDUCE_MOTION_CHANGED: &[&str] = &[
    "com.apple.accessibility.reduce.motion.status",
    "com.apple.accessibility.cache.reduce.motion",
];

/// SpringBoard's bundle id: the home screen (and lock screen) in front.
pub(crate) const SPRINGBOARD: &str = "com.apple.springboard";

/// `simctl ui … content_size`'s category for a text size. iOS has seven
/// standard sizes and five larger accessibility ones; `Default` is its
/// own default, `large`.
fn content_size(size: DeviceTextSize) -> &'static str {
    match size {
        DeviceTextSize::ExtraSmall => "extra-small",
        DeviceTextSize::Small => "small",
        DeviceTextSize::Default => "large",
        DeviceTextSize::Large => "extra-large",
        DeviceTextSize::ExtraLarge => "extra-extra-extra-large",
        DeviceTextSize::Largest => "accessibility-extra-extra-extra-large",
    }
}

/// The text size nearest a content size category simctl printed (it
/// prints some capitalized: "Small" after `content_size small`).
fn text_size_of(category: &str) -> Option<DeviceTextSize> {
    Some(match category.trim().to_ascii_lowercase().as_str() {
        "extra-small" => DeviceTextSize::ExtraSmall,
        "small" | "medium" => DeviceTextSize::Small,
        "large" => DeviceTextSize::Default,
        "extra-large" => DeviceTextSize::Large,
        "extra-extra-large" | "extra-extra-extra-large" => DeviceTextSize::ExtraLarge,
        c if c.starts_with("accessibility-") => DeviceTextSize::Largest,
        _ => return None,
    })
}

/// `simctl ui … appearance` / `increase_contrast` as read back: `None` for
/// "unknown" or "unsupported".
fn ui_value(printed: &str, on: &str, off: &str) -> Option<bool> {
    match printed.trim() {
        v if v == on => Some(true),
        v if v == off => Some(false),
        _ => None,
    }
}

/// A boolean `defaults read` printed (`1`/`0`); a preference never set is
/// off.
fn defaults_bool(read: &Result<String>) -> Option<bool> {
    match read {
        Ok(text) => match text.trim() {
            "1" | "true" | "YES" => Some(true),
            "0" | "false" | "NO" => Some(false),
            _ => None,
        },
        // Never set: "… does not exist" up to iOS 26, "Could not find key
        // …" from iOS 27.
        Err(e)
            if {
                let e = format!("{e:#}");
                e.contains("does not exist") || e.contains("Could not find key")
            } =>
        {
            Some(false)
        }
        Err(_) => None,
    }
}

/// The apps in `simctl listapps` (as JSON): every bundle id, and the user's
/// own (not the system's), each sorted.
fn apps_of(json: &[u8]) -> Result<(Vec<String>, Vec<String>)> {
    #[derive(Deserialize)]
    struct App {
        #[serde(rename = "ApplicationType", default)]
        kind: String,
    }
    let apps: BTreeMap<String, App> =
        serde_json::from_slice(json).context("simctl's app list isn't JSON")?;
    let user = apps
        .iter()
        .filter(|(_, app)| app.kind == "User")
        .map(|(id, _)| id.clone())
        .collect();
    Ok((apps.into_keys().collect(), user))
}

/// The running apps in the device's `launchctl list` (pid, status, label per
/// line): their pids and bundle ids, and SpringBoard's pid.
fn running_apps(list: &str) -> (Vec<(i32, String)>, Option<i32>) {
    let mut apps = Vec::new();
    let mut springboard = None;
    for line in list.lines() {
        let mut fields = line.split('\t');
        let (Some(pid), Some(_), Some(label)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(pid) = pid.trim().parse::<i32>() else {
            continue;
        };
        if label == "com.apple.SpringBoard" {
            springboard = Some(pid);
        } else if let Some(rest) = label.strip_prefix("UIKitApplication:") {
            let bundle = rest.split('[').next().unwrap_or(rest);
            apps.push((pid, bundle.to_string()));
        }
    }
    (apps, springboard)
}

/// The app RunningBoard put in front: of the running apps (those in
/// `known`, leaving out extensions such as widget renderers, which get the
/// same role), the one whose process has the focal UI role; else
/// SpringBoard's home or lock screen when it has it.
fn foreground(
    running: &[(i32, String)],
    springboard: Option<i32>,
    known: &[String],
    focal: impl Fn(i32) -> bool,
) -> Option<String> {
    running
        .iter()
        .filter(|(_, bundle)| known.binary_search(bundle).is_ok())
        .find(|(pid, _)| focal(*pid))
        .map(|(_, bundle)| bundle.clone())
        .or_else(|| {
            springboard
                .filter(|pid| focal(*pid))
                .map(|_| SPRINGBOARD.to_string())
        })
}

/// Whether a process on this Mac has the focal UI role (the app in front).
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn is_focal(pid: i32) -> bool {
    /// `<sys/resource.h>`: `PRIO_DARWIN_ROLE` and `PRIO_DARWIN_ROLE_UI_FOCAL`.
    const PRIO_DARWIN_ROLE: libc::c_int = 6;
    const ROLE_UI_FOCAL: libc::c_int = 1;
    // SAFETY: getpriority reads a process's role; it takes any pid and
    // answers -1 for one it can't see.
    let role = unsafe { libc::getpriority(PRIO_DARWIN_ROLE, pid as libc::id_t) };
    role == ROLE_UI_FOCAL
}

#[cfg(not(target_os = "macos"))]
fn is_focal(_pid: i32) -> bool {
    false
}

/// Turn an old-style or binary property list (what `simctl listapps`
/// prints) into JSON.
async fn plist_json(plist: &[u8]) -> Result<Vec<u8>> {
    let mut child = tokio::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("couldn't run plutil")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    stdin.write_all(plist).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("plutil: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

impl IosBackend {
    /// A foldable's inner screen ID (as last listed).
    fn inner_screen_of(&self, udid: &str) -> Option<u32> {
        self.foldables.lock().unwrap().get(udid).copied()
    }

    /// The device's attached screen, or a helper of its own for as long as
    /// `f` needs it.
    async fn helper_for(&self, udid: &str) -> Result<Arc<IosScreen>> {
        if let Some(screen) = self.attached(udid) {
            return Ok(screen);
        }
        let developer_dir = self.check_support().await.map_err(|why| anyhow!(why))?;
        Ok(Arc::new(
            IosScreen::start(
                &self.helper,
                udid,
                &developer_dir,
                self.set.as_deref(),
                self.inner_screen_of(udid),
            )
            .await?,
        ))
    }

    async fn set_reduce_motion(&self, udid: &str, on: bool) -> Result<()> {
        let value = if on { "true" } else { "false" };
        self.simctl(&[
            "spawn",
            udid,
            "defaults",
            "write",
            ACCESSIBILITY,
            REDUCE_MOTION,
            "-bool",
            value,
        ])
        .await?;
        for name in REDUCE_MOTION_CHANGED {
            self.simctl(&["spawn", udid, "notifyutil", "-p", name])
                .await?;
        }
        Ok(())
    }

    /// Every installed app's bundle id, and the user's own apps.
    async fn apps(&self, udid: &str) -> Result<(Vec<String>, Vec<String>)> {
        let mut command = self.xcrun();
        command.arg("simctl");
        if let Some(set) = &self.set {
            command.arg("--set").arg(set);
        }
        let out = command
            .args(["listapps", udid])
            .output()
            .await
            .context("couldn't run xcrun")?;
        if !out.status.success() {
            bail!("{}", simctl_error(&String::from_utf8_lossy(&out.stderr)));
        }
        apps_of(&plist_json(&out.stdout).await?)
    }

    async fn foreground_app(&self, udid: &str, known: &[String]) -> Result<Option<String>> {
        let list = self.simctl(&["spawn", udid, "launchctl", "list"]).await?;
        let (running, springboard) = running_apps(&list);
        Ok(foreground(&running, springboard, known, is_focal))
    }

    fn attached(&self, udid: &str) -> Option<Arc<IosScreen>> {
        self.attached
            .lock()
            .unwrap()
            .get(udid)
            .and_then(Weak::upgrade)
            .filter(|s| s.ended.borrow().is_none())
    }

    /// How a running foldable is folded: the hinge it was last folded to,
    /// and the screen its attached helper shows (SpringBoard swaps screens
    /// at a different angle opening than closing, so a half-open device
    /// may show either).
    fn fold_now(&self, udid: &str) -> FoldNow {
        let hinge = self.hinges.lock().unwrap().get(udid).copied();
        let shown = self
            .attached(udid)
            .and_then(|s| *s.screen_id.lock().unwrap())
            .map(|id| id == 1);
        match (hinge, shown) {
            (Some(hinge), Some(on_cover)) => FoldNow { hinge, on_cover },
            (Some(hinge), None) => FoldNow {
                hinge,
                on_cover: hinge <= 0.0,
            },
            // Not folded here since it booted: closed, unless it shows the
            // inner screen.
            (None, Some(false)) => FoldNow {
                hinge: 180.0,
                on_cover: false,
            },
            (None, _) => FoldNow::CLOSED,
        }
    }

    /// Fold `udid` to `degrees` (0 closed, 180 open), then wait for its
    /// helper to show the screen that goes with it.
    async fn fold(&self, udid: &str, degrees: f32) -> Result<()> {
        // Which devices fold is learned from the list: read it if it
        // wasn't yet (a fold asked for first thing after the daemon starts).
        if !self.foldables.lock().unwrap().contains_key(udid) {
            Backend::list(self).await?;
        }
        if !self.foldables.lock().unwrap().contains_key(udid) {
            bail!("This simulator doesn't fold: only foldables (an iPhone Duo) do.");
        }
        let ticket = self
            .fold_tickets
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.fold_newest
            .lock()
            .unwrap()
            .insert(udid.to_string(), ticket);
        let tool = self.fold_tool().await?;
        let tool = tool.to_string_lossy();
        let _one_at_a_time = self.folding.lock().await;
        // A newer fold of this device came while this one waited: that one
        // goes where the hinge should end up.
        if self.fold_newest.lock().unwrap().get(udid) != Some(&ticket) {
            return Ok(());
        }
        // Where it is going, from now: what is said of the device while it
        // folds (its screen switching midway) must not take it back to
        // where it was.
        let before = self
            .hinges
            .lock()
            .unwrap()
            .insert(udid.to_string(), degrees);
        if let Err(e) = self.fold_to(udid, &tool, &fold_position(degrees)).await {
            let mut hinges = self.hinges.lock().unwrap();
            match before {
                Some(before) => hinges.insert(udid.to_string(), before),
                None => hinges.remove(udid),
            };
            return Err(e.context("SpringBoard didn't fold the device"));
        }
        // The helper notices within a quarter of a second; a hinge between
        // closed and open may not change screens, so wait only for those.
        let cover = match degrees {
            d if d <= 0.0 => Some(true),
            d if d >= BOOK_HINGE => Some(false),
            _ => None,
        };
        if let (Some(cover), Some(screen)) = (cover, self.attached(udid)) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if screen.screen_id.lock().unwrap().map(|id| id == 1) == Some(cover) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Ok(())
    }

    /// Fold `udid` to `position` (0 to 1) through its fold program, started
    /// if it isn't running (or stopped since); one started just for this
    /// fold if that fails.
    async fn fold_to(&self, udid: &str, tool: &str, position: &str) -> Result<()> {
        let mut servers = self.fold_servers.lock().await;
        if !servers.contains_key(udid) {
            match self.start_fold_server(udid, tool).await {
                Ok(server) => {
                    servers.insert(udid.to_string(), server);
                }
                Err(e) => {
                    tracing::debug!("no fold program kept running: {e:#}");
                    drop(servers);
                    return self
                        .simctl(&["spawn", udid, tool, position])
                        .await
                        .map(drop);
                }
            }
        }
        let server = servers.get_mut(udid).expect("just added");
        match server.fold(position).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // It stopped (the device rebooted, SpringBoard restarted):
                // the next fold starts another.
                servers.remove(udid);
                Err(e)
            }
        }
    }

    async fn start_fold_server(&self, udid: &str, tool: &str) -> Result<FoldServer> {
        let mut command = self.xcrun();
        command.arg("simctl");
        if let Some(set) = &self.set {
            command.arg("--set").arg(set);
        }
        command
            .args(["spawn", udid, tool, "--serve"])
            .stdin(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().context("couldn't run xcrun")?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        match tokio::time::timeout(Duration::from_secs(15), lines.next_line()).await {
            Ok(Ok(Some(line))) if line.trim() == "ready" => Ok(FoldServer {
                _child: child,
                stdin,
                lines,
            }),
            Ok(Ok(Some(line))) => bail!("the fold program said {line:?}"),
            Ok(_) => bail!("the fold program stopped"),
            Err(_) => bail!("the fold program didn't start"),
        }
    }

    /// The fold program (`ios_fold.m`), compiled with this Mac's Xcode for
    /// the simulator the first time it's needed, with its entitlement
    /// linked into its `__TEXT,__entitlements` section. Kept in the
    /// temporary folder, by the source, Xcode and architecture it was built
    /// from.
    async fn fold_tool(&self) -> Result<PathBuf> {
        let mut built = self.fold_tool.lock().await;
        if let Some(path) = built.as_ref().filter(|p| p.exists()) {
            return Ok(path.clone());
        }
        let developer_dir = self.check_support().await.map_err(|why| anyhow!(why))?;
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            other => other,
        };
        let key = fnv1a(
            [
                FOLD_SOURCE.as_bytes(),
                developer_dir.as_os_str().as_encoded_bytes(),
                arch.as_bytes(),
            ]
            .concat()
            .as_slice(),
        );
        let path = std::env::temp_dir().join(format!("agentloft-ios-fold-{key:016x}"));
        if !path.exists() {
            let dir = path.with_extension(format!("build-{}", std::process::id()));
            tokio::fs::create_dir_all(&dir)
                .await
                .context("no temporary folder for the fold program")?;
            let result = self.build_fold_tool(&dir, arch, &path).await;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            result?;
        }
        *built = Some(path.clone());
        Ok(path)
    }

    /// Compile and sign the fold program in `dir`, and move it to `path`.
    async fn build_fold_tool(&self, dir: &Path, arch: &str, path: &Path) -> Result<()> {
        {
            let source = dir.join("fold.m");
            let entitlements = dir.join("entitlements.plist");
            let out = dir.join("fold");
            tokio::fs::write(&source, FOLD_SOURCE).await?;
            tokio::fs::write(&entitlements, FOLD_ENTITLEMENTS).await?;
            let built = self
                .xcrun()
                .args(["--sdk", "iphonesimulator", "clang", "-arch", arch])
                .args(["-mios-simulator-version-min=27.1", "-fobjc-arc", "-O2"])
                .arg("-Wno-arc-performSelector-leaks")
                .args(["-framework", "Foundation"])
                .arg(&source)
                .arg("-o")
                .arg(&out)
                .args(["-Xlinker", "-sectcreate", "-Xlinker", "__TEXT"])
                .args(["-Xlinker", "__entitlements", "-Xlinker"])
                .arg(&entitlements)
                .output()
                .await
                .context("couldn't run Xcode's clang")?;
            if !built.status.success() {
                bail!(
                    "couldn't build the fold program: {}",
                    String::from_utf8_lossy(&built.stderr).trim()
                );
            }
            let signed = tokio::process::Command::new("/usr/bin/codesign")
                .args(["-f", "-s", "-"])
                .arg(&out)
                .stdin(Stdio::null())
                .output()
                .await
                .context("couldn't run codesign")?;
            if !signed.status.success() {
                bail!(
                    "couldn't sign the fold program: {}",
                    String::from_utf8_lossy(&signed.stderr).trim()
                );
            }
            tokio::fs::rename(&out, path)
                .await
                .context("couldn't keep the fold program")?;
        }
        Ok(())
    }

    /// The orientation of the device's interface, from its helper (one is
    /// started for it when none is attached).
    async fn orientation(&self, udid: &str) -> Result<Option<DeviceOrientation>> {
        let screen = self.helper_for(udid).await?;
        Ok(*screen.orientation.lock().unwrap())
    }
}

impl Backend for IosBackend {
    fn platform(&self) -> DevicePlatform {
        DevicePlatform::Ios
    }

    fn support(&self) -> BoxFut<'_, std::result::Result<(), String>> {
        Box::pin(async move { Ok(self.check_support().await.map(drop)) })
    }

    fn list(&self) -> BoxFut<'_, Vec<DeviceInfo>> {
        Box::pin(async move {
            let json = self.simctl(&["list", "-j"]).await?;
            let list: SimctlList =
                serde_json::from_str(&json).context("simctl's list isn't JSON")?;
            let sizes = self.screen_sizes(&list).await;
            let devices = devices_of(&list, &sizes, &|udid| Some(self.fold_now(udid)));
            {
                let mut foldables = self.foldables.lock().unwrap();
                foldables.clear();
                foldables.extend(list.devices.values().flatten().filter_map(|d| {
                    let (id, _, _) = sizes.get(&d.device_type_identifier)?.inner?;
                    Some((d.udid.clone(), id))
                }));
                // A hinge is remembered only while the device runs.
                let running: std::collections::HashSet<&str> = devices
                    .iter()
                    .filter(|d| d.state == DeviceState::Running)
                    .map(|d| d.id.native())
                    .collect();
                self.hinges
                    .lock()
                    .unwrap()
                    .retain(|udid, _| running.contains(udid.as_str()));
            }
            Ok(devices)
        })
    }

    fn boot<'a>(&'a self, native: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move {
            self.hinges.lock().unwrap().remove(native);
            self.fold_servers.lock().await.remove(native);
            // `simctl boot` starts the device without Simulator.app.
            if let Err(e) = self.simctl(&["boot", native]).await
                && !already(&e, "Booted")
            {
                return Err(e);
            }
            tokio::time::timeout(BOOT_TIME, self.simctl(&["bootstatus", native]))
                .await
                .map_err(|_| {
                    anyhow!(
                        "the simulator didn't finish booting in {} minutes",
                        BOOT_TIME.as_secs() / 60
                    )
                })??;
            Ok(())
        })
    }

    fn shutdown<'a>(&'a self, native: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move {
            self.fold_servers.lock().await.remove(native);
            match self.simctl(&["shutdown", native]).await {
                Err(e) if !already(&e, "Shutdown") => Err(e),
                _ => Ok(()),
            }
        })
    }

    fn attach<'a>(&'a self, native: &'a str) -> BoxFut<'a, Arc<dyn Screen>> {
        Box::pin(async move {
            let developer_dir = self.check_support().await.map_err(|why| anyhow!(why))?;
            let screen = Arc::new(
                IosScreen::start(
                    &self.helper,
                    native,
                    &developer_dir,
                    self.set.as_deref(),
                    self.inner_screen_of(native),
                )
                .await?,
            );
            {
                let mut attached = self.attached.lock().unwrap();
                attached.retain(|_, s| s.strong_count() > 0);
                attached.insert(native.to_string(), Arc::downgrade(&screen));
            }
            if self.outlined.lock().unwrap().contains(native) {
                let screen = Arc::downgrade(&screen);
                tokio::spawn(async move {
                    let Some(screen) = screen.upgrade() else {
                        return;
                    };
                    if let Err(e) = screen.outline(true).await {
                        tracing::warn!("accessibility frames: {e:#}");
                    }
                });
            }
            Ok(screen as Arc<dyn Screen>)
        })
    }

    fn app<'a>(&'a self, native: &'a str, app: DeviceApp) -> BoxFut<'a, ()> {
        Box::pin(async move {
            match &app {
                DeviceApp::Install { path } => self.simctl(&["install", native, path]).await,
                DeviceApp::Launch { app } => self.simctl(&["launch", native, app]).await,
                DeviceApp::Terminate { app } => self.simctl(&["terminate", native, app]).await,
                DeviceApp::Uninstall { app } => self.simctl(&["uninstall", native, app]).await,
                DeviceApp::OpenUrl { url } => self.simctl(&["openurl", native, url]).await,
            }
            .map(drop)
        })
    }

    fn set<'a>(&'a self, native: &'a str, setting: DeviceSetting) -> BoxFut<'a, ()> {
        Box::pin(async move {
            match setting {
                DeviceSetting::Appearance { dark } => {
                    let look = if dark { "dark" } else { "light" };
                    self.simctl(&["ui", native, "appearance", look]).await
                }
                DeviceSetting::Location {
                    latitude,
                    longitude,
                } => {
                    let at = format!("{latitude},{longitude}");
                    self.simctl(&["location", native, "set", &at]).await
                }
                DeviceSetting::TextSize { size } => {
                    self.simctl(&["ui", native, "content_size", content_size(size)])
                        .await
                }
                DeviceSetting::Contrast { high } => {
                    let to = if high { "enabled" } else { "disabled" };
                    self.simctl(&["ui", native, "increase_contrast", to]).await
                }
                DeviceSetting::ReduceMotion { on } => self
                    .set_reduce_motion(native, on)
                    .await
                    .map(|()| String::new()),
                DeviceSetting::Permission {
                    app,
                    permission,
                    grant,
                } => {
                    let action = if grant { "grant" } else { "revoke" };
                    self.simctl(&["privacy", native, action, &permission, &app])
                        .await
                }
                DeviceSetting::Push { app, payload } => {
                    let payload = serde_json::to_vec(&payload)?;
                    self.simctl_with(&["push", native, &app, "-"], Some(&payload))
                        .await
                }
                DeviceSetting::Orientation { orientation } => {
                    let screen = self.helper_for(native).await?;
                    screen.orient(orientation).await.map(|()| String::new())
                }
                DeviceSetting::ScreenReader { .. } => {
                    bail!("iOS simulators have no VoiceOver: it runs only on real devices.")
                }
                DeviceSetting::AccessibilityFrames { on } => {
                    if on {
                        self.outlined.lock().unwrap().insert(native.to_string());
                    } else {
                        self.outlined.lock().unwrap().remove(native);
                    }
                    // Drawn in the pictures of the attached screen; one
                    // attached later outlines them from the start.
                    match self.attached(native) {
                        Some(screen) => screen.outline(on).await.map(|()| String::new()),
                        None => Ok(String::new()),
                    }
                }
                DeviceSetting::Network { .. } => bail!(
                    "An iOS simulator uses this Mac's network: it can't be turned off on its own."
                ),
                DeviceSetting::Posture { posture } => {
                    let degrees = match posture {
                        DevicePosture::Closed => 0.0,
                        DevicePosture::HalfOpened => BOOK_HINGE,
                        DevicePosture::Opened => 180.0,
                        DevicePosture::Flipped | DevicePosture::Tent => {
                            bail!("This iPhone folds only inward.")
                        }
                    };
                    self.fold(native, degrees).await.map(|()| String::new())
                }
                DeviceSetting::Hinge { degrees } => self
                    .fold(native, degrees.clamp(0.0, 180.0))
                    .await
                    .map(|()| String::new()),
            }
            .map(drop)
        })
    }

    fn settings<'a>(&'a self, native: &'a str) -> BoxFut<'a, DeviceSettings> {
        Box::pin(async move {
            let appearance = ["ui", native, "appearance"];
            let size = ["ui", native, "content_size"];
            let contrast = ["ui", native, "increase_contrast"];
            let motion = [
                "spawn",
                native,
                "defaults",
                "read",
                ACCESSIBILITY,
                REDUCE_MOTION,
            ];
            let (appearance, size, contrast, motion, apps) = tokio::join!(
                self.simctl(&appearance),
                self.simctl(&size),
                self.simctl(&contrast),
                self.simctl(&motion),
                self.apps(native),
            );
            let mut supported: Vec<String> = SETTINGS.iter().map(|s| s.to_string()).collect();
            if self.foldables.lock().unwrap().contains_key(native) {
                supported.extend(["posture".into(), "hinge".into()]);
            }
            let mut settings = DeviceSettings {
                supported,
                dark: appearance.ok().and_then(|a| ui_value(&a, "dark", "light")),
                text_size: size.ok().and_then(|s| text_size_of(&s)),
                contrast: contrast
                    .ok()
                    .and_then(|c| ui_value(&c, "enabled", "disabled")),
                reduce_motion: defaults_bool(&motion),
                ..Default::default()
            };
            // The rest only of a running device.
            let Ok((known, user)) = apps else {
                return Ok(settings);
            };
            settings.apps = user;
            let (front, orientation) = tokio::join!(
                self.foreground_app(native, &known),
                self.orientation(native)
            );
            settings.foreground_app = front.unwrap_or_else(|e| {
                tracing::debug!("the simulator's app in front: {e:#}");
                None
            });
            settings.orientation = orientation.unwrap_or_else(|e| {
                tracing::debug!("the simulator's orientation: {e:#}");
                None
            });
            // The outlines are the helper's own drawing: what the attached
            // one draws, else what one would.
            settings.accessibility_frames = Some(match self.attached(native) {
                Some(screen) => *screen.outlining.lock().unwrap(),
                None => self.outlined.lock().unwrap().contains(native),
            });
            Ok(settings)
        })
    }
}

enum Reply {
    Done(Option<String>),
    Capture(Vec<u8>),
}

type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;

/// A running simulator's screen: its helper process.
struct IosScreen {
    pictures: watch::Sender<Option<Arc<Picture>>>,
    ended: watch::Sender<Option<String>>,
    size: Arc<Mutex<Option<(u32, u32, f32)>>>,
    /// Which way the interface faces now.
    orientation: Arc<Mutex<Option<DeviceOrientation>>>,
    /// A foldable's built-in screen shown now (1: the cover).
    screen_id: Arc<Mutex<Option<u32>>>,
    /// Accessibility frames are outlined in its pictures.
    outlining: Mutex<bool>,
    requests: mpsc::UnboundedSender<String>,
    waiting: Waiting,
    next_id: AtomicU64,
    stop: Mutex<Option<oneshot::Sender<()>>>,
}

impl Drop for IosScreen {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.lock().unwrap().take() {
            let _ = stop.send(());
        }
    }
}

impl IosScreen {
    /// `inner`: a foldable's inner screen ID, for the helper to follow
    /// besides the main screen (other built-in screen IDs are CarPlay's and
    /// TV out's, never the device's own).
    async fn start(
        helper: &Helper,
        udid: &str,
        developer_dir: &Path,
        set: Option<&Path>,
        inner: Option<u32>,
    ) -> Result<Self> {
        let mut command = tokio::process::Command::new(&helper.exe);
        command
            .args(&helper.args)
            .arg(udid)
            .arg("--developer-dir")
            .arg(developer_dir);
        if let Some(set) = set {
            command.arg("--set").arg(set);
        }
        if let Some(inner) = inner {
            command.arg("--screen").arg(inner.to_string());
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("couldn't start {}", helper.exe.display()))?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        let mut stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;

        let pictures = watch::channel(None).0;
        let ended = watch::channel(None).0;
        let size = Arc::new(Mutex::new(None));
        let orientation = Arc::new(Mutex::new(None));
        let screen_id = Arc::new(Mutex::new(None));
        let waiting: Waiting = Arc::default();
        let (requests, mut requests_rx) = mpsc::unbounded_channel::<String>();
        let (stop, mut stopped) = oneshot::channel::<()>();
        let (ready_tx, ready_rx) = oneshot::channel::<std::result::Result<(), String>>();

        // What the helper says on stderr goes to the log; its last lines
        // explain an ending.
        let last_words = Arc::new(Mutex::new(Vec::<String>::new()));
        {
            let last_words = last_words.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!("simulator helper: {line}");
                    let mut words = last_words.lock().unwrap();
                    words.push(line);
                    let n = words.len();
                    if n > 5 {
                        words.drain(..n - 5);
                    }
                }
            });
        }

        // Requests in, until the screen is dropped; then let the helper go.
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    line = requests_rx.recv() => {
                        let Some(line) = line else { break };
                        if stdin.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    _ = &mut stopped => break,
                }
            }
            drop(stdin);
            if tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        });

        // Pictures and answers out.
        {
            let (pictures, ended, size, orientation, screen_id, waiting) = (
                pictures.clone(),
                ended.clone(),
                size.clone(),
                orientation.clone(),
                screen_id.clone(),
                waiting.clone(),
            );
            tokio::spawn(async move {
                let mut ready = Some(ready_tx);
                let why = loop {
                    let message = match read_message(&mut stdout).await {
                        Ok(Some(m)) => m,
                        Ok(None) => break None,
                        Err(e) => break Some(format!("The simulator's stream broke: {e}")),
                    };
                    match message {
                        (wire::KIND_FRAME, body) => {
                            if let Some((width, height, screen, jpeg)) = wire::split_frame(&body) {
                                pictures.send_replace(Some(Arc::new(Picture {
                                    jpeg: jpeg.to_vec(),
                                    width,
                                    height,
                                    // Screen 1 is a foldable's cover.
                                    on_cover: (screen != 0).then_some(screen == 1),
                                })));
                            }
                        }
                        (wire::KIND_CAPTURE, body) => {
                            if let Some((id, png)) = wire::split_capture(&body)
                                && let Some(tx) = waiting.lock().unwrap().remove(&id)
                            {
                                let _ = tx.send(Reply::Capture(png.to_vec()));
                            }
                        }
                        (wire::KIND_EVENT, body) => match serde_json::from_slice::<Event>(&body) {
                            Ok(Event::Ready { screen }) => {
                                *size.lock().unwrap() =
                                    Some((screen.width, screen.height, screen.scale));
                                *orientation.lock().unwrap() = screen.orientation;
                                *screen_id.lock().unwrap() = screen.screen;
                                if let Some(ready) = ready.take() {
                                    let _ = ready.send(Ok(()));
                                }
                            }
                            Ok(Event::Size { screen }) => {
                                *size.lock().unwrap() =
                                    Some((screen.width, screen.height, screen.scale));
                                *orientation.lock().unwrap() = screen.orientation;
                                *screen_id.lock().unwrap() = screen.screen;
                            }
                            Ok(Event::Done { id, error }) => {
                                if let Some(tx) = waiting.lock().unwrap().remove(&id) {
                                    let _ = tx.send(Reply::Done(error));
                                }
                            }
                            Ok(Event::Ended { reason }) => break Some(reason),
                            Err(e) => tracing::warn!("simulator helper: {e}"),
                        },
                        _ => {}
                    }
                };
                // Give stderr a moment to say why.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let why = why.unwrap_or_else(|| {
                    let words = last_words.lock().unwrap().join(" ");
                    if words.is_empty() {
                        "The simulator's stream stopped.".into()
                    } else {
                        format!("The simulator's stream stopped: {words}")
                    }
                });
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(why.clone()));
                }
                ended.send_replace(Some(why));
                waiting.lock().unwrap().clear();
            });
        }

        let screen = Self {
            pictures,
            ended,
            size,
            orientation,
            screen_id,
            outlining: Mutex::new(false),
            requests,
            waiting,
            next_id: AtomicU64::new(1),
            stop: Mutex::new(Some(stop)),
        };
        match tokio::time::timeout(ATTACH_TIME, ready_rx).await {
            Ok(Ok(Ok(()))) => Ok(screen),
            Ok(Ok(Err(why))) => Err(anyhow!(why)),
            Ok(Err(_)) => Err(anyhow!("The simulator helper stopped.")),
            Err(_) => Err(anyhow!(
                "The simulator's screen didn't open in {}s.",
                ATTACH_TIME.as_secs()
            )),
        }
    }

    /// Send `command`; with `answer`, wait for its answer.
    async fn request(&self, command: Command, answer: Option<Duration>) -> Result<Reply> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut line = serde_json::to_string(&Request { id, command })?;
        line.push('\n');
        let rx = answer.map(|_| {
            let (tx, rx) = oneshot::channel();
            self.waiting.lock().unwrap().insert(id, tx);
            rx
        });
        if self.requests.send(line).is_err() {
            self.waiting.lock().unwrap().remove(&id);
            bail!("The simulator's screen was closed.");
        }
        let (Some(rx), Some(wait)) = (rx, answer) else {
            return Ok(Reply::Done(None));
        };
        let reply = tokio::time::timeout(wait, rx).await;
        self.waiting.lock().unwrap().remove(&id);
        match reply {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(anyhow!(
                "{}",
                self.ended
                    .borrow()
                    .clone()
                    .unwrap_or_else(|| "The simulator's screen was closed.".into())
            )),
            Err(_) => Err(anyhow!(
                "The simulator didn't answer in {}s.",
                wait.as_secs()
            )),
        }
    }
}

impl IosScreen {
    /// Turn the device to face `orientation`, as the rotate buttons do.
    async fn orient(&self, orientation: DeviceOrientation) -> Result<()> {
        match self
            .request(
                Command::Orientation { orientation },
                Some(Duration::from_secs(10)),
            )
            .await?
        {
            Reply::Done(None) | Reply::Capture(_) => Ok(()),
            Reply::Done(Some(error)) => Err(anyhow!(error)),
        }
    }
}

impl IosScreen {
    /// Outline accessibility frames in the pictures, or stop. Turning them
    /// on answers once the first outlines are read (seconds, the first
    /// time).
    async fn outline(&self, on: bool) -> Result<()> {
        match self
            .request(
                Command::AccessibilityFrames { on },
                Some(Duration::from_secs(30)),
            )
            .await?
        {
            Reply::Done(None) | Reply::Capture(_) => {
                *self.outlining.lock().unwrap() = on;
                Ok(())
            }
            Reply::Done(Some(error)) => {
                *self.outlining.lock().unwrap() = false;
                Err(anyhow!(error))
            }
        }
    }
}

/// One message from the helper (see [`wire`]).
async fn read_message(
    input: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let kind = match input.read_u8().await {
        Ok(kind) => kind,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = input.read_u32().await? as usize;
    if len > wire::MAX_MESSAGE {
        return Err(std::io::Error::other(format!("a message of {len} bytes")));
    }
    let mut body = vec![0; len];
    input.read_exact(&mut body).await?;
    Ok(Some((kind, body)))
}

impl Screen for IosScreen {
    fn pictures(&self) -> watch::Receiver<Option<Arc<Picture>>> {
        self.pictures.subscribe()
    }

    fn watching(&self, on: bool) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut line) = serde_json::to_string(&Request {
            id,
            command: Command::Watch { on },
        }) {
            line.push('\n');
            let _ = self.requests.send(line);
        }
    }

    fn input(&self, input: DeviceInput) -> BoxFut<'_, ()> {
        Box::pin(async move {
            let wait = match &input {
                DeviceInput::Text { text } => Duration::from_secs(10 + text.len() as u64 / 20),
                _ => Duration::from_secs(10),
            };
            match self.request(Command::Input { input }, Some(wait)).await? {
                Reply::Done(None) => Ok(()),
                Reply::Done(Some(error)) => Err(anyhow!(error)),
                Reply::Capture(_) => Ok(()),
            }
        })
    }

    fn capture(&self) -> BoxFut<'_, Vec<u8>> {
        Box::pin(async move {
            match self
                .request(Command::Capture, Some(Duration::from_secs(15)))
                .await?
            {
                Reply::Capture(png) => Ok(png),
                Reply::Done(error) => {
                    Err(anyhow!(error.unwrap_or_else(|| "No picture came.".into())))
                }
            }
        })
    }

    fn size(&self) -> Option<(u32, u32, f32)> {
        *self.size.lock().unwrap()
    }

    fn ended(&self) -> watch::Receiver<Option<String>> {
        self.ended.subscribe()
    }

    fn on_cover(&self) -> Option<bool> {
        // Screen 1 is a foldable's cover; a phone with one screen says none.
        self.screen_id.lock().unwrap().map(|id| id == 1)
    }
}

/// A fold program kept running in a foldable (`fold --serve`, see
/// `ios_fold.m`): a position a line in, "done" or "error <status>" out.
struct FoldServer {
    /// Ended with the server (`kill_on_drop`).
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

impl FoldServer {
    async fn fold(&mut self, position: &str) -> Result<()> {
        self.stdin
            .write_all(format!("{position}\n").as_bytes())
            .await?;
        self.stdin.flush().await?;
        let answer = tokio::time::timeout(Duration::from_secs(25), self.lines.next_line())
            .await
            .context("the fold program didn't answer")??
            .context("the fold program stopped")?;
        match answer.trim() {
            "done" => Ok(()),
            "error 4" => bail!("SpringBoard didn't finish folding"),
            other => bail!("the fold program said {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"{
      "devicetypes": [
        {"productFamily": "iPhone", "bundlePath": "/T/iPhone 17 Pro.simdevicetype", "identifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro", "name": "iPhone 17 Pro"},
        {"productFamily": "iPad", "bundlePath": "/T/iPad Air.simdevicetype", "identifier": "com.apple.CoreSimulator.SimDeviceType.iPad-Air", "name": "iPad Air"},
        {"productFamily": "Apple Watch", "bundlePath": "/T/Watch.simdevicetype", "identifier": "com.apple.CoreSimulator.SimDeviceType.Watch", "name": "Apple Watch"}
      ],
      "runtimes": [
        {"identifier": "com.apple.CoreSimulator.SimRuntime.iOS-26-5", "name": "iOS 26.5", "version": "26.5", "platform": "iOS", "isAvailable": true},
        {"identifier": "com.apple.CoreSimulator.SimRuntime.watchOS-26-0", "name": "watchOS 26.0", "version": "26.0", "platform": "watchOS", "isAvailable": true},
        {"identifier": "com.apple.CoreSimulator.SimRuntime.iOS-18-0", "name": "iOS 18.0", "version": "18.0", "platform": "iOS", "isAvailable": false}
      ],
      "devices": {
        "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
          {"udid": "A7839768-C016-443C-BF65-F12413DF649C", "isAvailable": true, "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro", "state": "Booted", "name": "iPhone 17 Pro"},
          {"udid": "B4920A06-F8C4-41F0-8793-1E5FD626EE9E", "isAvailable": true, "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPad-Air", "state": "Shutdown", "name": "iPad Air"},
          {"udid": "C0000000-0000-0000-0000-000000000000", "isAvailable": false, "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro", "state": "Shutdown", "name": "Broken"},
          {"udid": "D0000000-0000-0000-0000-000000000000", "isAvailable": true, "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro", "state": "Shutting Down", "name": "Leaving"},
          {"udid": "E0000000-0000-0000-0000-000000000000", "isAvailable": true, "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro", "state": "Booting", "name": "Starting"}
        ],
        "com.apple.CoreSimulator.SimRuntime.watchOS-26-0": [
          {"udid": "F0000000-0000-0000-0000-000000000000", "isAvailable": true, "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.Watch", "state": "Shutdown", "name": "Watch"}
        ],
        "com.apple.CoreSimulator.SimRuntime.iOS-18-0": [
          {"udid": "G0000000-0000-0000-0000-000000000000", "isAvailable": false, "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro", "state": "Shutdown", "name": "Old"}
        ]
      },
      "pairs": {}
    }"#;

    #[test]
    fn lists_iphones_and_ipads_only() {
        let list: SimctlList = serde_json::from_str(LIST).unwrap();
        let sizes = HashMap::from([(
            "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro".to_string(),
            ScreenOf {
                width: 1206,
                height: 2622,
                scale: 3.0,
                inner: None,
            },
        )]);
        let devices = devices_of(&list, &sizes, &|_| None);
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["iPhone 17 Pro", "iPad Air", "Leaving", "Starting"]);
        let phone = &devices[0];
        assert_eq!(
            phone.id,
            DeviceId::ios("A7839768-C016-443C-BF65-F12413DF649C")
        );
        assert_eq!(phone.os, "iOS 26.5");
        assert_eq!(phone.state, DeviceState::Running);
        assert_eq!((phone.width, phone.height, phone.scale), (1206, 2622, 3.0));
        let pad = &devices[1];
        assert_eq!(pad.os, "iPadOS 26.5");
        assert_eq!(pad.state, DeviceState::Stopped);
        assert_eq!((pad.width, pad.height), (0, 0));
        assert_eq!(devices[2].state, DeviceState::ShuttingDown);
        assert_eq!(devices[3].state, DeviceState::Booting);
    }

    #[test]
    fn a_foldable_shows_the_screen_it_uses() {
        let list: SimctlList = serde_json::from_str(LIST).unwrap();
        let duo = ScreenOf {
            width: 1398,
            height: 2034,
            scale: 3.0,
            inner: Some((3, 2853, 2007)),
        };
        let sizes = HashMap::from([(
            "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro".to_string(),
            duo,
        )]);
        // Not known to be folded: closed, on its cover.
        let devices = devices_of(&list, &sizes, &|_| None);
        let phone = &devices[0];
        let fold = phone.fold.as_ref().expect("it folds");
        assert_eq!(fold.posture, DevicePosture::Closed);
        assert!(fold.on_cover && fold.cover_screen && fold.inward);
        assert_eq!(fold.axis, FoldAxis::Vertical);
        assert_eq!((phone.width, phone.height), (1398, 2034));
        // Opened: the inner screen.
        let open = FoldNow {
            hinge: 180.0,
            on_cover: false,
        };
        let devices = devices_of(&list, &sizes, &|_| Some(open));
        let phone = &devices[0];
        let fold = phone.fold.as_ref().unwrap();
        assert_eq!(fold.posture, DevicePosture::Opened);
        assert!(!fold.on_cover);
        assert_eq!((phone.width, phone.height), (2853, 2007));
        // A booting one is taken as closed whatever was said of it.
        let starting = devices.iter().find(|d| d.name == "Starting").unwrap();
        assert_eq!(
            starting.fold.as_ref().unwrap().posture,
            DevicePosture::Closed
        );
        // Half open.
        let book = FoldNow {
            hinge: BOOK_HINGE,
            on_cover: false,
        };
        assert_eq!(book.fold(None, None).posture, DevicePosture::HalfOpened);
    }

    #[test]
    fn a_foldables_inner_screen_from_its_capabilities() {
        let duo = br#"{"capabilities": {"displays": [
            {"displayType": "integrated", "screenID": 1, "width": 1398, "height": 2034, "nativeRotation": 0},
            {"displayType": "integrated", "screenID": 3, "width": 2007, "height": 2853, "nativeRotation": 270},
            {"displayType": "tvOut", "screenID": 2, "width": 720, "height": 480}
        ]}}"#;
        assert_eq!(inner_screen(duo), Some((3, 2853, 2007)));
        let phone = br#"{"capabilities": {"displays": [
            {"displayType": "integrated", "screenID": 1, "width": 1206, "height": 2622},
            {"displayType": "scene", "screenID": 4, "width": 7680, "height": 4320}
        ]}}"#;
        assert_eq!(inner_screen(phone), None);
        assert_eq!(inner_screen(b"{}"), None);
        assert_eq!(fold_position(0.0), "0.0000");
        assert_eq!(fold_position(180.0), "1.0000");
        assert_eq!(fold_position(90.0), "0.5000");
        assert_eq!(fold_position(400.0), "1.0000");
    }

    #[test]
    fn support_needs_an_ios_runtime() {
        assert!(has_ios_runtime(LIST).unwrap());
        let watch_only = r#"{"runtimes": [{"identifier": "w", "name": "watchOS 26.0", "platform": "watchOS", "isAvailable": true}]}"#;
        assert!(!has_ios_runtime(watch_only).unwrap());
        assert!(!has_ios_runtime(r#"{"runtimes": []}"#).unwrap());
        assert!(has_ios_runtime("not json").is_err());
    }

    #[test]
    fn screens_from_device_type_profiles() {
        let profile = br#"{"mainScreenHeight": 2622, "mainScreenScale": 3, "mainScreenWidth": 1206, "mainScreenWidthDPI": 460}"#;
        assert_eq!(screen_of_profile(profile), Some((1206, 2622, 3.0)));
        assert_eq!(screen_of_profile(b"{}"), None);
        let capabilities = br#"{"capabilities": {"MainScreenClass": 18, "ScreenDimensionsCapability": {"main-screen-height": 2622, "main-screen-pitch": 460, "main-screen-scale": 3, "main-screen-width": 1206}}}"#;
        assert_eq!(
            screen_of_capabilities(capabilities),
            Some((1206, 2622, 3.0))
        );
        assert_eq!(screen_of_capabilities(b"{}"), None);
    }

    #[test]
    fn text_sizes_map_to_content_sizes_and_back() {
        for size in [
            DeviceTextSize::ExtraSmall,
            DeviceTextSize::Small,
            DeviceTextSize::Default,
            DeviceTextSize::Large,
            DeviceTextSize::ExtraLarge,
            DeviceTextSize::Largest,
        ] {
            assert_eq!(text_size_of(content_size(size)), Some(size), "{size:?}");
        }
        assert_eq!(content_size(DeviceTextSize::Default), "large");
        // Sizes set elsewhere read as the nearest.
        assert_eq!(text_size_of("medium\n"), Some(DeviceTextSize::Small));
        assert_eq!(text_size_of("Small\n"), Some(DeviceTextSize::Small));
        assert_eq!(
            text_size_of("extra-extra-large"),
            Some(DeviceTextSize::ExtraLarge)
        );
        assert_eq!(
            text_size_of("accessibility-medium"),
            Some(DeviceTextSize::Largest)
        );
        assert_eq!(text_size_of("unknown"), None);
        assert_eq!(text_size_of("unsupported"), None);
    }

    #[test]
    fn ui_values_read_back() {
        assert_eq!(ui_value("dark\n", "dark", "light"), Some(true));
        assert_eq!(ui_value("light", "dark", "light"), Some(false));
        assert_eq!(ui_value("unsupported", "dark", "light"), None);
        assert_eq!(ui_value("enabled\n", "enabled", "disabled"), Some(true));
        assert_eq!(ui_value("unknown", "enabled", "disabled"), None);
        assert_eq!(defaults_bool(&Ok("1\n".into())), Some(true));
        assert_eq!(defaults_bool(&Ok("0\n".into())), Some(false));
        let never_set = anyhow!(
            "The domain/default pair of (com.apple.Accessibility, ReduceMotionEnabled) does not exist"
        );
        assert_eq!(defaults_bool(&Err(never_set)), Some(false));
        let never_set_27 = anyhow!(
            "Error: Could not find key 'ReduceMotionEnabled' in domain 'com.apple.Accessibility'."
        );
        assert_eq!(defaults_bool(&Err(never_set_27)), Some(false));
        assert_eq!(
            defaults_bool(&Err(anyhow!("Unable to lookup in current state: Shutdown"))),
            None
        );
    }

    #[test]
    fn user_apps_from_listapps() {
        let json = br#"{
            "com.apple.mobilesafari": {"ApplicationType": "System", "CFBundleIdentifier": "com.apple.mobilesafari"},
            "com.example.Mine": {"ApplicationType": "User", "CFBundleIdentifier": "com.example.Mine"},
            "com.apple.Maps": {"ApplicationType": "System"},
            "com.example.Another": {"ApplicationType": "User"}
        }"#;
        let (all, user) = apps_of(json).unwrap();
        assert_eq!(
            all,
            [
                "com.apple.Maps",
                "com.apple.mobilesafari",
                "com.example.Another",
                "com.example.Mine"
            ]
        );
        assert_eq!(user, ["com.example.Another", "com.example.Mine"]);
        assert!(apps_of(b"not json").is_err());
    }

    #[test]
    fn the_app_in_front_is_the_focal_one() {
        let list = "PID\tStatus\tLabel\n\
            80198\t0\tcom.apple.SpringBoard\n\
            80912\t0\tUIKitApplication:com.apple.Preferences[f6ab][rb-legacy]\n\
            80437\t0\tUIKitApplication:com.apple.chrono.WidgetRenderer-Default[1601][rb-legacy]\n\
            -\t0\tUIKitApplication:com.apple.mobilecal[0900][rb-legacy]\n\
            123\t0\tcom.apple.backboardd\n";
        let (running, springboard) = running_apps(list);
        assert_eq!(
            running,
            [
                (80912, "com.apple.Preferences".to_string()),
                (80437, "com.apple.chrono.WidgetRenderer-Default".to_string())
            ]
        );
        assert_eq!(springboard, Some(80198));
        let known = vec!["com.apple.Preferences".to_string()];
        // Settings in front (SpringBoard keeps its role too).
        let focal = |pids: &'static [i32]| move |pid: i32| pids.contains(&pid);
        assert_eq!(
            foreground(&running, springboard, &known, focal(&[80198, 80912])),
            Some("com.apple.Preferences".into())
        );
        // The home screen: a widget renderer is focal too, but isn't an app.
        assert_eq!(
            foreground(&running, springboard, &known, focal(&[80198, 80437])),
            Some(SPRINGBOARD.into())
        );
        assert_eq!(foreground(&running, springboard, &known, focal(&[])), None);
    }

    #[test]
    fn simctl_errors_read_well() {
        let stderr = "An error was encountered processing the command (domain=com.apple.CoreSimulator.SimError, code=405):\nUnable to boot device in current state: Booted\n";
        assert_eq!(
            simctl_error(stderr),
            "Unable to boot device in current state: Booted"
        );
        assert!(already(&anyhow!(simctl_error(stderr)), "Booted"));
        assert!(!already(&anyhow!("Invalid device: x"), "Booted"));
        assert_eq!(simctl_error(""), "simctl failed");
    }
}
