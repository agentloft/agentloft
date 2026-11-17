//! A video file, played where it is shown: in a reply or a message (a
//! Markdown image whose target is a video), and in the file viewer.
//!
//! The platform plays it and the app draws it, so the video scrolls, clips
//! and sits under menus like everything else, and the controls over it are
//! ours:
//! - on a Mac, AVFoundation decodes, keeps time and makes the sound; each
//!   frame comes out as a pixel buffer the GPU already holds, and GPUI
//!   paints that buffer;
//! - in the browser (the web client), a hidden `<video>` streams the file
//!   from the daemon (`/api/file`, in ranges) and makes the sound; each
//!   frame is copied off it and drawn as an image.
//!
//! Nothing runs for a video that isn't playing: the window is drawn again
//! only when the player has a new frame, and a paused player is let go of
//! after a while (its last frame stays as the picture).

#[cfg(any(target_os = "macos", target_arch = "wasm32"))]
use crate::press::ClaimsPress as _;
use gpui_kit::{AnyElement, App, SharedString, Window};
use std::path::{Path, PathBuf};

/// Whether the file at `path` is a video this build can play in place.
pub(crate) fn is_video(path: &Path) -> bool {
    PLAYS
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                ["mp4", "mov", "m4v"]
                    .iter()
                    .any(|video| ext.eq_ignore_ascii_case(video))
            })
}

/// Whether this build plays videos in place.
const PLAYS: bool = cfg!(any(target_os = "macos", target_arch = "wasm32"));

/// The video at `path` as a player as wide as what holds it. `key` tells it
/// from the other players beside it (the same file may be shown twice).
pub(crate) fn player(
    key: SharedString,
    path: PathBuf,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    imp::element(key, path, false, window, cx)
}

/// [`player`], filling what holds it in both directions.
pub(crate) fn player_filling(
    key: SharedString,
    path: PathBuf,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    imp::element(key, path, true, window, cx)
}

