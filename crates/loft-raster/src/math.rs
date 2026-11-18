//! Points, directions and turns in three dimensions.

use std::ops::{Add, Mul, Neg, Sub};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Self = Self::new(0., 0., 0.);
    pub const X: Self = Self::new(1., 0., 0.);
    pub const Y: Self = Self::new(0., 1., 0.);
    pub const Z: Self = Self::new(0., 0., 1.);

    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Self) -> Self {
        Self::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    /// The same direction, one long (or itself if it has no length).
    pub fn normalized(self) -> Self {
        let l = self.length();
        if l > 0. { self * (1. / l) } else { self }
    }
}

impl Add for Vec3 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self::new(self.x * k, self.y * k, self.z * k)
    }
}

impl Neg for Vec3 {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}

/// A turn: `m.apply(v)` is each row dotted with `v`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat3 {
    pub rows: [Vec3; 3],
}

impl Mat3 {
    pub const IDENTITY: Self = Self {
        rows: [Vec3::X, Vec3::Y, Vec3::Z],
    };

    /// A turn by `radians` about `axis`, counter-clockwise looking down the
    /// axis towards its origin (right-handed).
    pub fn rotation(axis: Vec3, radians: f32) -> Self {
        let a = axis.normalized();
        let (s, c) = radians.sin_cos();
        let t = 1. - c;
        Self {
            rows: [
                Vec3::new(
                    t * a.x * a.x + c,
                    t * a.x * a.y - s * a.z,
                    t * a.x * a.z + s * a.y,
                ),
                Vec3::new(
                    t * a.x * a.y + s * a.z,
                    t * a.y * a.y + c,
                    t * a.y * a.z - s * a.x,
                ),
                Vec3::new(
                    t * a.x * a.z - s * a.y,
                    t * a.y * a.z + s * a.x,
                    t * a.z * a.z + c,
                ),
            ],
        }
    }

    pub fn apply(&self, v: Vec3) -> Vec3 {
        Vec3::new(
            self.rows[0].dot(v),
            self.rows[1].dot(v),
            self.rows[2].dot(v),
        )
    }

    /// `self` after `first`: `(self * first).apply(v) == self.apply(first.apply(v))`.
    pub fn mul(&self, first: &Self) -> Self {
        let t = first.transpose();
        Self {
            rows: self
                .rows
                .map(|r| Vec3::new(r.dot(t.rows[0]), r.dot(t.rows[1]), r.dot(t.rows[2]))),
        }
    }

    /// The turn back (a turn's inverse is its transpose).
    pub fn transpose(&self) -> Self {
        let [a, b, c] = self.rows;
        Self {
            rows: [
                Vec3::new(a.x, b.x, c.x),
                Vec3::new(a.y, b.y, c.y),
                Vec3::new(a.z, b.z, c.z),
            ],
        }
    }
}

/// A turn, then a move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub rot: Mat3,
    pub pos: Vec3,
}

impl Transform {
    pub const IDENTITY: Self = Self {
        rot: Mat3::IDENTITY,
        pos: Vec3::ZERO,
    };

    pub fn rotate(rot: Mat3) -> Self {
        Self {
            rot,
            pos: Vec3::ZERO,
        }
    }

    pub fn translate(pos: Vec3) -> Self {
        Self {
            rot: Mat3::IDENTITY,
            pos,
        }
    }

    /// A turn by `radians` about the line through `through` along `axis`.
    pub fn rotate_about(axis: Vec3, through: Vec3, radians: f32) -> Self {
        let rot = Mat3::rotation(axis, radians);
        Self {
            rot,
            pos: through - rot.apply(through),
        }
    }

    pub fn point(&self, p: Vec3) -> Vec3 {
        self.rot.apply(p) + self.pos
    }

    pub fn dir(&self, d: Vec3) -> Vec3 {
        self.rot.apply(d)
    }

    /// `self`, then `outer`.
    pub fn then(&self, outer: &Self) -> Self {
        Self {
            rot: outer.rot.mul(&self.rot),
            pos: outer.point(self.pos),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-5
    }

    #[test]
    fn turns_are_right_handed() {
        let quarter = std::f32::consts::FRAC_PI_2;
        // About z, x goes to y.
        assert!(close(
            Mat3::rotation(Vec3::Z, quarter).apply(Vec3::X),
            Vec3::Y
        ));
        // About y, z goes to x; about x, y goes to z.
        assert!(close(
            Mat3::rotation(Vec3::Y, quarter).apply(Vec3::Z),
            Vec3::X
        ));
        assert!(close(
            Mat3::rotation(Vec3::X, quarter).apply(Vec3::Y),
            Vec3::Z
        ));
    }

    #[test]
    fn composing_and_undoing() {
        let a = Mat3::rotation(Vec3::new(1., 2., 3.), 0.7);
        let b = Mat3::rotation(Vec3::Y, -1.3);
        let v = Vec3::new(0.3, -2., 5.);
        assert!(close(a.mul(&b).apply(v), a.apply(b.apply(v))));
        assert!(close(a.transpose().apply(a.apply(v)), v));
        let t = Transform::rotate_about(Vec3::Y, Vec3::new(1., 0., 0.), std::f32::consts::PI);
        // A half turn about the line x = 1 takes the origin to x = 2.
        assert!(close(t.point(Vec3::ZERO), Vec3::new(2., 0., 0.)));
        let u = Transform::translate(Vec3::Z).then(&t);
        assert!(close(u.point(v), t.point(v + Vec3::Z)));
    }
}
