//! Triangles into pixels: a depth buffer, lit meshes, and textured screens
//! that remember which point of their picture each pixel shows.

use crate::math::{Mat3, Vec3};

/// The order of a pixel's four bytes, in the picture drawn into and in
/// textures (which are copied as they are).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Rgba,
    /// What GPUI paints.
    Bgra,
}

/// A picture to draw on a screen: four bytes a pixel, rows top to bottom,
/// in the target's [`Order`].
#[derive(Debug, Clone, Copy)]
pub struct Texture<'a> {
    pub width: usize,
    pub height: usize,
    pub pixels: &'a [u8],
}

/// A texture that owns its pixels (a smaller copy of a picture).
#[derive(Debug, Clone, Default)]
pub struct OwnedTexture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl OwnedTexture {
    pub fn as_texture(&self) -> Texture<'_> {
        Texture {
            width: self.width,
            height: self.height,
            pixels: &self.pixels,
        }
    }
}

impl Texture<'_> {
    fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0 && self.pixels.len() >= self.width * self.height * 4
    }

    /// Half as wide and as tall, each pixel the mean of four: drawn much
    /// smaller than it is, a picture sampled at full size would shimmer.
    pub fn half(&self) -> OwnedTexture {
        let (w, h) = ((self.width / 2).max(1), (self.height / 2).max(1));
        let mut pixels = vec![0u8; w * h * 4];
        if !self.is_valid() {
            return OwnedTexture {
                width: w,
                height: h,
                pixels,
            };
        }
        let src = self.pixels;
        let stride = self.width * 4;
        for y in 0..h {
            let (y0, y1) = (
                (2 * y).min(self.height - 1),
                (2 * y + 1).min(self.height - 1),
            );
            for x in 0..w {
                let (x0, x1) = ((2 * x).min(self.width - 1), (2 * x + 1).min(self.width - 1));
                for c in 0..4 {
                    let sum = u32::from(src[y0 * stride + x0 * 4 + c])
                        + u32::from(src[y0 * stride + x1 * 4 + c])
                        + u32::from(src[y1 * stride + x0 * 4 + c])
                        + u32::from(src[y1 * stride + x1 * 4 + c]);
                    pixels[(y * w + x) * 4 + c] = ((sum + 2) / 4) as u8;
                }
            }
        }
        OwnedTexture {
            width: w,
            height: h,
            pixels,
        }
    }

    /// The colour at `(u, v)` (fractions of the picture), blended from the
    /// four nearest pixels. `texels` is the picture a pixel at a time.
    #[inline(always)]
    fn sample(&self, texels: &[[u8; 4]], u: f32, v: f32) -> u32 {
        // In 256ths of a pixel.
        let fx = ((u * self.width as f32 - 0.5) * 256.) as i32;
        let fy = ((v * self.height as f32 - 0.5) * 256.) as i32;
        let fx = fx.clamp(0, (self.width as i32 - 1) << 8);
        let fy = fy.clamp(0, (self.height as i32 - 1) << 8);
        let (x0, y0) = ((fx >> 8) as usize, (fy >> 8) as usize);
        let (wx, wy) = ((fx & 255) as u32, (fy & 255) as u32);
        let dx = usize::from(x0 + 1 < self.width);
        let dy = if y0 + 1 < self.height { self.width } else { 0 };
        let i = y0 * self.width + x0;
        let texel = |i: usize| u32::from_le_bytes(texels[i]);
        let top = lerp_pixel(texel(i), texel(i + dx), wx);
        let bottom = lerp_pixel(texel(i + dy), texel(i + dy + dx), wx);
        lerp_pixel(top, bottom, wy)
    }
}

/// Two pixels mixed, `f` of 256 parts the second, all four bytes at once.
fn lerp_pixel(a: u32, b: u32, f: u32) -> u32 {
    let g = 256 - f;
    let rb = (((a & 0x00ff_00ff) * g + (b & 0x00ff_00ff) * f) >> 8) & 0x00ff_00ff;
    let ag = (((a >> 8) & 0x00ff_00ff) * g + ((b >> 8) & 0x00ff_00ff) * f) & 0xff00_ff00;
    rb | ag
}

/// How a surface takes the light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Material {
    /// Its colour, 0 to 1 a channel (red, green, blue).
    pub color: [f32; 3],
    /// How bright its highlights are (0: none).
    pub specular: f32,
    /// How tight they are (larger: smaller and sharper).
    pub shininess: f32,
}

