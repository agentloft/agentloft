//! A slide drawn as it is laid out: every shape of its [`Scene`] at its place,
//! scaled to the width there is.
//!
//! The scene's lengths are points on the slide; one factor (`scale`, pixels per
//! point) turns them into pixels, so a slide looks the same at any width. A
//! slide that is scrolled out of view builds nothing.

use gpui_kit::base::{TextSelectionHandle, TextSelectionRegistration, TextSelectionRun};
use gpui_kit::{
    AnyElement, App, Bounds, BoxShadow, CursorStyle, Div, Element, ElementId, Font, FontStyle,
    FontWeight, GlobalElementId, Hitbox, HitboxBehavior, Hsla, Image, ImageSource,
    InspectorElementId, InteractiveElement as _, IntoElement, LayoutId, ObjectFit,
    ParentElement as _, PathBuilder, Pixels, Point, Rgba, SharedString,
    StatefulInteractiveElement as _, StrikethroughStyle, Styled as _, StyledImage as _, StyledText,
    TextAlign, TextRun, UnderlineStyle, Window, canvas, div, fill, img, linear_color_stop,
    linear_gradient, point, prelude::FluentBuilder as _, px,
};
use loft_docs::scene::{
    Align, Anchor, Color, Dash, Fill, Geometry, LineSpacing, Paragraph, PathCmd, Picture, Run,
    Scene, Shape, ShapeKind, Stroke, Table, TextBody,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

pub type Images = Arc<HashMap<String, Arc<Image>>>;

/// A font's own line height as a multiple of its size, near enough for the
/// faces slides use.
const NATURAL_LINE: f32 = 1.2;

/// Picks, for a font a slide names, a family that is installed.
///
/// Asking the text system for a missing family gives its last-resort font at
/// regular weight, so bold and italic would be lost; a stand-in family chosen
/// here keeps them.
struct Fonts {
    text: std::rc::Rc<gpui_kit::WindowTextSystem>,
    /// The interface font: what text without a font of its own is set in.
    default: SharedString,
    resolved: RefCell<HashMap<String, SharedString>>,
    /// What selected text is painted over.
    selection: Hsla,
    /// Numbers the slide's pieces of text as they are built: the same piece
    /// gets the same number every frame, which is what its selection is
    /// kept under.
    texts: Cell<u64>,
}

impl Fonts {
    /// A piece of text that can be selected and copied, like text anywhere
    /// else in the app.
    fn selectable(&self, text: impl Into<SharedString>, runs: Vec<TextRun>) -> SelectableRuns {
        let n = self.texts.get();
        self.texts.set(n + 1);
        let text = text.into();
        SelectableRuns {
            id: ElementId::NamedInteger("text".into(), n),
            styled: StyledText::new(text.clone()).with_runs(runs),
            text,
            order: n,
            highlight: self.selection,
            hidden: false,
            finds: Vec::new(),
            stops: None,
        }
    }

    fn installed(&self, family: &str) -> bool {
        // Before the first look: a family the text system has once missed
        // stays missed.
        support_fonts::register(family);
        let wanted = gpui_kit::font(SharedString::from(family.to_string()));
        let id = self.text.resolve_font(&wanted);
        self.text
            .get_font_for_id(id)
            .is_some_and(|f| f.family.eq_ignore_ascii_case(family))
    }

    fn family(&self, wanted: Option<&str>) -> SharedString {
        let Some(wanted) = wanted else {
            return self.default.clone();
        };
        if let Some(found) = self.resolved.borrow().get(wanted) {
            return found.clone();
        }
        let found = if self.installed(wanted) {
            SharedString::from(wanted.to_string())
        } else {
            stand_ins(wanted)
                .iter()
                .find(|f| self.installed(f))
                .map(|f| SharedString::from(*f))
                .unwrap_or_else(|| self.default.clone())
        };
        self.resolved
            .borrow_mut()
            .insert(wanted.to_string(), found.clone());
        found
    }
}

/// Families to try in place of one that is not installed, closest first:
/// the metric-compatible open fonts, then what each system ships.
fn stand_ins(family: &str) -> &'static [&'static str] {
    let f = family.to_ascii_lowercase();
    let has = |names: &[&str]| names.iter().any(|n| f.starts_with(n));
    if has(&[
        "calibri", "aptos", "segoe ui", "tahoma", "verdana", "corbel", "candara",
    ]) {
        &[
            "Carlito",
            "Helvetica Neue",
            "Segoe UI",
            "Noto Sans",
            "Arial",
        ]
    } else if has(&["arial", "helvetica", "liberation sans"]) {
        &[
            "Arial",
            "Helvetica Neue",
            "Helvetica",
            "Liberation Sans",
            "Noto Sans",
        ]
    } else if has(&[
        "cambria",
        "constantia",
        "georgia",
        "garamond",
        "book antiqua",
        "palatino",
    ]) {
        &[
            "Caladea",
            "Georgia",
            "Times New Roman",
            "Liberation Serif",
            "Noto Serif",
        ]
    } else if has(&["times", "liberation serif"]) {
        &["Times New Roman", "Times", "Liberation Serif", "Noto Serif"]
    } else if has(&[
        "consolas",
        "courier",
        "lucida console",
        "cascadia",
        "menlo",
        "monaco",
    ]) {
        &[
            "Menlo",
            "Consolas",
            "Courier New",
            "Liberation Mono",
            "DejaVu Sans Mono",
        ]
    } else {
        &[]
    }
}

/// macOS keeps fonts for documents made elsewhere outside its font folders:
/// Carlito (drawn to Calibri's measurements), Garamond, Book Antiqua and
/// more. They are not in the font list until an app registers the ones it
/// needs for itself, which is what Keynote and Pages do when a document asks.
#[cfg(target_os = "macos")]
mod support_fonts {
    use std::collections::HashSet;
    use std::sync::Mutex;

    const DIR: &str = "/System/Library/PrivateFrameworks/FontServices.framework/Resources/Fonts/ApplicationSupport";

