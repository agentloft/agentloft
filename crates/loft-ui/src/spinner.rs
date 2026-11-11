//! Everything that moves in a loop, by one clock: the spinner, and what
//! fades (the "Thinking…" pulse, a waiting thread, a working subagent's halo).
//!
//! A frame is the whole window, laid out and painted again, and something
//! loops on screen for as long as an agent is at work. Left to itself each
//! looping thing asks for a frame at every refresh of the display: 120
//! windows a second, 42% of a CPU core (measured) for as long as any thread
//! ran. Here the clock decides when the window is drawn for them: twenty
//! times a second while something fades, ten while only spinners turn, and
//! not at all once nothing that loops is on screen (or the window is).
//!
//! So the spinner turns in steps, as the system's does: eight spokes, the
//! brightest leading and the rest fading behind it, one spoke on every
//! tenth of a second. Eight even spokes turning smoothly need sixty frames a
//! second to be seen to turn at all.

use gpui_kit::component::{Icon, Sizable, Size};
use gpui_kit::{
    AnyElement, App, EntityId, Global, Hsla, IntoElement, ParentElement as _, RenderOnce,
    Styled as _, Transformation, Window, div, percentage, prelude::FluentBuilder as _,
};
use std::time::Duration;
use web_time::Instant;

/// The spinner's drawing (see `assets.rs`).
pub(crate) const SPOKES: &str = "spinner/spokes.svg";

/// The clock's tick. What fades is drawn on every one; the spinner moves on
/// every other.
const TICK: Duration = Duration::from_millis(50);

/// Spokes, and so steps to a turn: once round in 0.8 s.
const STEPS: u64 = 8;

#[derive(Default)]
struct Clock {
    /// Ticks so far (it stops while nothing that loops is being drawn).
    ticks: u64,
    /// When the first looping thing was drawn: what fades goes by this.
    began: Option<Instant>,
    /// The views drawn since they were last told to draw, with something
    /// looping in them.
    views: Vec<EntityId>,
    /// One of them has something fading: it is told on every tick, not
    /// every other.
    fading: bool,
    /// The task that ticks is alive.
    running: bool,
}

impl Global for Clock {}

/// Have the view now being drawn drawn again at the clock's next tick (or
/// the one after, with nothing fading), starting the clock if it stopped.
fn keep_drawing(fading: bool, window: &Window, cx: &mut App) {
    if cx.reduce_motion() {
        return;
    }
    let view = window.current_view();
    let clock = cx.default_global::<Clock>();
    if !clock.views.contains(&view) {
        clock.views.push(view);
    }
    clock.fading |= fading;
    clock.began.get_or_insert_with(Instant::now);
    if std::mem::replace(&mut clock.running, true) {
        return;
    }
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(TICK).await;
            let more = cx.update(|cx| {
                let clock = cx.global_mut::<Clock>();
                clock.ticks += 1;
                // Spinners alone look the same until the next tick.
                if !clock.fading && clock.ticks % 2 == 1 {
                    return true;
                }
                clock.fading = false;
                let views = std::mem::take(&mut clock.views);
                if views.is_empty() {
                    // Nothing that loops was drawn since the last time: it
                    // is off screen, or the window is. The next one drawn
                    // starts the clock again.
                    clock.running = false;
                    return false;
                }
                for view in views {
                    cx.notify(view);
                }
                true
            });
            if !more {
                break;
            }
        }
    })
    .detach();
}

/// How far round the spinner is, in steps. All spinners turn together.
fn step(window: &Window, cx: &mut App) -> u64 {
    keep_drawing(false, window, cx);
    cx.default_global::<Clock>().ticks / 2 % STEPS
}

/// Where this moment is in a loop `period` long, from 0 to 1.
fn phase(period: Duration, window: &Window, cx: &mut App) -> f32 {
    keep_drawing(true, window, cx);
    match cx.default_global::<Clock>().began {
        Some(began) if !period.is_zero() => {
            (began.elapsed().as_nanos() % period.as_nanos()) as f32 / period.as_nanos() as f32
        }
        // Nothing moves (the system's "reduce motion" is on).
        _ => 0.,
    }
}

/// Something that goes round a loop `period` long: `draw` makes it for a
/// point in the loop (0 to 1), and the clock has it drawn again as the loop
/// goes on.
pub(crate) fn looping(period: Duration, draw: impl FnOnce(f32) -> AnyElement + 'static) -> Looping {
    Looping {
        period,
        draw: Box::new(draw),
    }
}

#[derive(IntoElement)]
pub(crate) struct Looping {
    period: Duration,
    draw: Box<dyn FnOnce(f32) -> AnyElement>,
}

impl RenderOnce for Looping {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        (self.draw)(phase(self.period, window, cx))
    }
}

/// A turning loader.
#[derive(IntoElement)]
pub(crate) struct Spinner {
    size: Size,
    color: Option<Hsla>,
}

impl Spinner {
    pub(crate) fn new() -> Self {
        Self {
            size: Size::Medium,
            color: None,
        }
    }

    pub(crate) fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

impl Sizable for Spinner {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl RenderOnce for Spinner {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let turned = step(window, cx) as f32 / STEPS as f32;
        div().child(
            Icon::empty()
                .path(SPOKES)
                .with_size(self.size)
                .when_some(self.color, |icon, color| icon.text_color(color))
                .transform(Transformation::rotate(percentage(turned))),
        )
    }
}
