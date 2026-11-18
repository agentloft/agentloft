//! iOS simulators and Android emulators on the daemon's computer
//! (ADR-0020). The manager here keeps the list of devices, boots and stops
//! them, and holds each running device's [`Screen`] while someone watches
//! it or an agent uses it, numbering its pictures for viewers as the
//! browser's pages do (`BusMsg::DeviceFrame`, newest picture only).
//!
//! Each platform is a [`Backend`] (`ios.rs`, `android.rs`); `fake.rs` is a
//! phone that needs neither, for tests and for trying the panel.

pub mod android;
pub mod backend;
pub mod fake;
pub mod ios;

pub use backend::{Backend, BoxFut, Picture, Screen};

use crate::BusMsg;
use loft_protocol::{
    DeviceApp, DeviceFrame, DeviceId, DeviceInfo, DeviceInput, DeviceSetting, DeviceState,
    DeviceSupport, ErrorBody, ErrorCode, ThreadId,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// How long a screen nobody watches is kept for an agent's next step.
const IDLE_SCREEN: Duration = Duration::from_secs(120);
/// How often the list is read again while someone watches it.
const LIST_EVERY: Duration = Duration::from_secs(4);
/// `DeviceDriven` at most this often per device.
const DRIVEN_EVERY: Duration = Duration::from_secs(1);
/// How long an agent's boot waits for the device to run (a cold Android
/// emulator can take minutes).
pub const BOOT_WAIT: Duration = Duration::from_secs(180);

type Result<T> = std::result::Result<T, ErrorBody>;

/// The backends a real daemon uses on this computer.
pub fn system_backends(env: &HashMap<String, String>) -> Vec<Arc<dyn Backend>> {
    let mut backends: Vec<Arc<dyn Backend>> = Vec::new();
    // iOS simulators: their screens come through a helper process, the
    // daemon's own executable (`agentloftd simulator`).
    // `AGENTLOFT_SIMULATOR_SET`: another device set than the user's
    // simulators (for trying a scratch daemon without touching them).
    #[cfg(target_os = "macos")]
    if let Ok(exe) = std::env::current_exe() {
        let mut ios = ios::IosBackend::new(ios::Helper::agentloftd(exe), env.clone());
        if let Some(set) = std::env::var_os("AGENTLOFT_SIMULATOR_SET").filter(|s| !s.is_empty()) {
            ios = ios.with_device_set(set.into());
        }
        backends.push(Arc::new(ios));
    }
    backends.push(Arc::new(android::AndroidBackend::new(env)));
    backends
}

pub struct Devices {
    backends: Vec<Arc<dyn Backend>>,
    bus: broadcast::Sender<BusMsg>,
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// The last listing, by id.
    known: BTreeMap<DeviceId, DeviceInfo>,
    support: DeviceSupport,
    listed: bool,
    /// Why a device couldn't boot or stream (cleared when it does).
    errors: HashMap<DeviceId, String>,
    /// The thread whose agent used each device last, and when it was said.
    agents: HashMap<DeviceId, (ThreadId, Option<Instant>)>,
    /// Boots under way (their state shows `Booting` whatever the backend
    /// lists).
    booting: HashMap<DeviceId, ()>,
    screens: HashMap<DeviceId, Attached>,
    /// Viewers per device, attached or not (a viewer of a stopped device
    /// gets its screen once it runs).
    watchers: HashMap<DeviceId, usize>,
    /// Subscribers of the list: the list is read again while there are.
    list_watchers: usize,
    list_task: Option<tokio::task::AbortHandle>,
}

struct Attached {
    screen: Arc<dyn Screen>,
    seq: u64,
    frame: Option<DeviceFrame>,
    last_used: Instant,
    task: tokio::task::AbortHandle,
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn not_found(id: &DeviceId) -> ErrorBody {
    ErrorBody::new(ErrorCode::NotFound, format!("No device {id}."))
}

fn failed(e: anyhow::Error) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, format!("{e:#}"))
}

impl Devices {
    pub fn new(backends: Vec<Arc<dyn Backend>>, bus: broadcast::Sender<BusMsg>) -> Self {
        Self {
            backends,
            bus,
            inner: Arc::default(),
        }
    }