impl Material {
    pub const fn new(color: [f32; 3], specular: f32, shininess: f32) -> Self {
        Self {
            color,
            specular,
            shininess,
        }
    }
}

/// A corner of a mesh's triangles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    pub pos: Vec3,
    /// Which way the surface faces here (one long).
    pub normal: Vec3,
    /// Index into the materials it is drawn with.
    pub material: u16,
}

/// Triangles, wound counter-clockwise seen from the side that is drawn.
#[derive(Debug, Clone, Default)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub triangles: Vec<[u32; 3]>,
}

impl Mesh {
    pub fn push(&mut self, v: Vertex) -> u32 {
        self.vertices.push(v);
        (self.vertices.len() - 1) as u32
    }

    pub fn append(&mut self, other: Mesh) {
        let base = self.vertices.len() as u32;
        self.vertices.extend(other.vertices);
        self.triangles
            .extend(other.triangles.into_iter().map(|t| t.map(|i| i + base)));
    }

    pub fn transform(&mut self, t: &crate::math::Transform) {
        for v in &mut self.vertices {
            v.pos = t.point(v.pos);
            v.normal = t.dir(v.normal);
        }
    }
}

/// A screen: a flat four-cornered piece of a picture, with round corners.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenQuad {
    /// Top left, top right, bottom right, bottom left, seen from in front
    /// (the side that is drawn). A rectangle.
    pub corners: [Vec3; 4],
    /// The point of the picture at each corner (fractions).
    pub uv: [(f32, f32); 4],
    /// Each corner's place on the whole screen it is part of (in the
    /// model's units, top left 0,0), for rounding that screen's corners: a
    /// half of a folding screen has only two of them.
    pub local: [(f32, f32); 4],
    /// The whole screen's size and its corners' radius, in the same units.
    pub screen: (f32, f32),
    pub radius: f32,
    /// Told back by [`Raster::hit`] for the pixels it shows (not 0).
    pub id: u8,
}

impl ScreenQuad {
    /// The place on the quad, as fractions along its top and left edges,
    /// where the line from `origin` along `dir` meets its plane.
    pub fn intersect(&self, origin: Vec3, dir: Vec3) -> Option<(f32, f32)> {
        let [tl, tr, _, bl] = self.corners;
        let (ex, ey) = (tr - tl, bl - tl);
        let n = ex.cross(ey);
        let denom = n.dot(dir);
        if denom.abs() < 1e-9 {
            return None;
        }
        let t = n.dot(tl - origin) / denom;
        if t <= 0. {
            return None;
        }
        let p = origin + dir * t - tl;
        Some((p.dot(ex) / ex.dot(ex), p.dot(ey) / ey.dot(ey)))
    }

    /// The point of the picture at fractions `(a, b)` along the quad.
    pub fn uv_at(&self, a: f32, b: f32) -> (f32, f32) {
        let [t0, t1, _, t3] = self.uv;
        (
            t0.0 + a * (t1.0 - t0.0) + b * (t3.0 - t0.0),
            t0.1 + a * (t1.1 - t0.1) + b * (t3.1 - t0.1),
        )
    }
}

/// Where the eye is and how it sees: the model turned by `rotation`, seen
/// from `distance` away along +z, `focal` pixels a unit at the origin's
/// depth times `distance`, the origin at `center`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub width: usize,
    pub height: usize,
    pub rotation: Mat3,
    pub distance: f32,
    pub focal: f32,
    pub center: (f32, f32),
}

/// A point projected: where in the picture, and one over its distance from
/// the eye (larger is nearer).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projected {
    pub x: f32,
    pub y: f32,
    pub inv_depth: f32,
}

/// Points nearer the eye than this are not drawn.
const NEAR: f32 = 1e-3;

impl Camera {
    pub fn project(&self, p: Vec3) -> Option<Projected> {
        let c = self.rotation.apply(p);
        let d = self.distance - c.z;
        if d < NEAR * self.distance {
            return None;
        }
        let inv = 1. / d;
        Some(Projected {
            x: self.center.0 + self.focal * c.x * inv,
            y: self.center.1 - self.focal * c.y * inv,
            inv_depth: inv,
        })
    }

    /// The line from the eye through pixel position `(x, y)`, in the
    /// model's space: where it starts, and which way it goes.
    pub fn ray(&self, x: f32, y: f32) -> (Vec3, Vec3) {
        let back = self.rotation.transpose();
        let dir = Vec3::new(
            (x - self.center.0) / self.focal,
            -(y - self.center.1) / self.focal,
            -1.,
        );
        (
            back.apply(Vec3::new(0., 0., self.distance)),
            back.apply(dir),
        )
    }

