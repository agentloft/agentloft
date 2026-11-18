//! Physical Android phones (ADR-0020), plugged in with USB debugging on or
//! paired for wireless debugging, driven through `adb` alone: nothing is
//! installed on the phone.
//!
//! - The list is `adb devices -l`: phones ready for commands run; ones
//!   waiting for the user to allow debugging are listed with what to do.
//!   Emulators are left to the emulator backend (`android/`), unless
//!   `AGENTLOFT_PHONE_INCLUDE_EMULATORS` says to treat them as phones (`1`
//!   for all, or a comma-separated list of serials): that is how this is
//!   tried without a phone.
//! - The screen is the system's `screenrecord` H.264, decoded on this
//!   computer (`stream.rs`).
//! - Touches go to a touch screen of our own made with the system's
//!   `uinput` command (`touch.rs`); keys and text through `input`.
//! - A phone can't be booted or shut down from here.

mod adb;
mod h264;
mod stream;
mod touch;
#[cfg(target_os = "macos")]
mod videotoolbox;

use super::{Backend, BoxFut, Picture, Screen};
use adb::{About, Adb, Listed};
use anyhow::{Context as _, anyhow, bail};
use loft_protocol::{
    DeviceApp, DeviceButton, DeviceId, DeviceInfo, DeviceInput, DeviceOrientation, DevicePlatform,
    DeviceSetting, DeviceSettings, DeviceState, DeviceTextSize, TouchPhase, page_modifiers,
};
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::watch;
use touch::{Fingers, Rotation, SCROLL_SLOT, ScrollDrag, Step};

/// The longest side of a streamed picture, in pixels.
pub(crate) const PICTURE_MAX: u32 = 1280;
/// How long `adb devices` is trusted (support and list ask in turn).
const DEVICES_FRESH: Duration = Duration::from_millis(1500);
/// A scrolling finger lifts once scrolls stopped coming for this long.
const SCROLL_QUIET: Duration = Duration::from_millis(150);

/// Which emulators count as phones.
#[derive(Debug, Clone, PartialEq)]
enum Emulators {
    None,
    All,
    Some(HashSet<String>),
}

impl Emulators {
    fn from_setting(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") | Some("0") => Self::None,
            Some("1") => Self::All,
            Some(list) => Self::Some(
                list.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect(),
            ),
        }
    }

    fn includes(&self, serial: &str) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Some(set) => set.contains(serial),
        }
    }
}