    fn backend(&self, id: &DeviceId) -> Result<Arc<dyn Backend>> {
        let platform = id.platform().ok_or_else(|| not_found(id))?;
        self.backends
            .iter()
            .find(|b| b.platform() == platform)
            .cloned()
            .ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("{platform:?} devices aren't available on this computer."),
                )
            })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read every backend's list again; says `DevicesChanged` (and each
    /// device's `DeviceState`) when something changed.
    pub async fn refresh(&self) -> (Vec<DeviceInfo>, DeviceSupport) {
        let mut support = DeviceSupport::default();
        let mut listed = Vec::new();
        for backend in &self.backends {
            let ok = match backend.support().await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(why)) => Err(why),
                Err(e) => Err(format!("{e:#}")),
            };
            let ok = match ok {
                Ok(()) => match backend.list().await {
                    Ok(devices) => {
                        listed.extend(devices);
                        Ok(())
                    }
                    Err(e) => Err(format!("{e:#}")),
                },
                err => err,
            };
            match backend.platform() {
                loft_protocol::DevicePlatform::Ios => {
                    support.ios = ok.is_ok();
                    support.ios_problem = ok.err();
                }
                loft_protocol::DevicePlatform::Android => {
                    support.android = ok.is_ok();
                    support.android_problem = ok.err();
                }
            }
        }
        let (changed, devices, attach) = {
            let mut inner = self.lock();
            let before: Vec<DeviceInfo> = inner.known.values().cloned().collect();
            let known: BTreeMap<DeviceId, DeviceInfo> =
                listed.into_iter().map(|d| (d.id.clone(), d)).collect();
            inner.known = known;
            let support_changed = inner.support != support;
            inner.support = support.clone();
            let first = !inner.listed;
            inner.listed = true;
            // Screens of devices that stopped or went away end.
            let gone: Vec<DeviceId> = inner
                .screens
                .keys()
                .filter(|id| {
                    inner
                        .known
                        .get(*id)
                        .is_none_or(|d| d.state != DeviceState::Running)
                })
                .cloned()
                .collect();
            for id in gone {
                inner.screens.remove(&id);
            }
            let devices: Vec<DeviceInfo> = inner
                .known
                .keys()
                .filter_map(|id| decorate(&inner, id))
                .collect();
            let changed: Vec<DeviceId> = devices
                .iter()
                .filter(|d| !before.iter().any(|b| b == *d))
                .map(|d| d.id.clone())
                .chain(
                    before
                        .iter()
                        .filter(|b| !inner.known.contains_key(&b.id))
                        .map(|b| b.id.clone()),
                )
                .collect();
            // Watched devices that now run get their screen.
            let attach: Vec<DeviceId> = inner
                .watchers
                .iter()
                .filter(|(id, n)| {
                    **n > 0
                        && !inner.screens.contains_key(*id)
                        && inner
                            .known
                            .get(*id)
                            .is_some_and(|d| d.state == DeviceState::Running)
                })
                .map(|(id, _)| id.clone())
                .collect();
            (
                (first || support_changed || !changed.is_empty()).then_some(changed),
                devices,
                attach,
            )
        };
        if let Some(changed) = changed {
            for id in changed {
                let _ = self.bus.send(BusMsg::DeviceState { id });
            }
            let _ = self.bus.send(BusMsg::DevicesChanged);
        }
        for id in attach {
            let _ = self.screen(&id).await;
        }
        (devices, support)
    }

    /// The list as last read (read now if it never was).
    pub async fn list(&self) -> (Vec<DeviceInfo>, DeviceSupport) {
        if !self.lock().listed {
            return self.refresh().await;
        }
        let inner = self.lock();
        let devices = inner
            .known
            .keys()
            .filter_map(|id| decorate(&inner, id))
            .collect();
        (devices, inner.support.clone())
    }

    pub fn info(&self, id: &DeviceId) -> Option<DeviceInfo> {
        decorate(&self.lock(), id)
    }

    /// Someone subscribed to (or left) the list: it is read again every few
    /// seconds while anyone does.
    pub fn watching_list(self: &Arc<Self>, on: bool) {
        let mut inner = self.lock();
        if on {
            inner.list_watchers += 1;
            if inner.list_task.is_none() {
                let me = Arc::downgrade(self);
                let task = tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(LIST_EVERY).await;
                        let Some(me) = me.upgrade() else { return };
                        me.refresh().await;
                    }
                });
                inner.list_task = Some(task.abort_handle());
            }
        } else {
            inner.list_watchers = inner.list_watchers.saturating_sub(1);
            if inner.list_watchers == 0
                && let Some(task) = inner.list_task.take()
            {
                task.abort();
            }
        }
    }

    /// A device as it is now, with its latest picture.
    pub async fn snapshot(&self, id: &DeviceId) -> Result<(DeviceInfo, Option<DeviceFrame>, u64)> {
        if !self.lock().listed {
            self.refresh().await;
        }
        let inner = self.lock();
        let info = decorate(&inner, id).ok_or_else(|| not_found(id))?;
        let (frame, seq) = inner
            .screens
            .get(id)
            .map_or((None, 0), |a| (a.frame.clone(), a.seq));
        Ok((info, frame, seq))
    }

    /// The newest picture, if newer than `seq`.
    pub fn frame_after(&self, id: &DeviceId, seq: u64) -> Option<DeviceFrame> {
        let inner = self.lock();
        let attached = inner.screens.get(id)?;
        attached.frame.clone().filter(|f| f.seq > seq)
    }

    /// A viewer started (or stopped) watching `id`.
    pub async fn watching(&self, id: &DeviceId, on: bool) {
        let screen = {
            let mut inner = self.lock();
            let n = inner.watchers.entry(id.clone()).or_default();
            if on {
                *n += 1;
            } else {
                *n = n.saturating_sub(1);
            }
            let watched = *n > 0;
            if !watched {
                inner.watchers.remove(id);
            }
            match inner.screens.get_mut(id) {
                Some(a) => {
                    a.last_used = Instant::now();
                    a.screen.watching(watched);
                    None
                }
                None => Some(watched),
            }
        };
        if screen == Some(true) {
            let _ = self.screen(id).await;
        }
    }

    /// The device's screen, attached now if it isn't.
    async fn screen(&self, id: &DeviceId) -> Result<Arc<dyn Screen>> {
        if let Some(a) = self.lock().screens.get_mut(id) {
            a.last_used = Instant::now();
            return Ok(a.screen.clone());
        }
        let backend = self.backend(id)?;
        let running = self
            .info(id)
            .is_some_and(|d| d.state == DeviceState::Running);
        if !running {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("{id} isn't running: boot it first."),
            ));
        }
        let screen = match backend.attach(id.native()).await {
            Ok(screen) => screen,
            Err(e) => {
                self.lock().errors.insert(id.clone(), format!("{e:#}"));
                let _ = self.bus.send(BusMsg::DeviceState { id: id.clone() });
                return Err(failed(e));
            }
        };
        let mut inner = self.lock();
        // Another caller may have attached meanwhile: keep theirs.
        if let Some(a) = inner.screens.get_mut(id) {
            return Ok(a.screen.clone());
        }
        let watched = inner.watchers.get(id).is_some_and(|n| *n > 0);
        screen.watching(watched);
        inner.errors.remove(id);
        let task = tokio::spawn(follow(
            Arc::downgrade(&self.inner),
            self.bus.clone(),
            id.clone(),
            screen.clone(),
        ));
        inner.screens.insert(
            id.clone(),
            Attached {
                screen: screen.clone(),
                seq: 0,
                frame: None,
                last_used: Instant::now(),
                task: task.abort_handle(),
            },
        );
        drop(inner);
        let _ = self.bus.send(BusMsg::DeviceState { id: id.clone() });
        Ok(screen)
    }

    /// An agent of `thread` is using `id`: remember it, and tell whoever
    /// has that thread open (at most once a second).
    fn used_by(&self, id: &DeviceId, thread: Option<&ThreadId>) {
        let Some(thread) = thread else { return };
        let say = {
            let mut inner = self.lock();
            let entry = inner
                .agents
                .entry(id.clone())
                .or_insert_with(|| (thread.clone(), None));
            let changed = entry.0 != *thread;
            entry.0 = thread.clone();
            let due = changed || entry.1.is_none_or(|at| at.elapsed() >= DRIVEN_EVERY);
            if due {
                entry.1 = Some(Instant::now());
            }
            due.then(|| decorate(&inner, id)).flatten()
        };
        if let Some(device) = say {
            let _ = self.bus.send(BusMsg::DeviceDriven {
                thread_id: thread.clone(),
                device,
            });
        }
    }

    pub async fn boot(self: &Arc<Self>, id: &DeviceId, thread: Option<&ThreadId>) -> Result<()> {
        let backend = self.backend(id)?;
        if !self.lock().listed {
            self.refresh().await;
        }
        match self.info(id) {
            None => return Err(not_found(id)),
            Some(d) if d.state == DeviceState::Running => {
                self.used_by(id, thread);
                return Ok(());
            }
            Some(_) => {}
        }
        {
            let mut inner = self.lock();
            if inner.booting.insert(id.clone(), ()).is_some() {
                return Ok(());
            }
            inner.errors.remove(id);
        }
        let _ = self.bus.send(BusMsg::DeviceState { id: id.clone() });
        let _ = self.bus.send(BusMsg::DevicesChanged);
        self.used_by(id, thread);
        let me = self.clone();
        let id = id.clone();
        tokio::spawn(async move {
            let booted = backend.boot(id.native()).await;
            {
                let mut inner = me.lock();
                inner.booting.remove(&id);
                if let Err(e) = &booted {
                    inner.errors.insert(id.clone(), format!("{e:#}"));
                }
            }
            me.refresh().await;
            let _ = me.bus.send(BusMsg::DeviceState { id: id.clone() });
            let _ = me.bus.send(BusMsg::DevicesChanged);
        });
        Ok(())
    }

    /// Boot `id` if it isn't running and wait until it is (for agents).
    pub async fn boot_and_wait(
        self: &Arc<Self>,
        id: &DeviceId,
        thread: Option<&ThreadId>,
        timeout: Duration,
    ) -> Result<DeviceInfo> {
        let mut bus = self.bus.subscribe();
        self.boot(id, thread).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let inner = self.lock();
                // Running first: an error left by an earlier broken stream
                // doesn't make a running device unusable.
                if let Some(info) = decorate(&inner, id)
                    && info.state == DeviceState::Running
                {
                    return Ok(info);
                }
                if let Some(error) = inner.errors.get(id) {
                    return Err(ErrorBody::new(ErrorCode::Internal, error.clone()));
                }
            }
            match tokio::time::timeout_at(deadline, bus.recv()).await {
                Err(_) => {
                    return Err(ErrorBody::new(
                        ErrorCode::Internal,
                        format!("{id} didn't finish booting in {}s.", timeout.as_secs()),
                    ));
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err(ErrorBody::new(ErrorCode::Internal, "Shutting down."));
                }
                Ok(_) => {}
            }
        }
    }

    pub async fn shutdown(&self, id: &DeviceId) -> Result<()> {
        let backend = self.backend(id)?;
        self.lock().screens.remove(id);
        backend.shutdown(id.native()).await.map_err(failed)?;
        self.refresh().await;
        Ok(())
    }

    pub async fn input(
        &self,
        id: &DeviceId,
        input: DeviceInput,
        thread: Option<&ThreadId>,
    ) -> Result<()> {
        let screen = self.screen(id).await?;
        self.used_by(id, thread);
        screen.input(input).await.map_err(failed)
    }

    pub async fn capture(&self, id: &DeviceId, thread: Option<&ThreadId>) -> Result<Vec<u8>> {
        let screen = self.screen(id).await?;
        self.used_by(id, thread);
        screen.capture().await.map_err(failed)
    }

    pub async fn app(
        &self,
        id: &DeviceId,
        app: DeviceApp,
        thread: Option<&ThreadId>,
    ) -> Result<()> {
        let backend = self.backend(id)?;
        if let DeviceApp::Install { path } = &app {
            check_install_path(path)?;
        }
        self.used_by(id, thread);
        backend.app(id.native(), app).await.map_err(failed)
    }

    pub async fn set(&self, id: &DeviceId, setting: DeviceSetting) -> Result<()> {
        let backend = self.backend(id)?;
        backend.set(id.native(), setting).await.map_err(failed)
    }

    /// Drop screens nobody watched or used for a while (called now and
    /// then by the core's housekeeping).
    pub fn tidy(&self) {
        let mut inner = self.lock();
        let watchers = inner.watchers.clone();
        inner.screens.retain(|id, a| {
            watchers.get(id).is_some_and(|n| *n > 0) || a.last_used.elapsed() < IDLE_SCREEN
        });
    }
}

