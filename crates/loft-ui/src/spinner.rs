//! The spinner, and how often anything that moves in a loop is redrawn.
//!
//! A looping animation asks for a new frame at every refresh of the display,
//! and a frame is the whole window, laid out and painted again. A spinner is
//! on screen for as long as an agent is at work, so on a 120 Hz display that
//! was 120 windows a second: a third of a CPU core (measured) for as long as
//! any thread ran. Nothing here needs more than a 60 Hz display shows.

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable, Size};
use gpui_kit::{
    Animation, AnimationExt as _, App, Hsla, IntoElement, ParentElement as _, RenderOnce,
    Styled as _, Transformation, Window, div, ease_in_out, percentage, prelude::FluentBuilder as _,
};
use std::time::Duration;

/// Frames a second for what turns. The spinner's eight spokes are 45°
/// apart and it turns up to 15° in a sixtieth of a second: any fewer frames
/// and it would seem to stutter, or to turn backwards. A little over 60, so
/// that the frame after next of a 120 Hz display is never just missed.
pub(crate) const TURNING_FPS: f32 = 66.;

/// Frames a second for what only fades or swells over a second or more
/// (the "Thinking…" pulse, a working subagent's halo): the change from one
/// frame to the next is a few hundredths either way.
pub(crate) const FADING_FPS: f32 = 25.;

/// A turning loader: the component library's (its icon, speed and easing),
/// redrawn [`TURNING_FPS`] times a second instead of at every refresh.
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
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div().child(
            Icon::new(IconName::Loader)
                .with_size(self.size)
                .when_some(self.color, |icon, color| icon.text_color(color))
                .with_animation(
                    "circle",
                    Animation::new(Duration::from_millis(800))
                        .repeat()
                        .with_easing(ease_in_out)
                        .with_max_fps(TURNING_FPS),
                    |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
                ),
        )
    }
}
