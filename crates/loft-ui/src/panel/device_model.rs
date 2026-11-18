//! The Device panel's 3D model (ADR-0020): the device drawn as a phone,
//! tablet or foldable that the person can turn, its live screen mapped onto
//! it and still touchable; and for foldables, the controls that fold it.
//!
//! The model is drawn by `loft-raster` (GPUI has no 3D) into a picture the
//! size of the panel at the window's scale, only when something it shows
//! changed (the device's picture, the view, the fold) and at most about 30
//! times a second. Each screen pixel of that picture knows which point of
//! the device's picture it shows, so a press there is a touch exactly where
//! it lands; a press anywhere else turns the model.
//!
//! A child of `device.rs` so it can reach the view's own fields; that file
//! only asks it to draw and hands it presses it doesn't want.

use super::DeviceView;
use crate::theme::Palette;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{Disableable as _, Selectable as _, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, CursorStyle, DispatchPhase, Entity, ImageId, IntoElement,
    MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement as _, PinchEvent, Pixels, Point,
    RenderImage, ScrollEvent, SharedString, Styled as _, Subscription, Window, div, img, px,
};
use loft_protocol::{
    DeviceFold, DevicePlatform, DevicePosture, DeviceSetting, DeviceState, FoldAxis, Request,
};
use loft_raster::device::{self as model, Kind, Live, Model, Scene, Style, View};
use loft_raster::{Camera, Order, OwnedTexture, Raster, Texture};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

/// Degrees the model turns for each point the pointer moves.
const TURN_PER_POINT: f32 = 0.45;
/// It tips up or down this far at most (never quite upside down).
const PITCH_LIMIT: f32 = 85.;
/// At most this often a new picture of the model (about 30 a second).
const FRAME_EVERY: Duration = Duration::from_millis(33);
/// How close the camera may come, and how far it may go.
const ZOOM: (f32, f32) = (0.6, 2.4);
/// A drag that stopped this long before the button was let go doesn't
/// spin on.
const FLICK_WITHIN: Duration = Duration::from_millis(80);
/// Degrees a second a flick spins the model at most.
const MAX_SPIN: f32 = 720.;
/// The hinge is sent this often at most while it is dragged.
const HINGE_EVERY: Duration = Duration::from_millis(200);
/// Pixels of the model's picture at most (a huge window draws it at a
/// lower scale rather than slowly).
const MAX_PIXELS: f32 = 5_000_000.;

/// What the 3D view keeps between frames.
pub(super) struct Model3d {
    /// The person asked for 3D (the floating card is always 2D).
    pub on: bool,
    view: View,
    /// Degrees a second it still turns after a flick (yaw, pitch).
    velocity: (f32, f32),
    turn: Option<Turn>,
    tween: Option<Tween>,
    /// The hinge as drawn: it eases toward the device's.
    shown_hinge: Option<f32>,
    /// A hinge angle being dragged (slider or pinch), shown before the
    /// device has taken it.
    preview: Option<Preview>,
    hinge_sent: Option<Instant>,
    /// A hinge to set once the device can take it (the dev hook's).
    want_hinge: Option<f32>,
    /// The device's screens as first seen (upright, a guess for a device
    /// first seen turned): a foldable's main screen is seen only while it
    /// is open, its cover only while shut.
    natural_main: Option<(f32, f32)>,
    natural_cover: Option<(f32, f32)>,
    raster: Raster,
    image: Option<Arc<RenderImage>>,
    drawn: Option<Key>,
    drawn_at: Option<Instant>,
    /// What the picture shows and where, for presses on it.
    scene: Option<Drawn>,
    /// The device's picture made smaller, for a model drawn small.
    mips: Option<(ImageId, Vec<OwnedTexture>)>,
    /// A redraw is already asked for.
    wake: bool,
    last_tick: Option<Instant>,
    /// Frame times since the last report.
    times: (f64, u32, Option<Instant>),
    slider: Option<(Entity<SliderState>, Subscription)>,
    /// The screen a touch began on (it stays on it).
    touch_screen: Cell<Option<u8>>,
    /// The pointer is over the live screen (else it would turn the model).
    over_screen: bool,
    /// A pinch going on: over the device, it folds; else it zooms.
    pinch_folds: Option<bool>,
    /// The screen the live picture was on when last drawn: when it moves
    /// to the other, the model turns to show it.
    last_live: Option<Live>,
    /// When the device was last folded (asked here, or its live screen
    /// changed): turning the model doesn't fold it again for a moment.
    folded_at: Option<Instant>,
    /// A foldable's last picture of each screen (main, cover), and how it
    /// was turned: while the model's hinge already shows the other screen
    /// and the device hasn't switched yet, that screen shows its last
    /// picture rather than nothing.
    last_of: [Option<(Arc<RenderImage>, i8)>; 2],
    /// The screen the device's own pictures were last of.
    device_live: Option<Live>,
}

struct Turn {
    last: Point<Pixels>,
    at: Instant,
}

struct Tween {
    from: View,
    to: View,
    start: Instant,
    length: Duration,
}

#[derive(Clone, Copy)]
struct Preview {
    degrees: f32,
    /// Let go: it shows until the device says so (or a moment passes).
    released: Option<Instant>,
    /// A posture asked for: done once the device is in it (its hinge may
    /// not be the one shown, as a half-open device's isn't).
    posture: Option<DevicePosture>,
}

/// Everything a picture of the model depends on.
#[derive(Clone, PartialEq)]
struct Key {
    size: (usize, usize),
    view: View,
    model: Model,
    live: Live,
    roll: i8,
    picture: Option<ImageId>,
}

struct Drawn {
    scene: Scene,
    camera: Camera,
    /// Picture pixels a point. (Where the panel is comes from its area,
    /// as it is now: it may move without the model being drawn again.)
    per_point: f32,
}