/// An app to install must be on this computer, named by its whole path
/// (the daemon has no folder of its own to resolve a relative one in).
fn check_install_path(path: &str) -> Result<()> {
    let p = std::path::Path::new(path);
    if !p.is_absolute() {
        return Err(ErrorBody::new(
            ErrorCode::BadRequest,
            format!("Give the app's absolute path, not `{path}`."),
        ));
    }
    if !p.exists() {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("No app at {path}: build it first, or check the path."),
        ));
    }
    Ok(())
}

/// `id` as viewers see it: the backend's listing, with boots under way,
/// errors, the agent that used it and the attached screen's size.
fn decorate(inner: &Inner, id: &DeviceId) -> Option<DeviceInfo> {
    let mut info = inner.known.get(id)?.clone();
    if inner.booting.contains_key(id) && info.state != DeviceState::Running {
        info.state = DeviceState::Booting;
    }
    if let Some(error) = inner.errors.get(id) {
        info.error = Some(error.clone());
    }
    if let Some((thread, _)) = inner.agents.get(id) {
        info.agent = Some(thread.clone());
    }
    if let Some((width, height, scale)) = inner.screens.get(id).and_then(|a| a.screen.size()) {
        info.width = width;
        info.height = height;
        info.scale = scale;
    }
    Some(info)
}

