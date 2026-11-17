//! A phone that needs neither Xcode nor the Android SDK: for tests, and for
//! trying the device panel (`AGENTLOFT_FAKE_DEVICES=1` on a daemon). Its
//! screen is a coloured picture with a dot where it was last touched; it
//! keeps everything it was sent in [`FakeBackend::log`].

use super::{Backend, BoxFut, Picture, Screen};
use loft_protocol::{
    DeviceApp, DeviceButton, DeviceId, DeviceInfo, DeviceInput, DevicePlatform, DeviceSetting,
    DeviceState, TouchPhase,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

const WIDTH: u32 = 390;
const HEIGHT: u32 = 844;

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
}

struct FakeState {
    running: bool,
    log: Vec<FakeEvent>,
}

impl FakeBackend {
    /// One fake device of `platform`, stopped, that boots in `boot_time`.
    pub fn new(platform: DevicePlatform, boot_time: Duration) -> Self {
        Self {
            platform,
            state: Arc::new(Mutex::new(FakeState {
                running: false,
                log: Vec::new(),
            })),
            boot_time,
        }
    }

    pub fn id(&self) -> DeviceId {
        match self.platform {
            DevicePlatform::Ios => DeviceId::ios("FAKE-PHONE"),
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
            let running = self.state.lock().unwrap().running;
            Ok(vec![DeviceInfo {
                id: self.id(),
                platform: self.platform,
                name: "Fake Phone".into(),
                os: match self.platform {
                    DevicePlatform::Ios => "iOS 0.0".into(),
                    DevicePlatform::Android => "Android 0".into(),
                },
                state: if running {
                    DeviceState::Running
                } else {
                    DeviceState::Stopped
                },
                width: WIDTH,
                height: HEIGHT,
                scale: 1.0,
                error: None,
                agent: None,
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
            let screen = FakeScreen::new(self.state.clone());
            Ok(Arc::new(screen) as Arc<dyn Screen>)
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
            self.record(FakeEvent::Set(setting));
            Ok(())
        })
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

    fn dims(look: &Look) -> (u32, u32) {
        if look.landscape {
            (HEIGHT, WIDTH)
        } else {
            (WIDTH, HEIGHT)
        }
    }

    fn draw(&self) {
        let look = *self.look.lock().unwrap();
        let (w, h) = Self::dims(&look);
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
            let (w, h) = Self::dims(&look);
            let img = image::RgbImage::from_pixel(w, h, image::Rgb([40, 90, 160]));
            let mut png = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
            Ok(png)
        })
    }

    fn size(&self) -> Option<(u32, u32, f32)> {
        let look = *self.look.lock().unwrap();
        let (w, h) = Self::dims(&look);
        Some((w, h, 1.0))
    }

    fn ended(&self) -> watch::Receiver<Option<String>> {
        self.ended.subscribe()
    }
}