impl Default for Model3d {
    fn default() -> Self {
        Self {
            on: false,
            view: View::default(),
            velocity: (0., 0.),
            turn: None,
            tween: None,
            shown_hinge: None,
            preview: None,
            hinge_sent: None,
            want_hinge: None,
            natural_main: None,
            natural_cover: None,
            raster: Raster::new(Order::Bgra),
            image: None,
            drawn: None,
            drawn_at: None,
            scene: None,
            mips: None,
            wake: false,
            last_tick: None,
            times: (0., 0, None),
            slider: None,
            touch_screen: Cell::new(None),
            over_screen: false,
            pinch_folds: None,
            last_live: None,
            folded_at: None,
            last_of: [None, None],
            device_live: None,
        }
    }
}

impl Model3d {
    /// Another device: forget what was learned about this one.
    pub(super) fn forget(&mut self, cx: &mut gpui_kit::App) {
        let on = self.on;
        self.release(cx);
        *self = Self {
            on,
            ..Self::default()
        };
    }

    /// Free the model's picture (its texture).
    pub(super) fn release(&mut self, cx: &mut gpui_kit::App) {
        if let Some(old) = self.image.take() {
            cx.drop_image(old, None);
        }
        self.drawn = None;
        self.scene = None;
    }

    pub(super) fn is_turning(&self) -> bool {
        self.turn.is_some()
    }

    /// Move the view on, as time passes: a tween, a flick's spin, the hinge
    /// easing toward `hinge`. Whether there is more to come, and whether a
    /// spin just came to rest.
    fn tick(&mut self, now: Instant, fold: Option<(f32, DevicePosture)>) -> (bool, bool) {
        let hinge = fold.map(|(h, _)| h);
        let dt = self
            .last_tick
            .map_or(0., |t| now.duration_since(t).as_secs_f32().min(0.1));
        self.last_tick = Some(now);
        let mut rested = false;
        if let Some(t) = &self.tween {
            let k = now.duration_since(t.start).as_secs_f32() / t.length.as_secs_f32();
            if k >= 1. {
                self.view = View {
                    yaw: wrap(t.to.yaw),
                    ..t.to
                };
                self.tween = None;
            } else {
                let e = 1. - (1. - k).powi(3);
                let mix = |a: f32, b: f32| a + (b - a) * e;
                self.view = View {
                    yaw: mix(t.from.yaw, t.to.yaw),
                    pitch: mix(t.from.pitch, t.to.pitch),
                    zoom: mix(t.from.zoom, t.to.zoom),
                };
            }
        } else if self.turn.is_none() && self.velocity != (0., 0.) {
            self.view.yaw = wrap(self.view.yaw + self.velocity.0 * dt);
            self.view.pitch =
                (self.view.pitch + self.velocity.1 * dt).clamp(-PITCH_LIMIT, PITCH_LIMIT);
            let keep = (-5. * dt).exp();
            self.velocity = (self.velocity.0 * keep, self.velocity.1 * keep);
            if self.velocity.0.hypot(self.velocity.1) < 4. {
                self.velocity = (0., 0.);
                rested = true;
            }
        }
        // A dragged hinge let go shows until the device has it.
        if let Some(p) = self.preview
            && let Some(at) = p.released
            && (hinge.is_some_and(|h| (h - p.degrees).abs() < 1.)
                || p.posture
                    .is_some_and(|want| fold.is_some_and(|(_, now)| now == want))
                || now.duration_since(at) > PREVIEW_FOR)
        {
            self.preview = None;
        }
        let target = self.preview.map(|p| p.degrees).or(hinge);
        let mut easing = false;
        match (target, self.shown_hinge) {
            (Some(target), Some(shown)) if (target - shown).abs() > 0.3 => {
                // About as long as the device takes to swing its hinge.
                let k = 1. - (-HINGE_EASE * dt).exp();
                self.shown_hinge = Some(shown + (target - shown) * k);
                easing = true;
            }
            (target, _) => self.shown_hinge = target,
        }
        let more = self.tween.is_some() || self.velocity != (0., 0.) || easing;
        (more, rested)
    }
}

/// The view in which the model's front (+z) points along `dir` (in the
/// eye's space), the yaw taken nearest `now`'s so the turn is short.
fn facing_view(dir: loft_raster::Vec3, now: View) -> View {
    // Tipped by pitch after turning by yaw, +z goes to
    // (sin yaw, -cos yaw sin pitch, cos yaw cos pitch).
    let d = dir.normalized();
    let yaw = d.x.clamp(-1., 1.).asin().to_degrees();
    let pitch = (-d.y).atan2(d.z).to_degrees();
    let yaw = yaw + 360. * ((now.yaw - yaw) / 360.).round();
    View {
        yaw,
        pitch: pitch.clamp(-PITCH_LIMIT, PITCH_LIMIT),
        zoom: now.zoom,
    }
}

/// How long after a fold the device's pictures may be dark or turned the
/// old screen's way (an iPhone Duo's cover takes about three seconds).
const FOLD_SETTLES: Duration = Duration::from_secs(4);

/// Whether a picture is (nearly) all black: a screen just switched on or
/// off, which a fold shows instead of the screen's last picture.
fn is_dark(picture: &RenderImage) -> bool {
    let size = picture.size(0);
    let (w, h) = (size.width.0 as usize, size.height.0 as usize);
    let Some(bytes) = picture.as_bytes(0) else {
        return false;
    };
    if w == 0 || h == 0 || bytes.len() < w * h * 4 {
        return false;
    }
    const GRID: usize = 10;
    (0..GRID).all(|gy| {
        (0..GRID).all(|gx| {
            let (x, y) = ((2 * gx + 1) * w / (2 * GRID), (2 * gy + 1) * h / (2 * GRID));
            let i = (y * w + x) * 4;
            bytes[i] < 12 && bytes[i + 1] < 12 && bytes[i + 2] < 12
        })
    })
}