    /// How far toward the eye a direction in the model points (1: straight
    /// at it).
    pub fn facing(&self, normal: Vec3) -> f32 {
        self.rotation.apply(normal).normalized().z
    }
}

/// What one pixel of a screen shows: which screen, and where on its
/// picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub id: u8,
    pub u: f32,
    pub v: f32,
}

/// The pixels one frame touched, for clearing them before the next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Region {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl Region {
    fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    fn grow(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        if self.is_empty() {
            *self = Region { x0, y0, x1, y1 };
        } else {
            self.x0 = self.x0.min(x0);
            self.y0 = self.y0.min(y0);
            self.x1 = self.x1.max(x1);
            self.y1 = self.y1.max(y1);
        }
    }
}

/// Draws into a picture the caller owns (four bytes a pixel, cleared to
/// transparent), keeping a depth for each pixel and, for screens, what it
/// shows. Keep one and reuse it: its buffers are cleared only where the
/// last frame drew.
#[derive(Debug, Clone)]
pub struct Raster {
    pub order: Order,
    width: usize,
    height: usize,
    /// One over the distance of what each pixel shows (0: nothing).
    depth: Vec<f32>,
    /// For screens' pixels: the picture's point, 16 bits a coordinate.
    hits: Vec<u32>,
    /// Which screen each pixel shows (0: none).
    ids: Vec<u8>,
    /// Each row's first and past-last pixel drawn (the model has no holes
    /// across a row, near enough: its outline is where rows end).
    rows: Vec<(u32, u32)>,
    drawn: Region,
}

/// Where the light comes from, in the eye's space: a key light above left,
/// a weaker fill from the right, and some from everywhere.
const KEY: Vec3 = Vec3::new(-0.45, 0.62, 0.64);
const FILL: Vec3 = Vec3::new(0.7, -0.15, 0.7);
const AMBIENT: f32 = 0.34;

impl Raster {
    pub fn new(order: Order) -> Self {
        Self {
            order,
            width: 0,
            height: 0,
            depth: Vec::new(),
            hits: Vec::new(),
            ids: Vec::new(),
            rows: Vec::new(),
            drawn: Region::default(),
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Start a frame of `width` by `height` pixels.
    pub fn begin(&mut self, width: usize, height: usize) {
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            self.depth = vec![0.; width * height];
            self.hits = vec![0; width * height];
            self.ids = vec![0; width * height];
            self.rows = vec![EMPTY_ROW; height];
        } else if !self.drawn.is_empty() {
            let r = self.drawn;
            for y in r.y0..r.y1 {
                let row = y * width;
                self.depth[row + r.x0..row + r.x1].fill(0.);
                self.ids[row + r.x0..row + r.x1].fill(0);
            }
            self.rows[r.y0..r.y1].fill(EMPTY_ROW);
        }
        self.drawn = Region::default();
    }

    /// Whether anything is drawn at `(x, y)`.
    pub fn covered(&self, x: usize, y: usize) -> bool {
        x < self.width && y < self.height && self.depth[y * self.width + x] > 0.
    }

    /// What the pixel at `(x, y)` shows of a screen, if a screen.
    pub fn hit(&self, x: usize, y: usize) -> Option<Hit> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = y * self.width + x;
        let id = self.ids[i];
        (id != 0).then(|| {
            let h = self.hits[i];
            Hit {
                id,
                u: (h >> 16) as f32 / 65535.,
                v: (h & 0xffff) as f32 / 65535.,
            }
        })
    }

    fn pack(&self, rgb: [u8; 3]) -> u32 {
        let [r, g, b] = rgb;
        match self.order {
            Order::Rgba => u32::from_le_bytes([r, g, b, 255]),
            Order::Bgra => u32::from_le_bytes([b, g, r, 255]),
        }
    }