pub struct PhoneBackend {
    adb: Adb,
    emulators: Emulators,
    decoders: Vec<stream::Decode>,
    /// A recording's length in seconds (`None`: as long as the phone
    /// allows); tests make it short to see recordings handed over.
    record_limit: Option<u32>,
    devices: Mutex<Option<(Instant, Vec<Listed>)>>,
    /// What each phone said about itself, by serial (it doesn't change
    /// while plugged in).
    about: Mutex<HashMap<String, About>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PhoneBackend {
    /// `env` is the login shell's; `AGENTLOFT_PHONE_INCLUDE_EMULATORS` and
    /// `AGENTLOFT_PHONE_DECODER` (`videotoolbox`, `ffmpeg`, `screencap`:
    /// the first to try) are read from it or from the daemon's own.
    pub fn new(env: &HashMap<String, String>) -> Self {
        let setting = |key: &str| {
            std::env::var(key)
                .ok()
                .or_else(|| env.get(key).cloned())
                .filter(|v| !v.is_empty())
        };
        Self {
            adb: Adb::find(env),
            emulators: Emulators::from_setting(
                setting("AGENTLOFT_PHONE_INCLUDE_EMULATORS").as_deref(),
            ),
            decoders: stream::decoders(
                adb::find_ffmpeg(env),
                setting("AGENTLOFT_PHONE_DECODER").as_deref(),
            ),
            record_limit: None,
            devices: Mutex::default(),
            about: Mutex::default(),
        }
    }

    /// Recordings of at most `secs` (to try handing over from one to the
    /// next).
    pub fn with_record_limit(mut self, secs: u32) -> Self {
        self.record_limit = Some(secs);
        self
    }

    /// Treat these emulators (by adb serial) as phones.
    pub fn with_emulators(mut self, serials: &[&str]) -> Self {
        self.emulators = Emulators::Some(serials.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Make pictures this way first (`videotoolbox`, `ffmpeg`, `screencap`).
    pub fn with_decoder(mut self, name: &str) -> Self {
        let ffmpeg = self.decoders.iter().find_map(|d| match d {
            stream::Decode::Ffmpeg(p) => Some(p.clone()),
            _ => None,
        });
        self.decoders = stream::decoders(ffmpeg, Some(name));
        self
    }

    /// The decoders in the order they will be tried.
    pub fn decoder_names(&self) -> Vec<&'static str> {
        self.decoders.iter().map(stream::Decode::name).collect()
    }

    /// The phones adb knows, in any state.
    async fn phones(&self) -> anyhow::Result<Vec<Listed>> {
        if let Some((at, listed)) = lock(&self.devices).as_ref()
            && at.elapsed() < DEVICES_FRESH
        {
            return Ok(listed.clone());
        }
        let listed: Vec<Listed> = self
            .adb
            .devices()
            .await?
            .into_iter()
            .filter(|d| !d.is_emulator() || self.emulators.includes(&d.serial))
            .collect();
        // Forget what unplugged phones said.
        lock(&self.about).retain(|serial, _| listed.iter().any(|d| &d.serial == serial));
        *lock(&self.devices) = Some((Instant::now(), listed.clone()));
        Ok(listed)
    }

    async fn about(&self, serial: &str) -> anyhow::Result<About> {
        if let Some(about) = lock(&self.about).get(serial) {
            return Ok(about.clone());
        }
        let text = self.adb.script(serial, adb::ABOUT_SCRIPT).await?;
        let about = adb::parse_about(&text);
        lock(&self.about).insert(serial.to_owned(), about.clone());
        Ok(about)
    }

    /// The phone `serial`, if adb has it ready.
    async fn ready(&self, serial: &str) -> anyhow::Result<()> {
        // Not the cached list: a command is about to go to it.
        lock(&self.devices).take();
        let listed = self.phones().await?;
        match listed.iter().find(|d| d.serial == serial) {
            Some(d) if d.ready() => Ok(()),
            Some(d) => bail!("{}", problem(d)),
            None => bail!(
                "the phone {serial} isn't connected. Plug it in (or pair it for wireless \
                 debugging) with USB debugging on."
            ),
        }
    }

    fn info(listed: &Listed, about: Option<&About>) -> DeviceInfo {
        let name = about
            .map(|a| a.name.clone())
            .filter(|n| !n.is_empty())
            .or_else(|| listed.model.as_ref().map(|m| m.replace('_', " ")))
            .unwrap_or_else(|| format!("Android phone {}", listed.serial));
        let os = about
            .map(|a| a.release.clone())
            .filter(|r| !r.is_empty())
            .map_or_else(|| "Android".to_owned(), |r| format!("Android {r}"));
        DeviceInfo {
            id: DeviceId::android_phone(&listed.serial),
            platform: DevicePlatform::Android,
            name,
            os,
            state: if listed.ready() {
                DeviceState::Running
            } else {
                DeviceState::Stopped
            },
            width: about.map_or(0, |a| a.width),
            height: about.map_or(0, |a| a.height),
            scale: about.map_or(0.0, |a| density_scale(a.density)),
            error: (!listed.ready()).then(|| problem(listed)),
            agent: None,
            physical: true,
            fold: None,
        }
    }
}

/// What the user must do about a phone adb can't use yet.
fn problem(listed: &Listed) -> String {
    match listed.state.as_str() {
        "unauthorized" | "authorizing" => "Allow USB debugging on the phone: unlock it and \
             accept the prompt (tick \"Always allow from this computer\")."
            .into(),
        "no permissions" => "This computer may not open the phone over USB: add a udev rule \
             for it (see developer.android.com/studio/run/device), then plug it in again."
            .into(),
        "offline" => "The phone isn't answering adb: unplug it and plug it in again.".into(),
        other => format!("The phone is in {other} mode, not ready for commands."),
    }
}

fn density_scale(density: u32) -> f32 {
    if density == 0 {
        0.0
    } else {
        density as f32 / 160.0
    }
}

/// `width` × `height` scaled to fit `max` on the long side (even sizes).
pub(crate) fn fit(width: u32, height: u32, max: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= max {
        return (width, height);
    }
    let scale = max as f64 / long as f64;
    let even = |v: u32| (((v as f64 * scale).round() as u32) & !1).max(2);
    (even(width), even(height))
}

/// A JPEG's size, from its frame header.
pub(crate) fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < jpeg.len() {
        if jpeg[i] != 0xff {
            return None;
        }
        let marker = jpeg[i + 1];
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let h = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]) as u32;
            let w = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

impl Backend for PhoneBackend {
    fn platform(&self) -> DevicePlatform {
        DevicePlatform::Android
    }

    fn support(&self) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async move {
            if self.adb.path.is_none() {
                return Ok(Err(
                    "Agentloft couldn't find adb, which it needs to show an \
                     Android phone. Install Android SDK Platform-Tools (Android Studio's \
                     SDK Manager, or `brew install android-platform-tools`)."
                        .into(),
                ));
            }
            match self.phones().await {
                Ok(phones) if !phones.is_empty() => Ok(Ok(())),
                Ok(_) => Ok(Err("No Android phone is connected. Plug one in with USB \
                     debugging on (Settings ▸ Developer options), or pair it for wireless \
                     debugging."
                    .into())),
                Err(e) => Ok(Err(format!("adb couldn't list phones: {e:#}"))),
            }
        })
    }

    fn list(&self) -> BoxFut<'_, Vec<DeviceInfo>> {
        Box::pin(async move {
            let phones = self.phones().await?;
            let mut devices = Vec::with_capacity(phones.len());
            for listed in &phones {
                let about = if listed.ready() {
                    match self.about(&listed.serial).await {
                        Ok(about) => Some(about),
                        Err(e) => {
                            tracing::debug!(serial = listed.serial, "reading a phone: {e:#}");
                            None
                        }
                    }
                } else {
                    None
                };
                devices.push(Self::info(listed, about.as_ref()));
            }
            Ok(devices)
        })
    }

    fn boot<'a>(&'a self, native: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move { self.ready(serial_of(native)?).await })
    }

    fn shutdown<'a>(&'a self, _native: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move { bail!("a phone can't be shut down from here") })
    }

    fn attach<'a>(&'a self, native: &'a str) -> BoxFut<'a, Arc<dyn Screen>> {
        Box::pin(async move {
            let serial = serial_of(native)?;
            self.ready(serial).await?;
            let about = self.about(serial).await?;
            if about.width == 0 || about.height == 0 {
                bail!("the phone didn't say its screen's size");
            }
            let screen = PhoneScreen::start(
                self.adb.clone(),
                serial.to_owned(),
                &about,
                self.decoders.clone(),
                self.record_limit,
            );
            Ok(screen as Arc<dyn Screen>)
        })
    }

    fn app<'a>(&'a self, native: &'a str, app: DeviceApp) -> BoxFut<'a, ()> {
        Box::pin(async move {
            let serial = serial_of(native)?;
            self.ready(serial).await?;
            run_app(&self.adb, serial, app).await
        })
    }

    fn set<'a>(&'a self, native: &'a str, setting: DeviceSetting) -> BoxFut<'a, ()> {
        Box::pin(async move {
            let serial = serial_of(native)?;
            self.ready(serial).await?;
            let wireless = lock(&self.devices)
                .as_ref()
                .and_then(|(_, l)| l.iter().find(|d| d.serial == serial).map(Listed::wireless))
                .unwrap_or(false);
            apply_setting(&self.adb, serial, setting, wireless).await
        })
    }

    fn settings<'a>(&'a self, native: &'a str) -> BoxFut<'a, DeviceSettings> {
        Box::pin(async move {
            let serial = serial_of(native)?;
            self.ready(serial).await?;
            let text = self.adb.script(serial, SETTINGS_SCRIPT).await?;
            Ok(parse_settings(&text))
        })
    }
}