/// A fold-in's hinge from which its main screen, not its cover, is the one
/// seen (about where an iPhone Duo switches screens as it opens).
const FACES_OPEN: f32 = 70.;

/// How long a hinge asked for shows before the device has it: a fold of an
/// iPhone Duo can take over a second to start.
const PREVIEW_FOR: Duration = Duration::from_secs(4);

/// How fast the drawn hinge eases toward the device's (per second): it
/// gets there in about half a second, as the device's hinge does.
const HINGE_EASE: f32 = 7.;

/// The hinge a posture puts a device at, for showing it at once.
fn posture_hinge(posture: DevicePosture, fold: &loft_protocol::DeviceFold) -> f32 {
    let flat = 180_f32.clamp(fold.hinge_min, fold.hinge_max);
    match posture {
        DevicePosture::Closed => fold.hinge_min,
        DevicePosture::HalfOpened => ((fold.hinge_min + flat) / 2.).max(fold.hinge_min),
        DevicePosture::Opened => flat,
        DevicePosture::Flipped => fold.hinge_max,
        DevicePosture::Tent => (flat + fold.hinge_max) / 2.,
    }
}

/// Which of a foldable's screens a picture of `shape` (width, height) is
/// of, and whether it is turned on that screen. The screen is the one whose
/// upright shape is nearest the picture's (a foldable's two screens differ
/// clearly that way, whereas one screen turned can look much like the
/// other upright: an iPhone Duo's inner screen on its side is nearly its
/// cover's shape); `on_cover` (what the device last said) decides a close
/// call.
fn which_screen(
    shape: (f32, f32),
    main: (f32, f32),
    cover: (f32, f32),
    on_cover: bool,
) -> (Live, bool) {
    let ratio = |(w, h): (f32, f32)| (w.max(1.) / h.max(1.)).ln();
    let of = ratio(shape);
    let off = |r: f32| (of - r).abs();
    let (main_off, cover_off) = (off(ratio(main)), off(ratio(cover)));
    let screen = if (main_off - cover_off).abs() < 0.05 {
        if on_cover { Live::Cover } else { Live::Main }
    } else if cover_off < main_off {
        Live::Cover
    } else {
        Live::Main
    };
    (
        screen,
        turned_on(shape, if screen == Live::Cover { cover } else { main }),
    )
}

/// Whether a picture of `shape` is turned on its side on a screen of
/// `screen`'s upright shape.
fn turned_on(shape: (f32, f32), screen: (f32, f32)) -> bool {
    let ratio = |(w, h): (f32, f32)| (w.max(1.) / h.max(1.)).ln();
    let (of, r) = (ratio(shape), ratio(screen));
    (of + r).abs() + 0.05 < (of - r).abs()
}

/// An angle in degrees, kept within a turn either way.
fn wrap(degrees: f32) -> f32 {
    (degrees + 180.).rem_euclid(360.) - 180.
}

/// A foldable's main screen before it has been seen open: guessed from its
/// cover.
fn guess_main(kind: Kind, cover: (f32, f32)) -> (f32, f32) {
    match kind {
        Kind::Flip => (1.6 * cover.0, 3.8 * cover.0),
        _ => (2.05 * cover.0, 0.88 * cover.1),
    }
}

fn posture_label(posture: DevicePosture) -> &'static str {
    match posture {
        DevicePosture::Closed => "Closed",
        DevicePosture::HalfOpened => "Half open",
        DevicePosture::Opened => "Open",
        DevicePosture::Flipped => "Flipped",
        DevicePosture::Tent => "Tent",
    }
}

impl DeviceView {
    /// Whether the device is drawn as a 3D model now.
    pub(super) fn three_d(&self) -> bool {
        self.model.on && !self.floating
    }

    pub(super) fn toggle_3d(&mut self, cx: &mut Context<Self>) {
        self.model.on = !self.model.on;
        if !self.model.on {
            self.model.release(cx);
            self.model.turn = None;
        }
        cx.notify();
    }

    /// Show it in 3D, turned `yaw` and tipped `pitch` degrees, and fold it
    /// to `hinge` degrees once it can (the `device3d:` dev hook).
    pub fn show_3d(&mut self, yaw: f32, pitch: f32, hinge: Option<f32>, cx: &mut Context<Self>) {
        self.model.on = true;
        self.model.view = View {
            yaw: wrap(yaw),
            pitch: pitch.clamp(-PITCH_LIMIT, PITCH_LIMIT),
            zoom: 1.,
        };
        self.model.want_hinge = hinge;
        cx.notify();
    }

    /// Face-on again.
    fn restore_view(&mut self, cx: &mut Context<Self>) {
        self.tween_to(View::default(), Duration::from_millis(320));
        cx.notify();
    }

    fn tween_to(&mut self, to: View, length: Duration) {
        self.model.velocity = (0., 0.);
        self.model.tween = Some(Tween {
            from: self.model.view,
            to,
            start: Instant::now(),
            length,
        });
    }

