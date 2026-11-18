//! A phone that needs neither Xcode nor the Android SDK: for tests, and for
//! trying the device panel (`AGENTLOFT_FAKE_DEVICES=1` on a daemon). Its
//! screen is a coloured picture with a dot where it was last touched; it
//! keeps everything it was sent in [`FakeBackend::log`].
//!
//! [`FakeBackend::foldable`] is a fake Android foldable (`android:Fake_Fold`)
//! that folds like a Pixel Fold: its main screen is wide while open, and
//! its pictures show the narrow cover screen while it is folded shut.

use super::{Backend, BoxFut, Picture, Screen};
use loft_protocol::{
    DeviceApp, DeviceButton, DeviceFold, DeviceId, DeviceInfo, DeviceInput, DevicePlatform,
    DevicePosture, DeviceSetting, DeviceSettings, DeviceState, FoldAxis, TouchPhase,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

const WIDTH: u32 = 390;
const HEIGHT: u32 = 844;
/// The fake foldable's main screen, open, and its cover screen.
const FOLD_OPEN: (u32, u32) = (740, 680);
const FOLD_COVER: (u32, u32) = (360, 780);

#[derive(Debug, Clone, PartialEq)]
pub enum FakeEvent {
    Input(DeviceInput),
    App(DeviceApp),
    Set(DeviceSetting),
    Boot,
    Shutdown,
}

pub struct FakeBackend {
    platform: DevicePlatform,
    state: Arc<Mutex<FakeState>>,
    boot_time: Duration,
    /// The screen attached last: a fold redraws it at its new size.
    screen: Mutex<std::sync::Weak<FakeScreen>>,
}

struct FakeState {
    running: bool,
    log: Vec<FakeEvent>,
    /// A foldable's fold, as it is now.
    fold: Option<DeviceFold>,
    settings: DeviceSettings,
}

impl FakeState {
    /// The screen's size as the pictures show it, before turning.
    fn screen(&self) -> (u32, u32) {
        match &self.fold {
            Some(fold) if fold.on_cover => FOLD_COVER,
            Some(_) => FOLD_OPEN,
            None => (WIDTH, HEIGHT),
        }
    }

    fn set_hinge(&mut self, degrees: f32) {
        let Some(fold) = &mut self.fold else { return };
        fold.hinge = degrees.clamp(fold.hinge_min, fold.hinge_max);
        fold.posture = match fold.hinge {
            h if h < 15. => DevicePosture::Closed,
            h if h < 165. => DevicePosture::HalfOpened,
            _ => DevicePosture::Opened,
        };
        fold.on_cover = fold.posture == DevicePosture::Closed;
    }

    fn apply(&mut self, setting: &DeviceSetting) {
        let s = &mut self.settings;
        match setting {
            DeviceSetting::Appearance { dark } => s.dark = Some(*dark),
            DeviceSetting::TextSize { size } => s.text_size = Some(*size),
            DeviceSetting::Contrast { high } => s.contrast = Some(*high),
            DeviceSetting::ReduceMotion { on } => s.reduce_motion = Some(*on),
            DeviceSetting::ScreenReader { on } => s.screen_reader = Some(*on),
            DeviceSetting::AccessibilityFrames { on } => s.accessibility_frames = Some(*on),
            DeviceSetting::Network { on } => s.network = Some(*on),
            DeviceSetting::Orientation { orientation } => s.orientation = Some(*orientation),
            DeviceSetting::Posture { posture } => {
                let degrees = match posture {
                    DevicePosture::Closed => 0.,
                    DevicePosture::HalfOpened => 90.,
                    _ => 180.,
                };
                self.set_hinge(degrees);
            }
            DeviceSetting::Hinge { degrees } => self.set_hinge(*degrees),
            DeviceSetting::Location { .. }
            | DeviceSetting::Permission { .. }
            | DeviceSetting::Push { .. } => {}
        }
    }
}

/// Every setting a fake takes, as `DeviceSettings::supported` names them.
const FAKE_SETTINGS: &[&str] = &[
    "appearance",
    "location",
    "text_size",
    "contrast",
    "reduce_motion",
    "screen_reader",
    "accessibility_frames",
    "permission",
    "push",
    "network",
    "orientation",
];

impl FakeBackend {
    /// One fake device of `platform`, stopped, that boots in `boot_time`.
    pub fn new(platform: DevicePlatform, boot_time: Duration) -> Self {
        Self {
            platform,
            state: Arc::new(Mutex::new(FakeState {
                running: false,
                log: Vec::new(),
                fold: None,
                settings: DeviceSettings {
                    supported: FAKE_SETTINGS.iter().map(|s| s.to_string()).collect(),
                    dark: Some(false),
                    apps: vec!["com.example.fake".into()],
                    foreground_app: Some("com.example.fake".into()),
                    ..Default::default()
                },
            })),
            boot_time,
            screen: Mutex::new(std::sync::Weak::new()),
        }
    }

    /// A fake Android foldable (`android:Fake_Fold`), open flat.
    pub fn foldable(boot_time: Duration) -> Self {
        let fake = Self::new(DevicePlatform::Android, boot_time);
        {
            let mut state = fake.state.lock().unwrap();
            state.fold = Some(DeviceFold {
                axis: FoldAxis::Vertical,
                inward: true,
                cover_screen: true,
                postures: vec![
                    DevicePosture::Closed,
                    DevicePosture::HalfOpened,
                    DevicePosture::Opened,
                ],
                posture: DevicePosture::Opened,
                hinge: 180.,
                hinge_min: 0.,
                hinge_max: 180.,
                on_cover: false,
            });
            state.settings.supported.push("posture".into());
            state.settings.supported.push("hinge".into());
        }
        fake
    }

    pub fn id(&self) -> DeviceId {
        match self.platform {
            DevicePlatform::Ios => DeviceId::ios("FAKE-PHONE"),
            DevicePlatform::Android if self.state.lock().unwrap().fold.is_some() => {
                DeviceId::android("Fake_Fold")
            }
            DevicePlatform::Android => DeviceId::android("Fake_Phone"),
        }
    }

    pub fn log(&self) -> Vec<FakeEvent> {
        self.state.lock().unwrap().log.clone()
    }

    fn record(&self, event: FakeEvent) {
        self.state.lock().unwrap().log.push(event);
    }
}

impl Backend for FakeBackend {
    fn platform(&self) -> DevicePlatform {
        self.platform
    }

    fn support(&self) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async { Ok(Ok(())) })
    }

    fn list(&self) -> BoxFut<'_, Vec<DeviceInfo>> {
        Box::pin(async move {
            let (running, fold, (width, height)) = {
                let state = self.state.lock().unwrap();
                (state.running, state.fold.clone(), state.screen())
            };
            Ok(vec![DeviceInfo {
                id: self.id(),
                platform: self.platform,
                name: if fold.is_some() {
                    "Fake Fold".into()
                } else {
                    "Fake Phone".into()
                },
                os: match self.platform {
                    DevicePlatform::Ios => "iOS 0.0".into(),
                    DevicePlatform::Android => "Android 0".into(),
                },
                state: if running {
                    DeviceState::Running
                } else {
                    DeviceState::Stopped
                },
                width,
                height,
                scale: 1.0,
                error: None,
                agent: None,
                physical: false,
                fold,
            }])
        })
    }

    fn boot<'a>(&'a self, _native: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move {
            self.record(FakeEvent::Boot);
            tokio::time::sleep(self.boot_time).await;
            self.state.lock().unwrap().running = true;
            Ok(())
        })
    }

    fn shutdown<'a>(&'a self, _native: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move {
            self.record(FakeEvent::Shutdown);
            self.state.lock().unwrap().running = false;
            Ok(())
        })
    }

    fn attach<'a>(&'a self, _native: &'a str) -> BoxFut<'a, Arc<dyn Screen>> {
        Box::pin(async move {
            if !self.state.lock().unwrap().running {
                anyhow::bail!("not running");
            }
            let screen = Arc::new(FakeScreen::new(self.state.clone()));
            *self.screen.lock().unwrap() = Arc::downgrade(&screen);
            Ok(screen as Arc<dyn Screen>)
        })
    }

    fn app<'a>(&'a self, _native: &'a str, app: DeviceApp) -> BoxFut<'a, ()> {
        Box::pin(async move {
            self.record(FakeEvent::App(app));
            Ok(())
        })
    }

    fn set<'a>(&'a self, _native: &'a str, setting: DeviceSetting) -> BoxFut<'a, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            if matches!(
                setting,
                DeviceSetting::Posture { .. } | DeviceSetting::Hinge { .. }
            ) && state.fold.is_none()
            {
                anyhow::bail!("This device doesn't fold.");
            }
            state.apply(&setting);
            state.log.push(FakeEvent::Set(setting));
            drop(state);
            if let Some(screen) = self.screen.lock().unwrap().upgrade() {
                screen.draw();
            }
            Ok(())
        })
    }

    fn settings<'a>(&'a self, _native: &'a str) -> BoxFut<'a, DeviceSettings> {
        Box::pin(async move { Ok(self.state.lock().unwrap().settings.clone()) })
    }
}