    static TRIED: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);

    fn file_for(family: &str) -> Option<&'static str> {
        Some(match family.to_ascii_lowercase().as_str() {
            "carlito" => "Carlito.ttc",
            "garamond" => "Garamond.ttc",
            "book antiqua" => "BookAntiqua.ttc",
            "bookman old style" => "BookmanOldStyle.ttc",
            "century gothic" => "CenturyGothic.ttc",
            "century schoolbook" => "CenturySchoolbook.ttc",
            _ => return None,
        })
    }

    /// Makes `family` available to this process if the system has it in store.
    pub fn register(family: &str) {
        let Some(file) = file_for(family) else { return };
        let Ok(mut tried) = TRIED.lock() else { return };
        if !tried.get_or_insert_with(HashSet::new).insert(file) {
            return;
        }
        let path = format!("{DIR}/{file}");
        if std::path::Path::new(&path).exists() {
            register_file(&path);
        }
    }

    /// `svg` with its text as outlines, set in the fonts the system has
    /// plus the ones kept in `DIR`.
    ///
    /// The toolkit's SVG renderer reads the font folders, not what this
    /// process registered, so text left to it would be set in another font
    /// than the one its lines were measured in: wider, and past the edges of
    /// its shape.
    pub fn outlined(svg: &str) -> Option<String> {
        use std::sync::{Arc, LazyLock};
        use usvg::fontdb::Database;

        static FONTS: LazyLock<Arc<Database>> = LazyLock::new(|| {
            let mut db = Database::new();
            db.load_system_fonts();
            db.load_fonts_dir(DIR);
            Arc::new(db)
        });
        let options = usvg::Options {
            fontdb: FONTS.clone(),
            ..Default::default()
        };
        let tree = usvg::Tree::from_str(svg, &options).ok()?;
        Some(tree.to_string(&usvg::WriteOptions::default()))
    }

    #[allow(unsafe_code)]
    fn register_file(path: &str) {
        use objc2::rc::Retained;
        use objc2_foundation::{NSString, NSURL};

        #[link(name = "CoreText", kind = "framework")]
        unsafe extern "C" {
            fn CTFontManagerRegisterFontsForURL(
                url: *const std::ffi::c_void,
                scope: u32,
                error: *mut *mut std::ffi::c_void,
            ) -> bool;
        }
        /// `kCTFontManagerScopeProcess`: for this process only, until it exits.
        const SCOPE_PROCESS: u32 = 1;

        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        // SAFETY: `url` is a live NSURL, which is a CFURL (toll-free bridged)
        // and outlives the call; a null error pointer is allowed and means
        // "don't report".
        unsafe {
            CTFontManagerRegisterFontsForURL(
                Retained::as_ptr(&url).cast(),
                SCOPE_PROCESS,
                std::ptr::null_mut(),
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod support_fonts {
    pub fn register(_family: &str) {}

    pub fn outlined(_svg: &str) -> Option<String> {
        None
    }
}

fn hsla(c: Color) -> Hsla {
    Rgba {
        r: c.r as f32 / 255.,
        g: c.g as f32 / 255.,
        b: c.b as f32 / 255.,
        a: c.a as f32 / 255.,
    }
    .into()
}

/// A place on a slide: one of its shapes (by position in the scene), a point
/// or an area, given as fractions of the slide's width and height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Spot {
    Shape(usize),
    At(f32, f32),
    Area { x: f32, y: f32, w: f32, h: f32 },
}

/// What the pointer does on something notes can be left on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pointer {
    /// The button went down here (a place in the window).
    Down(Point<Pixels>),
    /// It moved to here with no button held.
    Over(Point<Pixels>),
    /// It left.
    Out,
}

/// Told what the pointer does while notes can be left.
pub type OnPointer = Rc<dyn Fn(Pointer, &mut Window, &mut App)>;

/// What is drawn over a slide for notes: numbered pins with what they are
/// about outlined, the place a note is being written about, the shape under
/// the pointer and an area being dragged out.
#[derive(Clone)]
pub struct Marks {
    pub accent: Hsla,
    pub on_accent: Hsla,
    pub pins: Vec<(u32, Spot)>,
    pub active: Option<Spot>,
    /// The shapes in the area a note is being written about.
    pub picked: Vec<usize>,
    /// The shape under the pointer and what to call it.
    pub hover: Option<(usize, SharedString)>,
    /// An area being dragged out, as fractions: left, top, width, height.
    pub marquee: Option<[f32; 4]>,
    /// While notes can be left: told what the pointer does on the slide.
    pub on: Option<OnPointer>,
    /// Where the slide was painted last, for whoever handles the pointer.
    pub bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

/// The slide, as wide as its parent and as tall as its proportions make it.
///
/// `key` tells this slide from the others shown with it.
pub fn slide(
    key: usize,
    scene: Arc<Scene>,
    size: (f32, f32),
    images: Images,
    marks: Option<Marks>,
) -> impl IntoElement {
    let (sw, sh) = (size.0.max(1.), size.1.max(1.));
    canvas(
        move |bounds, window, cx| {
            if !window.content_mask().bounds.intersects(&bounds) {
                return None;
            }
            let scale = f32::from(bounds.size.width) / sw;
            if let Some(marks) = &marks {
                marks.bounds.set(Some(bounds));
            }
            let fonts = Fonts {
                text: window.text_system().clone(),
                default: window.text_style().font_family.clone(),
                resolved: RefCell::default(),
                selection: gpui_kit::base::Theme::global(cx).tokens.colors.selection,
                texts: Cell::new(0),
            };
            let mut root = draw(&scene, (sw, sh), scale, &images, &fonts);
            if let Some(marks) = &marks {
                root = root.child(marks_el(&scene, (sw, sh), scale, marks));
            }
            // Under an id of the slide's own: the ids inside (pieces of
            // text, shapes to click) repeat from slide to slide.
            let mut el = div()
                .id(("slide", key))
                .size_full()
                .child(root)
                .into_any_element();
            el.prepaint_as_root(bounds.origin, bounds.size.into(), window, cx);
            Some(el)
        },
        |_, el, window, cx| {
            if let Some(mut el) = el {
                el.paint(window, cx);
            }
        },
    )
    .w_full()
    .aspect_ratio(sw / sh)
}

fn draw(scene: &Scene, (sw, sh): (f32, f32), scale: f32, images: &Images, fonts: &Fonts) -> Div {
    let mut root = div()
        .relative()
        .w(px(sw * scale))
        .h(px(sh * scale))
        .overflow_hidden();
    root = match &scene.background {
        Fill::None => root,
        Fill::Solid(c) => root.bg(hsla(*c)),
        Fill::Gradient {
            angle,
            from,
            to,
            mid,
            radial,
        } => match bands(*angle, *from, *to, mid, *radial, sw * scale, sh * scale) {
            Some(bands) => root.children(bands),
            None => root.bg(gradient(*angle, *from, *to)),
        },
        Fill::Image(name) => match images.get(name) {
            Some(image) => root.child(
                img(ImageSource::Image(image.clone()))
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .object_fit(ObjectFit::Cover),
            ),
            None => root,
        },
    };
    for shape in &scene.shapes {
        root = root.child(shape_el(shape, scale, images, fonts));
        // A chart's or a diagram's parts are placed on the slide themselves.
        if let ShapeKind::Drawing(drawing) = &shape.kind {
            for part in &drawing.shapes {
                root = root.child(shape_el(part, scale, images, fonts));
            }
        }
    }
    root
}

/// A shape's box in pixels, grown to something a pointer can hit when it is a
/// line or a sliver.
pub(crate) fn hit_box(shape: &Shape, scale: f32) -> (f32, f32, f32, f32) {
    const MIN: f32 = 10.;
    let (mut x, mut y) = (shape.x * scale, shape.y * scale);
    let (mut w, mut h) = (shape.w * scale, shape.h * scale);
    if w < MIN {
        x -= (MIN - w) / 2.;
        w = MIN;
    }
    if h < MIN {
        y -= (MIN - h) / 2.;
        h = MIN;
    }
    (x, y, w, h)
}

/// The layer over a slide's shapes that takes the pointer for notes and shows
/// them.
fn marks_el(scene: &Scene, (sw, sh): (f32, f32), scale: f32, marks: &Marks) -> AnyElement {
    let (w, h) = (sw * scale, sh * scale);
    let accent = marks.accent;
    let mut layer = div()
        .id("slide-marks")
        .absolute()
        .top_0()
        .left_0()
        .w(px(w))
        .h(px(h));
    if let Some(on) = &marks.on {
        let (down, over, out) = (on.clone(), on.clone(), on.clone());
        layer = layer
            .cursor(CursorStyle::Crosshair)
            .on_mouse_down(gpui_kit::MouseButton::Left, move |event, window, cx| {
                down(Pointer::Down(event.position), window, cx)
            })
            .on_mouse_move(move |event, window, cx| {
                if event.pressed_button.is_none() {
                    over(Pointer::Over(event.position), window, cx)
                }
            })
            .on_hover(move |hovered, window, cx| {
                if !*hovered {
                    out(Pointer::Out, window, cx)
                }
            });
    }
    let boxed = |ix: usize| scene.shapes.get(ix).map(|s| hit_box(s, scale));
    let area = |x: f32, y: f32, aw: f32, ah: f32| (x * w, y * h, aw * w, ah * h);
    // What a note is about, outlined, with its pin on the top right corner;
    // a point has only the pin, its tip on the point.
    for (n, spot) in &marks.pins {
        let rect = match *spot {
            Spot::Shape(ix) => boxed(ix),
            Spot::Area { x, y, w: aw, h: ah } => Some(area(x, y, aw, ah)),
            Spot::At(fx, fy) => {
                let at = (fx * w + PIN / 2., fy * h - PIN / 2.);
                layer = layer.child(pin_el(Some(*n), at, (w, h), marks));
                None
            }
        };
        if let Some((x, y, bw, bh)) = rect {
            layer = layer
                .child(mark(accent).left(px(x)).top(px(y)).w(px(bw)).h(px(bh)))
                .child(pin_el(Some(*n), (x + bw, y), (w, h), marks));
        }
    }
    for &ix in &marks.picked {
        if let Some((x, y, bw, bh)) = boxed(ix) {
            layer = layer.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(bw))
                    .h(px(bh))
                    .rounded(px(3.))
                    .border_1()
                    .border_color(accent.opacity(0.67))
                    .bg(accent.opacity(0.06)),
            );
        }
    }
    let lit = |x: f32, y: f32, bw: f32, bh: f32| {
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(bw))
            .h(px(bh))
            .rounded(px(4.))
            .border_2()
            .border_color(accent)
            .bg(accent.opacity(0.12))
    };
    match marks.active {
        Some(Spot::Shape(ix)) => {
            if let Some((x, y, bw, bh)) = boxed(ix) {
                layer = layer.child(lit(x, y, bw, bh));
            }
        }
        Some(Spot::Area { x, y, w: aw, h: ah }) => {
            let (x, y, bw, bh) = area(x, y, aw, ah);
            layer = layer.child(marquee(accent).left(px(x)).top(px(y)).w(px(bw)).h(px(bh)));
        }
        Some(Spot::At(fx, fy)) => {
            layer = layer.child(pin_el(None, (fx * w, fy * h), (w, h), marks));
        }
        None => {}
    }
    if let Some((ix, name)) = &marks.hover
        && let Some((x, y, bw, bh)) = boxed(*ix)
    {
        layer = layer.child(lit(x, y, bw, bh)).child(
            tag(name.clone(), accent, marks.on_accent)
                .left(px(x.clamp(0., (w - 40.).max(0.))))
                .top(px((y - TAG).max(0.))),
        );
    }
    if let Some([x, y, aw, ah]) = marks.marquee {
        let (x, y, bw, bh) = area(x, y, aw, ah);
        layer = layer.child(marquee(accent).left(px(x)).top(px(y)).w(px(bw)).h(px(bh)));
    }
    layer.into_any_element()
}