    /// The device as a model: which, with the picture on which screen, and
    /// turned how far (a device turned on its side turns the model).
    fn describe(&mut self) -> Option<(Model, Live, i8)> {
        let info = self.device()?.clone();
        let running = info.state == DeviceState::Running;
        let picture = self.picture.as_ref().filter(|_| running);
        let (w, h) = if info.width > 0 && info.height > 0 {
            (info.width as f32, info.height as f32)
        } else if let Some(p) = picture {
            let s = p.size(0);
            (s.width.0 as f32, s.height.0 as f32)
        } else {
            (super::DEFAULT_SCREEN.0, super::DEFAULT_SCREEN.1)
        };
        let turned = |natural: (f32, f32)| {
            i8::from((natural.0 > natural.1) != (w > h) && (w - h).abs() > 1.)
        };
        let live = |screen| {
            if picture.is_some() {
                screen
            } else {
                Live::None
            }
        };
        if let Some(fold) = &info.fold {
            let kind = match fold.axis {
                FoldAxis::Vertical => Kind::Book,
                FoldAxis::Horizontal => Kind::Flip,
            };
            let pair = |(a, b): (u32, u32)| (a as f32, b as f32);
            // Both screens' shapes, as the device says them: then which
            // screen a picture is of, and whether it's turned, comes from
            // the picture's own shape, which can't disagree with what it
            // shows (the device's state and its pictures arrive apart
            // while it folds).
            if let (Some(main), Some(cover)) = (
                fold.main_size.map(pair),
                fold.cover_size.map(pair).filter(|_| fold.cover_screen),
            ) {
                let shape = picture.map_or((w, h), |p| {
                    let s = p.size(0);
                    (s.width.0 as f32, s.height.0 as f32)
                });
                // The daemon says which screen a picture is of; only its
                // shape is left to say whether it's turned on it.
                let (screen, turned) = match self.picture_on_cover.filter(|_| picture.is_some()) {
                    Some(on_cover) => {
                        let screen = if on_cover { Live::Cover } else { Live::Main };
                        (
                            screen,
                            turned_on(shape, if on_cover { cover } else { main }),
                        )
                    }
                    None => which_screen(shape, main, cover, fold.on_cover),
                };
                tracing::trace!(
                    ?shape, ?main, ?cover, on_cover = fold.on_cover, ?screen, turned,
                    hinge = ?self.model.shown_hinge, "which screen"
                );
                let model = Model {
                    kind,
                    main,
                    cover: Some(cover),
                    inward: fold.inward,
                    hinge: self.model.shown_hinge.unwrap_or(fold.hinge),
                };
                return Some((model, live(screen), i8::from(turned)));
            }
            let m = &mut self.model;
            let seen = if fold.on_cover {
                *m.natural_cover.get_or_insert((w, h))
            } else {
                *m.natural_main.get_or_insert((w, h))
            };
            let cover_seen = m.natural_cover;
            let main = m
                .natural_main
                .unwrap_or_else(|| guess_main(kind, cover_seen.unwrap_or((w, h))));
            let cover = fold
                .cover_screen
                .then(|| cover_seen.unwrap_or_else(|| model::guess_cover(kind, main)));
            let on_cover = fold.on_cover && cover.is_some();
            let model = Model {
                kind,
                main,
                cover,
                inward: fold.inward,
                hinge: m.shown_hinge.unwrap_or(fold.hinge),
            };
            let screen = if on_cover { Live::Cover } else { Live::Main };
            return Some((model, live(screen), turned(seen)));
        }
        let style = match info.platform {
            DevicePlatform::Ios => Style::Ios,
            DevicePlatform::Android => Style::Android,
        };
        let (long, short) = (w.max(h), w.min(h));
        let name = info.name.to_lowercase();
        let tablet = name.contains("ipad") || name.contains("tablet") || long / short < 1.5;
        // Android tablets stand on their long side; everything else upright.
        let natural = if tablet && style == Style::Android {
            (long, short)
        } else {
            (short, long)
        };
        let model = Model {
            kind: if tablet {
                Kind::Tablet(style)
            } else {
                Kind::Phone(style)
            },
            main: natural,
            cover: None,
            inward: true,
            hinge: 180.,
        };
        Some((model, live(Live::Main), turned(natural)))
    }

