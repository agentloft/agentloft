//! Android emulators (ADR-0020), through the SDK's own tools: the AVDs are
//! read from their files, an emulator is started headless with its gRPC
//! control service on (`-grpc-use-token`), and a running one is driven
//! through that service (`EmulatorController`: streamed screenshots,
//! touches, keys, the rotation sensor). `adb` does what only the system
//! inside can: installing and starting apps, dark mode.
//!
//! An emulator Agentloft starts is its own process group, never recorded
//! as an agent: it outlives the daemon (the startup reaper only ends
//! recorded agent processes) and is stopped from the panel.

mod grpc;
mod input;
mod sdk;

use super::{Backend, BoxFut, Picture, Screen};
use anyhow::{Context as _, anyhow, bail};
use grpc::Grpc;
use input::{Finger, ScrollDrag, Turn};
use loft_protocol::{
    DeviceApp, DeviceButton, DeviceInfo, DeviceInput, DevicePlatform, DeviceSetting, DeviceState,
    TouchPhase,
};
use sdk::{Avd, Running, Sdk};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// The longest side of a streamed picture, in pixels.
const PICTURE_MAX: u32 = 1280;
/// At most this many pictures a second.
const PICTURE_EVERY: Duration = Duration::from_millis(33);
const JPEG_QUALITY: u8 = 80;
/// A cold boot of a new AVD can take minutes.
const BOOT_TIMEOUT: Duration = Duration::from_secs(300);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);

pub struct AndroidBackend {
    env: HashMap<String, String>,
    sdk: Sdk,
    adb: Adb,
    /// Control connections by emulator pid, reused across calls.
    conns: Mutex<HashMap<u32, Grpc>>,
    /// Emulators (by pid) known to have finished booting.
    booted: Mutex<HashSet<u32>>,
}

/// The SDK's adb, run with the login shell's environment.
#[derive(Clone)]
struct Adb {
    path: Option<PathBuf>,
    env: Arc<HashMap<String, String>>,
}

impl Adb {
    /// Run adb (on `serial` when given) and return what it printed.
    async fn run(
        &self,
        serial: Option<&str>,
        args: &[&str],
        timeout: Duration,
    ) -> anyhow::Result<String> {
        let adb = self
            .path
            .as_ref()
            .ok_or_else(|| anyhow!("adb wasn't found"))?;
        let mut cmd = tokio::process::Command::new(adb);
        cmd.envs(self.env.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        if let Some(serial) = serial {
            cmd.args(["-s", serial]);
        }
        cmd.args(args);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| {
                anyhow!(
                    "adb {} took longer than {}s",
                    args.join(" "),
                    timeout.as_secs()
                )
            })?
            .context("running adb")?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let why = [stderr.trim(), stdout.trim()]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("it failed");
            bail!("adb {}: {why}", args.first().copied().unwrap_or_default());
        }
        Ok(stdout)
    }

    /// A shell command on the device; adb joins the words with spaces, so
    /// each is quoted for the device's shell.
    async fn shell(&self, serial: &str, words: &[&str]) -> anyhow::Result<String> {
        let line = words
            .iter()
            .map(|w| sdk::shell_quote(w))
            .collect::<Vec<_>>()
            .join(" ");
        self.run(Some(serial), &["shell", &line], Duration::from_secs(60))
            .await
    }
}