/// The size of a pin, in pixels.
pub const PIN: f32 = 20.;
/// How tall the label over what the pointer is on is.
pub const TAG: f32 = 20.;

/// A pin centred on `at`, kept inside a `within`-sized area.
fn pin_el(n: Option<u32>, at: (f32, f32), within: (f32, f32), marks: &Marks) -> Div {
    let x = (at.0 - PIN / 2.).clamp(0., (within.0 - PIN).max(0.));
    let y = (at.1 - PIN / 2.).clamp(0., (within.1 - PIN).max(0.));
    pin(n, marks.accent, marks.on_accent)
        .absolute()
        .left(px(x))
        .top(px(y))
}

/// A pin, for any document: the caller places it. With a number, the drop
/// that marks a note (its tip at the bottom left); without, the dot that
/// shows where a note being written will go.
pub fn pin(n: Option<u32>, accent: Hsla, on_accent: Hsla) -> Div {
    div()
        .flex_none()
        .size(px(PIN))
        .rounded(px(PIN / 2.))
        .when(n.is_some(), |d| d.rounded_bl(px(2.)))
        .bg(accent)
        .border_1()
        .border_color(on_accent)
        .shadow_md()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.))
        .line_height(px(11.))
        .font_weight(FontWeight::BOLD)
        .text_color(on_accent)
        .when_some(n, |d, n| d.child(n.to_string()))
}

/// The dashed outline around what a note is about: the caller places it.
pub fn mark(accent: Hsla) -> Div {
    div()
        .absolute()
        .rounded(px(5.))
        .border_1()
        .border_dashed()
        .border_color(accent)
}

/// The area being dragged out: the caller places it.
pub fn marquee(accent: Hsla) -> Div {
    mark(accent).rounded(px(6.)).bg(accent.opacity(0.08))
}