    /// Draw the model again if what it shows changed (at most every
    /// [`FRAME_EVERY`]), and keep animations going. Called while the view
    /// renders.
    pub(super) fn prepare_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(area) = self.area.get() else {
            return;
        };
        let now = Instant::now();
        let fold = self.device().and_then(|d| d.fold.clone());
        let (more, rested) = self
            .model
            .tick(now, fold.as_ref().map(|f| (f.hinge, f.posture)));
        if rested {
            self.after_turn(cx);
        }
        if let (Some(degrees), Some(_), true) = (self.model.want_hinge, &fold, self.is_running()) {
            self.model.want_hinge = None;
            self.send_hinge(degrees, cx);
        }
        let Some((m, mut live, mut roll)) = self.describe() else {
            return;
        };
        let mut picture = self.picture.clone().filter(|_| live != Live::None);
        // A fold-in's screen facing out is the cover while the hinge is
        // nearly shut, the main one once it opens: it shows the device's
        // live picture once the device has switched, and its own last
        // picture until then (a fold takes the device longer than the
        // model).
        if matches!(m.kind, Kind::Book | Kind::Flip) && m.cover.is_some() && m.inward {
            let slot = |screen: Live| match screen {
                Live::Cover => Some(1),
                Live::Main => Some(0),
                Live::None => None,
            };
            // The device switched screens: a fold is settling.
            if live != Live::None {
                if self.model.device_live.is_some_and(|before| before != live) {
                    self.model.folded_at = Some(now);
                }
                self.model.device_live = Some(live);
            }
            // While a fold settles (the device switches screens, blanks
            // them and turns its interface the old screen's way for a few
            // seconds), a picture that is dark or turned isn't shown: that
            // screen's last good one is. (Settled, a foldable's screens
            // show their interface upright.)
            let settling = self
                .model
                .folded_at
                .is_some_and(|at| now.duration_since(at) < FOLD_SETTLES);
            let live_good = picture
                .as_ref()
                .is_some_and(|p| !(settling && (roll != 0 || is_dark(p))));
            if live_good && let (Some(p), Some(i)) = (&picture, slot(live)) {
                self.model.last_of[i] = Some((p.clone(), roll));
            }
            let facing = if m.hinge < FACES_OPEN {
                Live::Cover
            } else {
                Live::Main
            };
            if live != Live::None
                && (facing != live || !live_good)
                && let Some((p, r)) = slot(facing).and_then(|i| self.model.last_of[i].clone())
            {
                tracing::trace!(?facing, from = ?live, live_good, "showing the facing screen's last picture");
                live = facing;
                roll = r;
                picture = Some(p);
            }
        }
        // The live picture moved to the other screen (the device folded or
        // unfolded): turn the model to show it, unless it is being turned.
        if live != Live::None {
            if let Some(before) = self.model.last_live
                && before != live
            {
                self.model.folded_at = Some(now);
                let view = self.model.view;
                if self.model.turn.is_none() && view.yaw.abs() > 75. {
                    self.tween_to(View { yaw: 0., ..view }, Duration::from_millis(450));
                    self.wake_in(FRAME_EVERY, cx);
                }
            }
            self.model.last_live = Some(live);
        }
        let (aw, ah) = (f32::from(area.size.width), f32::from(area.size.height));
        let mut scale = window.scale_factor();
        if aw * ah * scale * scale > MAX_PIXELS {
            scale = (MAX_PIXELS / (aw * ah)).sqrt();
        }
        let (w, h) = ((aw * scale).round() as usize, (ah * scale).round() as usize);
        if w < 4 || h < 4 {
            return;
        }
        let key = Key {
            size: (w, h),
            view: self.model.view,
            model: m,
            live,
            roll,
            picture: picture.as_ref().map(|p| p.id),
        };
        if self.model.drawn.as_ref() != Some(&key) {
            let since = self.model.drawn_at.map(|t| now.duration_since(t));
            match since {
                Some(since) if since < FRAME_EVERY => self.wake_in(FRAME_EVERY - since, cx),
                _ => {
                    self.draw_model(key, picture, scale, window, cx);
                }
            }
        }
        if more {
            self.wake_in(FRAME_EVERY, cx);
        }
    }

    fn wake_in(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.model.wake, true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this.model.wake = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn draw_model(
        &mut self,
        key: Key,
        picture: Option<Arc<RenderImage>>,
        scale: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (w, h) = key.size;
        let scene = model::scene(&key.model, key.live, key.roll);
        let camera = model::camera(w, h, scene.extent, key.view);
        let m = &mut self.model;
        // The picture, made smaller first if it is drawn at half its size
        // or less.
        let base = picture.as_ref().and_then(|p| {
            let size = p.size(0);
            Some((
                p.id,
                Texture {
                    width: size.width.0 as usize,
                    height: size.height.0 as usize,
                    pixels: p.as_bytes(0)?,
                },
            ))
        });
        let texture = base.map(|(id, base)| {
            let screen = match key.live {
                Live::Cover => key.model.cover.unwrap_or(key.model.main),
                _ => key.model.main,
            };
            let long = |(a, b): (f32, f32)| a.max(b);
            let per_texel = camera.focal / camera.distance * long(screen)
                / long((base.width as f32, base.height as f32)).max(1.);
            let level = model::mip_level(per_texel);
            if m.mips.as_ref().is_none_or(|(of, _)| *of != id) {
                m.mips = Some((id, Vec::new()));
            }
            let levels = &mut m.mips.as_mut().expect("just set").1;
            while levels.len() < level {
                let next = levels
                    .last()
                    .map_or_else(|| base.half(), |t| t.as_texture().half());
                levels.push(next);
            }
            match level {
                0 => base,
                n => levels[n - 1].as_texture(),
            }
        });
        let start = Instant::now();
        let mut color = vec![0u8; w * h * 4];
        model::render(&scene, &camera, &mut m.raster, &mut color, texture);
        let took = start.elapsed();
        let Some(pixels) = image::RgbaImage::from_raw(w as u32, h as u32, color) else {
            return;
        };
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]));
        if let Some(old) = m.image.replace(image) {
            cx.drop_image(old, Some(window));
        }
        m.drawn = Some(key);
        m.drawn_at = Some(Instant::now());
        m.scene = Some(Drawn {
            scene,
            camera,
            per_point: scale,
        });
        // How long frames take, now and then.
        let (total, n, since) = &mut m.times;
        *total += took.as_secs_f64() * 1000.;
        *n += 1;
        let since = *since.get_or_insert(start);
        if start.duration_since(since) > Duration::from_secs(2) {
            tracing::debug!(
                "3D device model: {:.2} ms a frame on average, {} frames, {w}×{h}",
                *total / f64::from(*n),
                *n
            );
            m.times = (0., 0, Some(start));
        }
    }

    /// The model's picture, over the panel.
    pub(super) fn model_view(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let image = self.model.image.clone()?;
        let drawn = self.model.scene.as_ref()?;
        let (w, h) = self.model.drawn.as_ref()?.size;
        let faced = self.model.view == View::default() && self.model.tween.is_none();
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(
                    img(image)
                        .absolute()
                        .top_0()
                        .left_0()
                        .w(px(w as f32 / drawn.per_point))
                        .h(px(h as f32 / drawn.per_point)),
                )
                .when(!faced, |d| {
                    d.child(
                        div().absolute().top(px(8.)).right(px(8.)).child(
                            Button::new("device-restore-view")
                                .cursor_pointer()
                                .small()
                                .icon(gpui_kit::assets::IconName::Undo2)
                                .label("Restore view")
                                .on_click(cx.listener(|this, _, _, cx| this.restore_view(cx))),
                        ),
                    )
                })
                .into_any_element(),
        )
    }

    /// The pointer's place in the model's picture (its pixels).
    fn model_pixel(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let d = self.model.scene.as_ref()?;
        let origin = self.area.get()?.origin;
        Some((
            f32::from(position.x - origin.x) * d.per_point,
            f32::from(position.y - origin.y) * d.per_point,
        ))
    }

    /// A window position on the live screen as a fraction of its picture:
    /// pressed (`clamp` false) only where the screen is drawn; dragged on
    /// (`clamp`), kept on the screen the touch began on.
    pub(super) fn screen_point_3d(
        &self,
        position: Point<Pixels>,
        clamp: bool,
    ) -> Option<(f32, f32)> {
        let (x, y) = self.model_pixel(position)?;
        let d = self.model.scene.as_ref()?;
        if clamp {
            let id = self.model.touch_screen.get()?;
            return model::pick(&d.scene, &d.camera, id, x, y);
        }
        if x < 0. || y < 0. {
            return None;
        }
        let hit = self.model.raster.hit(x as usize, y as usize)?;
        self.model.touch_screen.set(Some(hit.id));
        Some((hit.u, hit.v))
    }

    /// Whether the model covers the pointer's place.
    fn over_model(&self, position: Point<Pixels>) -> bool {
        self.model_pixel(position).is_some_and(|(x, y)| {
            x >= 0. && y >= 0. && self.model.raster.covered(x as usize, y as usize)
        })
    }

    /// A press off the live screen: turn the model.
    pub(super) fn start_turn(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        // A control over the model (Restore view) has it.
        if crate::press::claimed(cx) {
            return;
        }
        tracing::debug!(?position, view = ?self.model.view, "3D: turning");
        gpui_kit::component::GlobalState::suppress_text_selection(cx);
        cx.stop_propagation();
        self.model.turn = Some(Turn {
            last: position,
            at: Instant::now(),
        });
        self.model.velocity = (0., 0.);
        self.model.tween = None;
        cx.notify();
    }

    fn turn_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(turn) = &mut self.model.turn else {
            return;
        };
        let now = Instant::now();
        let dt = now.duration_since(turn.at).as_secs_f32().max(1e-3);
        let dx = f32::from(position.x - turn.last.x) * TURN_PER_POINT;
        let dy = f32::from(position.y - turn.last.y) * TURN_PER_POINT;
        tracing::trace!(dx, dy, "3D: turned");
        turn.last = position;
        turn.at = now;
        let v = &mut self.model.velocity;
        let fling = |a: f32, b: f32| (0.5 * a + 0.5 * b / dt).clamp(-MAX_SPIN, MAX_SPIN);
        *v = (fling(v.0, dx), fling(v.1, dy));
        let view = &mut self.model.view;
        view.yaw = wrap(view.yaw + dx);
        view.pitch = (view.pitch + dy).clamp(-PITCH_LIMIT, PITCH_LIMIT);
        cx.notify();
    }

    fn end_turn(&mut self, cx: &mut Context<Self>) {
        let Some(turn) = self.model.turn.take() else {
            return;
        };
        if turn.at.elapsed() > FLICK_WITHIN {
            self.model.velocity = (0., 0.);
        }
        if self.model.velocity == (0., 0.) {
            self.after_turn(cx);
        }
        cx.notify();
    }

    /// Turned so the cover faces the eye while the device is open: fold it
    /// shut so the cover is the live screen, and turn the model so the
    /// cover still faces the eye once shut. A device the model can't fold
    /// that way (no cover, no Closed posture) is left as it is.
    fn after_turn(&mut self, cx: &mut Context<Self>) {
        let Some(fold) = self.device().and_then(|d| d.fold.clone()) else {
            return;
        };
        // Just folded: the model turned itself to show the live screen, or
        // the device is still on its way.
        if self
            .model
            .folded_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(3))
        {
            return;
        }
        if !self.is_running()
            || fold.on_cover
            || !fold.cover_screen
            || !fold.inward
            || !matches!(
                fold.posture,
                DevicePosture::HalfOpened | DevicePosture::Opened
            )
            || !fold.postures.contains(&DevicePosture::Closed)
        {
            return;
        }
        let Some(d) = &self.model.scene else {
            return;
        };
        let camera = model::camera(
            d.camera.width,
            d.camera.height,
            d.scene.extent,
            self.model.view,
        );
        if model::facing(&d.scene, &camera) != Live::Cover {
            return;
        }
        let Some(normal) = d.scene.cover_normal else {
            return;
        };
        tracing::debug!("the cover faces the eye: folding the device shut");
        self.set_posture(DevicePosture::Closed, cx);
        // Shut, the cover faces the model's front: turn the view so it
        // faces the eye just as it does now.
        let to = facing_view(camera.rotation.apply(normal), self.model.view);
        self.tween_to(to, Duration::from_millis(450));
    }

    /// The wheel off the live screen: closer or further.
    pub(super) fn zoom_3d(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(window.line_height());
        self.zoom_by((f32::from(delta.y) * 0.003).exp(), cx);
        cx.stop_propagation();
    }

    fn zoom_by(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.model.tween = None;
        self.model.view.zoom = (self.model.view.zoom * factor).clamp(ZOOM.0, ZOOM.1);
        cx.notify();
    }

    /// A trackpad pinch: over a foldable, folds it (pinching out opens);
    /// anywhere else, zooms.
    pub(super) fn on_pinch_3d(
        &mut self,
        event: &PinchEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.three_d() {
            return;
        }
        cx.stop_propagation();
        let fold = self.device().and_then(|d| d.fold.clone());
        let starts = fold.is_some() && self.is_running() && self.over_model(event.position);
        let folds = *self.model.pinch_folds.get_or_insert(starts);
        let ended = matches!(
            event.phase,
            gpui_kit::TouchPhase::Ended | gpui_kit::TouchPhase::Cancelled
        );
        if ended {
            self.model.pinch_folds = None;
        }
        match fold.filter(|_| folds) {
            Some(fold) => {
                let from = self.model.preview.map_or(fold.hinge, |p| p.degrees);
                let degrees = (from + event.delta * 150.).clamp(fold.hinge_min, fold.hinge_max);
                self.drag_hinge(degrees, ended, cx);
            }
            None => self.zoom_by(1. + event.delta, cx),
        }
    }

    /// The pointer moved over the panel: which cursor.
    pub(super) fn hover_3d(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.three_d() || event.pressed_button.is_some() {
            return;
        }
        let over = self.is_running()
            && self.model_pixel(event.position).is_some_and(|(x, y)| {
                x >= 0. && y >= 0. && self.model.raster.hit(x as usize, y as usize).is_some()
            });
        if over != self.model.over_screen {
            self.model.over_screen = over;
            cx.notify();
        }
    }

    pub(super) fn cursor_3d(&self) -> CursorStyle {
        if self.model.turn.is_some() {
            CursorStyle::ClosedHand
        } else if self.model.over_screen {
            CursorStyle::PointingHand
        } else {
            CursorStyle::OpenHand
        }
    }

    // -- folding ------------------------------------------------------------------

    fn set_posture(&mut self, posture: DevicePosture, cx: &mut Context<Self>) {
        let Some(id) = self.chosen.clone() else {
            return;
        };
        // The model swings there at once; the device follows.
        let fold = self.device().and_then(|d| d.fold.clone());
        self.model.preview = fold.map(|f| Preview {
            degrees: posture_hinge(posture, &f),
            released: Some(Instant::now()),
            posture: Some(posture),
        });
        self.model.folded_at = Some(Instant::now());
        self.ask(
            Request::DeviceSet {
                id,
                setting: DeviceSetting::Posture { posture },
            },
            cx,
        );
        cx.notify();
    }

    fn send_hinge(&mut self, degrees: f32, cx: &mut Context<Self>) {
        let Some(id) = self.chosen.clone() else {
            return;
        };
        self.model.hinge_sent = Some(Instant::now());
        self.ask(
            Request::DeviceSet {
                id,
                setting: DeviceSetting::Hinge { degrees },
            },
            cx,
        );
    }

    /// The hinge dragged to `degrees` (slider or pinch): shown at once, sent
    /// now and then while dragged and when let go.
    fn drag_hinge(&mut self, degrees: f32, released: bool, cx: &mut Context<Self>) {
        tracing::trace!(degrees, released, "hinge dragged");
        self.model.preview = Some(Preview {
            degrees,
            released: released.then(Instant::now),
            posture: None,
        });
        self.model.folded_at = Some(Instant::now());
        if released
            || self
                .model
                .hinge_sent
                .is_none_or(|at| at.elapsed() >= HINGE_EVERY)
        {
            self.send_hinge(degrees, cx);
        }
        cx.notify();
    }

    /// A foldable's controls, under the toolbar: its postures and its
    /// hinge.
    pub(super) fn fold_bar(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let fold: DeviceFold = self.device()?.fold.clone()?;
        let running = self.is_running();
        let (min, max) = (fold.hinge_min, fold.hinge_max.max(fold.hinge_min + 1.));
        // The slider, made for this device's range, following the device
        // while nobody drags it.
        if self.model.slider.as_ref().is_none_or(|(s, _)| {
            let s = s.read(cx);
            s.min_value() != min || s.max_value() != max
        }) {
            let state = cx.new(|_| {
                SliderState::new()
                    .min(min)
                    .max(max)
                    .step(1.)
                    .default_value(fold.hinge.clamp(min, max))
            });
            let sub = cx.subscribe(&state, |this, _, event: &SliderEvent, cx| match event {
                SliderEvent::Change(v) => {
                    tracing::trace!(value = v.start(), "slider changed");
                    this.drag_hinge(v.start(), false, cx)
                }
                SliderEvent::Release(v) => {
                    tracing::trace!(value = v.start(), "slider let go");
                    this.drag_hinge(v.start(), true, cx)
                }
            });
            self.model.slider = Some((state, sub));
        }
        let (slider, _) = self.model.slider.as_ref()?;
        let slider = slider.clone();
        let shown = self.model.preview.map_or(fold.hinge, |p| p.degrees);
        if self.model.preview.is_none()
            && (slider.read(cx).value().start() - fold.hinge).abs() > 0.5
        {
            slider.update(cx, |s, cx| {
                s.set_value(fold.hinge.clamp(min, max), window, cx)
            });
        }
        let tip = |posture| match (posture, fold.axis) {
            (DevicePosture::HalfOpened, FoldAxis::Horizontal) => {
                "Half open, like a laptop (tabletop)"
            }
            (DevicePosture::HalfOpened, FoldAxis::Vertical) => "Half open, like a book",
            (DevicePosture::Closed, _) => "Folded shut",
            (DevicePosture::Opened, _) => "Open flat",
            (DevicePosture::Flipped, _) => "Folded back, the screen outward",
            (DevicePosture::Tent, _) => "Standing on its edges, like a tent",
        };
        let mut bar = h_flex()
            .h(px(36.))
            .flex_shrink_0()
            .px_2()
            .gap_0p5()
            .items_center()
            .border_b_1()
            .border_color(p.border);
        for posture in fold.postures.iter().copied() {
            bar = bar.child(
                Button::new(SharedString::from(format!("device-posture-{posture:?}")))
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .label(posture_label(posture))
                    .tooltip(tip(posture))
                    .selected(fold.posture == posture)
                    .disabled(!running)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_posture(posture, cx))),
            );
        }
        // The hinge: whatever room is left.
        Some(
            bar.child(
                div()
                    .flex_1()
                    .min_w(px(48.))
                    .max_w(px(160.))
                    .ml_2()
                    .child(Slider::new(&slider).disabled(!running)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(36.))
                    .text_right()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(format!("{shown:.0}°")),
            )
            .into_any_element(),
        )
    }
}

