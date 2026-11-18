//! Device models: phones, tablets and foldables, built around the size of
//! their screens, with the live picture on the screen that shows it.
//!
//! Units are the screen's own pixels. The model stands upright facing +z
//! (x right, y up); a foldable's hinge runs down its middle, and its
//! moving half turns about it.

use crate::math::{Mat3, Transform, Vec3};
use crate::raster::{Camera, Material, Mesh, Raster, ScreenQuad, Texture};
use crate::shapes::Slab;

/// How the body looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Flat metal edges, very round corners, a square camera plateau.
    Ios,
    /// Rounder edges, a camera bar.
    Android,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Phone(Style),
    Tablet(Style),
    /// Folds like a book: two halves side by side, the hinge vertical.
    Book,
    /// Folds like a flip phone: one half above the other.
    Flip,
}

impl Kind {
    pub fn folds(self) -> bool {
        matches!(self, Kind::Book | Kind::Flip)
    }
}

/// A device to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Model {
    pub kind: Kind,
    /// The main screen's size in pixels, upright (not turned).
    pub main: (f32, f32),
    /// A foldable's cover screen, upright as it is seen folded shut.
    pub cover: Option<(f32, f32)>,
    /// The main screen folds inside (else outside, a fold-out).
    pub inward: bool,
    /// A foldable's hinge in degrees: 0 shut, 180 flat, 360 folded back.
    pub hinge: f32,
}

/// Which screen shows the live picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    None,
    Main,
    Cover,
}

/// The screens' ids in [`crate::Raster::hit`]s.
pub const MAIN: u8 = 1;
/// A foldable's main screen's moving half.
pub const MAIN_MOVING: u8 = 2;
pub const COVER: u8 = 3;

/// The materials' indices.
const GLASS: u16 = 0;
const FRAME: u16 = 1;
const BACK: u16 = 2;
const PLATEAU: u16 = 3;
const LENS: u16 = 4;
const RING: u16 = 5;
const FLASH: u16 = 6;

fn materials(style: Style) -> Vec<Material> {
    let glass = Material::new([0.03, 0.03, 0.035], 0.9, 60.);
    let lens = Material::new([0.02, 0.02, 0.04], 1.0, 80.);
    let flash = Material::new([0.93, 0.88, 0.74], 0.3, 10.);
    match style {
        Style::Ios => vec![
            glass,
            Material::new([0.6, 0.6, 0.62], 0.85, 36.),
            Material::new([0.36, 0.37, 0.39], 0.3, 14.),
            Material::new([0.4, 0.41, 0.43], 0.5, 28.),
            lens,
            Material::new([0.56, 0.56, 0.59], 0.8, 40.),
            flash,
        ],
        Style::Android => vec![
            glass,
            Material::new([0.25, 0.26, 0.28], 0.6, 24.),
            Material::new([0.2, 0.21, 0.24], 0.3, 12.),
            Material::new([0.11, 0.11, 0.12], 0.6, 40.),
            lens,
            Material::new([0.32, 0.32, 0.34], 0.7, 36.),
            flash,
        ],
    }
}

/// A screen in the scene, and whether it shows the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    pub quad: ScreenQuad,
    pub live: bool,
}

/// A model built and posed, ready to draw.
#[derive(Debug, Clone, Default)]
pub struct Scene {
    pub mesh: Mesh,
    pub materials: Vec<Material>,
    pub screens: Vec<Screen>,
    /// The size to fit into the view: the model's outline open flat (so a
    /// foldable stays the same size as it folds), as turned.
    pub extent: (f32, f32),
    /// Which way the main screen faces (its fixed half's), and the cover.
    pub main_normal: Vec3,
    pub cover_normal: Option<Vec3>,
}

/// The point of the picture at `(s, t)` on a screen `size` big (top left
/// 0,0), when the device is turned `roll` quarter turns counter-clockwise
/// (the picture stays upright, so it turns the other way on the screen).
pub fn picture_uv(s: f32, t: f32, size: (f32, f32), roll: i8) -> (f32, f32) {
    let (a, b) = (s / size.0.max(1e-6), t / size.1.max(1e-6));
    match roll.rem_euclid(4) {
        0 => (a, b),
        1 => (b, 1. - a),
        2 => (1. - a, 1. - b),
        _ => (1. - b, a),
    }
}