struct FakeScreen {
    state: Arc<Mutex<FakeState>>,
    pictures: watch::Sender<Option<Arc<Picture>>>,
    ended: watch::Sender<Option<String>>,
    look: Mutex<Look>,
}

#[derive(Clone, Copy)]
struct Look {
    hue: u8,
    dot: Option<(f32, f32)>,
    landscape: bool,
}

impl FakeScreen {
    fn new(state: Arc<Mutex<FakeState>>) -> Self {
        let screen = Self {
            state,
            pictures: watch::channel(None).0,
            ended: watch::channel(None).0,
            look: Mutex::new(Look {
                hue: 0,
                dot: None,
                landscape: false,
            }),
        };
        screen.draw();
        screen
    }

    fn dims(&self, look: &Look) -> (u32, u32) {
        let (w, h) = self.state.lock().unwrap().screen();
        if look.landscape { (h, w) } else { (w, h) }
    }

    fn draw(&self) {
        let look = *self.look.lock().unwrap();
        let (w, h) = self.dims(&look);
        let base = [
            [40u8, 90, 160],
            [150, 60, 60],
            [60, 140, 80],
            [130, 90, 160],
        ][look.hue as usize % 4];
        let mut img = image::RgbImage::from_pixel(w, h, image::Rgb(base));
        if let Some((x, y)) = look.dot {
            let (cx, cy) = ((x * w as f32) as i64, (y * h as f32) as i64);
            for dy in -12i64..=12 {
                for dx in -12i64..=12 {
                    let (px, py) = (cx + dx, cy + dy);
                    if dx * dx + dy * dy <= 144
                        && (0..w as i64).contains(&px)
                        && (0..h as i64).contains(&py)
                    {
                        img.put_pixel(px as u32, py as u32, image::Rgb([255, 255, 255]));
                    }
                }
            }
        }
        let mut jpeg = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80);
        if img.write_with_encoder(encoder).is_ok() {
            self.pictures.send_replace(Some(Arc::new(Picture {
                jpeg,
                width: w,
                height: h,
            })));
        }
    }
}

