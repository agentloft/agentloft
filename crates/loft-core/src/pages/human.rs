//! Human-like pointer and keyboard timing for agents' input
//! (`[browser] human_input`): what behavioural bot detection looks at
//! (where the pointer comes from, the shape and pace of its path, how long
//! a button is held, the rhythm of keys and of the wheel), kept quick
//! enough that tools don't drag on.
//!
//! Only plans live here (points, deltas and the waits between them), so
//! they can be tested without a browser; `input.rs` sends them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A point in the viewport, in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    fn distance(self, other: Point) -> f64 {
        (other.x - self.x).hypot(other.y - self.y)
    }

    /// Rounded to a tenth of a pixel.
    fn rounded(self) -> Point {
        Point::new(
            (self.x * 10.0).round() / 10.0,
            (self.y * 10.0).round() / 10.0,
        )
    }
}

/// A page's pointer and keyboard, as the agent left them.
#[derive(Debug, Default)]
pub struct InputState {
    /// Where the agent's pointer is; `None` until it first moves.
    pub(super) pointer: Option<Point>,
    /// Mouse buttons held down (left 1, right 2, middle 4).
    pub(super) buttons: u8,
    /// When the pointer last arrived somewhere.
    pub(super) arrived: Option<Instant>,
    /// When the last key went down.
    pub(super) last_key: Option<Instant>,
    /// The last target aimed at and the offset used for it, so that aiming
    /// at it again lands on the same spot.
    aim: Option<(Point, Point)>,
    pub(super) rng: Rng,
}

impl InputState {
    /// Where a person aiming at `target` would land: a pixel or two off,
    /// the same spot each time for the same target.
    pub(super) fn aim_at(&mut self, target: Point) -> Point {
        let offset = match self.aim {
            Some((last, offset)) if last.distance(target) <= 1.0 => offset,
            _ => {
                let offset = Point::new(self.rng.range(-3.0, 3.0), self.rng.range(-2.0, 2.0));
                self.aim = Some((target, offset));
                offset
            }
        };
        Point::new(target.x + offset.x, target.y + offset.y).rounded()
    }

    /// Where the pointer comes from when the page has none yet: somewhere
    /// near the target, as a hand already on the mouse would be.
    pub(super) fn start_near(&mut self, target: Point, viewport: Option<(f64, f64)>) -> Point {
        let start = Point::new(
            target.x + self.rng.range(-260.0, 260.0),
            target.y + self.rng.range(-180.0, 180.0),
        );
        clamp(start.rounded(), viewport)
    }
}

/// A small, fast random number generator (xorshift64*); timing jitter
/// needs nothing better, and each page's is seeded differently.
#[derive(Debug)]
pub struct Rng(u64);

impl Default for Rng {
    fn default() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let count = COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        Rng::seeded(nanos ^ count.rotate_left(17))
    }
}