/// A screen facing +z at height `z`: the part `x0..x1`, `y0..y1` (y up) of
/// a whole screen whose top left is at `origin` and which is `size` big.
#[allow(clippy::too_many_arguments)]
fn screen_part(
    (x0, x1): (f32, f32),
    (y0, y1): (f32, f32),
    z: f32,
    origin: (f32, f32),
    size: (f32, f32),
    radius: f32,
    roll: i8,
    id: u8,
) -> ScreenQuad {
    let corners = [(x0, y1), (x1, y1), (x1, y0), (x0, y0)];
    let local = corners.map(|(x, y)| (x - origin.0, origin.1 - y));
    ScreenQuad {
        corners: corners.map(|(x, y)| Vec3::new(x, y, z)),
        uv: local.map(|(s, t)| picture_uv(s, t, size, roll)),
        local,
        screen: size,
        radius,
        id,
    }
}

fn transform_quad(q: &mut ScreenQuad, t: &Transform) {
    q.corners = q.corners.map(|c| t.point(c));
}

/// A slab placed by `t`.
fn placed(slab: Slab, t: Transform) -> Mesh {
    let mut m = slab.mesh();
    m.transform(&t);
    m
}

/// A round lens (or flash) `d` across on a back at height `z` (facing -z),
/// standing out `depth`.
fn lens(x: f32, y: f32, z: f32, d: f32, depth: f32, front: u16, side: u16) -> Mesh {
    let slab = Slab {
        w: d,
        h: d,
        radii: [d / 2.; 4],
        depth,
        bevel: depth * 0.3,
        segments: 6,
        front,
        side,
        back: front,
    };
    // Facing -z: half a turn about y.
    placed(
        slab,
        Transform::rotate(Mat3::rotation(Vec3::Y, std::f32::consts::PI))
            .then(&Transform::translate(Vec3::new(x, y, z - depth / 2.))),
    )
}

/// A button standing out of a side: `side` is +1 (right), -1 (left) or,
/// with `top`, the top edge. `along` is where along that edge.
fn button(side: f32, top: bool, edge: f32, along: f32, length: f32, thick: f32, out: f32) -> Mesh {
    let slab = Slab {
        w: thick,
        h: length,
        radii: [thick.min(length) * 0.45; 4],
        depth: 2. * out,
        bevel: out * 0.4,
        segments: 3,
        front: FRAME,
        side: FRAME,
        back: FRAME,
    };
    let quarter = std::f32::consts::FRAC_PI_2;
    let t = if top {
        // Its front up (+y), its length along x.
        Transform::rotate(Mat3::rotation(Vec3::X, -quarter).mul(&Mat3::rotation(Vec3::Z, quarter)))
            .then(&Transform::translate(Vec3::new(along, edge, 0.)))
    } else {
        Transform::rotate(Mat3::rotation(Vec3::Y, side * quarter))
            .then(&Transform::translate(Vec3::new(side * edge, along, 0.)))
    };
    placed(slab, t)
}

/// A cover screen's size before it has been seen (the device was open):
/// most of a book's half, or a small square on a flip phone.
pub fn guess_cover(kind: Kind, main: (f32, f32)) -> (f32, f32) {
    match kind {
        Kind::Flip => (0.62 * main.0, 0.62 * main.0),
        _ => (0.47 * main.0, 1.02 * main.1),
    }
}

/// The model built, folded and turned `roll` quarter turns, with the
/// picture on the `live` screen (a foldable's picture turned on its screen,
/// the model itself not turned).
pub fn scene(model: &Model, live: Live, roll: i8) -> Scene {
    let mut scene = match model.kind {
        Kind::Phone(style) => slab_device(model.main, style, false, live, roll),
        Kind::Tablet(style) => slab_device(model.main, style, true, live, roll),
        // A foldable isn't turned: its interface turns on its screens (an
        // iPhone Duo turns it as it folds), so only the picture turns.
        Kind::Book | Kind::Flip => return foldable(model, live, roll),
    };
    if roll.rem_euclid(2) == 1 {
        scene.extent = (scene.extent.1, scene.extent.0);
    }
    let turn = Transform::rotate(Mat3::rotation(
        Vec3::Z,
        f32::from(roll) * std::f32::consts::FRAC_PI_2,
    ));
    scene.mesh.transform(&turn);
    scene.main_normal = turn.dir(scene.main_normal);
    scene.cover_normal = scene.cover_normal.map(|n| turn.dir(n));
    for s in &mut scene.screens {
        transform_quad(&mut s.quad, &turn);
    }
    scene
}