/// "0:07", "12:40", "1:02:05".
#[cfg_attr(
    not(any(target_os = "macos", target_arch = "wasm32")),
    allow(dead_code)
)]
fn clock(seconds: f64) -> String {
    let total = if seconds.is_finite() {
        seconds.max(0.) as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(not(any(target_os = "macos", target_arch = "wasm32")))]
mod imp {
    use super::*;
    use gpui_kit::{IntoElement as _, div};

    pub(super) fn element(
        _: SharedString,
        _: PathBuf,
        _: bool,
        _: &mut Window,
        _: &mut App,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(any(target_os = "macos", target_arch = "wasm32"))]
mod imp {
    use super::*;
    use crate::panel::page::Seen;
    use crate::theme::Palette;
    use backend::Native;
    use gpui_kit::assets::IconName;
    use gpui_kit::component::{Icon, Sizable as _, h_flex};
    use gpui_kit::prelude::FluentBuilder as _;
    use gpui_kit::{
        AppContext as _, Bounds, Context, DispatchPhase, Div, Element, ElementId, Entity, Global,
        GlobalElementId, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId,
        MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels,
        Render, Stateful, StatefulInteractiveElement as _, Styled as _, Task, black, canvas, div,
        linear_color_stop, linear_gradient, px, relative, white,
    };
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::Duration;
    use web_time::Instant;

    /// AVFoundation's player, and its frames as pixel buffers the GPU holds.
    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)] // objc2 calls into AVFoundation; each block has a SAFETY note.
    mod backend {
        use gpui_kit::{AnyElement, App, IntoElement as _, Styled as _, surface};
        use objc2::rc::Retained;
        use objc2::runtime::AnyObject;
        use objc2::{AnyThread as _, MainThreadMarker};
        use objc2_av_foundation::{
            AVPlayer, AVPlayerItem, AVPlayerItemStatus, AVPlayerItemVideoOutput,
        };
        use objc2_core_foundation::{CFRetained, CFString};
        use objc2_core_media::CMTime;
        use objc2_core_video::{
            CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
            CVPixelBufferGetWidth, kCVPixelBufferMetalCompatibilityKey,
            kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        };
        use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
        use std::path::Path;

        /// CoreMedia's usual timescale: divides evenly by the common frame rates.
        const TIMESCALE: i32 = 600;

        /// A frame: a pixel buffer the GPU can read.
        pub(super) type Frame = CFRetained<CVPixelBuffer>;

        pub(super) fn frame_size(frame: &Frame) -> (usize, usize) {
            (CVPixelBufferGetWidth(frame), CVPixelBufferGetHeight(frame))
        }

        pub(super) fn frame_element(frame: Frame) -> AnyElement {
            surface(frame).absolute().size_full().into_any_element()
        }

        /// A frame no longer shown (GPUI holds nothing of it here).
        pub(super) fn release(_: Frame, _: &mut App) {}

        /// The system's player for one file, with the output its frames are
        /// taken from.
        pub(super) struct Native {
            player: Retained<AVPlayer>,
            item: Retained<AVPlayerItem>,
            output: Retained<AVPlayerItemVideoOutput>,
        }

        /// A Core Foundation string as the Foundation string it also is.
        fn ns(string: &CFString) -> &NSString {
            // SAFETY: CFString and NSString are the same object ("toll-free
            // bridged"): a pointer to one is a pointer to the other.
            unsafe { &*(string as *const CFString).cast::<NSString>() }
        }

        fn time(seconds: f64) -> CMTime {
            // SAFETY: makes a value; nothing is read or kept.
            unsafe { CMTime::with_seconds(seconds.max(0.), TIMESCALE) }
        }

        impl Native {
            pub(super) fn open(path: &Path, muted: bool, volume: f32) -> Option<Self> {
                let mtm = MainThreadMarker::new()?;
                let path = NSString::from_str(path.to_str()?);
                // Frames in the one format GPUI draws (two planes, full range),
                // in memory the GPU can read without a copy.
                let format = NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange);
                let yes = NSNumber::new_bool(true);
                // SAFETY: the keys are constants of the system's; the values are
                // the numbers those keys take.
                let attributes: Retained<NSDictionary<NSString, AnyObject>> = unsafe {
                    NSDictionary::from_slices(
                        &[
                            ns(kCVPixelBufferPixelFormatTypeKey),
                            ns(kCVPixelBufferMetalCompatibilityKey),
                        ],
                        &[format.as_ref(), yes.as_ref()],
                    )
                };
                // SAFETY: plain AVFoundation calls on the main thread, with
                // objects that live as long as `Self` does.
                unsafe {
                    let url = NSURL::fileURLWithPath(&path);
                    let item = AVPlayerItem::playerItemWithURL(&url, mtm);
                    let output = AVPlayerItemVideoOutput::initWithPixelBufferAttributes(
                        AVPlayerItemVideoOutput::alloc(),
                        Some(&attributes),
                    );
                    item.addOutput(&output);
                    let player = AVPlayer::playerWithPlayerItem(Some(&item), mtm);
                    player.setMuted(muted);
                    player.setVolume(volume);
                    Some(Self {
                        player,
                        item,
                        output,
                    })
                }
            }

            /// The frame for where the video is now, if it is one not taken yet.
            pub(super) fn new_frame(&self) -> Option<Frame> {
                // SAFETY: main thread; the out-pointer may be null.
                let frame = unsafe {
                    let now = self.player.currentTime();
                    if !self.output.hasNewPixelBufferForItemTime(now) {
                        return None;
                    }
                    self.output
                        .copyPixelBufferForItemTime_itemTimeForDisplay(now, std::ptr::null_mut())?
                };
                let frame = CFRetained::from(frame);
                // GPUI draws this format and no other.
                (CVPixelBufferGetPixelFormatType(&frame)
                    == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)
                    .then_some(frame)
            }

            /// How long the video is, once that is known.
            pub(super) fn duration(&self) -> Option<f64> {
                // SAFETY: main thread.
                let seconds = unsafe { self.item.duration().seconds() };
                (seconds.is_finite() && seconds > 0.).then_some(seconds)
            }

            pub(super) fn position(&self) -> f64 {
                // SAFETY: main thread.
                let seconds = unsafe { self.player.currentTime().seconds() };
                if seconds.is_finite() {
                    seconds.max(0.)
                } else {
                    0.
                }
            }

            pub(super) fn failed(&self) -> bool {
                // SAFETY: main thread.
                unsafe { self.item.status() == AVPlayerItemStatus::Failed }
            }

            pub(super) fn moving(&self) -> bool {
                // SAFETY: main thread.
                unsafe { self.player.rate() != 0. }
            }

            pub(super) fn play(&self) {
                // SAFETY: main thread.
                unsafe { self.player.play() }
            }

            pub(super) fn pause(&self) {
                // SAFETY: main thread.
                unsafe { self.player.pause() }
            }

            pub(super) fn set_muted(&self, muted: bool) {
                // SAFETY: main thread.
                unsafe { self.player.setMuted(muted) }
            }

            pub(super) fn set_volume(&self, volume: f32) {
                // SAFETY: main thread.
                unsafe { self.player.setVolume(volume) }
            }

            /// Go to `seconds`: to that very frame, or (while the scrubber is
            /// dragged) to whichever is quickest to show near it.
            pub(super) fn seek(&self, seconds: f64, exact: bool) {
                let slack = time(if exact { 0. } else { 0.5 });
                // SAFETY: main thread.
                unsafe {
                    self.player.seekToTime_toleranceBefore_toleranceAfter(
                        time(seconds),
                        slack,
                        slack,
                    )
                }
            }
        }

        impl Drop for Native {
            fn drop(&mut self) {
                // SAFETY: main thread (a `Native` never leaves it: it is not
                // `Send`), and both objects are alive.
                unsafe {
                    self.player.pause();
                    self.item.removeOutput(&self.output);
                }
            }
        }
    }

    /// The browser's `<video>`, streaming the file from the daemon; its
    /// frames copied off it as images.
    #[cfg(target_arch = "wasm32")]
    mod backend {
        use gpui_kit::{
            AnyElement, App, IntoElement as _, ObjectFit, RenderImage, Styled as _,
            StyledImage as _, img,
        };
        use std::cell::Cell;
        use std::path::Path;
        use std::sync::Arc;
        use wasm_bindgen::JsCast as _;
        use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, HtmlVideoElement};

        /// Frames are copied at most this wide (the copy is the cost).
        const MAX_WIDTH: u32 = 1280;
        /// At most this many frames a second are copied.
        const MIN_STEP: f64 = 1. / 30.;
        /// `HAVE_CURRENT_DATA`: there is a frame to copy.
        const HAVE_CURRENT_DATA: u16 = 2;

        pub(super) type Frame = Arc<RenderImage>;

        pub(super) fn frame_size(frame: &Frame) -> (usize, usize) {
            let size = frame.size(0);
            (size.width.0 as usize, size.height.0 as usize)
        }

        pub(super) fn frame_element(frame: Frame) -> AnyElement {
            img(frame)
                .absolute()
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element()
        }

        /// Free a frame no longer shown: each is a new texture to GPUI.
        pub(super) fn release(frame: Frame, cx: &mut App) {
            cx.drop_image(frame, None);
        }

        /// Where the daemon streams the file at `path` (signed-in sessions,
        /// inside the folders they may read).
        fn url(path: &Path) -> String {
            let mut out = String::from("/api/file?path=");
            for b in path.to_string_lossy().bytes() {
                match b {
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' => {
                        out.push(b as char)
                    }
                    _ => out.push_str(&format!("%{b:02X}")),
                }
            }
            out
        }

        pub(super) struct Native {
            video: HtmlVideoElement,
            canvas: HtmlCanvasElement,
            context: CanvasRenderingContext2d,
            /// Where the video was at the last frame copied.
            copied_at: Cell<Option<f64>>,
        }

        impl Native {
            pub(super) fn open(path: &Path, muted: bool, volume: f32) -> Option<Self> {
                let document = web_sys::window()?.document()?;
                let video: HtmlVideoElement =
                    document.create_element("video").ok()?.dyn_into().ok()?;
                // In the page (some browsers stop a detached video), out of
                // sight and out of the way: GPUI draws what it shows.
                let _ = video.set_attribute(
                    "style",
                    "position:fixed;left:0;top:0;width:2px;height:2px;opacity:0;pointer-events:none",
                );
                video.set_preload("auto");
                video.set_muted(muted);
                video.set_volume(f64::from(volume));
                let _ = video.set_attribute("playsinline", "");
                video.set_src(&url(path));
                document.body()?.append_child(&video).ok()?;
                let canvas: HtmlCanvasElement =
                    document.create_element("canvas").ok()?.dyn_into().ok()?;
                let context: CanvasRenderingContext2d =
                    canvas.get_context("2d").ok()??.dyn_into().ok()?;
                Some(Self {
                    video,
                    canvas,
                    context,
                    copied_at: Cell::new(None),
                })
            }

            /// The frame for where the video is now, if it moved since the
            /// last one (and not more often than [`MIN_STEP`]).
            pub(super) fn new_frame(&self) -> Option<Frame> {
                if self.video.ready_state() < HAVE_CURRENT_DATA {
                    return None;
                }
                let (w, h) = (self.video.video_width(), self.video.video_height());
                if w == 0 || h == 0 {
                    return None;
                }
                let now = self.video.current_time();
                if let Some(at) = self.copied_at.get()
                    && (now == at || !self.video.paused() && (now - at).abs() < MIN_STEP)
                {
                    return None;
                }
                let scale = (f64::from(MAX_WIDTH) / f64::from(w)).min(1.);
                let (cw, ch) = (
                    (f64::from(w) * scale).round().max(1.) as u32,
                    (f64::from(h) * scale).round().max(1.) as u32,
                );
                if self.canvas.width() != cw || self.canvas.height() != ch {
                    self.canvas.set_width(cw);
                    self.canvas.set_height(ch);
                }
                self.context
                    .draw_image_with_html_video_element_and_dw_and_dh(
                        &self.video,
                        0.,
                        0.,
                        f64::from(cw),
                        f64::from(ch),
                    )
                    .ok()?;
                let data = self
                    .context
                    .get_image_data(0., 0., f64::from(cw), f64::from(ch))
                    .ok()?;
                let mut pixels = data.data().0;
                // RGBA to what GPUI paints (BGRA).
                for pixel in pixels.as_chunks_mut::<4>().0 {
                    pixel.swap(0, 2);
                }
                let image = image::RgbaImage::from_raw(cw, ch, pixels)?;
                self.copied_at.set(Some(now));
                Some(Arc::new(RenderImage::new(vec![image::Frame::new(image)])))
            }

            pub(super) fn duration(&self) -> Option<f64> {
                let seconds = self.video.duration();
                (seconds.is_finite() && seconds > 0.).then_some(seconds)
            }

            pub(super) fn position(&self) -> f64 {
                let seconds = self.video.current_time();
                if seconds.is_finite() {
                    seconds.max(0.)
                } else {
                    0.
                }
            }

            pub(super) fn failed(&self) -> bool {
                self.video.error().is_some()
            }

            pub(super) fn moving(&self) -> bool {
                !self.video.paused() && !self.video.ended()
            }

            pub(super) fn play(&self) {
                // A promise: refused only without the user having touched
                // the page, and the button press is that.
                let _ = self.video.play();
            }

            pub(super) fn pause(&self) {
                let _ = self.video.pause();
            }

            pub(super) fn set_muted(&self, muted: bool) {
                self.video.set_muted(muted);
            }

            pub(super) fn set_volume(&self, volume: f32) {
                self.video.set_volume(f64::from(volume));
            }

            pub(super) fn seek(&self, seconds: f64, _exact: bool) {
                self.video.set_current_time(seconds.max(0.));
                self.copied_at.set(None);
            }
        }

        impl Drop for Native {
            fn drop(&mut self) {
                let _ = self.video.pause();
                // Stop the download as well.
                let _ = self.video.remove_attribute("src");
                self.video.load();
                self.video.remove();
            }
        }
    }

    /// Tallest a player in the chat grows.
    const MAX_HEIGHT: f32 = 420.;
    /// The strip along the bottom the controls are in.
    const CONTROLS_HEIGHT: f32 = 52.;
    /// The box a player in the chat has: this wide for its height, or as
    /// wide as the video if that is wider.
    const BOX_RATIO: f32 = 16. / 9.;
    /// How often a playing video is asked for its next frame. (It answers
    /// with one at its own rate; the window is drawn again only then.)
    const TICK: Duration = Duration::from_millis(16);
    /// How long a video that isn't playing is waited on for a picture: its
    /// first, or the one where it was just moved to.
    const PICTURE_WAIT: Duration = Duration::from_secs(5);
    /// A video that plays out of sight for this long is paused: long enough
    /// that a row redrawn a moment late doesn't stop it, short enough that
    /// its sound doesn't follow into another thread.
    const UNSEEN: Duration = Duration::from_secs(1);
    /// A paused video lets go of its player after this long.
    const REST: Duration = Duration::from_secs(20);
    /// More players than this, and the ones out of sight are forgotten.
    const KEPT: usize = 24;

    pub(super) fn element(
        key: SharedString,
        path: PathBuf,
        fill: bool,
        _: &mut Window,
        _: &mut App,
    ) -> AnyElement {
        Video {
            id: ElementId::Name(key),
            path,
            fill,
        }
        .into_any_element()
    }

    /// The players there are, by where in which window they are drawn: a
    /// player outlives the element that draws it (a row scrolled away and
    /// back is still at the same place in the same video).
    #[derive(Default)]
    struct Players(HashMap<String, Entity<Player>>);
    impl Global for Players {}

    /// A player's place in the window. All it does is find its [`Player`].
    struct Video {
        id: ElementId,
        path: PathBuf,
        fill: bool,
    }

    impl IntoElement for Video {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for Video {
        type RequestLayoutState = AnyElement;
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> {
            Some(self.id.clone())
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(
            &mut self,
            global_id: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (LayoutId, Self::RequestLayoutState) {
            let place = format!(
                "{}:{global_id:?}",
                window.window_handle().window_id().as_u64()
            );
            let known = cx.default_global::<Players>().0.get(&place).cloned();
            let known = known.filter(|player| player.read(cx).path == self.path);
            let player = known.unwrap_or_else(|| {
                let (path, fill) = (self.path.clone(), self.fill);
                let player = cx.new(|cx| Player::new(path, fill, cx));
                let players = &mut cx.global_mut::<Players>().0;
                players.insert(place, player.clone());
                if players.len() > KEPT {
                    forget_unseen(cx);
                }
                player
            });
            let mut drawn = player.into_any_element();
            let layout = drawn.request_layout(window, cx);
            (layout, drawn)
        }

        fn prepaint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            drawn: &mut Self::RequestLayoutState,
            window: &mut Window,
            cx: &mut App,
        ) -> Self::PrepaintState {
            drawn.prepaint(window, cx);
        }

        fn paint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            drawn: &mut Self::RequestLayoutState,
            _: &mut Self::PrepaintState,
            window: &mut Window,
            cx: &mut App,
        ) {
            drawn.paint(window, cx);
        }
    }

    /// Drop the players that are neither on screen nor holding a video open.
    fn forget_unseen(cx: &mut App) {
        let gone: Vec<String> = cx
            .global::<Players>()
            .0
            .iter()
            .filter(|(_, player)| {
                let player = player.read(cx);
                player.native.is_none() && !player.seen.get().is_some_and(Seen::in_sight)
            })
            .map(|(place, _)| place.clone())
            .collect();
        let players = &mut cx.global_mut::<Players>().0;
        for place in gone {
            players.remove(&place);
        }
    }

    pub(super) struct Player {
        path: PathBuf,
        /// Fills what holds it (the file viewer), or is a box in the chat.
        fill: bool,
        /// The system's player, while the video plays or was just touched.
        native: Option<Native>,
        /// The frame showing: the video's latest, or its last before the
        /// player was let go of.
        frame: Option<backend::Frame>,
        /// That frame's width ÷ height.
        ratio: f32,
        /// Seconds, or zero until known.
        duration: f64,
        position: f64,
        playing: bool,
        muted: bool,
        /// 0 to 1: the system player's own volume, not the Mac's.
        volume: f32,
        /// The volume slider is held.
        adjusting: bool,
        failed: bool,
        /// The scrubber is held, and whether the video played when taken.
        scrubbing: bool,
        resume: bool,
        /// Paused, and a picture is still to come: until when to look.
        awaiting: Option<Instant>,
        /// Something asks the player for frames (see [`Self::tick`]).
        ticking: bool,
        /// Counts the players this has had, so a rest meant for an earlier
        /// one doesn't end a later one.
        rests: u64,
        rest: Option<Task<()>>,
        seen: Rc<Cell<Option<Seen>>>,
        unseen: Option<Instant>,
        /// Where the scrubber's track is drawn.
        track: Rc<Cell<Option<Bounds<Pixels>>>>,
        /// Where the volume slider's track is drawn.
        volume_track: Rc<Cell<Option<Bounds<Pixels>>>>,
    }

    /// The volume last chosen, so the next video starts there too (for as
    /// long as the app runs). Stored as `f32` bits.
    static LAST_VOLUME: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(0x3f80_0000); // 1.0

    impl Player {
        fn new(path: PathBuf, fill: bool, cx: &mut Context<Self>) -> Self {
            let mut this = Self {
                path,
                fill,
                native: None,
                frame: None,
                ratio: BOX_RATIO,
                duration: 0.,
                position: 0.,
                playing: false,
                muted: false,
                volume: f32::from_bits(LAST_VOLUME.load(std::sync::atomic::Ordering::Relaxed)),
                adjusting: false,
                failed: false,
                scrubbing: false,
                resume: false,
                awaiting: None,
                ticking: false,
                rests: 0,
                rest: None,
                seen: Rc::default(),
                unseen: None,
                track: Rc::default(),
                volume_track: Rc::default(),
            };
            // The first frame is the picture.
            this.open(cx);
            this.rest_later(cx);
            this
        }

        /// Have the system's player, at where the video was left.
        fn open(&mut self, cx: &mut Context<Self>) {
            if self.native.is_some() || self.failed {
                return;
            }
            match Native::open(&self.path, self.muted, self.volume) {
                Some(native) => {
                    if self.position > 0. {
                        native.seek(self.position, true);
                    }
                    self.native = Some(native);
                    self.await_picture(cx);
                }
                None => self.failed = true,
            }
        }

        fn await_picture(&mut self, cx: &mut Context<Self>) {
            self.awaiting = Some(Instant::now() + PICTURE_WAIT);
            self.keep_ticking(cx);
        }

        /// Ask the player for frames for as long as there is one to come.
        fn keep_ticking(&mut self, cx: &mut Context<Self>) {
            if self.ticking {
                return;
            }
            self.ticking = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    if !this.update(cx, |this, cx| this.tick(cx)).unwrap_or(false) {
                        break;
                    }
                }
            })
            .detach();
        }

        /// Take the player's new frame, if it has one, and say whether to
        /// come back for the next. Only a new frame (or a new second on the
        /// clock) has the window drawn again.
        fn tick(&mut self, cx: &mut Context<Self>) -> bool {
            let Some(native) = &self.native else {
                self.ticking = false;
                return false;
            };
            let mut changed = false;
            if native.failed() {
                self.failed = true;
                self.playing = false;
                self.native = None;
                self.ticking = false;
                cx.notify();
                return false;
            }
            if self.duration == 0.
                && let Some(duration) = native.duration()
            {
                self.duration = duration;
                changed = true;
            }
            if let Some(frame) = native.new_frame() {
                let (w, h) = backend::frame_size(&frame);
                if w > 0 && h > 0 {
                    self.ratio = w as f32 / h as f32;
                }
                if let Some(old) = self.frame.replace(frame) {
                    backend::release(old, cx);
                }
                self.awaiting = None;
                changed = true;
            }
            if self.playing {
                let position = native.position();
                changed |= position as u64 != self.position as u64;
                self.position = position;
                if !native.moving() {
                    // The video ran out (or the system stopped it).
                    if self.duration > 0. && position >= self.duration - 0.3 {
                        self.position = self.duration;
                    }
                    self.playing = false;
                    self.rest_later(cx);
                    changed = true;
                } else if self.seen.get().is_some_and(Seen::in_sight) {
                    self.unseen = None;
                } else if self.unseen.get_or_insert_with(Instant::now).elapsed() >= UNSEEN {
                    self.pause(cx);
                }
            }
            if changed {
                cx.notify();
            }
            let more = self.playing || self.awaiting.is_some_and(|until| Instant::now() < until);
            if !more {
                self.awaiting = None;
                self.ticking = false;
            }
            more
        }

        /// Let go of the system's player once the video has stayed paused.
        fn rest_later(&mut self, cx: &mut Context<Self>) {
            self.rests += 1;
            let rests = self.rests;
            self.rest = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REST).await;
                let _ = this.update(cx, |this, _| {
                    if this.rests == rests && !this.playing && !this.scrubbing {
                        this.native = None;
                    }
                });
            }));
        }

        fn play(&mut self, cx: &mut Context<Self>) {
            self.open(cx);
            let Some(native) = &self.native else {
                cx.notify();
                return;
            };
            if self.duration > 0. && self.position >= self.duration - 0.05 {
                native.seek(0., true);
                self.position = 0.;
            }
            native.play();
            self.playing = true;
            self.unseen = None;
            self.rests += 1;
            self.rest = None;
            self.keep_ticking(cx);
            cx.notify();
        }

        fn pause(&mut self, cx: &mut Context<Self>) {
            if let Some(native) = &self.native {
                native.pause();
                self.position = native.position();
            }
            self.playing = false;
            self.rest_later(cx);
            cx.notify();
        }

        fn toggle(&mut self, cx: &mut Context<Self>) {
            if self.playing {
                self.pause(cx);
            } else {
                self.play(cx);
            }
        }

        fn toggle_muted(&mut self, cx: &mut Context<Self>) {
            self.muted = !self.muted;
            // Unmuting a slider left at nothing would still be silent.
            if !self.muted && self.volume == 0. {
                self.volume = 0.5;
                LAST_VOLUME.store(self.volume.to_bits(), std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(native) = &self.native {
                native.set_muted(self.muted);
                native.set_volume(self.volume);
            }
            cx.notify();
        }

        /// Set the volume to where the pointer is over the slider's track.
        /// Moving it unmutes; taking it to nothing reads as muted.
        fn volume_to(&mut self, x: Pixels, cx: &mut Context<Self>) {
            let Some(track) = self.volume_track.get() else {
                return;
            };
            if track.size.width <= px(0.) {
                return;
            }
            let volume = ((x - track.origin.x) / track.size.width).clamp(0., 1.);
            self.volume = volume;
            self.muted = volume == 0.;
            LAST_VOLUME.store(volume.to_bits(), std::sync::atomic::Ordering::Relaxed);
            if let Some(native) = &self.native {
                native.set_volume(volume);
                native.set_muted(self.muted);
            }
            cx.notify();
        }

        fn volume_end(&mut self, x: Pixels, cx: &mut Context<Self>) {
            if !self.adjusting {
                return;
            }
            self.adjusting = false;
            self.volume_to(x, cx);
        }

        /// Move to where the pointer is over the scrubber's track.
        fn scrub_to(&mut self, x: Pixels, exact: bool, cx: &mut Context<Self>) {
            let Some(track) = self.track.get() else {
                return;
            };
            if self.duration <= 0. || track.size.width <= px(0.) {
                return;
            }
            let along = ((x - track.origin.x) / track.size.width).clamp(0., 1.);
            self.open(cx);
            let Some(native) = &self.native else {
                return;
            };
            self.position = self.duration * f64::from(along);
            native.seek(self.position, exact);
            self.await_picture(cx);
            cx.notify();
        }

        fn scrub_start(&mut self, x: Pixels, cx: &mut Context<Self>) {
            self.scrubbing = true;
            self.resume = self.playing;
            if self.playing {
                if let Some(native) = &self.native {
                    native.pause();
                }
                self.playing = false;
            }
            self.scrub_to(x, false, cx);
        }

        fn scrub_end(&mut self, x: Pixels, cx: &mut Context<Self>) {
            if !self.scrubbing {
                return;
            }
            self.scrubbing = false;
            self.scrub_to(x, true, cx);
            if self.resume {
                self.play(cx);
            } else {
                self.rest_later(cx);
                cx.notify();
            }
        }

        fn button(
            id: &'static str,
            icon: IconName,
            on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
            cx: &mut Context<Self>,
        ) -> Stateful<Div> {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(28.))
                .rounded_full()
                .cursor_pointer()
                .text_color(white())
                .hover(|d| d.bg(white().opacity(0.16)))
                .child(Icon::new(icon).small())
                // A press here is the button's, not the start of a selection
                // in the text around the video.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .claims_press()
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    on_click(this, cx);
                }))
        }
    }

    impl Render for Player {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let p = Palette::of(cx);
            let entity = cx.entity();
            let seen = self.seen.clone();
            let track = self.track.clone();
            let volume_track = self.volume_track.clone();
            let loudness = if self.muted { 0. } else { self.volume };
            let along = if self.duration > 0. {
                (self.position / self.duration).clamp(0., 1.) as f32
            } else {
                0.
            };
            let frame = div().id("video").group("video").relative().bg(black());
            let frame = if self.fill {
                frame.size_full()
            } else {
                frame
                    .w_full()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(p.border)
                    // A video that can't be shown doesn't need its room.
                    .map(|d| match self.failed {
                        true => d.h(px(64.)),
                        false => d
                            .max_h(px(MAX_HEIGHT))
                            .aspect_ratio(self.ratio.max(BOX_RATIO)),
                    })
            };
            if self.failed {
                let name = self
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                return frame
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(white().opacity(0.7))
                    .child(format!("{name} can't be played here"));
            }
            let controls = h_flex()
                .id("controls")
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(CONTROLS_HEIGHT))
                .px_2()
                .gap_1p5()
                .items_center()
                // A small player (in a message) squeezes its tracks, never
                // past its edge.
                .overflow_hidden()
                // Over a bright picture the controls still read.
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(black().opacity(0.), 0.),
                    linear_color_stop(black().opacity(0.7), 1.),
                ))
                // A press here is the controls' own, not the start of a
                // selection in the text around the video. (They don't hide
                // what is under them from the pointer: the video would stop
                // counting as pointed at, and take them away.)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                // A playing video shows them under the pointer only.
                .when(self.playing && !self.scrubbing && !self.adjusting, |d| {
                    d.invisible().group_hover("video", |d| d.visible())
                })
                .child(Self::button(
                    "play",
                    if self.playing {
                        IconName::Pause
                    } else {
                        IconName::Play
                    },
                    |this, cx| this.toggle(cx),
                    cx,
                ))
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(white())
                        .child(format!(
                            "{} / {}",
                            clock(self.position),
                            clock(self.duration)
                        )),
                )
                .child(
                    // The scrubber: a thin track in a taller strip, so it is
                    // easy to hit.
                    div()
                        .id("scrubber")
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .h(px(24.))
                        .mx_1p5()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                // The drag is the scrubber's, not a selection
                                // in the text around the video.
                                cx.stop_propagation();
                                this.scrub_start(event.position.x, cx);
                            }),
                        )
                        // (Let go before the next frame was drawn, which
                        // is where a drag's own listeners come from.)
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseUpEvent, _, cx| {
                                this.scrub_end(event.position.x, cx);
                            }),
                        )
                        .child(
                            div()
                                .relative()
                                .w_full()
                                .h(px(4.))
                                .rounded_full()
                                .bg(white().opacity(0.3))
                                .child(
                                    canvas(
                                        move |bounds, _, _| track.set(Some(bounds)),
                                        |_, _, _, _| {},
                                    )
                                    .absolute()
                                    .size_full(),
                                )
                                .child(
                                    div().h_full().w(relative(along)).rounded_full().bg(white()),
                                ),
                        ),
                )
                .child(Self::button(
                    "mute",
                    if self.muted {
                        IconName::VolumeX
                    } else {
                        IconName::Volume2
                    },
                    |this, cx| this.toggle_muted(cx),
                    cx,
                ))
                .child(
                    // The volume: a short track in a taller strip, like the
                    // scrubber.
                    div()
                        .id("volume")
                        .flex()
                        .flex_shrink(1.)
                        .min_w(px(16.))
                        .items_center()
                        .w(px(64.))
                        .h(px(24.))
                        .mr_1()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.adjusting = true;
                                this.volume_to(event.position.x, cx);
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseUpEvent, _, cx| {
                                this.volume_end(event.position.x, cx);
                            }),
                        )
                        .child(
                            div()
                                .relative()
                                .w_full()
                                .h(px(4.))
                                .rounded_full()
                                .bg(white().opacity(0.3))
                                .child(
                                    canvas(
                                        move |bounds, _, _| volume_track.set(Some(bounds)),
                                        |_, _, _, _| {},
                                    )
                                    .absolute()
                                    .size_full(),
                                )
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(loudness))
                                        .rounded_full()
                                        .bg(white()),
                                ),
                        ),
                );
            let scrubbing = self.scrubbing;
            let adjusting = self.adjusting;
            frame
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .children(self.frame.clone().map(backend::frame_element))
                        .child(
                            canvas(
                                move |_, window, _| seen.set(Some(Seen::now(window))),
                                move |_, _, window, _| {
                                    if !scrubbing && !adjusting {
                                        return;
                                    }
                                    // The pointer leaves the scrubber while
                                    // it is dragged: the drag goes on.
                                    let moved = entity.clone();
                                    window.on_mouse_event(
                                        move |event: &MouseMoveEvent, phase, _, cx| {
                                            if phase == DispatchPhase::Capture {
                                                moved.update(cx, |this, cx| {
                                                    let x = event.position.x;
                                                    match (event.dragging(), this.adjusting) {
                                                        (true, true) => this.volume_to(x, cx),
                                                        (true, false) => {
                                                            this.scrub_to(x, false, cx)
                                                        }
                                                        // Let go where nothing
                                                        // of ours heard it.
                                                        (false, true) => this.volume_end(x, cx),
                                                        (false, false) => this.scrub_end(x, cx),
                                                    }
                                                });
                                            }
                                        },
                                    );
                                    let released = entity.clone();
                                    window.on_mouse_event(
                                        move |event: &MouseUpEvent, phase, _, cx| {
                                            if phase == DispatchPhase::Capture
                                                && event.button == MouseButton::Left
                                            {
                                                released.update(cx, |this, cx| {
                                                    this.volume_end(event.position.x, cx);
                                                    this.scrub_end(event.position.x, cx);
                                                });
                                            }
                                        },
                                    );
                                },
                            )
                            .absolute()
                            .size_full(),
                        ),
                )
                // A click on the picture plays and pauses: everywhere but
                // where the controls are.
                .child(
                    div()
                        .id("picture")
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .bottom(px(CONTROLS_HEIGHT))
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| this.toggle(cx))),
                )
                .child(controls)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_read_like_a_player() {
        assert_eq!(clock(0.), "0:00");
        assert_eq!(clock(13.6), "0:13");
        assert_eq!(clock(760.), "12:40");
        assert_eq!(clock(3725.), "1:02:05");
        assert_eq!(clock(f64::NAN), "0:00");
    }

    #[test]
    fn videos_are_told_by_their_extension() {
        let playable = cfg!(target_os = "macos") && !crate::host::IS_WEB;
        assert_eq!(is_video(Path::new("/a/reel.MP4")), playable);
        assert_eq!(is_video(Path::new("clip.mov")), playable);
        assert!(!is_video(Path::new("/a/picture.png")));
        assert!(!is_video(Path::new("/a/mp4")));
    }
}
