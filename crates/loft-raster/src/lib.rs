//! A small software renderer for the Device panel's 3D device models
//! (ADR-0020): triangles with a depth buffer, lit by a fixed light, and
//! screens textured with the device's live picture (perspective-correct,
//! sampled between pixels, corners rounded). Each screen pixel remembers
//! which screen and which point of the picture it shows ([`Raster::hit`]),
//! so a press on the drawn screen becomes a touch exactly where it lands.
//!
//! No GPUI and no other dependency: the same code runs in the browser
//! client. Built with `opt-level = 3` even in debug builds (the root
//! `Cargo.toml`), since it runs on the UI's thread for every frame.

pub mod device;
mod math;
mod raster;
mod shapes;

pub use math::{Mat3, Transform, Vec3};
pub use raster::{
    Camera, Hit, Material, Mesh, Order, OwnedTexture, Projected, Raster, ScreenQuad, Texture,
    Vertex,
};
pub use shapes::{Slab, outline};

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera looking straight at the origin, one unit a pixel there.
    fn face_on(width: usize, height: usize) -> Camera {
        let distance = 1000.;
        Camera {
            width,
            height,
            rotation: Mat3::IDENTITY,
            distance,
            focal: distance,
            center: (width as f32 / 2., height as f32 / 2.),
        }
    }

    /// A square quad `size` across, centred at `z`.
    fn square(size: f32, z: f32, id: u8) -> ScreenQuad {
        let h = size / 2.;
        ScreenQuad {
            corners: [
                Vec3::new(-h, h, z),
                Vec3::new(h, h, z),
                Vec3::new(h, -h, z),
                Vec3::new(-h, -h, z),
            ],
            uv: [(0., 0.), (1., 0.), (1., 1.), (0., 1.)],
            local: [(0., 0.), (size, 0.), (size, size), (0., size)],
            screen: (size, size),
            radius: 0.,
            id,
        }
    }

    fn solid(rgba: [u8; 4], w: usize, h: usize) -> Vec<u8> {
        rgba.repeat(w * h)
    }

    fn pixel(color: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * width + x) * 4;
        [color[i], color[i + 1], color[i + 2], color[i + 3]]
    }

    #[test]
    fn a_quad_lands_where_it_should() {
        let (w, h) = (200, 200);
        let cam = face_on(w, h);
        let mut raster = Raster::new(Order::Rgba);
        let mut color = vec![0u8; w * h * 4];
        raster.begin(w, h);
        let red = solid([200, 10, 10, 255], 4, 4);
        let tex = Texture {
            width: 4,
            height: 4,
            pixels: &red,
        };
        raster.draw_screen(&mut color, &cam, &square(100., 0., 7), Some(tex));
        // Pixels 50..150 each way are the quad, the rest transparent.
        assert_eq!(pixel(&color, w, 50, 50), [200, 10, 10, 255]);
        assert_eq!(pixel(&color, w, 149, 149), [200, 10, 10, 255]);
        assert_eq!(pixel(&color, w, 49, 100)[3], 0);
        assert_eq!(pixel(&color, w, 150, 100)[3], 0);
        assert_eq!(pixel(&color, w, 100, 49)[3], 0);
        // And they know where on the picture they are.
        let hit = raster.hit(60, 90).unwrap();
        assert_eq!(hit.id, 7);
        assert!(
            (hit.u - 0.105).abs() < 1e-3 && (hit.v - 0.405).abs() < 1e-3,
            "{hit:?}"
        );
        assert_eq!(raster.hit(10, 10), None);
        // The next frame starts clean.
        let mut color = vec![0u8; w * h * 4];
        raster.begin(w, h);
        raster.draw_screen(&mut color, &cam, &square(20., 0., 1), None);
        assert_eq!(raster.hit(60, 90), None);
        // Without a picture: dark glass, nothing to touch.
        assert_eq!(pixel(&color, w, 100, 100), [10, 10, 13, 255]);
        assert_eq!(raster.hit(100, 100), None);
    }

    #[test]
    fn a_picture_is_sampled_between_its_pixels() {
        // Two pixels, black and white: halfway between is grey.
        let pixels = [0, 0, 0, 255, 255, 255, 255, 255];
        let tex = Texture {
            width: 2,
            height: 1,
            pixels: &pixels,
        };
        let (w, h) = (100, 10);
        let cam = face_on(w, h);
        let mut raster = Raster::new(Order::Rgba);
        let mut color = vec![0u8; w * h * 4];
        raster.begin(w, h);
        let mut q = square(100., 0., 1);
        q.corners = [
            Vec3::new(-50., 5., 0.),
            Vec3::new(50., 5., 0.),
            Vec3::new(50., -5., 0.),
            Vec3::new(-50., -5., 0.),
        ];
        q.local = [(0., 0.), (100., 0.), (100., 10.), (0., 10.)];
        q.screen = (100., 10.);
        raster.draw_screen(&mut color, &cam, &q, Some(tex));
        assert_eq!(pixel(&color, w, 5, 5)[0], 0);
        assert_eq!(pixel(&color, w, 95, 5)[0], 255);
        let mid = pixel(&color, w, 50, 5)[0];
        assert!((120..=135).contains(&mid), "{mid}");
    }

    #[test]
    fn the_nearer_surface_wins_whatever_the_order() {
        let (w, h) = (100, 100);
        let cam = face_on(w, h);
        let red = solid([255, 0, 0, 255], 1, 1);
        let blue = solid([0, 0, 255, 255], 1, 1);
        let tex = |p| Texture {
            width: 1,
            height: 1,
            pixels: p,
        };
        for near_first in [true, false] {
            let mut raster = Raster::new(Order::Rgba);
            let mut color = vec![0u8; w * h * 4];
            raster.begin(w, h);
            let near = square(40., 10., 1);
            let far = square(80., -10., 2);
            if near_first {
                raster.draw_screen(&mut color, &cam, &near, Some(tex(&red)));
                raster.draw_screen(&mut color, &cam, &far, Some(tex(&blue)));
            } else {
                raster.draw_screen(&mut color, &cam, &far, Some(tex(&blue)));
                raster.draw_screen(&mut color, &cam, &near, Some(tex(&red)));
            }
            assert_eq!(pixel(&color, w, 50, 50), [255, 0, 0, 255]);
            assert_eq!(raster.hit(50, 50).unwrap().id, 1);
            assert_eq!(pixel(&color, w, 15, 50), [0, 0, 255, 255]);
            assert_eq!(raster.hit(15, 50).unwrap().id, 2);
        }
    }

    #[test]
    fn back_faces_are_not_drawn() {
        let (w, h) = (100, 100);
        let mut cam = face_on(w, h);
        cam.rotation = Mat3::rotation(Vec3::Y, std::f32::consts::PI);
        let mut raster = Raster::new(Order::Rgba);
        let mut color = vec![0u8; w * h * 4];
        raster.begin(w, h);
        raster.draw_screen(&mut color, &cam, &square(50., 0., 1), None);
        assert_eq!(pixel(&color, w, 50, 50)[3], 0);
    }

    #[test]
    fn a_screen_has_round_corners() {
        let (w, h) = (200, 200);
        let cam = face_on(w, h);
        let white = solid([255; 4], 1, 1);
        let tex = Texture {
            width: 1,
            height: 1,
            pixels: &white,
        };
        let mut raster = Raster::new(Order::Rgba);
        let mut color = vec![0u8; w * h * 4];
        raster.begin(w, h);
        let mut q = square(100., 0., 1);
        q.radius = 20.;
        raster.draw_screen(&mut color, &cam, &q, Some(tex));
        // The very corner is cut off, the middle of an edge is not.
        assert_eq!(raster.hit(51, 51), None);
        assert_eq!(pixel(&color, w, 51, 51)[3], 0);
        assert!(raster.hit(100, 51).is_some());
        assert!(raster.hit(57, 57).is_some());
    }

    #[test]
    fn a_press_maps_back_to_the_picture_from_any_angle() {
        use device::{Kind, Live, Model, Style, View};
        let model = Model {
            kind: Kind::Phone(Style::Ios),
            main: (390., 844.),
            cover: None,
            inward: true,
            hinge: 180.,
        };
        let scene = device::scene(&model, Live::Main, 0);
        let pixels = solid([90, 90, 90, 255], 8, 8);
        let tex = Texture {
            width: 8,
            height: 8,
            pixels: &pixels,
        };
        for (yaw, pitch) in [(0., 0.), (35., -20.), (-60., 30.), (10., 70.)] {
            let cam = device::camera(
                600,
                800,
                scene.extent,
                View {
                    yaw,
                    pitch,
                    zoom: 1.,
                },
            );
            let mut raster = Raster::new(Order::Bgra);
            let mut color = vec![0u8; 600 * 800 * 4];
            device::render(&scene, &cam, &mut raster, &mut color, Some(tex));
            let mut checked = 0;
            for y in (0..800).step_by(37) {
                for x in (0..600).step_by(23) {
                    let Some(hit) = raster.hit(x, y) else {
                        continue;
                    };
                    // The ray through the pixel's centre meets the screen
                    // at the same point of the picture.
                    let (u, v) =
                        device::pick(&scene, &cam, hit.id, x as f32 + 0.5, y as f32 + 0.5).unwrap();
                    assert!(
                        (u - hit.u).abs() < 2e-3 && (v - hit.v).abs() < 2e-3,
                        "{yaw},{pitch} at {x},{y}: {hit:?} vs {u},{v}"
                    );
                    // And that point, projected, is the pixel.
                    let q = scene.screens[0].quad;
                    let p = q.corners[0]
                        + (q.corners[1] - q.corners[0]) * u
                        + (q.corners[3] - q.corners[0]) * v;
                    let back = cam.project(p).unwrap();
                    assert!((back.x - x as f32 - 0.5).abs() < 0.05);
                    assert!((back.y - y as f32 - 0.5).abs() < 0.05);
                    checked += 1;
                }
            }
            assert!(checked > 50, "{yaw},{pitch}: {checked}");
        }
    }

    #[test]
    fn a_picture_halves() {
        let pixels: Vec<u8> = [[0, 0, 0, 255], [255, 255, 255, 255]]
            .iter()
            .cycle()
            .take(4 * 2)
            .flatten()
            .copied()
            .collect();
        let tex = Texture {
            width: 4,
            height: 2,
            pixels: &pixels,
        };
        let half = tex.half();
        assert_eq!((half.width, half.height), (2, 1));
        assert!(half.pixels.chunks(4).all(|p| p[0] == 128 && p[3] == 255));
    }

    /// How long a frame takes, panel-sized at 2×:
    /// `cargo test -p loft-raster frame_time -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn frame_time() {
        use device::{Kind, Live, Model, Style, View};
        let (tw, th) = (1080, 2400);
        let pixels: Vec<u8> = (0..tw * th * 4).map(|i| (i % 251) as u8).collect();
        let tex = Texture {
            width: tw,
            height: th,
            pixels: &pixels,
        };
        let half = tex.half();
        let models = [
            (
                "iPhone",
                Kind::Phone(Style::Ios),
                (1179., 2556.),
                None,
                180.,
            ),
            (
                "Android",
                Kind::Phone(Style::Android),
                (1080., 2400.),
                None,
                180.,
            ),
            (
                "Fold open",
                Kind::Book,
                (2208., 1840.),
                Some((1080., 2092.)),
                180.,
            ),
            (
                "Fold half",
                Kind::Book,
                (2208., 1840.),
                Some((1080., 2092.)),
                100.,
            ),
        ];
        let (w, h) = (1000, 1500);
        let mut raster = Raster::new(Order::Bgra);
        for (name, kind, main, cover, hinge) in models {
            let model = Model {
                kind,
                main,
                cover,
                inward: true,
                hinge,
            };
            let scene = device::scene(&model, Live::Main, 0);
            let mut times = Vec::new();
            for (yaw, pitch) in [(0., 0.), (25., -15.), (-40., 20.), (160., 10.)]
                .into_iter()
                .cycle()
                .take(40)
            {
                let cam = device::camera(
                    w,
                    h,
                    scene.extent,
                    View {
                        yaw,
                        pitch,
                        zoom: 1.,
                    },
                );
                // The picture halved as the app does when it is drawn at
                // half its size or less.
                let t = match device::mip_level(cam.focal / cam.distance) {
                    0 => tex,
                    _ => half.as_texture(),
                };
                let start = std::time::Instant::now();
                let mut color = vec![0u8; w * h * 4];
                device::render(&scene, &cam, &mut raster, &mut color, Some(t));
                times.push(start.elapsed().as_secs_f64() * 1000.);
            }
            times.sort_by(f64::total_cmp);
            println!(
                "{name}: {} triangles, median {:.2} ms, worst {:.2} ms ({w}×{h})",
                scene.mesh.triangles.len(),
                times[times.len() / 2],
                times[times.len() - 1]
            );
        }
    }
}