impl Screen for FakeScreen {
    fn pictures(&self) -> watch::Receiver<Option<Arc<Picture>>> {
        self.pictures.subscribe()
    }

    fn watching(&self, _on: bool) {}

    fn input(&self, input: DeviceInput) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if !self.state.lock().unwrap().running {
                self.ended
                    .send_replace(Some("The device was shut down.".into()));
                anyhow::bail!("not running");
            }
            {
                let mut look = self.look.lock().unwrap();
                match &input {
                    DeviceInput::Touch { phase, x, y } => {
                        look.dot = (*phase != TouchPhase::Up && *phase != TouchPhase::Cancel)
                            .then_some((*x, *y));
                    }
                    // Several fingers: the dot where the first one is.
                    DeviceInput::Touches { phase, touches } => {
                        look.dot = (*phase != TouchPhase::Up && *phase != TouchPhase::Cancel)
                            .then(|| touches.first().map(|t| (t.x, t.y)))
                            .flatten();
                    }
                    DeviceInput::Button {
                        button: DeviceButton::Home,
                    } => look.hue = look.hue.wrapping_add(1),
                    DeviceInput::Button {
                        button: DeviceButton::RotateLeft | DeviceButton::RotateRight,
                    } => look.landscape = !look.landscape,
                    _ => {}
                }
            }
            self.state.lock().unwrap().log.push(FakeEvent::Input(input));
            self.draw();
            Ok(())
        })
    }

    fn capture(&self) -> BoxFut<'_, Vec<u8>> {
        Box::pin(async move {
            let look = *self.look.lock().unwrap();
            let (w, h) = self.dims(&look);
            let img = image::RgbImage::from_pixel(w, h, image::Rgb([40, 90, 160]));
            let mut png = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
            Ok(png)
        })
    }

    fn size(&self) -> Option<(u32, u32, f32)> {
        let look = *self.look.lock().unwrap();
        let (w, h) = self.dims(&look);
        Some((w, h, 1.0))
    }

    fn ended(&self) -> watch::Receiver<Option<String>> {
        self.ended.subscribe()
    }
}