/// A phone or tablet: one slab, one screen.
fn slab_device(screen: (f32, f32), style: Style, tablet: bool, live: Live, roll: i8) -> Scene {
    let (sw, sh) = (screen.0.max(1.), screen.1.max(1.));
    let short = sw.min(sh);
    let bezel = match (tablet, style) {
        (false, Style::Ios) => 0.045 * short,
        (false, Style::Android) => 0.04 * short,
        (true, _) => 0.055 * short,
    };
    let (bw, bh) = (sw + 2. * bezel, sh + 2. * bezel);
    let bshort = bw.min(bh);
    let (radius, thick, bevel) = match (tablet, style) {
        (false, Style::Ios) => (0.165 * bshort, 0.105 * bshort, 0.1),
        (false, Style::Android) => (0.11 * bshort, 0.11 * bshort, 0.32),
        (true, _) => (0.07 * bshort, 0.042 * bshort, 0.25),
    };
    let mut mesh = Slab {
        w: bw,
        h: bh,
        radii: [radius; 4],
        depth: thick,
        bevel: bevel * thick,
        segments: 10,
        front: GLASS,
        side: FRAME,
        back: BACK,
    }
    .mesh();
    let back = -thick / 2.;
    let margin = 0.045 * bshort;
    let out = if tablet { 0.006 } else { 0.012 } * bshort;
    let btn = 0.34 * thick;
    match (tablet, style) {
        (false, Style::Ios) => {
            // A square plateau, three lenses and a flash, top left seen
            // from behind (top right from in front).
            let p = 0.38 * bw;
            let (cx, cy) = (bw / 2. - margin - p / 2., bh / 2. - margin - p / 2.);
            let depth = 0.12 * thick;
            mesh.append(lens_plateau(cx, cy, back, p, p, 0.26 * p, depth));
            let z = back - depth;
            for (dx, dy) in [(0.22, 0.22), (0.22, -0.22), (-0.22, 0.)] {
                mesh.append(lens(
                    cx + dx * p,
                    cy + dy * p,
                    z,
                    0.4 * p,
                    0.12 * thick,
                    LENS,
                    RING,
                ));
            }
            mesh.append(lens(
                cx - 0.25 * p,
                cy + 0.3 * p,
                z,
                0.11 * p,
                0.03 * thick,
                FLASH,
                RING,
            ));
            mesh.append(button(1., false, bw / 2., 0.17 * bh, 0.12 * bh, btn, out));
            mesh.append(button(-1., false, bw / 2., 0.3 * bh, 0.035 * bh, btn, out));
            mesh.append(button(-1., false, bw / 2., 0.21 * bh, 0.065 * bh, btn, out));
            mesh.append(button(-1., false, bw / 2., 0.12 * bh, 0.065 * bh, btn, out));
        }
        (false, Style::Android) => {
            // A camera bar across the top of the back.
            let (w, h) = (0.46 * bw, 0.13 * bw);
            let (cx, cy) = (bw / 2. - margin - w / 2., bh / 2. - 1.5 * margin - h / 2.);
            let depth = 0.1 * thick;
            mesh.append(lens_plateau(cx, cy, back, w, h, h / 2., depth));
            let z = back - depth;
            for k in [0.6, 1.6] {
                mesh.append(lens(
                    cx + w / 2. - k * h,
                    cy,
                    z,
                    0.74 * h,
                    0.08 * thick,
                    LENS,
                    RING,
                ));
            }
            mesh.append(lens(
                cx + w / 2. - 2.45 * h,
                cy,
                z,
                0.25 * h,
                0.03 * thick,
                FLASH,
                RING,
            ));
            mesh.append(button(1., false, bw / 2., 0.2 * bh, 0.075 * bh, btn, out));
            mesh.append(button(1., false, bw / 2., 0.04 * bh, 0.16 * bh, btn, out));
        }
        (true, _) => {
            let d = 0.075 * bshort;
            let (cx, cy) = (bw / 2. - margin - d / 2., bh / 2. - margin - d / 2.);
            let depth = 0.12 * thick;
            mesh.append(lens_plateau(cx, cy, back, d, d, d * 0.3, depth));
            mesh.append(lens(
                cx,
                cy,
                back - depth,
                0.62 * d,
                0.1 * thick,
                LENS,
                RING,
            ));
            mesh.append(button(1., true, bh / 2., 0.36 * bw, 0.06 * bw, btn, out));
            mesh.append(button(1., false, bw / 2., 0.38 * bh, 0.08 * bh, btn, out));
            mesh.append(button(1., false, bw / 2., 0.28 * bh, 0.08 * bh, btn, out));
        }
    }
    let screen_radius = (radius - bezel).max(radius * 0.4);
    let quad = screen_part(
        (-sw / 2., sw / 2.),
        (-sh / 2., sh / 2.),
        thick / 2. + 0.004 * thick,
        (-sw / 2., sh / 2.),
        (sw, sh),
        screen_radius,
        roll,
        MAIN,
    );
    Scene {
        mesh,
        materials: materials(style),
        screens: vec![Screen {
            quad,
            live: live == Live::Main,
        }],
        extent: (bw + 2. * out, bh + 2. * out),
        main_normal: Vec3::Z,
        cover_normal: None,
    }
}