/// The adb serial in a phone's native id (`@<serial>`).
fn serial_of(native: &str) -> anyhow::Result<&str> {
    native
        .strip_prefix('@')
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{native} isn't a phone"))
}

async fn run_app(adb: &Adb, serial: &str, app: DeviceApp) -> anyhow::Result<()> {
    match app {
        DeviceApp::Install { path } => {
            let out = adb
                .run(
                    Some(serial),
                    &["install", "-r", &path],
                    Duration::from_secs(600),
                )
                .await?;
            if !out.contains("Success") {
                bail!("adb install: {}", out.trim());
            }
        }
        DeviceApp::Launch { app } => {
            // Its launcher activity, started like a tap on its icon.
            // (`monkey` refuses to run on phones without hardware keys.)
            let q = adb::shell_quote(&app);
            let out = adb
                .script(
                    serial,
                    &format!(
                        "c=$(cmd package resolve-activity --brief -c \
                         android.intent.category.LAUNCHER {q} 2>/dev/null | tail -n 1); \
                         case \"$c\" in */*) am start -n \"$c\" \
                         -a android.intent.action.MAIN -c android.intent.category.LAUNCHER \
                         -f 0x10200000 ;; *) monkey --pct-syskeys 0 -p {q} \
                         -c android.intent.category.LAUNCHER 1 2>&1 ;; esac"
                    ),
                )
                .await?;
            if out.contains("No activities found")
                || out.contains("monkey aborted")
                || out.contains("Error:")
            {
                bail!("{app} isn't installed, or has nothing to open");
            }
        }
        DeviceApp::Terminate { app } => {
            adb.shell(serial, &["am", "force-stop", &app]).await?;
        }
        DeviceApp::Uninstall { app } => {
            let out = adb
                .run(Some(serial), &["uninstall", &app], Duration::from_secs(120))
                .await?;
            if !out.contains("Success") {
                bail!("adb uninstall: {}", out.trim());
            }
        }
        DeviceApp::OpenUrl { url } => {
            let out = adb
                .shell(
                    serial,
                    &[
                        "am",
                        "start",
                        "-a",
                        "android.intent.action.VIEW",
                        "-d",
                        &url,
                    ],
                )
                .await?;
            if out.contains("Error:") {
                bail!("{}", out.trim());
            }
        }
    }
    Ok(())
}

/// Android's font scale for each text size, and back.
fn font_scale(size: DeviceTextSize) -> &'static str {
    match size {
        DeviceTextSize::ExtraSmall => "0.8",
        DeviceTextSize::Small => "0.85",
        DeviceTextSize::Default => "1.0",
        DeviceTextSize::Large => "1.15",
        DeviceTextSize::ExtraLarge => "1.3",
        DeviceTextSize::Largest => "2.0",
    }
}

fn text_size(scale: f32) -> DeviceTextSize {
    [
        DeviceTextSize::ExtraSmall,
        DeviceTextSize::Small,
        DeviceTextSize::Default,
        DeviceTextSize::Large,
        DeviceTextSize::ExtraLarge,
        DeviceTextSize::Largest,
    ]
    .into_iter()
    .min_by(|a, b| {
        let d = |s: DeviceTextSize| (font_scale(s).parse::<f32>().unwrap_or(1.0) - scale).abs();
        d(*a).total_cmp(&d(*b))
    })
    .unwrap_or(DeviceTextSize::Default)
}

const TALKBACK: &str =
    "com.google.android.marvin.talkback/com.google.android.marvin.talkback.TalkBackService";

/// The settings a phone takes, as `DeviceSettings::supported` names them.
const SETTINGS: &[&str] = &[
    "appearance",
    "text_size",
    "contrast",
    "reduce_motion",
    "accessibility_frames",
    "permission",
    "push",
    "network",
    "orientation",
];

