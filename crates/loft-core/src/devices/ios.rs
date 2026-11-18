//! iOS and iPadOS simulators, through Xcode (ADR-0020).
//!
//! Listing, booting, apps and settings go through `xcrun simctl`. A running
//! simulator's screen and input go through Xcode's private CoreSimulator and
//! SimulatorKit frameworks, in a helper process (`agentloftd simulator`,
//! the `loft-sim` crate) so that a crash there can't take the daemon down:
//! the helper sends JPEG pictures of the screen and carries out touches,
//! keys and buttons ([`loft_sim::wire`]).

use super::{Backend, BoxFut, Picture, Screen};
use anyhow::{Context, Result, anyhow, bail};
use loft_protocol::{
    DeviceApp, DeviceId, DeviceInfo, DeviceInput, DevicePlatform, DeviceSetting, DeviceState,
};
use loft_sim::wire::{self, Command, Event, Request};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
        let mut command = self.xcrun();
        command.arg("simctl");
        if let Some(set) = &self.set {
            command.arg("--set").arg(set);
        }
        let out = command
            .args(args)
            .output()
            .await
            .context("couldn't run xcrun")?;
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
    async fn screen_sizes(&self, list: &SimctlList) -> HashMap<String, (u32, u32, f32)> {
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

    async fn read_screen(&self, bundle: &str) -> Option<(u32, u32, f32)> {
        if bundle.is_empty() {
            return None;
        }
        let profile = Path::new(bundle).join("Contents/Resources/profile.plist");
        let out = tokio::process::Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&profile)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        out.status.success().then_some(())?;
        screen_of_profile(&out.stdout)
    }
}

/// A screen's width and height in pixels, and pixels per point.
type ScreenOf = (u32, u32, f32);

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

fn device_state(state: &str) -> DeviceState {
    match state {
        "Booted" => DeviceState::Running,
        "Booting" => DeviceState::Booting,
        "Shutting Down" => DeviceState::ShuttingDown,
        _ => DeviceState::Stopped,
    }
}

/// The iPhones and iPads in `simctl list -j`, with their screens' sizes
/// from `sizes` (by device type).
pub(crate) fn devices_of(
    list: &SimctlList,
    sizes: &HashMap<String, (u32, u32, f32)>,
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
            let (width, height, scale) = sizes
                .get(&device.device_type_identifier)
                .copied()
                .unwrap_or((0, 0, 0.0));
            out.push(DeviceInfo {
                id: DeviceId::ios(&device.udid),
                platform: DevicePlatform::Ios,
                name: device.name.clone(),
                os,
                state: device_state(&device.state),
                width,
                height,
                scale,
                error: None,
                agent: None,
                physical: false,
                fold: None,
            });
        }
    }
    out
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
            Ok(devices_of(&list, &sizes))
        })
    }

    fn boot<'a>(&'a self, native: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move {
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
            match self.simctl(&["shutdown", native]).await {
                Err(e) if !already(&e, "Shutdown") => Err(e),
                _ => Ok(()),
            }
        })
    }

    fn attach<'a>(&'a self, native: &'a str) -> BoxFut<'a, Arc<dyn Screen>> {
        Box::pin(async move {
            let developer_dir = self.check_support().await.map_err(|why| anyhow!(why))?;
            let screen =
                IosScreen::start(&self.helper, native, &developer_dir, self.set.as_deref()).await?;
            Ok(Arc::new(screen) as Arc<dyn Screen>)
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
                other => anyhow::bail!("{other:?} isn't supported on iOS simulators yet."),
            }
            .map(drop)
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
    async fn start(
        helper: &Helper,
        udid: &str,
        developer_dir: &Path,
        set: Option<&Path>,
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
            let (pictures, ended, size, waiting) = (
                pictures.clone(),
                ended.clone(),
                size.clone(),
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
                            if let Some((width, height, jpeg)) = wire::split_frame(&body) {
                                pictures.send_replace(Some(Arc::new(Picture {
                                    jpeg: jpeg.to_vec(),
                                    width,
                                    height,
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
                                if let Some(ready) = ready.take() {
                                    let _ = ready.send(Ok(()));
                                }
                            }
                            Ok(Event::Size { screen }) => {
                                *size.lock().unwrap() =
                                    Some((screen.width, screen.height, screen.scale));
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
            (1206, 2622, 3.0),
        )]);
        let devices = devices_of(&list, &sizes);
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