/// Number a screen's pictures as they come, and drop the screen when it
/// ends.
async fn follow(
    inner: std::sync::Weak<Mutex<Inner>>,
    bus: broadcast::Sender<BusMsg>,
    id: DeviceId,
    screen: Arc<dyn Screen>,
) {
    let mut pictures = screen.pictures();
    // A picture taken before we looked counts too.
    pictures.mark_changed();
    let mut ended = screen.ended();
    ended.mark_changed();
    let mut size = screen.size();
    let mut idle = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            changed = pictures.changed() => {
                if changed.is_err() {
                    break;
                }
                let picture = pictures.borrow_and_update().clone();
                let Some(picture) = picture else { continue };
                let Some(inner) = inner.upgrade() else { return };
                let seq = {
                    let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(a) = inner.screens.get_mut(&id) else { return };
                    a.seq += 1;
                    a.frame = Some(DeviceFrame {
                        seq: a.seq,
                        jpeg: picture.jpeg.clone().into(),
                    });
                    a.seq
                };
                let _ = bus.send(BusMsg::DeviceFrame { id: id.clone(), seq });
                let now = screen.size();
                if now != size {
                    size = now;
                    let _ = bus.send(BusMsg::DeviceState { id: id.clone() });
                }
            }
            changed = ended.changed() => {
                let why = match changed {
                    Ok(()) => ended.borrow_and_update().clone(),
                    Err(_) => Some("The device's stream stopped.".to_string()),
                };
                let Some(why) = why else { continue };
                if let Some(inner) = inner.upgrade() {
                    let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
                    inner.errors.insert(id.clone(), why);
                    // Dropping it aborts this task: say so first.
                    let _ = bus.send(BusMsg::DeviceState { id: id.clone() });
                    inner.screens.remove(&id);
                }
                return;
            }
            _ = idle.tick() => {
                let Some(inner) = inner.upgrade() else { return };
                let mut inner = inner.lock().unwrap_or_else(|e| e.into_inner());
                let watched = inner.watchers.get(&id).is_some_and(|n| *n > 0);
                if !watched
                    && inner
                        .screens
                        .get(&id)
                        .is_some_and(|a| a.last_used.elapsed() >= IDLE_SCREEN)
                {
                    inner.screens.remove(&id);
                    return;
                }
            }
        }
    }
    if let Some(inner) = inner.upgrade() {
        inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .screens
            .remove(&id);
    }
}

#[cfg(test)]
mod tests;