/// A raised rounded rectangle on the back (facing -z), `depth` high.
fn lens_plateau(cx: f32, cy: f32, back: f32, w: f32, h: f32, r: f32, depth: f32) -> Mesh {
    let slab = Slab {
        w,
        h,
        radii: [r; 4],
        depth: depth * 2.,
        bevel: depth * 0.5,
        segments: 8,
        front: PLATEAU,
        side: PLATEAU,
        back: PLATEAU,
    };
    // Half of it inside the back.
    placed(
        slab,
        Transform::rotate(Mat3::rotation(Vec3::Y, std::f32::consts::PI))
            .then(&Transform::translate(Vec3::new(cx, cy, back))),
    )
}

/// The dimensions of a foldable's halves.
struct Halves {
    /// Across and along the hinge.
    across: f32,
    along: f32,
    thick: f32,
    bezel: f32,
    radius: f32,
}

/// How the moving half sits for `model.hinge`: turned about the hinge,
/// whose line runs through the main screen's face (fold-in) or the back
/// (fold-out).
pub fn moving_half(model: &Model, thick: f32) -> Transform {
    let inward = model.inward;
    let hinge = if inward {
        model.hinge.clamp(0., 180.)
    } else {
        model.hinge.clamp(180., 360.)
    };
    let through = Vec3::new(0., 0., if inward { thick / 2. } else { -thick / 2. });
    let axis = if model.kind == Kind::Flip {
        Vec3::X
    } else {
        Vec3::Y
    };
    Transform::rotate_about(axis, through, (180. - hinge).to_radians())
}