    /// A lit mesh, its back faces left out.
    pub fn draw_mesh(
        &mut self,
        color: &mut [u8],
        camera: &Camera,
        mesh: &Mesh,
        materials: &[Material],
    ) {
        let key = KEY.normalized();
        let fill = FILL.normalized();
        let eye = Vec3::new(0., 0., camera.distance);
        let lit: Vec<Option<(Projected, [f32; 3])>> = mesh
            .vertices
            .iter()
            .map(|v| {
                let p = camera.project(v.pos)?;
                let m = materials
                    .get(usize::from(v.material))
                    .copied()
                    .unwrap_or(Material::new([1., 0., 1.], 0., 1.));
                let n = camera.rotation.apply(v.normal).normalized();
                let to_eye = (eye - camera.rotation.apply(v.pos)).normalized();
                let diffuse = AMBIENT + 0.62 * n.dot(key).max(0.) + 0.22 * n.dot(fill).max(0.);
                let half = (key + to_eye).normalized();
                let spec = m.specular * n.dot(half).max(0.).powf(m.shininess);
                // A little light grazing the edges, as on glass and metal.
                let rim = 0.12 * m.specular * (1. - n.dot(to_eye).max(0.)).powi(3);
                let c = m
                    .color
                    .map(|c| ((c * diffuse + spec + rim) * 255.).clamp(0., 255.));
                Some((p, c))
            })
            .collect();
        let (w, h) = (self.width, self.height);
        let order = self.order;
        let pixels = pixels_mut(color);
        for tri in &mesh.triangles {
            let [Some(a), Some(b), Some(c)] = tri.map(|i| lit[i as usize]) else {
                continue;
            };
            let pts = [a.0, b.0, c.0];
            let Some(setup) = Setup::new(pts) else {
                continue;
            };
            let iz = setup.plane([a.0.inv_depth, b.0.inv_depth, c.0.inv_depth]);
            let ch = |k: usize| setup.plane([a.1[k], b.1[k], c.1[k]]);
            let (pr, pg, pb) = (ch(0), ch(1), ch(2));
            let (r0, r1) = match order {
                Order::Rgba => (pr, pb),
                Order::Bgra => (pb, pr),
            };
            let Some(region) = spans(w, h, &pts, &mut self.rows, |y, x0, x1| {
                let fy = y as f32 + 0.5;
                let row = y * w;
                for x in x0..x1 {
                    let fx = x as f32 + 0.5;
                    let z = iz.at(fx, fy);
                    let i = row + x;
                    if z > self.depth[i] {
                        self.depth[i] = z;
                        self.ids[i] = 0;
                        pixels[i] = [
                            r0.at(fx, fy) as u8,
                            pg.at(fx, fy) as u8,
                            r1.at(fx, fy) as u8,
                            255,
                        ];
                    }
                }
            }) else {
                continue;
            };
            self.drawn.grow(region.x0, region.y0, region.x1, region.y1);
        }
    }