/// The label over what the pointer is on: the caller places it.
pub fn tag(name: SharedString, accent: Hsla, on_accent: Hsla) -> Div {
    div()
        .absolute()
        .h(px(TAG - 2.))
        .px(px(6.))
        .rounded(px(4.))
        .bg(accent)
        .flex()
        .items_center()
        .whitespace_nowrap()
        .text_size(px(11.))
        .line_height(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(on_accent)
        .child(name)
}

/// A gradient through more than two colours, over a `w` × `h` box: the
/// toolkit's gradients have two. Straight across or down, it is bands side
/// by side, each a gradient between two neighbours; at a slant, thin strips
/// across its way, each of one colour. `None` when there are only two
/// colours, which the toolkit draws itself.
fn bands(
    angle: f32,
    from: Color,
    to: Color,
    mid: &[(f32, Color)],
    radial: bool,
    w: f32,
    h: f32,
) -> Option<Vec<AnyElement>> {
    if mid.is_empty() && !radial {
        return None;
    }
    let mut stops = vec![(0f32, from)];
    stops.extend(mid.iter().map(|(p, c)| (p.clamp(0., 1.), *c)));
    stops.push((1., to));
    if radial {
        return Some(vec![rings(stops).into_any_element()]);
    }
    let turns = (angle.rem_euclid(360.) / 90.).round();
    if (angle.rem_euclid(360.) - turns * 90.).abs() > 1. {
        return Some(vec![strips(angle, stops).into_any_element()]);
    }
    let quarter = turns as i32 % 4;
    let mut out = Vec::new();
    for pair in stops.windows(2) {
        let ((p0, c0), (p1, c1)) = (pair[0], pair[1]);
        if p1 <= p0 {
            continue;
        }
        // Along the gradient's way; a hair over, so no seam shows between.
        let band = div().absolute().bg(gradient(angle, c0, c1));
        let (long_w, long_h) = (px((p1 - p0) * w + 0.5), px((p1 - p0) * h + 0.5));
        out.push(
            match quarter {
                0 => band.top_0().h(px(h)).left(px(p0 * w)).w(long_w),
                1 => band.left_0().w(px(w)).top(px(p0 * h)).h(long_h),
                2 => band.top_0().h(px(h)).left(px((1. - p1) * w)).w(long_w),
                _ => band.left_0().w(px(w)).top(px((1. - p1) * h)).h(long_h),
            }
            .into_any_element(),
        );
    }
    Some(out)
}

/// The colour a gradient has at `t` of its way (0 to 1).
fn color_along(stops: &[(f32, Color)], t: f32) -> Hsla {
    let pair = stops
        .windows(2)
        .find(|p| t <= p[1].0)
        .unwrap_or(&stops[stops.len().saturating_sub(2)..]);
    let ((p0, c0), (p1, c1)) = (pair[0], pair[pair.len() - 1]);
    let f = if p1 > p0 {
        ((t - p0) / (p1 - p0)).clamp(0., 1.)
    } else {
        0.
    };
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f) / 255.;
    Rgba {
        r: mix(c0.r, c1.r),
        g: mix(c0.g, c1.g),
        b: mix(c0.b, c1.b),
        a: mix(c0.a, c1.a),
    }
    .into()
}