/// While the model is turned: the window's mouse, wherever it goes.
pub(super) fn listen_turn(window: &mut Window, entity: Entity<DeviceView>) {
    let e = entity.clone();
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        e.update(cx, |v, cx| match ev.pressed_button {
            Some(MouseButton::Left) => v.turn_to(ev.position, cx),
            // Let go where we didn't hear of it.
            _ => v.end_turn(cx),
        });
    });
    window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
            entity.update(cx, |v, cx| v.end_turn(cx));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angles_stay_within_a_turn() {
        assert_eq!(wrap(190.), -170.);
        assert_eq!(wrap(-190.), 170.);
        assert_eq!(wrap(45.), 45.);
        assert_eq!(wrap(540.), -180.);
    }

    #[test]
    fn a_view_can_be_found_that_faces_a_direction() {
        use loft_raster::device::camera;
        for (yaw, pitch) in [(0., 0.), (40., -20.), (-70., 30.), (10., 80.)] {
            let want = View {
                yaw,
                pitch,
                zoom: 1.,
            };
            let dir = camera(100, 100, (1., 1.), want)
                .rotation
                .apply(loft_raster::Vec3::Z);
            let got = facing_view(
                dir,
                View {
                    yaw: yaw + 350.,
                    ..View::default()
                },
            );
            let back = camera(100, 100, (1., 1.), got)
                .rotation
                .apply(loft_raster::Vec3::Z);
            assert!((back - dir).length() < 1e-3, "{want:?} -> {got:?}");
            // The nearer way round.
            assert!((got.yaw - (yaw + 360.)).abs() < 1e-2, "{got:?}");
        }
    }

    #[test]
    fn the_hinge_eases_toward_the_device() {
        let mut m = Model3d::default();
        let t0 = Instant::now();
        // First seen: at once.
        assert_eq!(
            m.tick(t0, Some((180., DevicePosture::Opened))),
            (false, false)
        );
        assert_eq!(m.shown_hinge, Some(180.));
        // Shut: on its way, then there.
        let (more, _) = m.tick(
            t0 + Duration::from_millis(30),
            Some((0., DevicePosture::Closed)),
        );
        assert!(more);
        let h = m.shown_hinge.unwrap();
        assert!(h < 180. && h > 0., "{h}");
        let mut t = t0 + Duration::from_millis(30);
        for _ in 0..40 {
            t += Duration::from_millis(33);
            m.tick(t, Some((0., DevicePosture::Closed)));
        }
        assert_eq!(m.shown_hinge, Some(0.));
        // A dragged hinge shows instead, until the device catches up.
        m.preview = Some(Preview {
            degrees: 90.,
            released: Some(t),
            posture: None,
        });
        for _ in 0..40 {
            t += Duration::from_millis(33);
            m.tick(t, Some((0., DevicePosture::Closed)));
        }
        assert!((m.shown_hinge.unwrap() - 90.).abs() < 0.5);
        m.tick(
            t + Duration::from_millis(33),
            Some((90., DevicePosture::HalfOpened)),
        );
        assert!(m.preview.is_none());
    }

    #[test]
    fn a_foldables_picture_is_put_on_the_screen_of_its_shape() {
        // An iPhone Duo: a wide inner screen, a tall cover.
        let (main, cover) = ((2853., 2007.), (1398., 2034.));
        assert_eq!(
            which_screen((1902., 1338.), main, cover, true),
            (Live::Main, false)
        );
        assert_eq!(
            which_screen((932., 1356.), main, cover, false),
            (Live::Cover, false)
        );
        // Turned on a known screen: only the picture turns.
        assert!(turned_on((1356., 932.), cover));
        assert!(!turned_on((932., 1356.), cover));
        assert!(!turned_on((1902., 1338.), main));
    }

    #[test]
    fn postures_put_the_hinge_where_they_mean() {
        let fold = loft_protocol::DeviceFold {
            axis: FoldAxis::Vertical,
            inward: true,
            cover_screen: true,
            postures: vec![],
            posture: DevicePosture::Closed,
            hinge: 0.,
            hinge_min: 0.,
            hinge_max: 180.,
            on_cover: true,
            main_size: None,
            cover_size: None,
        };
        assert_eq!(posture_hinge(DevicePosture::Closed, &fold), 0.);
        assert_eq!(posture_hinge(DevicePosture::HalfOpened, &fold), 90.);
        assert_eq!(posture_hinge(DevicePosture::Opened, &fold), 180.);
    }

    #[test]
    fn a_black_picture_is_dark_and_a_lit_one_isnt() {
        let picture = |v: u8| {
            let pixels = image::RgbaImage::from_pixel(40, 30, image::Rgba([v, v, v, 255]));
            RenderImage::new(vec![image::Frame::new(pixels)])
        };
        assert!(is_dark(&picture(3)));
        assert!(!is_dark(&picture(200)));
    }

    #[test]
    fn a_flick_spins_on_and_comes_to_rest() {
        let mut m = Model3d {
            velocity: (300., 0.),
            ..Model3d::default()
        };
        let mut t = Instant::now();
        m.tick(t, None);
        let mut rested = false;
        for _ in 0..200 {
            t += Duration::from_millis(33);
            let (_, r) = m.tick(t, None);
            rested |= r;
        }
        assert!(rested);
        assert_eq!(m.velocity, (0., 0.));
        assert!(m.view.yaw.abs() > 20.);
    }
}