/// Reads what [`parse_settings`] takes, in one go.
const SETTINGS_SCRIPT: &str = "echo night=$(cmd uimode night 2>/dev/null); \
     echo font=$(settings get system font_scale); \
     echo contrast=$(settings get secure high_text_contrast_enabled); \
     echo anim=$(settings get global animator_duration_scale); \
     echo a11y=$(settings get secure enabled_accessibility_services); \
     echo talkback=$(pm path com.google.android.marvin.talkback 2>/dev/null | head -n 1); \
     echo layout=$(getprop debug.layout); \
     echo wifi=$(settings get global wifi_on); \
     echo data=$(settings get global mobile_data); \
     echo rot=$(dumpsys window displays 2>/dev/null | grep -m1 -o -E 'mRotation=[0-3]|mCurrentRotation=[0-3]'); \
     echo top=$(dumpsys activity activities 2>/dev/null | grep -m1 -E 'topResumedActivity|mResumedActivity'); \
     pm list packages -3 2>/dev/null";

fn parse_settings(text: &str) -> DeviceSettings {
    let mut s = DeviceSettings {
        supported: SETTINGS.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let mut values = HashMap::new();
    for line in text.lines().map(str::trim) {
        if let Some(app) = line.strip_prefix("package:") {
            s.apps.push(app.trim().to_owned());
        } else if let Some((k, v)) = line.split_once('=') {
            values.insert(k.to_owned(), v.trim().to_owned());
        }
    }
    s.apps.sort();
    let get = |k: &str| values.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let flag = |k: &str| get(k).filter(|v| *v != "null").map(|v| v == "1");
    s.dark = get("night").and_then(|v| {
        let v = v.rsplit(':').next()?.trim();
        match v {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    });
    s.text_size = Some(text_size(
        get("font").and_then(|v| v.parse().ok()).unwrap_or(1.0),
    ));
    s.contrast = Some(flag("contrast").unwrap_or(false));
    s.reduce_motion = Some(
        get("anim")
            .and_then(|v| v.parse::<f32>().ok())
            .is_some_and(|v| v == 0.0),
    );
    if get("talkback").is_some_and(|v| v.starts_with("package:")) {
        s.supported.push("screen_reader".into());
        s.screen_reader = Some(get("a11y").is_some_and(|v| v.contains(TALKBACK)));
    }
    s.accessibility_frames = Some(get("layout") == Some("true"));
    let wifi = get("wifi").is_some_and(|v| v != "0" && v != "null");
    let data = flag("data").unwrap_or(false);
    s.network = Some(wifi || data);
    s.orientation = get("rot")
        .and_then(|v| v.chars().last()?.to_digit(10))
        .map(|r| match r {
            1 => DeviceOrientation::LandscapeLeft,
            2 => DeviceOrientation::PortraitUpsideDown,
            3 => DeviceOrientation::LandscapeRight,
            _ => DeviceOrientation::Portrait,
        });
    s.foreground_app = get("top").and_then(foreground_package);
    s
}

/// The package in a `dumpsys activity` line naming an activity
/// (`… u0 com.example/.Main t12}`).
fn foreground_package(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|w| {
        let (package, _) = w.split_once('/')?;
        (package.contains('.')
            && package
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_'))
        .then(|| package.to_owned())
    })
}

fn rotation_of(orientation: DeviceOrientation) -> u8 {
    match orientation {
        DeviceOrientation::Portrait => 0,
        DeviceOrientation::LandscapeLeft => 1,
        DeviceOrientation::PortraitUpsideDown => 2,
        DeviceOrientation::LandscapeRight => 3,
    }
}

/// Turn the interface to `rotation`, with auto-rotate off (else the phone's
/// own sensor turns it back).
async fn set_rotation(adb: &Adb, serial: &str, rotation: u8) -> anyhow::Result<()> {
    adb.script(
        serial,
        &format!(
            "settings put system accelerometer_rotation 0; \
             settings put system user_rotation {}",
            rotation % 4
        ),
    )
    .await
    .map(|_| ())
}

async fn apply_setting(
    adb: &Adb,
    serial: &str,
    setting: DeviceSetting,
    wireless: bool,
) -> anyhow::Result<()> {
    let put = |table: &str, key: &str, value: &str| {
        format!("settings put {table} {key} {}", adb::shell_quote(value))
    };
    let script = match setting {
        DeviceSetting::Appearance { dark } => {
            format!("cmd uimode night {}", if dark { "yes" } else { "no" })
        }
        DeviceSetting::TextSize { size } => put("system", "font_scale", font_scale(size)),
        DeviceSetting::Contrast { high } => put(
            "secure",
            "high_text_contrast_enabled",
            if high { "1" } else { "0" },
        ),
        DeviceSetting::ReduceMotion { on } => {
            let v = if on { "0" } else { "1" };
            [
                "animator_duration_scale",
                "transition_animation_scale",
                "window_animation_scale",
            ]
            .iter()
            .map(|k| put("global", k, v))
            .collect::<Vec<_>>()
            .join("; ")
        }
        DeviceSetting::ScreenReader { on } => {
            let now = adb
                .shell(
                    serial,
                    &[
                        "settings",
                        "get",
                        "secure",
                        "enabled_accessibility_services",
                    ],
                )
                .await?;
            let mut services: Vec<&str> = now
                .trim()
                .split(':')
                .filter(|s| !s.is_empty() && *s != "null" && *s != TALKBACK)
                .collect();
            if on {
                if adb
                    .shell(
                        serial,
                        &["pm", "path", "com.google.android.marvin.talkback"],
                    )
                    .await
                    .is_err()
                {
                    bail!("TalkBack isn't installed on this phone");
                }
                services.push(TALKBACK);
            }
            let list = services.join(":");
            if list.is_empty() {
                "settings delete secure enabled_accessibility_services".into()
            } else {
                format!(
                    "{}; {}",
                    put("secure", "enabled_accessibility_services", &list),
                    put("secure", "accessibility_enabled", "1")
                )
            }
        }
        DeviceSetting::AccessibilityFrames { on } => format!(
            // Apps read the property again when poked.
            "setprop debug.layout {on}; service call activity 1599295570 >/dev/null"
        ),
        DeviceSetting::Permission {
            app,
            permission,
            grant,
        } => {
            let permission = if permission.contains('.') {
                permission
            } else {
                format!("android.permission.{}", permission.to_ascii_uppercase())
            };
            let out = adb
                .shell(
                    serial,
                    &[
                        "pm",
                        if grant { "grant" } else { "revoke" },
                        &app,
                        &permission,
                    ],
                )
                .await?;
            if out.contains("Exception") {
                bail!("{}", out.trim());
            }
            return Ok(());
        }
        DeviceSetting::Push { app, payload } => {
            let alert = &payload["aps"]["alert"];
            let (title, body) = match alert {
                serde_json::Value::String(body) => (app.clone(), body.clone()),
                other => (
                    other["title"].as_str().unwrap_or(&app).to_owned(),
                    other["body"].as_str().unwrap_or_default().to_owned(),
                ),
            };
            let out = adb
                .shell(
                    serial,
                    &[
                        "cmd",
                        "notification",
                        "post",
                        "-S",
                        "bigtext",
                        "-t",
                        &title,
                        "agentloft",
                        &body,
                    ],
                )
                .await?;
            if out.contains("Exception") || out.contains("Unknown command") {
                bail!("{}", out.trim());
            }
            return Ok(());
        }
        DeviceSetting::Network { on } => {
            if !on && wireless {
                bail!(
                    "this phone is connected over Wi-Fi: turning its network off would cut \
                     the connection"
                );
            }
            let v = if on { "enable" } else { "disable" };
            format!("svc wifi {v}; svc data {v}")
        }
        DeviceSetting::Orientation { orientation } => {
            return set_rotation(adb, serial, rotation_of(orientation)).await;
        }
        DeviceSetting::Location { .. } => {
            bail!("a phone's location can't be faked over adb (it needs a mock location app)")
        }
        other @ (DeviceSetting::Posture { .. } | DeviceSetting::Hinge { .. }) => {
            bail!("{other:?} is for foldable emulators")
        }
    };
    adb.script(serial, &script).await.map(|_| ())
}

/// Android's `KeyEvent` code for a DOM `KeyboardEvent.key`.
fn keycode(key: &str) -> Option<u32> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return match c.to_ascii_lowercase() {
            c @ 'a'..='z' => Some(29 + (c as u32 - 'a' as u32)),
            c @ '0'..='9' => Some(7 + (c as u32 - '0' as u32)),
            ' ' => Some(62),
            _ => None,
        };
    }
    if let Some(n) = key.strip_prefix('F').and_then(|n| n.parse::<u32>().ok())
        && (1..=12).contains(&n)
    {
        return Some(130 + n);
    }
    Some(match key {
        "Enter" | "Return" => 66,
        "Backspace" => 67,
        "Delete" | "Del" => 112,
        "Tab" => 61,
        "Escape" | "Esc" => 111,
        "ArrowLeft" | "Left" => 21,
        "ArrowRight" | "Right" => 22,
        "ArrowUp" | "Up" => 19,
        "ArrowDown" | "Down" => 20,
        "Home" => 122,
        "End" => 123,
        "PageUp" => 92,
        "PageDown" => 93,
        "Insert" => 124,
        "Spacebar" | "Space" => 62,
        "Shift" => 59,
        "Control" => 113,
        "Alt" => 57,
        "Meta" | "OS" | "Win" | "Command" | "Super" => 117,
        "CapsLock" => 115,
        "ContextMenu" => 82,
        "GoBack" => 4,
        "GoHome" => 3,
        "AppSwitch" => 187,
        "Power" => 26,
        "AudioVolumeUp" => 24,
        "AudioVolumeDown" => 25,
        "AudioVolumeMute" => 164,
        "MediaPlayPause" => 85,
        "MediaStop" => 86,
        "MediaTrackNext" => 87,
        "MediaTrackPrevious" => 88,
        _ => return None,
    })
}