fn serial(running: &Running) -> anyhow::Result<String> {
    running
        .serial()
        .ok_or_else(|| anyhow!("the emulator didn't say its adb name"))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AndroidBackend {
    pub fn new(env: &HashMap<String, String>) -> Self {
        let sdk = Sdk::find(env);
        Self {
            env: env.clone(),
            adb: Adb {
                path: sdk.adb.clone(),
                env: Arc::new(env.clone()),
            },
            sdk,
            conns: Mutex::default(),
            booted: Mutex::default(),
        }
    }

    async fn adb(
        &self,
        serial: Option<&str>,
        args: &[&str],
        timeout: Duration,
    ) -> anyhow::Result<String> {
        self.adb.run(serial, args, timeout).await
    }

    async fn shell(&self, running: &Running, words: &[&str]) -> anyhow::Result<String> {
        self.adb.shell(&serial(running)?, words).await
    }

    /// The emulator running `avd`, if one is (and answers).
    async fn running(&self, avd: &str) -> Option<Running> {
        for running in self.sdk.running().into_iter().filter(|r| r.avd == avd) {
            if self.alive(&running).await {
                return Some(running);
            }
        }
        None
    }

    /// Whether the emulator behind a discovery file is still there (a
    /// crashed one leaves its file behind).
    async fn alive(&self, running: &Running) -> bool {
        if let Some(port) = running.grpc_port {
            if lock(&self.conns)
                .get(&running.pid)
                .is_some_and(|c| !c.is_closed())
            {
                return true;
            }
            return tokio::time::timeout(
                Duration::from_millis(500),
                tokio::net::TcpStream::connect(("127.0.0.1", port)),
            )
            .await
            .is_ok_and(|c| c.is_ok());
        }
        // Started without gRPC: ask adb.
        let Some(serial) = running.serial() else {
            return false;
        };
        self.adb(None, &["devices"], Duration::from_secs(10))
            .await
            .is_ok_and(|out| sdk::adb_emulators(&out).contains(&serial))
    }

    /// The emulator's control service, connected once and reused.
    async fn grpc(&self, running: &Running) -> anyhow::Result<Grpc> {
        if let Some(conn) = lock(&self.conns).get(&running.pid)
            && !conn.is_closed()
        {
            return Ok(conn.clone());
        }
        let port = running
            .grpc_port
            .ok_or_else(|| anyhow!("this emulator was started without its control service"))?;
        if running.token.is_none() {
            bail!(
                "this emulator was started outside Agentloft (by Android Studio, say), which \
                 keeps its screen to itself. Stop it there, then boot it here."
            );
        }
        let conn = Grpc::connect(port, running.token.clone()).await?;
        lock(&self.conns).insert(running.pid, conn.clone());
        Ok(conn)
    }

    /// Whether a running emulator has finished booting.
    async fn has_booted(&self, running: &Running) -> bool {
        if lock(&self.booted).contains(&running.pid) {
            return true;
        }
        let booted = if running.token.is_some() {
            match self.grpc(running).await {
                Ok(conn) => conn
                    .unary::<_, grpc::EmulatorStatus>("getStatus", &grpc::Empty {})
                    .await
                    .is_ok_and(|s| s.booted),
                Err(_) => false,
            }
        } else {
            self.shell(running, &["getprop", "sys.boot_completed"])
                .await
                .is_ok_and(|out| out.trim() == "1")
        };
        if booted {
            lock(&self.booted).insert(running.pid);
        }
        booted
    }

    async fn running_or_err(&self, avd: &str) -> anyhow::Result<Running> {
        self.running(avd)
            .await
            .ok_or_else(|| anyhow!("{avd} isn't running"))
    }

    fn info(avd: &Avd, state: DeviceState) -> DeviceInfo {
        DeviceInfo {
            id: loft_protocol::DeviceId::android(&avd.name),
            platform: DevicePlatform::Android,
            name: avd.display.clone(),
            os: avd.os.clone(),
            state,
            width: avd.width,
            height: avd.height,
            scale: density_scale(avd.density),
            error: None,
            agent: None,
        }
    }

    /// Where an emulator Agentloft starts writes what it says.
    fn log_file(avd: &str) -> PathBuf {
        std::env::temp_dir()
            .join("agentloft-devices")
            .join(format!("android-{avd}.log"))
    }

    async fn start(&self, avd: &str) -> anyhow::Result<()> {
        let emulator = self
            .sdk
            .emulator
            .as_ref()
            .ok_or_else(|| anyhow!("the Android emulator wasn't found"))?;
        if self.sdk.avd(avd).is_none() {
            bail!("there's no Android virtual device named {avd}");
        }
        let log = Self::log_file(avd);
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let out = std::fs::File::create(&log)?;
        let mut cmd = tokio::process::Command::new(emulator);
        cmd.envs(&self.env)
            .args([
                "-avd",
                avd,
                "-no-window",
                "-no-boot-anim",
                "-no-audio",
                "-grpc-use-token",
            ])
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            // It outlives the daemon: its own process group, not killed with
            // this handle.
            .kill_on_drop(false);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        {
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().context("starting the Android emulator")?;
        tracing::info!(avd, pid = child.id(), "started an Android emulator");
        let (exited_tx, mut exited) = watch::channel(None);
        // Reap it when it ends (while the daemon is still here).
        tokio::spawn(async move {
            let status = child.wait().await;
            exited_tx.send_replace(Some(status.map(|s| s.to_string()).unwrap_or_default()));
        });
        let deadline = tokio::time::Instant::now() + BOOT_TIMEOUT;
        loop {
            if let Some(running) = self.running(avd).await
                && self.has_booted(&running).await
            {
                return Ok(());
            }
            let status = exited.borrow_and_update().clone();
            if let Some(status) = &status {
                // The launcher may hand over to the emulator proper and
                // leave: only a missing emulator is a failure.
                if self.running(avd).await.is_none() {
                    bail!(
                        "the emulator stopped ({status}): {}",
                        log_tail(&log).unwrap_or_else(|| "it said nothing".into())
                    );
                }
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("{avd} didn't finish booting in {}s", BOOT_TIMEOUT.as_secs());
            }
            let pause = tokio::time::sleep(Duration::from_millis(500));
            if status.is_none() {
                tokio::select! {
                    _ = pause => {}
                    _ = exited.changed() => {}
                }
            } else {
                pause.await;
            }
        }
    }

    fn forget(&self, pid: u32) {
        lock(&self.conns).remove(&pid);
        lock(&self.booted).remove(&pid);
    }
}

/// Whether process `pid` still exists.
async fn process_alive(pid: u32) -> bool {
    let pid = pid.to_string();
    #[cfg(windows)]
    let mut cmd = {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = tokio::process::Command::new("tasklist");
        cmd.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .creation_flags(CREATE_NO_WINDOW);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new("ps");
        cmd.args(["-p", &pid, "-o", "pid="]);
        cmd
    };
    cmd.stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match cmd.output().await {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            if cfg!(windows) {
                text.contains(&format!("\"{pid}\""))
            } else {
                out.status.success() && text.trim() == pid
            }
        }
        Err(_) => false,
    }
}

/// The last lines of an emulator's log worth showing: its errors when it
/// printed any.
fn log_tail(log: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log).ok()?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let errors: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.contains("ERROR") || l.contains("FATAL") || l.contains("PANIC"))
        .collect();
    let pick = if errors.is_empty() { &lines } else { &errors };
    let tail = pick[pick.len().saturating_sub(3)..].join("\n");
    (!tail.is_empty()).then_some(tail)
}