/// A foldable. Its fixed half is on the right (book) or at the bottom
/// (flip); the other turns about the hinge onto its front as it folds, and
/// carries the cover screen on its back.
fn foldable(model: &Model, live: Live, roll: i8) -> Scene {
    let book = model.kind != Kind::Flip;
    let (mw, mh) = (model.main.0.max(1.), model.main.1.max(1.));
    // Across the hinge, and along it.
    let (main_across, main_along) = if book { (mw, mh) } else { (mh, mw) };
    let cover = model.cover.unwrap_or_default();
    let (cover_across, cover_along) = if book { cover } else { (cover.1, cover.0) };
    let bezel = 0.035 * (main_across / 2.).min(main_along);
    let across = (main_across / 2. + bezel).max(cover_across + 2. * bezel);
    let along = (main_along + 2. * bezel).max(cover_along + 2. * bezel);
    let h = Halves {
        across,
        along,
        thick: 0.075 * across.min(along),
        bezel,
        radius: 0.11 * across.min(along),
    };
    // Built as a book (the hinge vertical, the moving half on the left),
    // then for a flip phone turned a quarter so that half is on top.
    let to_flip = Transform::rotate(Mat3::rotation(Vec3::Z, -std::f32::consts::FRAC_PI_2));
    let frame = |t: Transform| if book { t } else { t.then(&to_flip) };
    let small = 0.015 * across.min(along);
    let half = |left: bool| Slab {
        w: h.across,
        h: h.along,
        radii: if left {
            [h.radius, small, small, h.radius]
        } else {
            [small, h.radius, h.radius, small]
        },
        depth: h.thick,
        bevel: 0.3 * h.thick,
        segments: 10,
        front: GLASS,
        side: FRAME,
        back: BACK,
    };
    let mut fixed = half(false).mesh();
    fixed.transform(&frame(Transform::translate(Vec3::new(
        h.across / 2.,
        0.,
        0.,
    ))));
    let mut moving = half(true).mesh();
    moving.transform(&frame(Transform::translate(Vec3::new(
        -h.across / 2.,
        0.,
        0.,
    ))));

    // The fixed half's back: a camera bar and the buttons, on its outer
    // edge (a book's right, a flip phone's right side of its bottom half).
    let back = -h.thick / 2.;
    let margin = 0.05 * h.across.min(h.along);
    let depth = 0.12 * h.thick;
    let (btn, out) = (0.5 * h.thick, 0.012 * h.across.min(h.along));
    let (bar_w, bar_h, cx, cy, edge, at) = if book {
        let (w, bh) = (0.6 * h.across, 0.14 * h.across);
        let (cx, cy) = (h.across - margin - w / 2., h.along / 2. - margin - bh / 2.);
        (w, bh, cx, cy, h.across, [0.2 * h.along, 0.05 * h.along])
    } else {
        let (w, bh) = (0.4 * h.along, 0.12 * h.along);
        let (cx, cy) = (h.along / 2. - margin - w / 2., -margin - bh / 2.);
        (
            w,
            bh,
            cx,
            cy,
            h.along / 2.,
            [-0.25 * h.across, -0.45 * h.across],
        )
    };
    fixed.append(lens_plateau(cx, cy, back, bar_w, bar_h, bar_h / 2., depth));
    for k in [0.6, 1.6] {
        fixed.append(lens(
            cx + bar_w / 2. - k * bar_h,
            cy,
            back - depth,
            0.74 * bar_h,
            0.1 * h.thick,
            LENS,
            RING,
        ));
    }
    fixed.append(button(1., false, edge, at[0], 0.08 * h.along, btn, out));
    fixed.append(button(1., false, edge, at[1], 0.13 * h.along, btn, out));

    // The main screen, across both halves.
    let z = h.thick / 2. + 0.004 * h.thick;
    let size = (mw, mh);
    let origin = (-mw / 2., mh / 2.);
    let radius = (h.radius - bezel).max(h.radius * 0.4);
    let (fixed_part, moving_part) = if book {
        (
            screen_part(
                (0., mw / 2.),
                (-mh / 2., mh / 2.),
                z,
                origin,
                size,
                radius,
                roll,
                MAIN,
            ),
            screen_part(
                (-mw / 2., 0.),
                (-mh / 2., mh / 2.),
                z,
                origin,
                size,
                radius,
                roll,
                MAIN_MOVING,
            ),
        )
    } else {
        (
            screen_part(
                (-mw / 2., mw / 2.),
                (-mh / 2., 0.),
                z,
                origin,
                size,
                radius,
                roll,
                MAIN,
            ),
            screen_part(
                (-mw / 2., mw / 2.),
                (0., mh / 2.),
                z,
                origin,
                size,
                radius,
                roll,
                MAIN_MOVING,
            ),
        )
    };
    // The cover screen, placed as it is seen folded shut: over the fixed
    // half, at the moving half's back.
    let shut = moving_half(
        &Model {
            hinge: if model.inward { 0. } else { 360. },
            ..*model
        },
        h.thick,
    );
    let mut cover_quad = None;
    if model.cover.is_some() {
        let (cw, ch) = cover;
        // The middle of what the folded device covers (as a book, then
        // turned): centred, or for a flip phone just below the hinge.
        let (ccx, ccy) = if book {
            (h.across / 2., 0.)
        } else {
            (0., -(h.bezel + ch / 2.))
        };
        // How high the moving half's back is, shut.
        let top = shut.point(Vec3::new(0., 0., back - 0.004 * h.thick)).z;
        let mut q = screen_part(
            (ccx - cw / 2., ccx + cw / 2.),
            (ccy - ch / 2., ccy + ch / 2.),
            top,
            (ccx - cw / 2., ccy + ch / 2.),
            (cw, ch),
            0.12 * cw.min(ch),
            roll,
            COVER,
        );
        // From the shut pose back to the moving half's own (a half turn
        // undoes itself).
        transform_quad(&mut q, &shut);
        cover_quad = Some(q);
    }

    let mut screens = vec![Screen {
        quad: fixed_part,
        live: live == Live::Main,
    }];
    let mut moving_screens = vec![Screen {
        quad: moving_part,
        live: live == Live::Main,
    }];
    if let Some(q) = cover_quad {
        moving_screens.push(Screen {
            quad: q,
            live: live == Live::Cover,
        });
    }

    // Fold, and centre what is there.
    let pose = moving_half(model, h.thick);
    moving.transform(&pose);
    for s in &mut moving_screens {
        transform_quad(&mut s.quad, &pose);
    }
    let fixed_centre = frame(Transform::IDENTITY).point(Vec3::new(h.across / 2., 0., 0.));
    let moving_centre =
        pose.point(frame(Transform::IDENTITY).point(Vec3::new(-h.across / 2., 0., 0.)));
    let centre = Transform::translate(-((fixed_centre + moving_centre) * 0.5));
    let mut mesh = fixed;
    mesh.append(moving);
    mesh.transform(&centre);
    screens.extend(moving_screens);
    for s in &mut screens {
        transform_quad(&mut s.quad, &centre);
    }
    let cover_normal = cover_quad.map(|_| pose.dir(shut.dir(Vec3::Z)));
    let extent = if book {
        (2. * h.across + 2. * out, h.along)
    } else {
        (h.along + 2. * out, 2. * h.across)
    };
    Scene {
        mesh,
        materials: materials(Style::Android),
        screens,
        extent,
        main_normal: Vec3::Z,
        cover_normal,
    }
}

