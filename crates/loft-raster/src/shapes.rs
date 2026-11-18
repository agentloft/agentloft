//! Rounded slabs: a phone's body, a camera's plateau, a lens, a button.

use crate::math::Vec3;
use crate::raster::{Mesh, Vertex};

/// A point of an outline, and the outward direction there.
pub type OutlinePoint = ((f32, f32), (f32, f32));

/// A rounded rectangle's outline, centred on the origin, counter-clockwise
/// from the bottom right corner: each point with the outward direction
/// there. `radii`: top left, top right, bottom right, bottom left.
pub fn outline(w: f32, h: f32, radii: [f32; 4], segments: usize) -> Vec<OutlinePoint> {
    let limit = (w.min(h) / 2.).max(0.);
    let [tl, tr, br, bl] = radii.map(|r| r.clamp(0., limit));
    let corners = [
        (w / 2. - br, -h / 2. + br, br, -90f32),
        (w / 2. - tr, h / 2. - tr, tr, 0.),
        (-w / 2. + tl, h / 2. - tl, tl, 90.),
        (-w / 2. + bl, -h / 2. + bl, bl, 180.),
    ];
    let mut out = Vec::with_capacity(4 * (segments + 1));
    for (cx, cy, r, start) in corners {
        for i in 0..=segments {
            let a = (start + 90. * i as f32 / segments as f32).to_radians();
            let (s, c) = a.sin_cos();
            out.push(((cx + r * c, cy + r * s), (c, s)));
        }
    }
    out
}

/// A rounded rectangle `w` by `h`, `depth` thick, centred on the origin
/// with its front facing +z; its edges cut back by `bevel` all round.
#[derive(Debug, Clone, Copy)]
pub struct Slab {
    pub w: f32,
    pub h: f32,
    pub radii: [f32; 4],
    pub depth: f32,
    pub bevel: f32,
    pub segments: usize,
    /// Materials: its front, its edges and its back.
    pub front: u16,
    pub side: u16,
    pub back: u16,
}

impl Slab {
    pub fn mesh(&self) -> Mesh {
        let d = self.depth / 2.;
        let b = self
            .bevel
            .clamp(0., (self.depth / 2.).min(self.w.min(self.h) / 4.));
        let outer = outline(self.w, self.h, self.radii, self.segments);
        let inner = outline(
            self.w - 2. * b,
            self.h - 2. * b,
            self.radii.map(|r| (r - b).max(0.)),
            self.segments,
        );
        let mut mesh = Mesh::default();
        let ring = |mesh: &mut Mesh,
                    points: &[OutlinePoint],
                    z: f32,
                    normal: &dyn Fn((f32, f32)) -> Vec3,
                    material: u16| {
            let first = mesh.vertices.len() as u32;
            for &((x, y), n) in points {
                mesh.push(Vertex {
                    pos: Vec3::new(x, y, z),
                    normal: normal(n).normalized(),
                    material,
                });
            }
            first
        };
        let n = outer.len() as u32;
        // A band from ring `a` (in front) to ring `b` (behind), facing out.
        let band = |mesh: &mut Mesh, a: u32, b: u32| {
            for i in 0..n {
                let j = (i + 1) % n;
                mesh.triangles.push([a + i, b + i, b + j]);
                mesh.triangles.push([a + i, b + j, a + j]);
            }
        };
        // The faces: a fan from the middle.
        let cap = |mesh: &mut Mesh, z: f32, normal: Vec3, material: u16, front: bool| {
            let centre = mesh.push(Vertex {
                pos: Vec3::new(0., 0., z),
                normal,
                material,
            });
            let first = ring(mesh, &inner, z, &|_| normal, material);
            for i in 0..n {
                let j = (i + 1) % n;
                mesh.triangles.push(if front {
                    [centre, first + i, first + j]
                } else {
                    [centre, first + j, first + i]
                });
            }
        };
        cap(&mut mesh, d, Vec3::Z, self.front, true);
        cap(&mut mesh, -d, -Vec3::Z, self.back, false);
        let flat = |(x, y): (f32, f32)| Vec3::new(x, y, 0.);
        if b > 0. {
            let a = ring(
                &mut mesh,
                &inner,
                d,
                &|(x, y)| Vec3::new(x * 0.6, y * 0.6, 1.),
                self.side,
            );
            let c = ring(
                &mut mesh,
                &outer,
                d - b,
                &|(x, y)| Vec3::new(x, y, 0.4),
                self.side,
            );
            band(&mut mesh, a, c);
        }
        let a = ring(&mut mesh, &outer, d - b, &flat, self.side);
        let c = ring(&mut mesh, &outer, -d + b, &flat, self.side);
        band(&mut mesh, a, c);
        if b > 0. {
            let a = ring(
                &mut mesh,
                &outer,
                -d + b,
                &|(x, y)| Vec3::new(x, y, -0.4),
                self.side,
            );
            let c = ring(
                &mut mesh,
                &inner,
                -d,
                &|(x, y)| Vec3::new(x * 0.6, y * 0.6, -1.),
                self.side,
            );
            band(&mut mesh, a, c);
        }
        mesh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outline_goes_round_counter_clockwise() {
        let points = outline(4., 2., [0.5; 4], 4);
        assert_eq!(points.len(), 20);
        // Twice the area, by the shoelace formula: positive.
        let area2: f32 = (0..points.len())
            .map(|i| {
                let (a, b) = (points[i].0, points[(i + 1) % points.len()].0);
                a.0 * b.1 - b.0 * a.1
            })
            .sum();
        assert!(area2 > 0.);
        // Within its rectangle, and the area a bit less than 4 × 2.
        assert!(
            points
                .iter()
                .all(|((x, y), _)| x.abs() <= 2. && y.abs() <= 1.)
        );
        assert!(area2 / 2. < 8. && area2 / 2. > 7.5);
    }

    #[test]
    fn a_slab_faces_outward() {
        let mesh = Slab {
            w: 4.,
            h: 6.,
            radii: [1.; 4],
            depth: 1.,
            bevel: 0.2,
            segments: 6,
            front: 0,
            side: 1,
            back: 2,
        }
        .mesh();
        // Each triangle's winding agrees with the way it faces: away from
        // the middle.
        for t in &mesh.triangles {
            let [a, b, c] = t.map(|i| mesh.vertices[i as usize].pos);
            let n = (b - a).cross(c - a);
            if n.length() < 1e-6 {
                continue;
            }
            let middle = (a + b + c) * (1. / 3.);
            assert!(n.dot(middle) > 0., "{a:?} {b:?} {c:?}");
        }
    }
}