    /// A screen: the picture if there is one (its pixels then remember
    /// where on it they are, as screen `quad.id`), else dark glass.
    pub fn draw_screen(
        &mut self,
        color: &mut [u8],
        camera: &Camera,
        quad: &ScreenQuad,
        texture: Option<Texture<'_>>,
    ) {
        let texture = texture.filter(Texture::is_valid);
        let texels = texture.map_or(&[][..], |t| {
            &t.pixels.as_chunks::<4>().0[..t.width * t.height]
        });
        let dark = self.pack([10, 10, 13]);
        let Some(p) = quad
            .corners
            .iter()
            .map(|c| camera.project(*c))
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        let (sw, sh) = quad.screen;
        let radius = quad.radius.min(sw / 2.).min(sh / 2.).max(0.);
        let (w, h) = (self.width, self.height);
        let pixels = pixels_mut(color);
        // Counter-clockwise seen from in front (the corners go clockwise).
        for [i0, i1, i2] in [[0usize, 2, 1], [0, 3, 2]] {
            let pts = [p[i0], p[i1], p[i2]];
            let Some(setup) = Setup::new(pts) else {
                continue;
            };
            let izs = pts.map(|p| p.inv_depth);
            let iz = setup.plane(izs);
            let over = |k: [f32; 3]| setup.plane([k[0] * izs[0], k[1] * izs[1], k[2] * izs[2]]);
            let pu = over([quad.uv[i0].0, quad.uv[i1].0, quad.uv[i2].0]);
            let pv = over([quad.uv[i0].1, quad.uv[i1].1, quad.uv[i2].1]);
            let ps = over([quad.local[i0].0, quad.local[i1].0, quad.local[i2].0]);
            let pt = over([quad.local[i0].1, quad.local[i1].1, quad.local[i2].1]);
            let Some(region) = spans(w, h, &pts, &mut self.rows, |y, x0, x1| {
                let fy = y as f32 + 0.5;
                let row = y * w;
                // Stepped along the row (the depth is worked out afresh at
                // each pixel: it decides against the glass just behind).
                let start = x0 as f32 + 0.5;
                let (mut su, mut sv) = (pu.at(start, fy), pv.at(start, fy));
                let (mut ss, mut st) = (ps.at(start, fy), pt.at(start, fy));
                for x in x0..x1 {
                    let (u_z, v_z, s_z, t_z) = (su, sv, ss, st);
                    su += pu.dx;
                    sv += pv.dx;
                    ss += ps.dx;
                    st += pt.dx;
                    let fx = x as f32 + 0.5;
                    let z = iz.at(fx, fy);
                    let i = row + x;
                    if z <= self.depth[i] {
                        continue;
                    }
                    let d = 1. / z;
                    // How much of the pixel is inside the round corners.
                    let s = s_z * d;
                    let t = t_z * d;
                    let cx = (radius - s).max(s - (sw - radius)).max(0.);
                    let cy = (radius - t).max(t - (sh - radius)).max(0.);
                    let mut cover = 1.;
                    if cx > 0. && cy > 0. {
                        // Model units a pixel, here: the corner's edge is
                        // smoothed over one pixel.
                        let per_pixel = d / camera.focal;
                        let out = (cx * cx + cy * cy).sqrt() - radius;
                        cover = (0.5 - out / per_pixel.max(1e-6)).clamp(0., 1.);
                        if cover <= 0. {
                            continue;
                        }
                    }
                    self.depth[i] = z;
                    let lit = match &texture {
                        Some(tex) => {
                            let (u, v) = ((u_z * d).clamp(0., 1.), (v_z * d).clamp(0., 1.));
                            if cover >= 0.5 {
                                self.ids[i] = quad.id;
                                self.hits[i] = ((u * 65535.) as u32) << 16 | (v * 65535.) as u32;
                            } else {
                                self.ids[i] = 0;
                            }
                            tex.sample(texels, u, v)
                        }
                        None => {
                            self.ids[i] = 0;
                            dark
                        }
                    };
                    pixels[i] = if cover < 1. {
                        // Over the glass around it: blend toward dark.
                        lerp_pixel(dark, lit, (cover * 256.) as u32) | 0xff00_0000
                    } else {
                        lit | 0xff00_0000
                    }
                    .to_le_bytes();
                }
            }) else {
                continue;
            };
            self.drawn.grow(region.x0, region.y0, region.x1, region.y1);
        }
    }

    /// Soften the model's outline: each outline pixel's opacity becomes
    /// the share of its neighbourhood the model covers. Only the ends of
    /// rows, and what sticks out past the rows above and below, are looked
    /// at.
    pub fn smooth_outline(&self, color: &mut [u8]) {
        let r = self.drawn;
        if r.is_empty() {
            return;
        }
        let (w, h) = (self.width, self.height);
        let alpha = |color: &[u8], x: usize, y: usize| color[(y * w + x) * 4 + 3] != 0;
        let mut edge = Vec::new();
        for y in r.y0..r.y1 {
            let (from, to) = self.rows[y];
            if from >= to {
                continue;
            }
            let (from, to) = (from as usize, to as usize);
            let row = |y: Option<usize>| match y.and_then(|y| self.rows.get(y)) {
                Some(&(a, b)) if a < b => (a as usize, b as usize),
                _ => (usize::MAX, 0),
            };
            let (above, below) = (row(y.checked_sub(1)), row(Some(y + 1).filter(|&y| y < h)));
            // Inside both neighbours' rows and away from this row's ends:
            // nothing there is outline.
            let inner = (
                above.0.max(below.0).max(from + 1),
                above.1.min(below.1).min(to - 1),
            );
            let ranges = if inner.0 < inner.1 {
                [(from, inner.0), (inner.1, to)]
            } else {
                [(from, to), (to, to)]
            };
            for (a, b) in ranges {
                edge.extend((a..b).map(|x| (x, y)));
            }
        }
        let mut changes = Vec::with_capacity(edge.len());
        for (x, y) in edge {
            if !alpha(color, x, y) {
                continue;
            }
            let mut n = 0;
            let mut inside = 0;
            for yy in y.saturating_sub(1)..(y + 2).min(h) {
                for xx in x.saturating_sub(1)..(x + 2).min(w) {
                    n += 1;
                    inside += usize::from(alpha(color, xx, yy));
                }
            }
            if inside < n {
                changes.push((y * w + x, (255 * inside / n).max(96) as u8));
            }
        }
        for (i, a) in changes {
            color[i * 4 + 3] = a;
        }
    }
}