/// How the model is seen: turned `yaw` degrees about its upright axis
/// (positive: its front to the right), then tipped `pitch` (positive: its
/// top toward the eye), and `zoom` times the size that fits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            yaw: 0.,
            pitch: 0.,
            zoom: 1.,
        }
    }
}

/// Of the view, how much the model's outline fills face-on.
const FIT: f32 = 0.84;

/// The camera for a `width` by `height` picture of a model whose outline
/// is `extent`.
pub fn camera(width: usize, height: usize, extent: (f32, f32), view: View) -> Camera {
    let (ew, eh) = (extent.0.max(1.), extent.1.max(1.));
    let fit = (width as f32 / ew).min(height as f32 / eh) * FIT * view.zoom;
    let distance = 3.2 * ew.max(eh);
    Camera {
        width,
        height,
        rotation: Mat3::rotation(Vec3::X, view.pitch.to_radians())
            .mul(&Mat3::rotation(Vec3::Y, view.yaw.to_radians())),
        distance,
        focal: fit * distance,
        center: (width as f32 / 2., height as f32 / 2.),
    }
}

/// How many times to halve the picture before drawing it `scale` times its
/// size (pixels of the view a pixel of the picture): drawn at half its size
/// or less, a picture sampled whole would shimmer as it moves.
pub fn mip_level(scale: f32) -> usize {
    let mut level = 0;
    let mut s = scale.max(1e-3);
    while s <= 0.5 && level < 4 {
        s *= 2.;
        level += 1;
    }
    level
}

/// Which screen faces the eye, if one does clearly.
pub fn facing(scene: &Scene, camera: &Camera) -> Live {
    let main = camera.facing(scene.main_normal);
    let cover = scene.cover_normal.map_or(-1., |n| camera.facing(n));
    // Shut, both face the same way: the cover is the one on the outside.
    if cover > 0.35 && cover >= main {
        Live::Cover
    } else if main > 0.35 {
        Live::Main
    } else {
        Live::None
    }
}

