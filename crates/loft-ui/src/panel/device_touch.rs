//! Two fingers from one pointer, for the Device panel (ADR-0020), as
//! Simulator.app does it: Option-drag pinches (a second finger mirrored
//! around the screen's centre), Option+Shift-drag moves both fingers
//! together (a two-finger pan), and a trackpad pinch over the screen is a
//! two-finger pinch around where it happens. Points are fractions of the
//! screen, as `DeviceInput::Touches` takes them.

use crate::theme::Palette;
use gpui_kit::{AnyElement, Bounds, IntoElement, Pixels, Styled as _, div, px};
use loft_protocol::TouchPoint;

/// How two fingers follow one pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwoFingers {
    /// The second finger mirrors the first around the screen's centre.
    Pinch,
    /// The second finger keeps its distance from the first: both move.
    Pan,
}

impl TwoFingers {
    /// What the modifiers held at a press ask for: Option pinches,
    /// Option+Shift pans, anything else is one finger.
    pub fn from_modifiers(alt: bool, shift: bool) -> Option<Self> {
        match (alt, shift) {
            (true, true) => Some(Self::Pan),
            (true, false) => Some(Self::Pinch),
            _ => None,
        }
    }
}

/// A gesture of two fingers under way: how they follow the pointer, and
/// where the pointer went down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fingers {
    pub mode: TwoFingers,
    pub start: (f32, f32),
}

impl Fingers {
    /// Where both fingers are with the pointer at `now`: the pointer's
    /// first, kept on the screen.
    pub fn at(&self, now: (f32, f32)) -> [TouchPoint; 2] {
        let mirror = |(x, y): (f32, f32)| (1. - x, 1. - y);
        let second = match self.mode {
            TwoFingers::Pinch => mirror(now),
            TwoFingers::Pan => {
                let (sx, sy) = mirror(self.start);
                (sx + now.0 - self.start.0, sy + now.1 - self.start.1)
            }
        };
        [point(now), point(second)]
    }
}

/// A trackpad pinch: its centre, and how far each finger is from it (a
/// fraction of the screen's width).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackpadPinch {
    pub centre: (f32, f32),
    pub spread: f32,
}

impl TrackpadPinch {
    /// Where a pinch starts: fingers this far either side of the centre.
    pub const START_SPREAD: f32 = 0.1;
    const MIN_SPREAD: f32 = 0.02;
    const MAX_SPREAD: f32 = 0.48;

    pub fn new(centre: (f32, f32)) -> Self {
        Self {
            centre,
            spread: Self::START_SPREAD,
        }
    }

    /// The trackpad zoomed by `delta` (0.1: 10% more): the fingers spread
    /// as much.
    pub fn zoom(&mut self, delta: f32) {
        self.spread = (self.spread * (1. + delta)).clamp(Self::MIN_SPREAD, Self::MAX_SPREAD);
    }

    /// Both fingers: side by side across the centre.
    pub fn fingers(&self) -> [TouchPoint; 2] {
        let (x, y) = self.centre;
        [point((x - self.spread, y)), point((x + self.spread, y))]
    }
}

fn point((x, y): (f32, f32)) -> TouchPoint {
    TouchPoint {
        x: x.clamp(0., 1.),
        y: y.clamp(0., 1.),
    }
}

/// Rings where the fingers are, over the screen drawn at `screen` inside
/// `area` (both window bounds): filled while they touch, hollow while
/// Option only shows where they would go.
pub fn finger_marks(
    fingers: &[TouchPoint],
    touching: bool,
    screen: Bounds<Pixels>,
    area: Bounds<Pixels>,
    p: &Palette,
) -> Vec<AnyElement> {
    const SIZE: f32 = 26.;
    fingers
        .iter()
        .map(|f| {
            let x = screen.origin.x - area.origin.x + screen.size.width * f.x;
            let y = screen.origin.y - area.origin.y + screen.size.height * f.y;
            div()
                .absolute()
                .left(x - px(SIZE / 2.))
                .top(y - px(SIZE / 2.))
                .size(px(SIZE))
                .rounded_full()
                .border_2()
                .border_color(gpui_kit::white().opacity(0.9))
                .when_touching(touching, p)
                .into_any_element()
        })
        .collect()
}

trait Touching {
    fn when_touching(self, touching: bool, p: &Palette) -> Self;
}

impl Touching for gpui_kit::Div {
    fn when_touching(self, touching: bool, p: &Palette) -> Self {
        if touching {
            self.bg(p.accent.opacity(0.55)).shadow_md()
        } else {
            self.bg(gpui_kit::white().opacity(0.15))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xy(p: TouchPoint) -> (f32, f32) {
        ((p.x * 1000.).round() / 1000., (p.y * 1000.).round() / 1000.)
    }

    #[test]
    fn option_pinches_and_option_shift_pans() {
        assert_eq!(
            TwoFingers::from_modifiers(true, false),
            Some(TwoFingers::Pinch)
        );
        assert_eq!(
            TwoFingers::from_modifiers(true, true),
            Some(TwoFingers::Pan)
        );
        assert_eq!(TwoFingers::from_modifiers(false, true), None);
        assert_eq!(TwoFingers::from_modifiers(false, false), None);
    }

    #[test]
    fn a_pinch_mirrors_the_second_finger_around_the_centre() {
        let pinch = Fingers {
            mode: TwoFingers::Pinch,
            start: (0.3, 0.4),
        };
        let [a, b] = pinch.at((0.3, 0.4));
        assert_eq!(xy(a), (0.3, 0.4));
        assert_eq!(xy(b), (0.7, 0.6));
        // Dragging out spreads them apart through the centre.
        let [a, b] = pinch.at((0.1, 0.2));
        assert_eq!(xy(a), (0.1, 0.2));
        assert_eq!(xy(b), (0.9, 0.8));
        // At the centre they meet.
        let [a, b] = pinch.at((0.5, 0.5));
        assert_eq!(xy(a), xy(b));
    }

    #[test]
    fn a_pan_moves_both_fingers_together() {
        let pan = Fingers {
            mode: TwoFingers::Pan,
            start: (0.3, 0.4),
        };
        let [a, b] = pan.at((0.4, 0.2));
        assert_eq!(xy(a), (0.4, 0.2));
        // The second started mirrored (0.7, 0.6) and moved as far.
        assert_eq!(xy(b), (0.8, 0.4));
        // Off the screen's edge: kept on it.
        let [_, b] = pan.at((0.9, 0.4));
        assert_eq!(xy(b), (1.0, 0.6));
    }

    #[test]
    fn a_trackpad_pinch_spreads_the_fingers_and_stays_on_the_screen() {
        let mut pinch = TrackpadPinch::new((0.5, 0.5));
        let [a, b] = pinch.fingers();
        assert_eq!(xy(a), (0.4, 0.5));
        assert_eq!(xy(b), (0.6, 0.5));
        pinch.zoom(1.0);
        let [a, b] = pinch.fingers();
        assert_eq!(xy(a), (0.3, 0.5));
        assert_eq!(xy(b), (0.7, 0.5));
        // Never apart by more than the screen, nor on top of each other.
        for _ in 0..20 {
            pinch.zoom(1.0);
        }
        assert!(pinch.spread <= 0.48);
        for _ in 0..40 {
            pinch.zoom(-0.5);
        }
        assert!(pinch.spread >= 0.02);
        // Near an edge: the far finger is kept on the screen.
        let mut edge = TrackpadPinch::new((0.05, 0.5));
        edge.zoom(0.0);
        assert_eq!(xy(edge.fingers()[0]), (0.0, 0.5));
    }
}