const EMPTY_ROW: (u32, u32) = (u32::MAX, 0);

/// The picture's bytes, four a pixel.
fn pixels_mut(color: &mut [u8]) -> &mut [[u8; 4]] {
    color.as_chunks_mut::<4>().0
}

/// A value across a triangle: `c + dx * x + dy * y`.
#[derive(Debug, Clone, Copy)]
struct Plane {
    dx: f32,
    dy: f32,
    c: f32,
}

impl Plane {
    #[inline(always)]
    fn at(&self, x: f32, y: f32) -> f32 {
        self.c + self.dx * x + self.dy * y
    }
}

/// A triangle facing the eye, ready to spread values across.
struct Setup {
    x0: f32,
    y0: f32,
    e1: (f32, f32),
    e2: (f32, f32),
    area2: f32,
}

impl Setup {
    /// `None` for a triangle seen from behind (clockwise on the picture)
    /// or edge on.
    fn new(p: [Projected; 3]) -> Option<Self> {
        let e1 = (p[1].x - p[0].x, p[1].y - p[0].y);
        let e2 = (p[2].x - p[0].x, p[2].y - p[0].y);
        let area2 = e1.0 * e2.1 - e2.0 * e1.1;
        // Counter-clockwise in the model is clockwise on the picture, whose
        // y runs down: front faces have a negative area here.
        (area2 < -1e-6).then_some(Self {
            x0: p[0].x,
            y0: p[0].y,
            e1,
            e2,
            area2,
        })
    }

    fn plane(&self, f: [f32; 3]) -> Plane {
        let (d1, d2) = (f[1] - f[0], f[2] - f[0]);
        let dx = (d1 * self.e2.1 - d2 * self.e1.1) / self.area2;
        let dy = (d2 * self.e1.0 - d1 * self.e2.0) / self.area2;
        Plane {
            dx,
            dy,
            c: f[0] - dx * self.x0 - dy * self.y0,
        }
    }
}

/// Calls `row(y, from, to)` for each row of pixels whose centres the
/// triangle covers (`to` excluded), inside a `w` by `h` picture. The
/// triangle faces the eye (see [`Setup::new`]). The pixels it may have
/// touched, if any.
fn spans(
    w: usize,
    h: usize,
    p: &[Projected; 3],
    rows: &mut [(u32, u32)],
    mut row: impl FnMut(usize, usize, usize),
) -> Option<Region> {
    let ymin = p[0].y.min(p[1].y).min(p[2].y);
    let ymax = p[0].y.max(p[1].y).max(p[2].y);
    let y0 = (ymin - 0.5).ceil().max(0.);
    let y1 = ((ymax - 0.5).floor() + 1.).min(h as f32);
    if y1 <= y0 || y1.is_nan() || y0.is_nan() {
        return None;
    }
    let edges = [(0, 1), (1, 2), (2, 0)].map(|(i, j)| (p[i].x, p[i].y, p[j].x, p[j].y));
    let mut region = Region::default();
    for (y, extent) in rows
        .iter_mut()
        .enumerate()
        .take(y1 as usize)
        .skip(y0 as usize)
    {
        let fy = y as f32 + 0.5;
        let (mut lo, mut hi) = (f32::NEG_INFINITY, f32::INFINITY);
        for &(xi, yi, xj, yj) in &edges {
            // Inside: (x - xi)(yj - yi) - (fy - yi)(xj - xi) >= 0.
            let a = yj - yi;
            let b = -xi * a - (fy - yi) * (xj - xi);
            if a > 0. {
                lo = lo.max(-b / a);
            } else if a < 0. {
                hi = hi.min(-b / a);
            } else if b < 0. {
                lo = f32::INFINITY;
            }
        }
        // A hair wider, so neighbours' shared edges leave no cracks.
        let from = (lo - 0.5 - 1e-4).ceil().max(0.);
        let to = ((hi - 0.5 + 1e-4).floor() + 1.).min(w as f32);
        if to > from {
            let (from, to) = (from as usize, to as usize);
            row(y, from, to);
            region.grow(from, y, to, y + 1);
            *extent = (extent.0.min(from as u32), extent.1.max(to as u32));
        }
    }
    (!region.is_empty()).then_some(region)
}