fn button_keycode(button: DeviceButton) -> Option<u32> {
    Some(match button {
        DeviceButton::Home => 3,
        DeviceButton::Back => 4,
        DeviceButton::Recents => 187,
        DeviceButton::Power => 26,
        DeviceButton::VolumeUp => 24,
        DeviceButton::VolumeDown => 25,
        DeviceButton::RotateLeft | DeviceButton::RotateRight => return None,
    })
}

fn modifier_keycodes(modifiers: u8) -> Vec<u32> {
    [
        (page_modifiers::CONTROL, 113),
        (page_modifiers::ALT, 57),
        (page_modifiers::SHIFT, 59),
        (page_modifiers::META, 117),
    ]
    .into_iter()
    .filter(|(bit, _)| modifiers & bit != 0)
    .map(|(_, code)| code)
    .collect()
}

/// The `input` commands that type `text`: `input text` takes one line of
/// printable ASCII (`%s` for a space); newlines and tabs are keys. `None`
/// when it has characters `input` can't type.
fn text_commands(text: &str) -> Option<Vec<String>> {
    if !text
        .chars()
        .all(|c| c == '\n' || c == '\t' || (' '..='~').contains(&c))
    {
        return None;
    }
    let mut commands = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            commands.push("input keyevent 66".to_owned());
        }
        for (j, part) in line.split('\t').enumerate() {
            if j > 0 {
                commands.push("input keyevent 61".to_owned());
            }
            if !part.is_empty() {
                commands.push(format!(
                    "input text {}",
                    adb::shell_quote(&part.replace(' ', "%s"))
                ));
            }
        }
    }
    Some(commands)
}

