//! A simulator's screen when it is turned.
//!
//! The device's framebuffer is always as the screen is built (portrait for
//! an iPhone): when the device turns, iOS draws its interface turned inside
//! it. Pictures are turned back so the interface is upright, and touches,
//! given as fractions of those pictures, are turned the other way before
//! they go to the device's digitizer, which also only knows the screen as
//! built.

/// Which way the frontmost interface is turned, as CoreSimulator reports it
/// (`UIInterfaceOrientation`: 1 portrait, 2 upside down, 3 landscape left,
/// 4 landscape right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Turn {
    #[default]
    Portrait,
    UpsideDown,
    /// The interface's top is at the screen's left edge as built
    /// (`UIInterfaceOrientationLandscapeLeft`: the device was turned
    /// clockwise).
    LandscapeLeft,
    /// The interface's top is at the screen's right edge as built (the
    /// device was turned anticlockwise).
    LandscapeRight,
}

impl Turn {
    pub fn from_interface(orientation: u32) -> Self {
        match orientation {
            2 => Self::UpsideDown,
            3 => Self::LandscapeLeft,
            4 => Self::LandscapeRight,
            _ => Self::Portrait,
        }
    }

    /// Quarter turns clockwise that make the framebuffer's picture upright.
    pub fn clockwise_quarters(self) -> u8 {
        match self {
            Self::Portrait => 0,
            Self::LandscapeLeft => 1,
            Self::UpsideDown => 2,
            Self::LandscapeRight => 3,
        }
    }

    /// The picture's size once turned upright.
    pub fn shown_size(self, width: u32, height: u32) -> (u32, u32) {
        if self.clockwise_quarters() % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// A point of the upright picture (fractions, 0,0 top left) as a point
    /// of the screen as built, where the digitizer takes it.
    pub fn to_device(self, x: f64, y: f64) -> (f64, f64) {
        let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        match self.clockwise_quarters() {
            0 => (x, y),
            // Turned clockwise to be shown: the screen's left edge is the
            // picture's top.
            1 => (y, 1.0 - x),
            2 => (1.0 - x, 1.0 - y),
            _ => (1.0 - y, x),
        }
    }

    /// The other way: a point of the screen as built, in the upright picture.
    pub fn to_shown(self, x: f64, y: f64) -> (f64, f64) {
        match self.clockwise_quarters() {
            0 => (x, y),
            1 => (1.0 - y, x),
            2 => (1.0 - x, 1.0 - y),
            _ => (y, 1.0 - x),
        }
    }
}

/// How the device itself is held (`UIDeviceOrientation`: 1 portrait,
/// 2 upside down, 3 landscape left, home side on the right; 4 landscape
/// right). This is what Simulator.app's Rotate Left and Rotate Right send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held(pub u32);

impl Held {
    /// How a device whose interface shows `turn` is most likely held (the
    /// interface turns the opposite way to the device).
    pub fn from_turn(turn: Turn) -> Self {
        Self(match turn {
            Turn::Portrait => 1,
            Turn::UpsideDown => 2,
            Turn::LandscapeRight => 3,
            Turn::LandscapeLeft => 4,
        })
    }

    /// After turning the device a quarter anticlockwise (Rotate Left).
    pub fn left(self) -> Self {
        Self(match self.0 {
            1 => 3,
            3 => 2,
            2 => 4,
            _ => 1,
        })
    }

    /// After turning the device a quarter clockwise (Rotate Right).
    pub fn right(self) -> Self {
        Self(match self.0 {
            1 => 4,
            4 => 2,
            2 => 3,
            _ => 1,
        })
    }
}

/// The size of a picture of a `width` × `height` screen at `scale` pixels
/// per point: at most two pixels per point (plenty for a panel on a Retina
/// display) and at most `longest` pixels on its long side.
pub fn picture_size(width: u32, height: u32, scale: f32, longest: u32) -> (u32, u32) {
    let mut factor = if scale > 2.0 { 2.0 / scale as f64 } else { 1.0 };
    let long = width.max(height) as f64 * factor;
    if long > longest as f64 {
        factor *= longest as f64 / long;
    }
    if factor >= 1.0 {
        return (width, height);
    }
    (
        ((width as f64 * factor).round() as u32).max(1),
        ((height as f64 * factor).round() as u32).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    #[test]
    fn upright_points_map_to_the_screen_as_built() {
        // Landscape right (what Rotate Left gives an iPhone): the
        // interface's top is the built screen's right edge, so the picture's
        // top left corner is the built screen's top right one.
        let turn = Turn::LandscapeRight;
        assert!(close(turn.to_device(0.0, 0.0), (1.0, 0.0)));
        assert!(close(turn.to_device(1.0, 0.0), (1.0, 1.0)));
        assert!(close(turn.to_device(0.0, 1.0), (0.0, 0.0)));
        // Landscape left: the picture's top left is the built bottom left.
        let turn = Turn::LandscapeLeft;
        assert!(close(turn.to_device(0.0, 0.0), (0.0, 1.0)));
        assert!(close(turn.to_device(1.0, 0.0), (0.0, 0.0)));
        assert!(close(Turn::UpsideDown.to_device(0.2, 0.3), (0.8, 0.7)));
        assert!(close(Turn::Portrait.to_device(0.2, 0.3), (0.2, 0.3)));
        // Outside the picture is its edge.
        assert!(close(Turn::Portrait.to_device(-0.5, 1.5), (0.0, 1.0)));
    }

    #[test]
    fn turning_there_and_back_is_the_same_point() {
        for turn in [
            Turn::Portrait,
            Turn::UpsideDown,
            Turn::LandscapeLeft,
            Turn::LandscapeRight,
        ] {
            for (x, y) in [(0.1, 0.9), (0.5, 0.5), (0.33, 0.07)] {
                let (dx, dy) = turn.to_device(x, y);
                assert!(close(turn.to_shown(dx, dy), (x, y)), "{turn:?} {x},{y}");
            }
        }
    }

    #[test]
    fn interface_orientations_and_sizes() {
        assert_eq!(Turn::from_interface(1), Turn::Portrait);
        assert_eq!(Turn::from_interface(4), Turn::LandscapeRight);
        assert_eq!(Turn::from_interface(0), Turn::Portrait);
        assert_eq!(Turn::LandscapeLeft.shown_size(1206, 2622), (2622, 1206));
        assert_eq!(Turn::UpsideDown.shown_size(1206, 2622), (1206, 2622));
    }

    #[test]
    fn rotating_the_device_goes_round() {
        let portrait = Held(1);
        assert_eq!(portrait.left(), Held(3));
        assert_eq!(portrait.left().left().left().left(), portrait);
        assert_eq!(portrait.right(), Held(4));
        assert_eq!(portrait.left().right(), portrait);
        // The interface turns against the device.
        assert_eq!(Held::from_turn(Turn::LandscapeRight), Held(3));
        assert_eq!(Held::from_turn(Turn::LandscapeLeft), Held(4));
    }

    #[test]
    fn pictures_are_at_most_two_pixels_a_point() {
        // An iPhone 17 Pro at 3x is pictured at 2x.
        assert_eq!(picture_size(1206, 2622, 3.0, 2400), (804, 1748));
        // A 2x screen stays as it is unless it's very large.
        assert_eq!(picture_size(1640, 2360, 2.0, 2400), (1640, 2360));
        assert_eq!(picture_size(2064, 2752, 2.0, 2400), (1800, 2400));
        assert_eq!(picture_size(320, 480, 1.0, 2400), (320, 480));
    }
}