fn density_scale(density: u32) -> f32 {
    if density == 0 {
        0.0
    } else {
        density as f32 / 160.0
    }
}

impl Backend for AndroidBackend {
    fn platform(&self) -> DevicePlatform {
        DevicePlatform::Android
    }

    fn support(&self) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async move {
            let missing = match (&self.sdk.emulator, &self.sdk.adb) {
                (None, None) => Some("the Android emulator and adb"),
                (None, Some(_)) => Some("the Android emulator"),
                (Some(_), None) => Some("adb (the SDK's platform tools)"),
                _ => None,
            };
            if let Some(missing) = missing {
                return Ok(Err(format!(
                    "Agentloft couldn't find {missing}. Install Android Studio and, in its SDK \
                     Manager, the Android Emulator and Android SDK Platform-Tools \
                     (or set ANDROID_HOME to your SDK)."
                )));
            }
            if self.sdk.avds().is_empty() {
                return Ok(Err(
                    "There are no Android virtual devices yet. Create one in Android Studio's \
                     Device Manager."
                        .into(),
                ));
            }
            Ok(Ok(()))
        })
    }

    fn list(&self) -> BoxFut<'_, Vec<DeviceInfo>> {
        Box::pin(async move {
            let avds = self.sdk.avds();
            let mut devices = Vec::with_capacity(avds.len());
            for avd in &avds {
                let state = match self.running(&avd.name).await {
                    Some(running) if self.has_booted(&running).await => DeviceState::Running,
                    Some(_) => DeviceState::Booting,
                    None => DeviceState::Stopped,
                };
                devices.push(Self::info(avd, state));
            }
            Ok(devices)
        })
    }

    fn boot<'a>(&'a self, native: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move {
            if let Some(running) = self.running(native).await {
                // Already up, or on its way (started elsewhere, say).
                let deadline = tokio::time::Instant::now() + BOOT_TIMEOUT;
                while !self.has_booted(&running).await {
                    if tokio::time::Instant::now() >= deadline || !self.alive(&running).await {
                        bail!("{native} didn't finish booting");
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                return Ok(());
            }
            self.start(native).await
        })
    }

    fn shutdown<'a>(&'a self, native: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move {
            let Some(running) = self.running(native).await else {
                return Ok(());
            };
            // Gracefully (it saves its quick-boot snapshot), through its
            // control service, else its console.
            let asked = match self.grpc(&running).await {
                Ok(conn) => conn
                    .unary::<_, grpc::Empty>(
                        "setVmState",
                        &grpc::VmRunState {
                            state: grpc::run_state::SHUTDOWN,
                        },
                    )
                    .await
                    .map(|_| ()),
                Err(e) => Err(e),
            };
            if let Err(e) = asked {
                tracing::debug!("shutting {native} down over gRPC: {e:#}");
                let serial = serial(&running)?;
                self.adb(Some(&serial), &["emu", "kill"], Duration::from_secs(20))
                    .await?;
            }
            let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
            while self.alive(&running).await {
                if tokio::time::Instant::now() >= deadline {
                    self.forget(running.pid);
                    bail!("{native} didn't stop in {}s", SHUTDOWN_TIMEOUT.as_secs());
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            // Its control port closes before it has finished saving and let
            // go of the AVD: booting again before then would fail.
            while process_alive(running.pid).await {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            self.forget(running.pid);
            Ok(())
        })
    }

    fn attach<'a>(&'a self, native: &'a str) -> BoxFut<'a, Arc<dyn Screen>> {
        Box::pin(async move {
            let running = self.running_or_err(native).await?;
            let conn = self.grpc(&running).await?;
            let avd = self.sdk.avd(native);
            let (width, height, density) = match &avd {
                Some(a) if a.width > 0 && a.height > 0 => (a.width, a.height, a.density),
                _ => {
                    // Its config doesn't say: a full-size picture does.
                    let image: grpc::Image = conn
                        .unary(
                            "getScreenshot",
                            &grpc::ImageFormat {
                                format: grpc::img_format::RGB888,
                                ..Default::default()
                            },
                        )
                        .await?;
                    let (w, h, turn) = image.dims();
                    let (w, h) = Turn::from_skin(turn).size(w, h);
                    (w, h, avd.as_ref().map_or(0, |a| a.density))
                }
            };
            // Without a keyboard the system has no device for the
            // emulator's key events: keys go through adb.
            let keys = match &avd {
                Some(a) if !a.keyboard => Keys::Adb {
                    adb: self.adb.clone(),
                    serial: serial(&running)?,
                },
                _ => Keys::Emulator,
            };
            let turn = sensor_turn(&conn).await.unwrap_or_default();
            let screen = AndroidScreen::start(conn, keys, width, height, density);
            *lock(&screen.shared.turn) = turn;
            Ok(screen as Arc<dyn Screen>)
        })
    }

    fn app<'a>(&'a self, native: &'a str, app: DeviceApp) -> BoxFut<'a, ()> {
        Box::pin(async move {
            let running = self.running_or_err(native).await?;
            let serial = serial(&running)?;
            match app {
                DeviceApp::Install { path } => {
                    let out = self
                        .adb(
                            Some(&serial),
                            &["install", "-r", &path],
                            Duration::from_secs(600),
                        )
                        .await?;
                    if !out.contains("Success") {
                        bail!("adb install: {}", out.trim());
                    }
                }
                DeviceApp::Launch { app } => {
                    let out = self
                        .shell(
                            &running,
                            &[
                                "monkey",
                                "-p",
                                &app,
                                "-c",
                                "android.intent.category.LAUNCHER",
                                "1",
                            ],
                        )
                        .await?;
                    if out.contains("No activities found") || out.contains("monkey aborted") {
                        bail!("{app} isn't installed, or has nothing to open");
                    }
                }
                DeviceApp::Terminate { app } => {
                    self.shell(&running, &["am", "force-stop", &app]).await?;
                }
                DeviceApp::Uninstall { app } => {
                    let out = self
                        .adb(
                            Some(&serial),
                            &["uninstall", &app],
                            Duration::from_secs(120),
                        )
                        .await?;
                    if !out.contains("Success") {
                        bail!("adb uninstall: {}", out.trim());
                    }
                }
                DeviceApp::OpenUrl { url } => {
                    let out = self
                        .shell(
                            &running,
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
        })
    }

    fn set<'a>(&'a self, native: &'a str, setting: DeviceSetting) -> BoxFut<'a, ()> {
        Box::pin(async move {
            let running = self.running_or_err(native).await?;
            match setting {
                DeviceSetting::Appearance { dark } => {
                    self.shell(
                        &running,
                        &["cmd", "uimode", "night", if dark { "yes" } else { "no" }],
                    )
                    .await?;
                }
                DeviceSetting::Location {
                    latitude,
                    longitude,
                } => {
                    let gps = grpc::GpsState {
                        passive_update: false,
                        latitude,
                        longitude,
                        satellites: 12,
                        ..Default::default()
                    };
                    match self.grpc(&running).await {
                        Ok(conn) => {
                            conn.unary::<_, grpc::Empty>("setGps", &gps).await?;
                        }
                        Err(_) => {
                            let serial = serial(&running)?;
                            self.adb(
                                Some(&serial),
                                &[
                                    "emu",
                                    "geo",
                                    "fix",
                                    &longitude.to_string(),
                                    &latitude.to_string(),
                                ],
                                Duration::from_secs(20),
                            )
                            .await?;
                        }
                    }
                }
            }
            Ok(())
        })
    }
}

fn scroll_step(shared: &Shared, step: Finger) -> grpc::Input {
    match step {
        Finger::Down(x, y) | Finger::Move(x, y) => finger(shared, SCROLL_FINGER, x, y, true),
        Finger::Up(x, y) => finger(shared, SCROLL_FINGER, x, y, false),
    }
}

/// How the device is turned, from the emulator's rotation sensor.
async fn sensor_turn(conn: &Grpc) -> anyhow::Result<Turn> {
    let model: grpc::PhysicalModelValue = conn
        .unary(
            "getPhysicalModel",
            &grpc::PhysicalModelValue {
                target: grpc::physical::ROTATION,
                ..Default::default()
            },
        )
        .await?;
    let z = model
        .value
        .and_then(|v| v.data.get(2).copied())
        .unwrap_or_default();
    Ok(Turn::from_degrees(z))
}

/// A raw picture from the emulator, waiting to be encoded.
struct Raw {
    rgb: bytes::Bytes,
    width: u32,
    height: u32,
}

struct Shared {
    /// The display as built, in pixels, and its dots per inch.
    width: u32,
    height: u32,
    density: u32,
    turn: Mutex<Turn>,
}

/// How keys reach the device.
enum Keys {
    /// The emulator's own key events (into its virtual keyboard).
    Emulator,
    /// `input keyevent` and `input text` through adb, for devices built
    /// without a keyboard (`hw.keyboard = no`).
    Adb { adb: Adb, serial: String },
}

/// A running emulator's screen: pictures from its `streamScreenshot` while
/// someone watches, input over one `streamInputEvent` kept open (so it
/// arrives in order).
struct AndroidScreen {
    conn: Grpc,
    keys: Keys,
    shared: Arc<Shared>,
    pictures: watch::Sender<Option<Arc<Picture>>>,
    ended: watch::Sender<Option<String>>,
    watching: watch::Sender<bool>,
    input: Arc<tokio::sync::Mutex<InputState>>,
    tasks: Vec<tokio::task::AbortHandle>,
}

#[derive(Default)]
struct InputState {
    sender: Option<grpc::Sender>,
    /// Where the finger is down, to lift it again on cancel.
    touching: Option<(f32, f32)>,
    scroll: ScrollDrag,
}

/// The finger that touches, and the one that scrolls.
const TOUCH_FINGER: i32 = 0;
const SCROLL_FINGER: i32 = 1;
/// A scrolling finger lifts once scrolls stopped coming for this long.
const SCROLL_QUIET: Duration = Duration::from_millis(150);

/// Send input events in order, on the stream kept open for them (opened
/// again if it broke).
async fn send_events(
    conn: &Grpc,
    state: &mut InputState,
    events: Vec<grpc::Input>,
) -> anyhow::Result<()> {
    for event in events {
        let message = grpc::InputEvent { input: Some(event) };
        let sent = match state.sender.as_mut() {
            Some(sender) => sender.send(&message).is_ok(),
            None => false,
        };
        if !sent {
            let mut sender = conn.client_stream("streamInputEvent").await?;
            sender.send(&message)?;
            state.sender = Some(sender);
        }
    }
    Ok(())
}

/// A finger at a point of the picture, down or lifted.
fn finger(shared: &Shared, id: i32, x: f32, y: f32, down: bool) -> grpc::Input {
    let turn = *lock(&shared.turn);
    let (x, y) = turn.to_display(x, y, shared.width, shared.height);
    grpc::Input::Touch(grpc::TouchEvent {
        touches: vec![grpc::Touch {
            x,
            y,
            identifier: id,
            pressure: if down { 1024 } else { 0 },
            touch_major: if down { 8 } else { 0 },
            touch_minor: 0,
        }],
        display: 0,
    })
}

impl Drop for AndroidScreen {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl AndroidScreen {
    fn start(conn: Grpc, keys: Keys, width: u32, height: u32, density: u32) -> Arc<Self> {
        let shared = Arc::new(Shared {
            width,
            height,
            density,
            turn: Mutex::new(Turn::Portrait),
        });
        let (pictures, _) = watch::channel(None);
        let (ended, _) = watch::channel(None);
        let (watching, _) = watch::channel(false);
        let (raw_tx, raw_rx) = watch::channel::<Option<Arc<Raw>>>(None);
        let mut tasks = Vec::new();
        tasks.push(
            tokio::spawn(stream_pictures(
                conn.clone(),
                shared.clone(),
                watching.subscribe(),
                raw_tx,
            ))
            .abort_handle(),
        );
        tasks.push(tokio::spawn(encode_pictures(raw_rx, pictures.clone())).abort_handle());
        let gone = ended.clone();
        let watched = conn.clone();
        tasks.push(
            tokio::spawn(async move {
                watched.closed().await;
                gone.send_replace(Some("The emulator stopped.".into()));
            })
            .abort_handle(),
        );
        Arc::new(Self {
            conn,
            keys,
            shared,
            pictures,
            ended,
            watching,
            input: Arc::default(),
            tasks,
        })
    }

    fn turn(&self) -> Turn {
        *lock(&self.shared.turn)
    }

    async fn send(&self, events: Vec<grpc::Input>) -> anyhow::Result<()> {
        let mut state = self.input.lock().await;
        send_events(&self.conn, &mut state, events).await
    }

    fn key(key: &str, event_type: i32) -> grpc::Input {
        grpc::Input::Key(grpc::KeyboardEvent {
            event_type,
            key: key.to_owned(),
            ..Default::default()
        })
    }

    /// Press a key (named as the emulator names it) with `modifiers` held.
    async fn press(&self, name: &str, modifiers: u8) -> anyhow::Result<()> {
        let held = input::modifier_keys(modifiers);
        let Keys::Adb { adb, serial } = &self.keys else {
            let mut events: Vec<grpc::Input> = held
                .iter()
                .map(|m| Self::key(m, grpc::key_event::DOWN))
                .collect();
            events.push(Self::key(name, grpc::key_event::PRESS));
            events.extend(held.iter().rev().map(|m| Self::key(m, grpc::key_event::UP)));
            return self.send(events).await;
        };
        let Some(code) = input::android_keycode(name) else {
            // A character without a key code of its own: type it.
            return self.type_text(name.to_owned()).await;
        };
        let mut words = vec!["input".to_owned()];
        if held.is_empty() {
            words.push("keyevent".into());
        } else {
            words.push("keycombination".into());
            words.extend(
                held.iter()
                    .filter_map(|m| input::android_keycode(m))
                    .map(|c| c.to_string()),
            );
        }
        words.push(code.to_string());
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        adb.shell(serial, &words).await.map(|_| ())
    }

    async fn type_text(&self, text: String) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if !input::typeable(&text) {
            // Beyond ASCII: through the clipboard, then paste.
            self.conn
                .unary::<_, grpc::Empty>("setClipboard", &grpc::ClipData { text })
                .await?;
            return match &self.keys {
                Keys::Emulator => {
                    self.send(vec![
                        Self::key("Control", grpc::key_event::DOWN),
                        Self::key("v", grpc::key_event::PRESS),
                        Self::key("Control", grpc::key_event::UP),
                    ])
                    .await
                }
                Keys::Adb { adb, serial } => adb
                    .shell(
                        serial,
                        &["input", "keyevent", &input::PASTE_KEYCODE.to_string()],
                    )
                    .await
                    .map(|_| ()),
            };
        }
        let Keys::Adb { adb, serial } = &self.keys else {
            return self
                .send(vec![grpc::Input::Key(grpc::KeyboardEvent {
                    event_type: grpc::key_event::PRESS,
                    text,
                    ..Default::default()
                })])
                .await;
        };
        // `input text` types one line, `%s` for each space.
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                adb.shell(serial, &["input", "keyevent", "66"]).await?;
            }
            for (j, part) in line.split('\t').enumerate() {
                if j > 0 {
                    adb.shell(serial, &["input", "keyevent", "61"]).await?;
                }
                if !part.is_empty() {
                    adb.shell(serial, &["input", "text", &part.replace(' ', "%s")])
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn rotate(&self, counter_clockwise: bool) -> anyhow::Result<()> {
        // From where it is now: something else may have turned it.
        let now = sensor_turn(&self.conn)
            .await
            .unwrap_or_else(|_| self.turn());
        let turn = now.turned(counter_clockwise);
        self.conn
            .unary::<_, grpc::Empty>(
                "setPhysicalModel",
                &grpc::PhysicalModelValue {
                    target: grpc::physical::ROTATION,
                    status: 0,
                    value: Some(grpc::ParameterValue {
                        data: vec![0.0, 0.0, turn.degrees()],
                    }),
                    interpolation: 1,
                },
            )
            .await?;
        *lock(&self.shared.turn) = turn;
        Ok(())
    }
}

impl Screen for AndroidScreen {
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
                    let shared = &self.shared;
                    let mut state = self.input.lock().await;
                    let event = match phase {
                        TouchPhase::Down | TouchPhase::Move => {
                            state.touching = Some((x, y));
                            finger(shared, TOUCH_FINGER, x, y, true)
                        }
                        TouchPhase::Up => {
                            state.touching = None;
                            finger(shared, TOUCH_FINGER, x, y, false)
                        }
                        TouchPhase::Cancel => {
                            let (lx, ly) = state.touching.take().unwrap_or((x, y));
                            finger(shared, TOUCH_FINGER, lx, ly, false)
                        }
                    };
                    send_events(&self.conn, &mut state, vec![event]).await
                }
                DeviceInput::Scroll { x, y, dx, dy } => {
                    let mut state = self.input.lock().await;
                    let events = state
                        .scroll
                        .scroll(x, y, dx, dy)
                        .into_iter()
                        .map(|step| scroll_step(&self.shared, step))
                        .collect();
                    send_events(&self.conn, &mut state, events).await?;
                    // Lift it once no other scroll came for a moment.
                    let count = state.scroll.count;
                    let (input, conn, shared) =
                        (self.input.clone(), self.conn.clone(), self.shared.clone());
                    tokio::spawn(async move {
                        tokio::time::sleep(SCROLL_QUIET).await;
                        let mut state = input.lock().await;
                        if state.scroll.count == count
                            && let Some(step) = state.scroll.lift()
                        {
                            let lift = scroll_step(&shared, step);
                            let _ = send_events(&conn, &mut state, vec![lift]).await;
                        }
                    });
                    Ok(())
                }
                DeviceInput::Text { text } => self.type_text(text).await,
                DeviceInput::Key { key, modifiers } => {
                    let name = input::key_name(&key)
                        .ok_or_else(|| anyhow!("Android has no key for {key:?}"))?;
                    self.press(&name, modifiers).await
                }
                DeviceInput::Button { button } => match button {
                    DeviceButton::RotateLeft => self.rotate(true).await,
                    DeviceButton::RotateRight => self.rotate(false).await,
                    other => {
                        let key = input::button_key(other).expect("not a rotation");
                        self.press(key, 0).await
                    }
                },
            }
        })
    }

    fn capture(&self) -> BoxFut<'_, Vec<u8>> {
        Box::pin(async move {
            let image: grpc::Image = self
                .conn
                .unary(
                    "getScreenshot",
                    &grpc::ImageFormat {
                        format: grpc::img_format::PNG,
                        ..Default::default()
                    },
                )
                .await?;
            if !image.image.starts_with(b"\x89PNG") {
                bail!("the emulator's screenshot wasn't a PNG");
            }
            Ok(image.image.to_vec())
        })
    }

    fn size(&self) -> Option<(u32, u32, f32)> {
        let (w, h) = self.turn().size(self.shared.width, self.shared.height);
        Some((w, h, density_scale(self.shared.density)))
    }

    fn ended(&self) -> watch::Receiver<Option<String>> {
        self.ended.subscribe()
    }
}