/// A gradient spreading from the middle of the element's box to its
/// corners, painted as ovals one inside the other, the outermost first.
fn rings(stops: Vec<(f32, Color)>) -> impl IntoElement {
    const RINGS: usize = 48;
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let centre = bounds.center();
            // The ovals reach past the box, to its corners: keep to the box.
            let mask = gpui_kit::ContentMask { bounds };
            window.with_content_mask(Some(mask), |window| {
                window.paint_quad(fill(bounds, color_along(&stops, 1.)));
                // An oval through the corners is √2 times the box each way.
                let (rx, ry) = (
                    w / 2. * std::f32::consts::SQRT_2,
                    h / 2. * std::f32::consts::SQRT_2,
                );
                const K: f32 = 0.552_284_8;
                for i in (0..RINGS).rev() {
                    let t = (i as f32 + 0.5) / RINGS as f32;
                    let grow = (i + 1) as f32 / RINGS as f32;
                    let (a, b) = (rx * grow, ry * grow);
                    let at = |x: f32, y: f32| point(centre.x + px(x), centre.y + px(y));
                    let mut path = PathBuilder::fill();
                    path.move_to(at(0., -b));
                    path.cubic_bezier_to(at(a, 0.), at(a * K, -b), at(a, -b * K));
                    path.cubic_bezier_to(at(0., b), at(a, b * K), at(a * K, b));
                    path.cubic_bezier_to(at(-a, 0.), at(-a * K, b), at(-a, b * K));
                    path.cubic_bezier_to(at(0., -b), at(-a, -b * K), at(-a * K, -b));
                    path.close();
                    if let Ok(path) = path.build() {
                        window.paint_path(path, color_along(&stops, t));
                    }
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// A slanted gradient over the element's box, painted as strips across the
/// gradient's way, each the colour the gradient has there.
fn strips(angle: f32, stops: Vec<(f32, Color)>) -> impl IntoElement {
    const STRIPS: usize = 64;
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let (dy, dx) = angle.to_radians().sin_cos();
            // How far along the gradient's way each corner is.
            let along = |(x, y): (f32, f32)| x * dx + y * dy;
            let corners = [(0., 0.), (w, 0.), (w, h), (0., h)];
            let (lo, hi) = corners.iter().fold((f32::MAX, f32::MIN), |(lo, hi), c| {
                (lo.min(along(*c)), hi.max(along(*c)))
            });
            if hi - lo < 0.01 {
                return;
            }
            let color_at = |t: f32| color_along(&stops, t);
            // The part of a polygon on one side of a line across the way.
            let cut = |poly: &[(f32, f32)], at: f32, keep_above: bool| -> Vec<(f32, f32)> {
                let inside = |p: (f32, f32)| (along(p) >= at) == keep_above;
                let mut out = Vec::new();
                for i in 0..poly.len() {
                    let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
                    if inside(a) {
                        out.push(a);
                    }
                    if inside(a) != inside(b) {
                        let f = (at - along(a)) / (along(b) - along(a));
                        out.push((a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f));
                    }
                }
                out
            };
            for i in 0..STRIPS {
                let (t0, t1) = (i as f32 / STRIPS as f32, (i + 1) as f32 / STRIPS as f32);
                // A hair over, so no seam shows between strips.
                let from = lo + (hi - lo) * t0 - 0.3;
                let upto = lo + (hi - lo) * t1 + 0.3;
                let strip = cut(&cut(&corners, from, true), upto, false);
                if strip.len() < 3 {
                    continue;
                }
                let mut path = PathBuilder::fill();
                for (n, (x, y)) in strip.iter().enumerate() {
                    let p = point(bounds.origin.x + px(*x), bounds.origin.y + px(*y));
                    if n == 0 {
                        path.move_to(p);
                    } else {
                        path.line_to(p);
                    }
                }
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, color_at((t0 + t1) / 2.));
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// DrawingML measures a gradient's angle from "to the right", clockwise; CSS
/// (and GPUI) from "to the top".
fn gradient(angle: f32, from: Color, to: Color) -> gpui_kit::Background {
    linear_gradient(
        (angle + 90.).rem_euclid(360.),
        linear_color_stop(hsla(from), 0.),
        linear_color_stop(hsla(to), 1.),
    )
}

fn shape_el(shape: &Shape, scale: f32, images: &Images, fonts: &Fonts) -> AnyElement {
    let (w, h) = (shape.w * scale, shape.h * scale);
    let mut el = div()
        .absolute()
        .left(px(shape.x * scale))
        .top(px(shape.y * scale))
        .w(px(w))
        .h(px(h));
    let shadow = shape.shadow.map(|s| BoxShadow {
        color: hsla(s.color),
        offset: point(px(s.dx * scale), px(s.dy * scale)),
        blur_radius: px(s.blur * scale),
        spread_radius: px(0.),
        inset: s.inset,
    });
    let radius = match &shape.geometry {
        Geometry::RoundRect(r) => r * scale,
        Geometry::Ellipse => w.min(h) / 2.,
        _ => 0.,
    };

    match &shape.kind {
        ShapeKind::Picture(picture) => {
            if let Some(shadow) = shadow {
                el = el.rounded(px(radius)).shadow(vec![shadow]);
            }
            if let Some(image) = images.get(&picture.image) {
                el = el.child(picture_el(picture, image.clone(), (w, h), radius));
            }
            if let Some(stroke) = &shape.stroke {
                el = el.child(outline(&shape.geometry, None, Some(*stroke), shape, scale));
            }
        }
        ShapeKind::Table(table) => {
            el = el.child(table_el(table, scale, fonts));
        }
        // Drawn by the caller, part by part.
        ShapeKind::Drawing(_) => {}
        ShapeKind::Unsupported(what) => {
            let ink = Hsla {
                h: 0.,
                s: 0.,
                l: 0.5,
                a: 0.8,
            };
            el = el
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4. * scale))
                .border_1()
                .border_dashed()
                .border_color(ink)
                .text_size(px((12. * scale).max(1.)))
                .text_color(ink)
                .child(SharedString::from(what.clone()));
        }
        ShapeKind::Shape => {
            // A box the toolkit can draw itself: sharper and cheaper than a
            // path, and it can cast a shadow.
            let boxy = shape.rotation.rem_euclid(180.).abs() < 0.01
                && match &shape.geometry {
                    Geometry::Rect | Geometry::RoundRect(_) => true,
                    Geometry::Ellipse => (w - h).abs() < 0.5,
                    Geometry::Path(_) => false,
                };
            if boxy {
                el = el.rounded(px(radius));
                el = match &shape.fill {
                    Fill::None => el,
                    Fill::Solid(c) => el.bg(hsla(*c)),
                    Fill::Gradient {
                        angle,
                        from,
                        to,
                        mid,
                        radial,
                    } => {
                        // Bands have square corners: only a rectangle takes them.
                        let square = matches!(shape.geometry, Geometry::Rect);
                        match bands(*angle, *from, *to, mid, *radial, w, h).filter(|_| square) {
                            Some(bands) => el.children(bands),
                            None => el.bg(gradient(*angle, *from, *to)),
                        }
                    }
                    Fill::Image(name) => match images.get(name) {
                        Some(image) => el.child(
                            img(ImageSource::Image(image.clone()))
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .rounded(px(radius))
                                .object_fit(ObjectFit::Fill),
                        ),
                        None => el,
                    },
                };
                if let Some(s) = &shape.stroke {
                    el = el
                        .border(px((s.width * scale).max(0.5)))
                        .border_color(hsla(s.color));
                    if s.dash != Dash::Solid {
                        el = el.border_dashed();
                    }
                }
                if let Some(shadow) = shadow
                    && shape.fill != Fill::None
                {
                    el = el.shadow(vec![shadow]);
                }
            } else {
                let fill = match &shape.fill {
                    Fill::Solid(c) => Some(gpui_kit::Background::from(hsla(*c))),
                    Fill::Gradient {
                        angle, from, to, ..
                    } => Some(gradient(*angle, *from, *to)),
                    _ => None,
                };
                el = el.child(outline(&shape.geometry, fill, shape.stroke, shape, scale));
            }
        }
    }
    if let Some(body) = &shape.text {
        let angle = (shape.rotation + body.turn).rem_euclid(360.);
        if !(0.01..=359.99).contains(&angle) {
            el = el.child(
                text_column(body, scale, fonts)
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(w))
                    .h(px(h)),
            );
        } else if let Some(turned) = turned_text(body, (shape.w, shape.h), angle, scale, fonts) {
            el = el.child(turned);
        }
    }
    el.into_any_element()
}

fn picture_el(picture: &Picture, image: Arc<Image>, (w, h): (f32, f32), radius: f32) -> Div {
    let [l, t, r, b] = picture.crop;
    let cropped = l > 0. || t > 0. || r > 0. || b > 0.;
    if !cropped {
        return div().size_full().child(
            img(ImageSource::Image(image))
                .size_full()
                .rounded(px(radius))
                .object_fit(ObjectFit::Fill),
        );
    }
    // Show the kept part: the whole image is that much larger than the frame.
    let (iw, ih) = (w / (1. - l - r).max(0.05), h / (1. - t - b).max(0.05));
    div().size_full().overflow_hidden().child(
        img(ImageSource::Image(image))
            .absolute()
            .left(px(-l * iw))
            .top(px(-t * ih))
            .w(px(iw))
            .h(px(ih))
            .object_fit(ObjectFit::Fill),
    )
}

/// A shape's outline as path commands in its unit box.
fn unit_path(geometry: &Geometry, w: f32, h: f32) -> Vec<PathCmd> {
    // How far a cubic's control point sits from the corner for a quarter circle.
    const K: f32 = 0.552_284_8;
    match geometry {
        Geometry::Path(cmds) => cmds.clone(),
        Geometry::Rect => vec![
            PathCmd::Move(0., 0.),
            PathCmd::Line(1., 0.),
            PathCmd::Line(1., 1.),
            PathCmd::Line(0., 1.),
            PathCmd::Close,
        ],
        Geometry::Ellipse => {
            let c = 0.5 * K;
            vec![
                PathCmd::Move(0.5, 0.),
                PathCmd::Cubic(0.5 + c, 0., 1., 0.5 - c, 1., 0.5),
                PathCmd::Cubic(1., 0.5 + c, 0.5 + c, 1., 0.5, 1.),
                PathCmd::Cubic(0.5 - c, 1., 0., 0.5 + c, 0., 0.5),
                PathCmd::Cubic(0., 0.5 - c, 0.5 - c, 0., 0.5, 0.),
                PathCmd::Close,
            ]
        }
        Geometry::RoundRect(r) => {
            // `w` and `h` are in pixels here, the radius in the same unit.
            let rx = if w > 0. { (r / w).min(0.5) } else { 0. };
            let ry = if h > 0. { (r / h).min(0.5) } else { 0. };
            let (cx, cy) = (rx * (1. - K), ry * (1. - K));
            vec![
                PathCmd::Move(rx, 0.),
                PathCmd::Line(1. - rx, 0.),
                PathCmd::Cubic(1. - cx, 0., 1., cy, 1., ry),
                PathCmd::Line(1., 1. - ry),
                PathCmd::Cubic(1., 1. - cy, 1. - cx, 1., 1. - rx, 1.),
                PathCmd::Line(rx, 1.),
                PathCmd::Cubic(cx, 1., 0., 1. - cy, 0., 1. - ry),
                PathCmd::Line(0., ry),
                PathCmd::Cubic(0., cy, cx, 0., rx, 0.),
                PathCmd::Close,
            ]
        }
    }
}

/// Paints a shape's outline as a path: filled, stroked, with arrowheads.
fn outline(
    geometry: &Geometry,
    fill: Option<gpui_kit::Background>,
    stroke: Option<Stroke>,
    shape: &Shape,
    scale: f32,
) -> impl IntoElement + use<> {
    let (w, h) = (shape.w * scale, shape.h * scale);
    let radius_px = match geometry {
        Geometry::RoundRect(r) => Geometry::RoundRect(r * scale),
        other => other.clone(),
    };
    let cmds = unit_path(&radius_px, w, h);
    let (sin, cos) = shape.rotation.to_radians().sin_cos();
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let origin = bounds.origin;
            let at = |u: f32, v: f32| -> Point<Pixels> {
                // Into pixels, then turned about the shape's centre.
                let (x, y) = (u * w - w / 2., v * h - h / 2.);
                point(
                    origin.x + px(w / 2. + x * cos - y * sin),
                    origin.y + px(h / 2. + x * sin + y * cos),
                )
            };
            let trace = |b: &mut PathBuilder| {
                for c in &cmds {
                    match *c {
                        PathCmd::Move(x, y) => b.move_to(at(x, y)),
                        PathCmd::Line(x, y) => b.line_to(at(x, y)),
                        PathCmd::Cubic(ax, ay, bx, by, x, y) => {
                            b.cubic_bezier_to(at(x, y), at(ax, ay), at(bx, by))
                        }
                        PathCmd::Close => b.close(),
                    }
                }
            };
            if cmds.len() < 2 {
                return;
            }
            let closed = cmds.contains(&PathCmd::Close);
            if let Some(fill) = fill
                && closed
            {
                let mut b = PathBuilder::fill();
                trace(&mut b);
                if let Ok(path) = b.build() {
                    window.paint_path(path, fill);
                }
            }
            let Some(s) = stroke else { return };
            let width = (s.width * scale).max(0.5);
            let mut b = PathBuilder::stroke(px(width));
            b = match s.dash {
                Dash::Solid => b,
                Dash::Dash => b.dash_array(&[px(width * 4.), px(width * 3.)]),
                Dash::Dot => b.dash_array(&[px(width), px(width * 2.)]),
            };
            trace(&mut b);
            if let Ok(path) = b.build() {
                window.paint_path(path, hsla(s.color));
            }
            // Arrowheads: a triangle at an end, pointing along its last segment.
            let ends = |c: &PathCmd| match *c {
                PathCmd::Move(x, y) | PathCmd::Line(x, y) | PathCmd::Cubic(_, _, _, _, x, y) => {
                    Some(at(x, y))
                }
                PathCmd::Close => None,
            };
            let points: Vec<Point<Pixels>> = cmds.iter().filter_map(ends).collect();
            let mut head = |tip: Point<Pixels>, from: Point<Pixels>| {
                let (dx, dy) = (f32::from(tip.x - from.x), f32::from(tip.y - from.y));
                let len = (dx * dx + dy * dy).sqrt();
                if len < 0.01 {
                    return;
                }
                let (ux, uy) = (dx / len, dy / len);
                let size = (width * 3.).max(4. * scale);
                let back = point(tip.x - px(ux * size), tip.y - px(uy * size));
                let side = point(px(-uy * size * 0.5), px(ux * size * 0.5));
                // The tip sits a little past the line's end, so the stroke's
                // square end does not show beside it.
                let tip = point(tip.x + px(ux * width * 0.5), tip.y + px(uy * width * 0.5));
                let mut b = PathBuilder::fill();
                b.move_to(tip);
                b.line_to(point(back.x + side.x, back.y + side.y));
                b.line_to(point(back.x - side.x, back.y - side.y));
                b.close();
                if let Ok(path) = b.build() {
                    window.paint_path(path, hsla(s.color));
                }
            };
            if points.len() >= 2 && !closed {
                if s.end_arrow {
                    head(points[points.len() - 1], points[points.len() - 2]);
                }
                if s.start_arrow {
                    head(points[0], points[1]);
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// A text frame: its paragraphs stacked, kept clear of the frame's insets and
/// held to its top, middle or bottom.
fn text_column(body: &TextBody, scale: f32, fonts: &Fonts) -> Div {
    let [l, t, r, b] = body.insets;
    let mut col = div()
        .flex()
        .flex_col()
        .pl(px(l * scale))
        .pt(px(t * scale))
        .pr(px(r * scale))
        .pb(px(b * scale));
    col = match body.anchor {
        Anchor::Top => col.justify_start(),
        Anchor::Middle => col.justify_center(),
        Anchor::Bottom => col.justify_end(),
    };
    if !body.wrap {
        col = col.whitespace_nowrap();
    }
    for p in &body.paragraphs {
        col = col.child(paragraph_el(p, scale, fonts));
    }
    col
}

fn font_for(run: &Run, fonts: &Fonts) -> Font {
    Font {
        family: fonts.family(run.font.as_deref()),
        features: Default::default(),
        fallbacks: None,
        weight: if run.bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        style: if run.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
    }
}

fn text_run(run: &Run, len: usize, size_px: f32, fonts: &Fonts) -> TextRun {
    let color = hsla(run.color);
    TextRun {
        len,
        font: font_for(run, fonts),
        color,
        background_color: None,
        underline: run.underline.then_some(UnderlineStyle {
            thickness: px((size_px / 14.).max(1.)),
            color: Some(color),
            wavy: false,
        }),
        strikethrough: run.strike.then_some(StrikethroughStyle {
            thickness: px((size_px / 14.).max(1.)),
            color: Some(color),
        }),
    }
}

fn paragraph_el(p: &Paragraph, scale: f32, fonts: &Fonts) -> Div {
    let tallest = p.runs.iter().map(|r| r.size).fold(0f32, f32::max);
    let size = if tallest > 0. { tallest } else { p.size };
    let size_px = (size * scale).max(1.);
    let line = match p.line_spacing {
        LineSpacing::Multiple(m) => size_px * NATURAL_LINE * m,
        LineSpacing::Exact(v) => v * scale,
    }
    .max(1.);
    let mut el = div()
        .relative()
        .w_full()
        .flex_none()
        .mt(px(p.space_before * scale))
        .mb(px(p.space_after * scale))
        .pl(px(p.margin_left * scale))
        .text_size(px(size_px))
        .line_height(px(line))
        .text_align(match p.align {
            Align::Left => TextAlign::Left,
            Align::Center => TextAlign::Center,
            Align::Right => TextAlign::Right,
        });
    if p.runs.is_empty() {
        // A blank line is as tall as a line of text.
        return el.h(px(line));
    }
    if let (Some(bullet), Some(first)) = (&p.bullet, p.runs.first()) {
        let mut ink = first.clone();
        ink.bold = false;
        ink.italic = false;
        ink.underline = false;
        ink.strike = false;
        if let Some(c) = bullet.color {
            ink.color = c;
        }
        if bullet.font.is_some() {
            ink.font = bullet.font.clone();
        }
        let text: SharedString = bullet.text.clone().into();
        let bullet_px = (size_px * bullet.scale).max(1.);
        let run = text_run(&ink, text.len(), bullet_px, fonts);
        // The line keeps the paragraph's height, so the bullet sits on the
        // first line's baseline whatever its own size.
        el = el.child(
            div()
                .absolute()
                .top_0()
                .left(px(((p.margin_left + p.indent).max(0.)) * scale))
                .whitespace_nowrap()
                .text_size(px(bullet_px))
                .child(StyledText::new(text).with_runs(vec![run])),
        );
    }
    let uniform = p.runs.iter().all(|r| (r.size - size).abs() < 0.01);
    if uniform {
        let mut text = String::new();
        let mut runs = Vec::with_capacity(p.runs.len());
        for r in &p.runs {
            text.push_str(&r.text);
            runs.push(text_run(r, r.text.len(), size_px, fonts));
        }
        return el.child(fonts.selectable(text, runs));
    }
    // Several sizes on one line: each word is its own piece on a shared
    // baseline, and lines break between pieces.
    let mut row = div().flex().flex_wrap().items_baseline().w_full();
    row = match p.align {
        Align::Left => row.justify_start(),
        Align::Center => row.justify_center(),
        Align::Right => row.justify_end(),
    };
    for r in &p.runs {
        let piece_px = (r.size * scale).max(1.);
        for (i, part) in r.text.split('\n').enumerate() {
            if i > 0 {
                row = row.child(div().w_full().h_0());
            }
            for word in part.split_inclusive(' ') {
                let word: SharedString = word.to_string().into();
                let run = text_run(r, word.len(), piece_px, fonts);
                row = row.child(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .text_size(px(piece_px))
                        .child(fonts.selectable(word, vec![run])),
                );
            }
        }
    }
    el.child(row)
}

fn table_el(table: &Table, scale: f32, fonts: &Fonts) -> Div {
    let mut grid = div().flex().flex_col().size_full();
    for row in &table.rows {
        let mut line = div()
            .flex()
            .flex_none()
            .w_full()
            .min_h(px(row.height * scale));
        for (ix, cell) in row.cells.iter().enumerate() {
            // A cell covered from the left has no room of its own: the cell
            // that spans over it already took its column.
            if cell.col_span == 0 {
                continue;
            }
            let width: f32 = table.columns.iter().skip(ix).take(cell.col_span).sum();
            let mut el = div().flex_none().w(px(width * scale)).min_h_full();
            el = match &cell.fill {
                Fill::Solid(c) => el.bg(hsla(*c)),
                Fill::Gradient {
                    angle, from, to, ..
                } => el.bg(gradient(*angle, *from, *to)),
                _ => el,
            };
            if let Some(s) = &cell.border {
                el = el
                    .border(px((s.width * scale).max(0.5)))
                    .border_color(hsla(s.color));
            }
            if let Some(body) = &cell.text
                && !cell.covered
            {
                el = el.child(text_column(body, scale, fonts).size_full());
            }
            line = line.child(el);
        }
        grid = grid.child(line);
    }
    grid
}

/// What found text is marked with: every match, and the one in turn.
const FOUND: Hsla = Hsla {
    h: 0.14,
    s: 1.,
    l: 0.5,
    a: 0.4,
};
const FOUND_CURRENT: Hsla = Hsla {
    h: 0.08,
    s: 1.,
    l: 0.5,
    a: 0.6,
};

/// Styled text that takes part in the window's text selection: it can be
/// dragged over, alone or together with the text around it, and copied.
///
/// This is gpui-base's `SelectableText` for text whose runs differ in font
/// and colour, which that element has no way to be given.
struct SelectableRuns {
    id: ElementId,
    text: SharedString,
    styled: StyledText,
    /// Its place in reading order.
    order: u64,
    highlight: Hsla,
    /// Laid out and selectable but not painted: the words are already in the
    /// picture underneath.
    hidden: bool,
    /// Ranges of the text to mark as found; the flag is for the one in turn.
    finds: Vec<(Range<usize>, bool)>,
    /// For hidden text on one line: where each character starts, from the
    /// left edge, and where the last one ends, as the picture has them. What
    /// is selected or found is marked there, not where this element's own
    /// font would put it.
    stops: Option<Vec<f32>>,
}

/// Text that is there to be selected, searched and copied over a picture
/// that already shows it (a PDF page): laid out by its parent's font size,
/// never painted itself. `n` tells it from its neighbours.
pub fn hidden_text(
    n: u64,
    text: SharedString,
    run: TextRun,
    selection: Hsla,
    finds: Vec<(Range<usize>, bool)>,
    stops: Vec<f32>,
) -> impl IntoElement {
    SelectableRuns {
        id: ElementId::NamedInteger("page-text".into(), n),
        styled: StyledText::new(text.clone()).with_runs(vec![run]),
        text,
        order: n,
        highlight: selection,
        hidden: true,
        finds,
        stops: Some(stops),
    }
}

impl IntoElement for SelectableRuns {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableRuns {
    type RequestLayoutState = Option<TextSelectionHandle>;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // The selection lives with the element from frame to frame.
        let handle = global_id.map(|id| {
            window.with_element_state(id, |kept: Option<TextSelectionHandle>, _| {
                let handle =
                    kept.unwrap_or_else(|| TextSelectionHandle::new(self.text.clone(), cx));
                (handle.clone(), handle)
            })
        });
        let (layout_id, ()) = self
            .styled
            .request_layout(global_id, inspector_id, window, cx);
        (layout_id, handle)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled
            .prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        if let Some(handle) = handle {
            let registration = TextSelectionRegistration::new(hitbox.clone(), bounds)
                .with_document_order(self.order)
                .with_text_bounds(vec![bounds])
                .with_rendered_element(handle, window, cx);
            handle.register(registration, window, cx);
        }
        hitbox
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        let layout = self.styled.layout().clone();
        let line = layout.line_height();
        let area = layout.bounds();
        // The bands that cover a range of the text: one on a single line;
        // else the first line's rest, the full lines between, and the last
        // line's start.
        let bands = |range: &Range<usize>| -> Vec<Bounds<Pixels>> {
            if let Some(stops) = &self.stops {
                // Bytes to characters, then to the places the picture has.
                let at = |byte: usize| {
                    let ix = self.text[..byte.min(self.text.len())].chars().count();
                    stops.get(ix).or(stops.last()).copied().unwrap_or(0.)
                };
                return vec![Bounds::from_corners(
                    point(bounds.left() + px(at(range.start)), bounds.top()),
                    point(bounds.left() + px(at(range.end)), bounds.bottom()),
                )];
            }
            let (Some(start), Some(end)) = (
                layout.position_for_index(range.start),
                layout.position_for_index(range.end),
            ) else {
                return Vec::new();
            };
            if start.y == end.y {
                return vec![Bounds::from_corners(start, point(end.x, end.y + line))];
            }
            let mut bands = vec![Bounds::from_corners(
                start,
                point(area.right(), start.y + line),
            )];
            if end.y > start.y + line {
                bands.push(Bounds::from_corners(
                    point(area.left(), start.y + line),
                    point(area.right(), end.y),
                ));
            }
            bands.push(Bounds::from_corners(
                point(area.left(), end.y),
                point(end.x, end.y + line),
            ));
            bands
        };
        for (range, current) in &self.finds {
            let color = if *current { FOUND_CURRENT } else { FOUND };
            for band in bands(range) {
                window.paint_quad(fill(band, color));
            }
        }
        if let Some(handle) = handle {
            let projection = handle.update_runs(
                &[
                    TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)
                        .with_document_order(self.order),
                ],
                cx,
            );
            for range in projection.ranges().iter().flatten() {
                for band in bands(range) {
                    window.paint_quad(fill(band, self.highlight));
                }
            }
        }
        if self.hidden {
            return;
        }
        self.styled.paint(
            global_id,
            inspector_id,
            bounds,
            &mut (),
            &mut (),
            window,
            cx,
        );
    }
}

/// Text that is not level: turned with its shape, or running down or up it.
///
/// The toolkit paints letters upright only; what it can turn is an SVG. So
/// the text is broken into lines here, with the same measurements level text
/// gets, and written out as an SVG whose lines are turned as one. Being a
/// picture, it cannot be selected.
///
/// `(w, h)` is the shape in points, `angle` how far the text is turned in
/// all (the shape's turn and the text's own), in degrees clockwise.
fn turned_text(
    body: &TextBody,
    (w, h): (f32, f32),
    angle: f32,
    scale: f32,
    fonts: &Fonts,
) -> Option<AnyElement> {
    // The box the lines are set in: the shape's, lying on its side when the
    // text runs along its height.
    let quarter = (body.turn.rem_euclid(180.) - 90.).abs() < 0.01;
    let (lw, lh) = if quarter { (h, w) } else { (w, h) };
    let [il, it, ir, ib] = body.insets;
    let inner = (lw - il - ir).max(1.);

    let mut svg = String::new();
    let mut y = 0f32;
    let esc = |t: &str| {
        t.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    for p in &body.paragraphs {
        let tallest = p.runs.iter().map(|r| r.size).fold(0f32, f32::max);
        let size = if tallest > 0. { tallest } else { p.size }.max(1.);
        let line = match p.line_spacing {
            LineSpacing::Multiple(m) => size * NATURAL_LINE * m,
            LineSpacing::Exact(v) => v,
        }
        .max(1.);
        y += p.space_before;
        if p.runs.is_empty() {
            y += line + p.space_after;
            continue;
        }
        let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
        let runs: Vec<TextRun> = p
            .runs
            .iter()
            .map(|r| text_run(r, r.text.len(), size, fonts))
            .collect();
        let room = (inner - p.margin_left).max(1.);
        let Ok(hard_lines) = fonts.text.shape_text(
            text.clone().into(),
            px(size),
            &runs,
            body.wrap.then_some(px(room)),
            None,
        ) else {
            continue;
        };
        let (x, anchor) = match p.align {
            Align::Left => (il + p.margin_left, "start"),
            Align::Center => (il + p.margin_left + room / 2., "middle"),
            Align::Right => (il + inner, "end"),
        };
        let mut first = true;
        // Where each hard line starts in the paragraph's text.
        let mut offset = 0usize;
        for hard in &hard_lines {
            let layout = &hard.unwrapped_layout;
            let mut cuts: Vec<usize> = hard
                .wrap_boundaries
                .iter()
                .filter_map(|b| Some(layout.runs.get(b.run_ix)?.glyphs.get(b.glyph_ix)?.index))
                .collect();
            cuts.insert(0, 0);
            cuts.push(hard.text.len());
            for pair in cuts.windows(2) {
                let (from, to) = (offset + pair[0], offset + pair[1]);
                let baseline = y + (line - size) / 2. + size * 0.8;
                if first && let (Some(bullet), Some(lead)) = (&p.bullet, p.runs.first()) {
                    let c = bullet.color.unwrap_or(lead.color);
                    svg.push_str(&format!(
                        r##"<text x="{:.2}" y="{baseline:.2}" font-size="{:.2}" font-family="{}" fill="#{:02x}{:02x}{:02x}">{}</text>"##,
                        il + (p.margin_left + p.indent).max(0.),
                        size * bullet.scale,
                        svg_family(&fonts.family(bullet.font.as_deref().or(lead.font.as_deref()))),
                        c.r,
                        c.g,
                        c.b,
                        esc(&bullet.text),
                    ));
                }
                first = false;
                svg.push_str(&format!(
                    r#"<text x="{x:.2}" y="{baseline:.2}" font-size="{size:.2}" text-anchor="{anchor}" xml:space="preserve">"#
                ));
                // The pieces of each run that fall on this line.
                let mut at = 0usize;
                for r in &p.runs {
                    let (start, end) = (at.max(from), (at + r.text.len()).min(to));
                    at += r.text.len();
                    if start >= end {
                        continue;
                    }
                    let Some(piece) = text.get(start..end) else {
                        continue;
                    };
                    let piece = piece.trim_end_matches('\n');
                    if piece.is_empty() {
                        continue;
                    }
                    svg.push_str(&format!(
                        r##"<tspan font-family="{}" font-weight="{}" font-style="{}" fill="#{:02x}{:02x}{:02x}" fill-opacity="{:.3}"{}>{}</tspan>"##,
                        svg_family(&fonts.family(r.font.as_deref())),
                        if r.bold { 700 } else { 400 },
                        if r.italic { "italic" } else { "normal" },
                        r.color.r,
                        r.color.g,
                        r.color.b,
                        r.color.a as f32 / 255.,
                        if r.underline {
                            r#" text-decoration="underline""#
                        } else {
                            ""
                        },
                        esc(piece),
                    ));
                }
                svg.push_str("</text>");
                y += line;
            }
            offset += hard.text.len() + 1;
        }
        y += p.space_after;
    }
    if svg.is_empty() {
        return None;
    }
    // The lines, moved to where the frame's anchor holds them.
    let free = lh - it - ib - y;
    let top = it
        + match body.anchor {
            Anchor::Top => 0.,
            Anchor::Middle => free / 2.,
            Anchor::Bottom => free,
        };
    // The picture is the box the turned text reaches, centred on the shape.
    let (sin, cos) = angle.to_radians().sin_cos();
    let (bw, bh) = (
        (lw * cos).abs() + (lh * sin).abs(),
        (lw * sin).abs() + (lh * cos).abs(),
    );
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {bw:.2} {bh:.2}"><g transform="translate({:.2} {:.2}) rotate({angle:.3}) translate({:.2} {:.2})">{svg}</g></svg>"#,
        bw / 2.,
        bh / 2.,
        -lw / 2.,
        -lh / 2. + top,
    );
    Some(
        img(ImageSource::Image(turned_picture(svg)))
            .absolute()
            .left(px((w - bw) / 2. * scale))
            .top(px((h - bh) / 2. * scale))
            .w(px(bw * scale))
            .h(px(bh * scale))
            .into_any_element(),
    )
}

/// A font family for an SVG: the one asked for, then ones every system has,
/// for when the SVG renderer does not know the first.
fn svg_family(family: &str) -> String {
    let safe: String = family.chars().filter(|c| *c != '\'' && *c != '"').collect();
    format!("'{safe}', 'Helvetica Neue', Arial, sans-serif")
}

/// The image of an SVG, the same one for the same SVG: a picture made anew
/// every frame would be decoded anew every frame.
fn turned_picture(svg: String) -> Arc<Image> {
    thread_local! {
        static MADE: RefCell<HashMap<String, Arc<Image>>> = RefCell::default();
    }
    MADE.with(|made| {
        let mut made = made.borrow_mut();
        if let Some(image) = made.get(&svg) {
            return image.clone();
        }
        // Zooming makes a new picture per size: do not keep them all.
        if made.len() > 200 {
            made.clear();
        }
        let drawn = support_fonts::outlined(&svg).unwrap_or_else(|| svg.clone());
        let image = Arc::new(Image::from_bytes(
            gpui_kit::ImageFormat::Svg,
            drawn.into_bytes(),
        ));
        made.insert(svg, image.clone());
        image
    })
}