/// Draw `scene` into `color` (`camera.width` by `camera.height`, four
/// bytes a pixel, transparent), the picture `texture` on its live screen.
pub fn render(
    scene: &Scene,
    camera: &Camera,
    raster: &mut Raster,
    color: &mut [u8],
    texture: Option<Texture<'_>>,
) {
    raster.begin(camera.width, camera.height);
    // Screens first: the glass under them is then hidden by depth, not
    // drawn and drawn over.
    for s in &scene.screens {
        raster.draw_screen(color, camera, &s.quad, texture.filter(|_| s.live));
    }
    raster.draw_mesh(color, camera, &scene.mesh, &scene.materials);
    raster.smooth_outline(color);
}

/// The picture's point under pixel position `(x, y)` on screen `id`'s
/// plane, kept on that screen (a finger that slides off its edge stays at
/// the edge). A foldable's main screen is in two halves: a finger may go
/// from one to the other.
pub fn pick(scene: &Scene, camera: &Camera, id: u8, x: f32, y: f32) -> Option<(f32, f32)> {
    let (origin, dir) = camera.ray(x, y);
    let main = |i: u8| i == MAIN || i == MAIN_MOVING;
    let mut best: Option<(f32, (f32, f32))> = None;
    for s in scene
        .screens
        .iter()
        .filter(|s| s.quad.id == id || (main(id) && main(s.quad.id)))
    {
        let Some((a, b)) = s.quad.intersect(origin, dir) else {
            continue;
        };
        // How far outside the quad (0: on it).
        let off = (-a).max(a - 1.).max(0.) + (-b).max(b - 1.).max(0.);
        let uv = s.quad.uv_at(a.clamp(0., 1.), b.clamp(0., 1.));
        if best.is_none_or(|(o, _)| off < o) {
            best = Some((off, uv));
        }
    }
    best.map(|(_, uv)| uv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(hinge: f32) -> Model {
        Model {
            kind: Kind::Book,
            main: (740., 680.),
            cover: Some((360., 780.)),
            inward: true,
            hinge,
        }
    }

    fn quad(scene: &Scene, id: u8) -> ScreenQuad {
        scene.screens.iter().find(|s| s.quad.id == id).unwrap().quad
    }

    fn normal(q: &ScreenQuad) -> Vec3 {
        let [tl, tr, _, bl] = q.corners;
        // Corners go clockwise seen from in front.
        (bl - tl).cross(tr - tl).normalized()
    }

    fn close(a: Vec3, b: Vec3, tolerance: f32) -> bool {
        (a - b).length() < tolerance
    }

    #[test]
    fn a_book_open_flat_is_one_screen_in_two_halves() {
        let s = scene(&book(180.), Live::Main, 0);
        let (fixed, moving) = (quad(&s, MAIN), quad(&s, MAIN_MOVING));
        assert!(close(normal(&fixed), Vec3::Z, 1e-4));
        assert!(close(normal(&moving), Vec3::Z, 1e-4));
        // They meet at the hinge, the moving half on the left, and the
        // picture is split there.
        assert!(close(moving.corners[1], fixed.corners[0], 1e-3));
        assert!((moving.uv[1].0 - 0.5).abs() < 1e-4 && (fixed.uv[1].0 - 1.).abs() < 1e-4);
        // Centred, and both halves face the eye: the main screen does.
        assert!(fixed.corners[0].x.abs() < 1e-3);
        let cam = camera(800, 600, s.extent, View::default());
        assert_eq!(facing(&s, &cam), Live::Main);
    }

    #[test]
    fn a_book_folded_shut_shows_its_cover() {
        let s = scene(&book(0.), Live::Cover, 0);
        let moving = quad(&s, MAIN_MOVING);
        let fixed = quad(&s, MAIN);
        // The halves' screens face each other, on top of each other.
        assert!(close(normal(&moving), -Vec3::Z, 1e-4));
        let gap = moving.corners[0].z - fixed.corners[0].z;
        assert!(gap.abs() < 1., "{gap}");
        // The cover faces out, upright and not mirrored: its top left is
        // up and to the left.
        let cover = quad(&s, COVER);
        assert!(close(normal(&cover), Vec3::Z, 1e-4));
        assert!(cover.corners[0].x < cover.corners[1].x);
        assert!(cover.corners[0].y > cover.corners[3].y);
        // Above both halves' screens, and centred.
        assert!(cover.corners[0].z > fixed.corners[0].z);
        let mid = (cover.corners[0] + cover.corners[2]) * 0.5;
        assert!(mid.x.abs() < 1. && mid.y.abs() < 1., "{mid:?}");
        let cam = camera(800, 600, s.extent, View::default());
        assert_eq!(facing(&s, &cam), Live::Cover);
        // Turned round, neither screen faces the eye.
        let behind = camera(
            800,
            600,
            s.extent,
            View {
                yaw: 180.,
                ..View::default()
            },
        );
        assert_eq!(facing(&s, &behind), Live::None);
    }

    #[test]
    fn a_book_half_open_stands_at_a_right_angle() {
        let s = scene(&book(90.), Live::Main, 0);
        let moving = normal(&quad(&s, MAIN_MOVING));
        // The moving half's screen faces right, toward the fixed one.
        assert!(close(moving, Vec3::X, 1e-4), "{moving:?}");
        assert!(close(normal(&quad(&s, MAIN)), Vec3::Z, 1e-4));
    }

    #[test]
    fn a_flip_phone_folds_its_top_half_down() {
        let model = Model {
            kind: Kind::Flip,
            main: (400., 960.),
            cover: Some((250., 250.)),
            inward: true,
            hinge: 180.,
        };
        let open = scene(&model, Live::Main, 0);
        let top = quad(&open, MAIN_MOVING);
        let bottom = quad(&open, MAIN);
        assert!(top.corners[3].y > bottom.corners[3].y);
        // The picture's top half on the top half.
        assert!((top.uv[0].1).abs() < 1e-4 && (top.uv[3].1 - 0.5).abs() < 1e-4);
        assert!(open.extent.1 > open.extent.0);
        let shut = scene(&Model { hinge: 0., ..model }, Live::Cover, 0);
        let cover = quad(&shut, COVER);
        assert!(close(normal(&cover), Vec3::Z, 1e-4));
        assert!(cover.corners[0].y > cover.corners[3].y, "upright");
        assert!(cover.corners[0].x < cover.corners[1].x, "not mirrored");
        assert!(close(normal(&quad(&shut, MAIN_MOVING)), -Vec3::Z, 1e-4));
    }

    #[test]
    fn a_fold_out_folds_backward() {
        let model = Model {
            kind: Kind::Book,
            main: (800., 700.),
            cover: None,
            inward: false,
            hinge: 360.,
        };
        let s = scene(&model, Live::Main, 0);
        // The moving half's part of the main screen now faces back.
        assert!(close(normal(&quad(&s, MAIN_MOVING)), -Vec3::Z, 1e-4));
        assert!(s.screens.iter().all(|q| q.quad.id != COVER));
    }

    #[test]
    fn a_turned_device_keeps_its_picture_upright() {
        // A quarter turn counter-clockwise: the screen's natural top left
        // is now at the bottom left, which is the picture's (0, 1).
        assert_eq!(picture_uv(0., 0., (10., 20.), 1), (0., 1.));
        assert_eq!(picture_uv(10., 0., (10., 20.), 1), (0., 0.));
        assert_eq!(picture_uv(0., 0., (10., 20.), -1), (1., 0.));
        let phone = Model {
            kind: Kind::Phone(Style::Ios),
            main: (390., 844.),
            cover: None,
            inward: true,
            hinge: 180.,
        };
        let s = scene(&phone, Live::Main, 1);
        assert!(s.extent.0 > s.extent.1);
        let q = quad(&s, MAIN);
        // The corner showing the picture's top left is at the top left.
        let i = q.uv.iter().position(|&(u, v)| u == 0. && v == 0.).unwrap();
        let c = q.corners[i];
        assert!(c.x < 0. && c.y > 0., "{c:?}");
    }
}