/// Type `text` on the phone with `input`. The commands go to adb on its
/// stdin: what an agent types is on no command line of this computer.
async fn type_text(adb: &Adb, serial: &str, text: &str) -> anyhow::Result<()> {
    let commands = text_commands(text).ok_or_else(|| {
        anyhow!("adb can only type plain ASCII on a phone: type the rest on the phone itself")
    })?;
    for chunk in commands.chunks(8) {
        adb.script_on_stdin(serial, &chunk.join("\n")).await?;
    }
    Ok(())
}

/// How touches reach the phone.
enum TouchPath {
    /// Not set up yet (or broken: set up again at the next touch).
    Unset,
    /// Our own touch screen (`uinput`).
    Uinput(Box<Uinput>),
    /// `input motionevent`, one finger and slower, where `uinput` isn't
    /// there (before Android 11) or refused.
    Shell,
}

/// The `uinput` command on the phone, and where its lines go.
struct Uinput {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
}

struct Touch {
    path: TouchPath,
    fingers: Fingers,
    scroll: ScrollDrag,
    /// Where the one finger is (for `Cancel`), in fractions of the picture.
    at: Option<(f32, f32)>,
}

/// Start the `uinput` command with our touch screen; returns once the
/// system has it.
async fn start_uinput(
    adb: &Adb,
    serial: &str,
    width: u32,
    height: u32,
) -> anyhow::Result<(tokio::process::Child, tokio::process::ChildStdin)> {
    let count = || async {
        adb.script(
            serial,
            &format!(
                "dumpsys input 2>/dev/null | grep -c {}",
                adb::shell_quote(&format!("Device [0-9]*: {}", touch::DEVICE_NAME))
            ),
        )
        .await
        .ok()
        .and_then(|n| n.trim().parse::<u32>().ok())
        .unwrap_or(0)
    };
    let before = count().await;
    let mut last = anyhow!("uinput didn't start");
    // Older systems know only the `usb` and `bluetooth` buses.
    for bus in ["virtual", "usb"] {
        let mut cmd = adb.command(Some(serial))?;
        cmd.args(["shell", "-T", "uinput", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("starting uinput")?;
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let line = touch::register(width, height, bus) + "\n";
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                let mut err = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut err).await;
                }
                last = anyhow!("uinput stopped ({status}): {}", err.trim());
                break;
            }
            if count().await > before {
                tracing::debug!(serial, bus, "a touch screen of our own is on the phone");
                return Ok((child, stdin));
            }
            if Instant::now() >= deadline {
                last = anyhow!("the phone didn't take the touch screen");
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Err(last)
}

/// A phone's screen: pictures from `stream.rs` while someone watches,
/// touches through our touch screen, keys and text through `input`.
struct PhoneScreen {
    adb: Adb,
    serial: String,
    /// The display as built, in pixels, and its dots per inch.
    width: u32,
    height: u32,
    density: u32,
    rotation: watch::Sender<Rotation>,
    pictures: watch::Sender<Option<Arc<Picture>>>,
    ended: watch::Sender<Option<String>>,
    watching: watch::Sender<bool>,
    touch: Arc<tokio::sync::Mutex<Touch>>,
    /// Keys and text in the order they came.
    keys: tokio::sync::Mutex<()>,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Drop for PhoneScreen {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl PhoneScreen {
    fn start(
        adb: Adb,
        serial: String,
        about: &About,
        decoders: Vec<stream::Decode>,
        limit: Option<u32>,
    ) -> Arc<Self> {
        let (rotation, _) = watch::channel(Rotation::default());
        let (pictures, _) = watch::channel(None);
        let (ended, _) = watch::channel(None);
        let (watching, _) = watch::channel(false);
        let touch = Arc::new(tokio::sync::Mutex::new(Touch {
            path: TouchPath::Unset,
            fingers: Fingers::new(about.width, about.height),
            scroll: ScrollDrag::default(),
            at: None,
        }));
        let mut tasks = Vec::new();
        tasks.push(
            tokio::spawn(follow_rotation(
                adb.clone(),
                serial.clone(),
                rotation.clone(),
            ))
            .abort_handle(),
        );
        let cfg = stream::Config {
            adb: adb.clone(),
            serial: serial.clone(),
            width: about.width,
            height: about.height,
            decoders,
            limit,
            quiet: Arc::default(),
        };
        tasks.push(
            tokio::spawn(stream::run(
                cfg,
                watching.subscribe(),
                rotation.subscribe(),
                pictures.clone(),
                ended.clone(),
            ))
            .abort_handle(),
        );
        let screen = Arc::new(Self {
            adb,
            serial,
            width: about.width,
            height: about.height,
            density: about.density,
            rotation,
            pictures,
            ended,
            watching,
            touch,
            keys: tokio::sync::Mutex::new(()),
            tasks,
        });
        // Set the touch screen up now, so the first touch doesn't wait.
        let me = Arc::downgrade(&screen);
        tokio::spawn(async move {
            let Some(me) = me.upgrade() else { return };
            let mut touch = me.touch.lock().await;
            me.ensure_touch(&mut touch).await;
        });
        screen
    }

    async fn ensure_touch(&self, touch: &mut Touch) {
        if !matches!(touch.path, TouchPath::Unset) {
            return;
        }
        touch.path = match start_uinput(&self.adb, &self.serial, self.width, self.height).await {
            Ok((child, stdin)) => TouchPath::Uinput(Box::new(Uinput {
                _child: child,
                stdin,
            })),
            Err(e) => {
                tracing::info!(
                    serial = self.serial,
                    "touches through `input` (slower, one finger): {e:#}"
                );
                TouchPath::Shell
            }
        };
        touch.fingers = Fingers::new(self.width, self.height);
    }

    fn rotation(&self) -> Rotation {
        *self.rotation.borrow()
    }

    /// Fingers to points of the picture (`None` lifts), as one frame.
    async fn fingers(
        &self,
        touch: &mut Touch,
        changes: &[(usize, Option<(f32, f32)>)],
    ) -> anyhow::Result<()> {
        self.ensure_touch(touch).await;
        let rotation = self.rotation();
        let natural: Vec<_> = changes
            .iter()
            .map(|&(slot, at)| (slot, at.map(|(x, y)| rotation.to_natural(x, y))))
            .collect();
        match &mut touch.path {
            TouchPath::Uinput(uinput) => {
                let Some(line) = touch.fingers.frame(&natural) else {
                    return Ok(());
                };
                let sent = async {
                    uinput.stdin.write_all(line.as_bytes()).await?;
                    uinput.stdin.write_all(b"\n").await?;
                    uinput.stdin.flush().await
                }
                .await;
                if let Err(e) = sent {
                    // Set up again at the next touch.
                    touch.path = TouchPath::Unset;
                    bail!("the phone's touch screen went away: {e}");
                }
                Ok(())
            }
            TouchPath::Shell => {
                if changes
                    .iter()
                    .any(|(slot, _)| *slot != 0 && *slot != SCROLL_SLOT)
                {
                    bail!("this phone takes one finger at a time here (it has no `uinput`)");
                }
                let (w, h) = rotation.size(self.width, self.height);
                for &(slot, at) in changes {
                    let down = touch.fingers.is_down(slot);
                    let (action, (x, y)) = match (at, down) {
                        (Some(p), false) => ("DOWN", p),
                        (Some(p), true) => ("MOVE", p),
                        (None, true) => ("UP", touch.at.unwrap_or((0.5, 0.5))),
                        (None, false) => continue,
                    };
                    let (px, py) = (
                        (x.clamp(0.0, 1.0) * w as f32).round() as u32,
                        (y.clamp(0.0, 1.0) * h as f32).round() as u32,
                    );
                    touch.fingers.frame(&[(slot, at)]);
                    if at.is_some() {
                        touch.at = at;
                    }
                    self.adb
                        .script(
                            &self.serial,
                            &format!("input motionevent {action} {px} {py}"),
                        )
                        .await?;
                }
                Ok(())
            }
            TouchPath::Unset => bail!("the phone's touch screen isn't ready"),
        }
    }

    async fn key(&self, code: u32, modifiers: u8) -> anyhow::Result<()> {
        let held = modifier_keycodes(modifiers);
        let line = if held.is_empty() {
            format!("input keyevent {code}")
        } else {
            let mut words: Vec<String> = held.iter().map(u32::to_string).collect();
            words.push(code.to_string());
            format!("input keycombination {}", words.join(" "))
        };
        let _order = self.keys.lock().await;
        self.adb.script(&self.serial, &line).await.map(|_| ())
    }

    async fn rotate(&self, counter_clockwise: bool) -> anyhow::Result<()> {
        let now = self.rotation().0;
        let next = if counter_clockwise { now + 1 } else { now + 3 } % 4;
        set_rotation(&self.adb, &self.serial, next).await?;
        self.rotation.send_replace(Rotation(next));
        Ok(())
    }
}

/// Follow how the phone's display is turned: a loop on the phone prints it
/// every second (also when unchanged, so it ends at the first line after
/// adb lets go), and stops by itself after ten minutes in case it doesn't
/// (it is started again at once).
async fn follow_rotation(adb: Adb, serial: String, rotation: watch::Sender<Rotation>) {
    const SCRIPT: &str = "i=0; while [ $i -lt 600 ]; do i=$((i+1)); \
         r=$(dumpsys window displays 2>/dev/null | grep -m1 -o -E 'mRotation=[0-3]|mCurrentRotation=[0-3]'); \
         [ -z \"$r\" ] && r=$(dumpsys input 2>/dev/null | grep -m1 -o 'SurfaceOrientation: [0-3]'); \
         echo \"r=$r\"; sleep 1; done";
    loop {
        let started = async {
            let mut cmd = adb.command(Some(&serial))?;
            cmd.args(["shell", SCRIPT]).stderr(Stdio::null());
            let mut child = cmd.spawn()?;
            let out = child.stdout.take().ok_or_else(|| anyhow!("no output"))?;
            let mut lines = tokio::io::BufReader::new(out).lines();
            while let Some(line) = lines.next_line().await? {
                if let Some(r) = line
                    .trim()
                    .chars()
                    .last()
                    .and_then(|c| c.to_digit(10))
                    .filter(|r| *r < 4)
                {
                    rotation.send_if_modified(|now| {
                        let changed = now.0 != r as u8;
                        now.0 = r as u8;
                        changed
                    });
                }
            }
            drop(child);
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = started {
            tracing::debug!(serial, "following a phone's rotation: {e:#}");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

impl Screen for PhoneScreen {
    fn pictures(&self) -> watch::Receiver<Option<Arc<Picture>>> {
        self.pictures.subscribe()
    }

    fn watching(&self, on: bool) {
        self.watching.send_replace(on);
    }

    fn input(&self, input: DeviceInput) -> BoxFut<'_, ()> {
        Box::pin(async move {
            match input {
                DeviceInput::Touch { phase, x, y } => {
                    let mut touch = self.touch.lock().await;
                    let change = match phase {
                        TouchPhase::Down | TouchPhase::Move => {
                            touch.at = Some((x, y));
                            vec![(0, Some((x, y)))]
                        }
                        TouchPhase::Up => {
                            touch.at = Some((x, y));
                            vec![(0, Some((x, y))), (0, None)]
                        }
                        TouchPhase::Cancel => vec![(0, None)],
                    };
                    self.fingers(&mut touch, &change).await?;
                    if matches!(phase, TouchPhase::Up | TouchPhase::Cancel) {
                        touch.at = None;
                    }
                    Ok(())
                }
                DeviceInput::Touches { phase, touches } => {
                    let mut touch = self.touch.lock().await;
                    let n = touches.len().min(SCROLL_SLOT);
                    let changes: Vec<_> = match phase {
                        TouchPhase::Down | TouchPhase::Move => touches[..n]
                            .iter()
                            .enumerate()
                            .map(|(i, t)| (i, Some((t.x, t.y))))
                            .collect(),
                        TouchPhase::Up | TouchPhase::Cancel => {
                            (0..SCROLL_SLOT).map(|i| (i, None)).collect()
                        }
                    };
                    self.fingers(&mut touch, &changes).await
                }
                DeviceInput::Scroll { x, y, dx, dy } => {
                    let mut touch = self.touch.lock().await;
                    let steps = touch.scroll.scroll(x, y, dx, dy);
                    for step in steps {
                        let change = match step {
                            Step::Down(x, y) | Step::Move(x, y) => (SCROLL_SLOT, Some((x, y))),
                            Step::Up => (SCROLL_SLOT, None),
                        };
                        self.fingers(&mut touch, &[change]).await?;
                    }
                    let count = touch.scroll.count;
                    drop(touch);
                    // Lift it once no other scroll came for a moment.
                    let screen_touch = self.touch.clone();
                    let rotation = self.rotation.subscribe();
                    let (adb, serial, w, h) = (
                        self.adb.clone(),
                        self.serial.clone(),
                        self.width,
                        self.height,
                    );
                    tokio::spawn(async move {
                        tokio::time::sleep(SCROLL_QUIET).await;
                        let mut touch = screen_touch.lock().await;
                        if touch.scroll.count != count || !touch.scroll.lift() {
                            return;
                        }
                        let rotation = *rotation.borrow();
                        lift_scroll(&mut touch, &adb, &serial, rotation, w, h).await;
                    });
                    Ok(())
                }
                DeviceInput::Text { text } => {
                    let _order = self.keys.lock().await;
                    type_text(&self.adb, &self.serial, &text).await
                }
                DeviceInput::Key { key, modifiers } => {
                    let code =
                        keycode(&key).ok_or_else(|| anyhow!("Android has no key for {key:?}"))?;
                    self.key(code, modifiers).await
                }
                DeviceInput::Button { button } => match button {
                    DeviceButton::RotateLeft => self.rotate(true).await,
                    DeviceButton::RotateRight => self.rotate(false).await,
                    other => {
                        self.key(button_keycode(other).expect("not a turn"), 0)
                            .await
                    }
                },
            }
        })
    }

    fn capture(&self) -> BoxFut<'_, Vec<u8>> {
        Box::pin(async move {
            let png = self
                .adb
                .bytes(
                    Some(&self.serial),
                    &["exec-out", "screencap", "-p"],
                    Duration::from_secs(30),
                )
                .await?;
            if !png.starts_with(b"\x89PNG") {
                bail!("the phone's screenshot wasn't a PNG");
            }
            Ok(png)
        })
    }

    fn size(&self) -> Option<(u32, u32, f32)> {
        let (w, h) = self.rotation().size(self.width, self.height);
        Some((w, h, density_scale(self.density)))
    }

    fn ended(&self) -> watch::Receiver<Option<String>> {
        self.ended.subscribe()
    }
}

/// Lift the scrolling finger (after the scrolls stopped).
async fn lift_scroll(
    touch: &mut Touch,
    adb: &Adb,
    serial: &str,
    rotation: Rotation,
    width: u32,
    height: u32,
) {
    match &mut touch.path {
        TouchPath::Uinput(uinput) => {
            if let Some(line) = touch.fingers.frame(&[(SCROLL_SLOT, None)]) {
                let _ = uinput.stdin.write_all(format!("{line}\n").as_bytes()).await;
                let _ = uinput.stdin.flush().await;
            }
        }
        TouchPath::Shell => {
            if touch.fingers.is_down(SCROLL_SLOT) {
                touch.fingers.frame(&[(SCROLL_SLOT, None)]);
                let (w, h) = rotation.size(width, height);
                let (x, y) = touch.at.unwrap_or((0.5, 0.5));
                let _ = adb
                    .script(
                        serial,
                        &format!(
                            "input motionevent UP {} {}",
                            (x * w as f32) as u32,
                            (y * h as f32) as u32
                        ),
                    )
                    .await;
            }
        }
        TouchPath::Unset => {}
    }
}

#[cfg(test)]
mod tests;