impl Rng {
    pub fn seeded(seed: u64) -> Self {
        // splitmix64, so that close seeds start far apart (and never at 0).
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng(if z == 0 { 1 } else { z })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `[low, high)`.
    pub fn range(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.unit()
    }

    /// A duration uniform in `[low, high]` milliseconds.
    pub fn millis(&mut self, low: f64, high: f64) -> Duration {
        Duration::from_secs_f64(self.range(low, high) / 1000.0)
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

/// `p` kept on screen: not negative, and inside the viewport when its size
/// is known.
pub fn clamp(p: Point, viewport: Option<(f64, f64)>) -> Point {
    let (mut x, mut y) = (p.x.max(0.0), p.y.max(0.0));
    if let Some((width, height)) = viewport {
        x = x.min((width - 1.0).max(0.0));
        y = y.min((height - 1.0).max(0.0));
    }
    Point::new(x, y)
}

/// The longest a pointer move takes.
const MAX_MOVE: f64 = 700.0;
/// Moves at least this long may overshoot.
const OVERSHOOT_FROM: f64 = 250.0;

/// How long a person's move over `distance` pixels takes, before jitter
/// (milliseconds): about 150 for 40 px, 400 for 400 px, 500 for 1000 px.
fn move_millis(distance: f64) -> f64 {
    124.0 * (1.0 + distance / 17.0).ln()
}

/// Slow away, fast across, slow on arrival (minimum jerk).
fn ease_in_out(t: f64) -> f64 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// A pointer move from `from` to `to` as a person makes it: the points to
/// send, each with the wait after it. Empty for a move under 2 px (one
/// event straight to `to` does). The last point is `to` exactly.
pub fn path(
    rng: &mut Rng,
    from: Point,
    to: Point,
    viewport: Option<(f64, f64)>,
) -> Vec<(Point, Duration)> {
    let distance = from.distance(to);
    if distance < 2.0 {
        return Vec::new();
    }
    let (dx, dy) = ((to.x - from.x) / distance, (to.y - from.y) / distance);
    let overshoot = (distance > OVERSHOOT_FROM && rng.chance(0.25)).then(|| rng.range(4.0, 12.0));
    let end = match overshoot {
        Some(by) => Point::new(to.x + dx * by, to.y + dy * by),
        None => to,
    };

    let jitter = if rng.chance(0.5) {
        rng.range(0.80, 0.85)
    } else {
        rng.range(1.15, 1.20)
    };
    let total = (move_millis(distance) * jitter).min(MAX_MOVE);
    // The curve bows to one side by 4–18 % of the distance: a cubic Bézier
    // whose two control points sit off the line (the curve reaches ¾ of
    // their offset).
    let bow = distance * rng.range(0.04, 0.18) * if rng.chance(0.5) { 1.0 } else { -1.0 } / 0.75;
    let (nx, ny) = (-dy, dx);
    let (a, b) = (rng.range(0.2, 0.4), rng.range(0.6, 0.8));
    let c1 = Point::new(
        from.x + (end.x - from.x) * a + nx * bow,
        from.y + (end.y - from.y) * a + ny * bow,
    );
    let c2 = Point::new(
        from.x + (end.x - from.x) * b + nx * bow,
        from.y + (end.y - from.y) * b + ny * bow,
    );
    let bezier = |t: f64| {
        let u = 1.0 - t;
        let (w0, w1, w2, w3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        Point::new(
            w0 * from.x + w1 * c1.x + w2 * c2.x + w3 * end.x,
            w0 * from.y + w1 * c1.y + w2 * c2.y + w3 * end.y,
        )
    };

    // Sample times every 8–17 ms, stretched to end at `total`.
    let mut gaps = Vec::new();
    let mut elapsed = 0.0;
    while elapsed < total || gaps.len() < 2 {
        let gap = rng.range(8.0, 17.0);
        gaps.push(gap);
        elapsed += gap;
    }
    let mut samples = Vec::with_capacity(gaps.len() + 7);
    let mut at = 0.0;
    let count = gaps.len();
    let tremor = 0.6;
    for (i, gap) in gaps.into_iter().enumerate() {
        at += gap;
        let t = (at / elapsed).min(1.0);
        let mut p = bezier(ease_in_out(t));
        if i + 1 < count {
            // A hand's tremor, fading as it arrives.
            let fade = 1.0 - t;
            p.x += rng.range(-tremor, tremor) * fade;
            p.y += rng.range(-tremor, tremor) * fade;
        } else {
            p = end;
        }
        samples.push((
            clamp(p.rounded(), viewport),
            Duration::from_secs_f64(gap / 1000.0),
        ));
    }
    if overshoot.is_some() {
        // Back from past the target, in a few short steps.
        let steps = 3 + (rng.unit() * 5.0) as usize;
        for i in 1..=steps {
            let t = ease_in_out(i as f64 / steps as f64);
            let p = if i == steps {
                to
            } else {
                Point::new(end.x + (to.x - end.x) * t, end.y + (to.y - end.y) * t)
            };
            samples.push((clamp(p.rounded(), viewport), rng.millis(10.0, 18.0)));
        }
    }
    if let Some(last) = samples.last_mut() {
        last.0 = clamp(to, viewport);
    }
    samples
}

/// A wheel scroll as a mouse wheel or trackpad gives it: one event under
/// 40 px, else a glide of about 60 events a second, big deltas first.
/// Each with the wait after it; the deltas add up to `(dx, dy)` exactly.
pub fn glide(rng: &mut Rng, dx: f64, dy: f64) -> Vec<((f64, f64), Duration)> {
    let length = dx.hypot(dy);
    if length < 40.0 {
        return vec![((dx, dy), Duration::ZERO)];
    }
    let total = (140.0 + 0.45 * length).min(650.0) * rng.range(0.85, 1.15);
    let count = ((total / 16.7).round() as usize).max(4);
    // Ease-out: how far along it is after a share `t` of its time.
    let progress = |t: f64| 1.0 - (1.0 - t).powi(3);
    let round = |v: f64| (v * 100.0).round() / 100.0;
    let mut events = Vec::with_capacity(count);
    let (mut sent_x, mut sent_y) = (0.0, 0.0);
    for i in 1..=count {
        let (ex, ey) = if i == count {
            (dx - sent_x, dy - sent_y)
        } else {
            let p = progress(i as f64 / count as f64);
            (round(dx * p - sent_x), round(dy * p - sent_y))
        };
        sent_x += ex;
        sent_y += ey;
        events.push(((ex, ey), rng.millis(15.5, 18.0)));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_quick_and_lands_on_the_target() {
        for seed in 0..200 {
            let mut rng = Rng::seeded(seed);
            let to = Point::new(400.0, 300.0);
            let samples = path(&mut rng, Point::new(10.0, 10.0), to, Some((1280.0, 800.0)));
            assert!(samples.len() > 5, "{samples:?}");
            let last = samples.last().unwrap().0;
            assert!(last.distance(to) < 40.0);
            assert_eq!(last, to);
            let mut total = Duration::ZERO;
            for (p, wait) in &samples {
                let ms = wait.as_secs_f64() * 1000.0;
                assert!((8.0..=18.0).contains(&ms), "{ms}");
                assert!(p.x >= 0.0 && p.y >= 0.0);
                total += *wait;
            }
            assert!(total < Duration::from_millis(800), "{total:?}");
        }
    }

    #[test]
    fn a_tiny_move_is_one_event() {
        let mut rng = Rng::seeded(1);
        assert!(path(&mut rng, Point::new(5.0, 5.0), Point::new(6.0, 5.0), None).is_empty());
    }

    #[test]
    fn durations_grow_with_distance() {
        assert!((move_millis(40.0) - 150.0).abs() < 25.0);
        assert!((move_millis(400.0) - 400.0).abs() < 30.0);
        assert!((move_millis(1000.0) - 500.0).abs() < 30.0);
        // Even the longest move, slowed down, is capped.
        let mut rng = Rng::seeded(7);
        for _ in 0..50 {
            let samples = path(
                &mut rng,
                Point::new(0.0, 0.0),
                Point::new(1270.0, 790.0),
                None,
            );
            let total: Duration = samples.iter().map(|s| s.1).sum();
            // 700 ms of path plus an overshoot's way back.
            assert!(total < Duration::from_millis(850), "{total:?}");
        }
    }

    #[test]
    fn paths_bow_to_one_side() {
        let mut rng = Rng::seeded(3);
        let samples = path(
            &mut rng,
            Point::new(0.0, 100.0),
            Point::new(500.0, 100.0),
            None,
        );
        let off = samples
            .iter()
            .map(|(p, _)| (p.y - 100.0).abs())
            .fold(0.0, f64::max);
        assert!(off > 5.0 && off < 120.0, "{off}");
    }

    #[test]
    fn aims_land_on_the_same_spot() {
        let mut state = InputState::default();
        let target = Point::new(100.0, 50.0);
        let a = state.aim_at(target);
        assert!(
            (a.x - 100.0).abs() <= 3.0 && (a.y - 50.0).abs() <= 2.0,
            "{a:?}"
        );
        assert!((state.aim_at(Point::new(100.5, 50.0)).x - a.x - 0.5).abs() < 0.11);
        assert_eq!(state.aim_at(target), a);
        let start = state.start_near(target, Some((800.0, 600.0)));
        assert!((start.x - 100.0).abs() <= 260.0 && (start.y - 50.0).abs() <= 180.0);
    }

    #[test]
    fn a_glide_adds_up() {
        for seed in 0..100 {
            let mut rng = Rng::seeded(seed);
            let events = glide(&mut rng, 0.0, 600.0);
            assert!(events.len() >= 4);
            let sum: f64 = events.iter().map(|e| e.0.1).sum();
            assert!((sum - 600.0).abs() < 1e-6, "{sum}");
            // Big deltas first.
            assert!(events[0].0.1 > events[events.len() - 1].0.1);
            for (_, wait) in &events {
                let ms = wait.as_secs_f64() * 1000.0;
                assert!((15.5..=18.0).contains(&ms), "{ms}");
            }
            let total: Duration = events.iter().map(|e| e.1).sum();
            assert!(total < Duration::from_millis(900), "{total:?}");
        }
        let mut rng = Rng::seeded(1);
        let sideways = glide(&mut rng, -123.45, 67.0);
        let (x, y) = sideways
            .iter()
            .fold((0.0, 0.0), |(x, y), e| (x + e.0.0, y + e.0.1));
        assert!((x + 123.45).abs() < 1e-6 && (y - 67.0).abs() < 1e-6);
        assert_eq!(glide(&mut rng, 0.0, 10.0).len(), 1);
    }

    #[test]
    fn points_stay_on_screen() {
        assert_eq!(
            clamp(Point::new(-5.0, 900.0), Some((800.0, 600.0))),
            Point::new(0.0, 599.0)
        );
        assert_eq!(clamp(Point::new(-5.0, 900.0), None), Point::new(0.0, 900.0));
    }
}