/// Take the emulator's pictures while someone watches. It sends one each
/// time the screen changes, already turned the way the device is and at
/// most as large as asked.
async fn stream_pictures(
    conn: Grpc,
    shared: Arc<Shared>,
    mut watching: watch::Receiver<bool>,
    raw: watch::Sender<Option<Arc<Raw>>>,
) {
    let format = grpc::ImageFormat {
        format: grpc::img_format::RGB888,
        width: PICTURE_MAX,
        height: PICTURE_MAX,
        ..Default::default()
    };
    loop {
        if watching.wait_for(|on| *on).await.is_err() || conn.is_closed() {
            return;
        }
        let mut replies = match conn.server_stream("streamScreenshot", &format).await {
            Ok(replies) => replies,
            Err(e) => {
                tracing::debug!("emulator screenshots: {e:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let mut last = (0, 0);
        loop {
            let image = tokio::select! {
                image = replies.next::<grpc::Image>() => image,
                _ = watching.wait_for(|on| !*on) => break,
            };
            let image = match image {
                Ok(Some(image)) => image,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("emulator screenshots: {e:#}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    break;
                }
            };
            let (width, height, turn) = image.dims();
            *lock(&shared.turn) = Turn::from_skin(turn);
            let picture = if width == 0 || height == 0 {
                // The display is off (the power button): a dark screen of
                // the last size.
                if last == (0, 0) {
                    continue;
                }
                Raw {
                    rgb: vec![0; (last.0 * last.1 * 3) as usize].into(),
                    width: last.0,
                    height: last.1,
                }
            } else if image.image.len() == (width * height * 3) as usize {
                last = (width, height);
                Raw {
                    rgb: image.image,
                    width,
                    height,
                }
            } else {
                continue;
            };
            raw.send_replace(Some(Arc::new(picture)));
        }
    }
}

/// Encode the newest raw picture as JPEG, at most [`PICTURE_EVERY`]; the
/// ones that came in between are skipped.
async fn encode_pictures(
    mut raw: watch::Receiver<Option<Arc<Raw>>>,
    pictures: watch::Sender<Option<Arc<Picture>>>,
) {
    while raw.changed().await.is_ok() {
        let started = tokio::time::Instant::now();
        let Some(next) = raw.borrow_and_update().clone() else {
            continue;
        };
        let encoded = tokio::task::spawn_blocking(move || encode(&next)).await;
        match encoded {
            Ok(Ok(picture)) => {
                pictures.send_replace(Some(Arc::new(picture)));
            }
            Ok(Err(e)) => tracing::debug!("encoding an emulator picture: {e:#}"),
            Err(_) => return,
        }
        tokio::time::sleep_until(started + PICTURE_EVERY).await;
    }
}

fn encode(raw: &Raw) -> anyhow::Result<Picture> {
    let image = image::RgbImage::from_raw(raw.width, raw.height, raw.rgb.to_vec())
        .ok_or_else(|| anyhow!("a picture of the wrong size"))?;
    let mut jpeg = Vec::with_capacity(raw.rgb.len() / 8);
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY);
    image.write_with_encoder(encoder)?;
    Ok(Picture {
        jpeg,
        width: raw.width,
        height: raw.height,
    })
}

#[cfg(test)]
mod tests;
