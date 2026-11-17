//! PresentationML → [`Scene`]: every shape of a slide with its place, fill,
//! outline and styled text, after following what it inherits.
//!
//! A slide's shape takes whatever it does not say itself from the matching
//! placeholder of its layout, then of its master; text styles also come from
//! the master's title/body/other styles and the presentation's defaults;
//! colours named by role (`tx1`, `accent2`) go through the master's colour map
//! to the theme.
//!
//! Charts are worked out into shapes by [`crate::chart`]; diagrams (SmartArt)
//! are read from the drawing the application saved with them. Not drawn:
//! chart kinds that module does not know and embedded objects without a
//! picture (they become [`ShapeKind::Unsupported`]), and effects other than
//! an outer shadow.

use std::collections::HashMap;
use std::rc::Rc;

use crate::dom::Node;
use crate::opc::{ImageSet, Package, Rels};
use crate::scene::*;

const EMU_PER_PT: f64 = 12_700.;
/// Groups nested deeper than this are not followed.
const MAX_GROUP_DEPTH: usize = 32;
/// A slide keeps at most this many shapes.
const MAX_SHAPES: usize = 20_000;
/// Text without a size anywhere up the chain.
const DEFAULT_FONT_SIZE: f32 = 18.;

fn pt(emu: f64) -> f32 {
    (emu / EMU_PER_PT) as f32
}

struct Part {
    root: Node,
    rels: Rels,
}

#[derive(Default)]
struct Theme {
    colors: HashMap<String, Color>,
    major_font: Option<String>,
    minor_font: Option<String>,
    /// Outline widths of the theme's three line styles.
    line_widths: Vec<f32>,
    /// The theme's effect styles, which a shape's style picks by number.
    effects: Vec<Node>,
}

struct Master {
    part: Rc<Part>,
    theme: Rc<Theme>,
    clr_map: HashMap<String, String>,
    default_text: ListStyle,
    title: ListStyle,
    body: ListStyle,
    other: ListStyle,
}

/// What stays the same for one part's shapes.
pub(crate) struct Env<'e> {
    theme: &'e Theme,
    clr_map: &'e HashMap<String, String>,
    rels: &'e Rels,
    slide_no: usize,
}

// ---------------------------------------------------------------- colours

fn hex_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(Color::rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

fn preset_color(name: &str) -> Option<Color> {
    Some(match name {
        "black" => Color::rgb(0, 0, 0),
        "white" => Color::rgb(255, 255, 255),
        "red" => Color::rgb(255, 0, 0),
        "green" => Color::rgb(0, 128, 0),
        "blue" => Color::rgb(0, 0, 255),
        "yellow" => Color::rgb(255, 255, 0),
        "orange" => Color::rgb(255, 165, 0),
        "purple" => Color::rgb(128, 0, 128),
        "gray" | "grey" => Color::rgb(128, 128, 128),
        "lightGray" | "ltGray" => Color::rgb(211, 211, 211),
        "darkGray" | "dkGray" => Color::rgb(169, 169, 169),
        "cyan" | "aqua" => Color::rgb(0, 255, 255),
        "magenta" | "fuchsia" => Color::rgb(255, 0, 255),
        _ => return None,
    })
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1. / 2.4) - 0.055
    }
}

fn rgb_to_hsl(c: Color) -> (f32, f32, f32) {
    let (r, g, b) = (c.r as f32 / 255., c.g as f32 / 255., c.b as f32 / 255.);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.;
    if (max - min).abs() < f32::EPSILON {
        return (0., 0., l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2. - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6. } else { 0. }
    } else if max == g {
        (b - r) / d + 2.
    } else {
        (r - g) / d + 4.
    };
    (h / 6., s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32, a: u8) -> Color {
    let to = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    if s <= 0. {
        return Color {
            r: to(l),
            g: to(l),
            b: to(l),
            a,
        };
    }
    let q = if l < 0.5 { l * (1. + s) } else { l + s - l * s };
    let p = 2. * l - q;
    let hue = |mut t: f32| {
        if t < 0. {
            t += 1.;
        }
        if t > 1. {
            t -= 1.;
        }
        if t < 1. / 6. {
            p + (q - p) * 6. * t
        } else if t < 0.5 {
            q
        } else if t < 2. / 3. {
            p + (q - p) * (2. / 3. - t) * 6.
        } else {
            p
        }
    };
    Color {
        r: to(hue(h + 1. / 3.)),
        g: to(hue(h)),
        b: to(hue(h - 1. / 3.)),
        a,
    }
}

impl Env<'_> {
    pub(crate) fn scheme(&self, name: &str) -> Option<Color> {
        let mapped = self.clr_map.get(name).map(String::as_str);
        let key = mapped.unwrap_or(match name {
            "bg1" => "lt1",
            "tx1" => "dk1",
            "bg2" => "lt2",
            "tx2" => "dk2",
            other => other,
        });
        self.theme.colors.get(key).copied()
    }

    /// The colour named by a child of `node` (`a:srgbClr`, `a:schemeClr`, …),
    /// with its adjustments applied.
    pub(crate) fn color(&self, node: &Node) -> Option<Color> {
        for c in &node.children {
            let base = match c.name.as_str() {
                "srgbClr" => c.attr("val").and_then(hex_color),
                "schemeClr" => c.attr("val").and_then(|v| self.scheme(v)),
                "sysClr" => c.attr("lastClr").and_then(hex_color).or_else(|| {
                    Some(match c.attr("val")? {
                        "window" => Color::WHITE,
                        _ => Color::BLACK,
                    })
                }),
                "prstClr" => c.attr("val").and_then(preset_color),
                "scrgbClr" => {
                    let ch = |n: &str| {
                        c.num(n)
                            .map(|v| ((v / 100_000.).clamp(0., 1.) * 255.) as u8)
                    };
                    Some(Color::rgb(ch("r")?, ch("g")?, ch("b")?))
                }
                _ => continue,
            };
            let mut color = base?;
            for m in &c.children {
                let Some(v) = m.num("val").map(|v| (v / 100_000.) as f32) else {
                    continue;
                };
                match m.name.as_str() {
                    "alpha" => color.a = (v.clamp(0., 1.) * 255.).round() as u8,
                    "lumMod" | "lumOff" | "satMod" => {
                        let (h, mut s, mut l) = rgb_to_hsl(color);
                        match m.name.as_str() {
                            "lumMod" => l *= v,
                            "lumOff" => l += v,
                            _ => s = (s * v).clamp(0., 1.),
                        }
                        color = hsl_to_rgb(h, s, l, color.a);
                    }
                    // Tint mixes towards white and shade towards black, in
                    // linear light (which is why 75% tint of black is a mid grey).
                    "tint" | "shade" => {
                        let tint = m.name == "tint";
                        let mix = |ch: u8| {
                            let lin = srgb_to_linear(ch as f32 / 255.);
                            let lin = if tint { lin * v + (1. - v) } else { lin * v };
                            (linear_to_srgb(lin).clamp(0., 1.) * 255.).round() as u8
                        };
                        color = Color {
                            r: mix(color.r),
                            g: mix(color.g),
                            b: mix(color.b),
                            a: color.a,
                        };
                    }
                    _ => {}
                }
            }
            return Some(color);
        }
        None
    }

    pub(crate) fn font(&self, typeface: &str) -> Option<String> {
        let t = typeface.trim();
        if t.starts_with("+mj") {
            self.theme.major_font.clone()
        } else if t.starts_with("+mn") {
            self.theme.minor_font.clone()
        } else if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    }
}

// ------------------------------------------------------------ text styles

#[derive(Debug, Clone, Default, PartialEq)]
struct RunStyle {
    size: Option<f32>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strike: Option<bool>,
    color: Option<Color>,
    font: Option<String>,
    /// Shown in capitals whatever was typed.
    caps: Option<bool>,
}

impl RunStyle {
    fn over(&mut self, top: &RunStyle) {
        macro_rules! take {
            ($($f:ident),*) => { $( if top.$f.is_some() { self.$f = top.$f.clone(); } )* };
        }
        take!(size, bold, italic, underline, strike, color, font, caps);
    }
}

#[derive(Debug, Clone, PartialEq)]
enum BulletStyle {
    None,
    Char(String),
    AutoNum { scheme: String, start: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Spacing {
    /// A fraction of the line.
    Pct(f32),
    Pts(f32),
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ParaStyle {
    align: Option<Align>,
    mar_l: Option<f32>,
    indent: Option<f32>,
    bullet: Option<BulletStyle>,
    bullet_color: Option<Color>,
    bullet_font: Option<String>,
    /// The bullet's size as a fraction of the text's.
    bullet_size: Option<f32>,
    spc_bef: Option<Spacing>,
    spc_aft: Option<Spacing>,
    ln_spc: Option<Spacing>,
    run: RunStyle,
}

impl ParaStyle {
    fn over(&mut self, top: &ParaStyle) {
        macro_rules! take {
            ($($f:ident),*) => { $( if top.$f.is_some() { self.$f = top.$f.clone(); } )* };
        }
        take!(
            align,
            mar_l,
            indent,
            bullet,
            bullet_color,
            bullet_font,
            bullet_size,
            spc_bef,
            spc_aft,
            ln_spc
        );
        self.run.over(&top.run);
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ListStyle {
    levels: [ParaStyle; 9],
}

impl ListStyle {
    fn over(&mut self, top: &ListStyle) {
        for (mine, theirs) in self.levels.iter_mut().zip(&top.levels) {
            mine.over(theirs);
        }
    }
}

fn spacing(node: &Node) -> Option<Spacing> {
    if let Some(p) = node.child("spcPct") {
        // Either thousandths of a percent ("90000") or a percentage ("90%").
        let raw = p.attr("val")?.trim();
        let v = match raw.strip_suffix('%') {
            Some(pct) => pct.parse::<f32>().ok()? / 100.,
            None => raw.parse::<f32>().ok()? / 100_000.,
        };
        return Some(Spacing::Pct(v));
    }
    let p = node.child("spcPts")?;
    Some(Spacing::Pts(p.num("val")? as f32 / 100.))
}

impl Env<'_> {
    /// `a:rPr`, `a:defRPr` or `a:endParaRPr`.
    fn run_style(&self, node: &Node) -> RunStyle {
        RunStyle {
            size: node.num("sz").map(|v| v as f32 / 100.),
            bold: node.flag("b"),
            italic: node.flag("i"),
            underline: node.attr("u").map(|u| u != "none"),
            strike: node.attr("strike").map(|s| s != "noStrike"),
            color: node.child("solidFill").and_then(|f| self.color(f)),
            font: node
                .child("latin")
                .and_then(|l| l.attr("typeface"))
                .and_then(|t| self.font(t)),
            caps: node.attr("cap").map(|c| c != "none"),
        }
    }

    /// `a:pPr` or one of `a:lvl1pPr` … `a:lvl9pPr`.
    fn para_style(&self, node: &Node) -> ParaStyle {
        let bullet = node.children.iter().find_map(|c| match c.name.as_str() {
            "buNone" => Some(BulletStyle::None),
            "buChar" => Some(BulletStyle::Char(c.attr("char").unwrap_or("•").to_string())),
            "buBlip" => Some(BulletStyle::Char("•".to_string())),
            "buAutoNum" => Some(BulletStyle::AutoNum {
                scheme: c.attr("type").unwrap_or("arabicPeriod").to_string(),
                start: c.num("startAt").map(|v| v as u32).unwrap_or(1).max(1),
            }),
            _ => None,
        });
        ParaStyle {
            align: node.attr("algn").map(|a| match a {
                "ctr" => Align::Center,
                "r" => Align::Right,
                _ => Align::Left,
            }),
            mar_l: node.num("marL").map(pt),
            indent: node.num("indent").map(pt),
            bullet,
            bullet_color: node.child("buClr").and_then(|c| self.color(c)),
            bullet_font: node
                .child("buFont")
                .and_then(|f| f.attr("typeface"))
                .and_then(|t| self.font(t)),
            bullet_size: node
                .child("buSzPct")
                .and_then(|p| p.num("val"))
                .map(|v| (v / 100_000.) as f32),
            spc_bef: node.child("spcBef").and_then(spacing),
            spc_aft: node.child("spcAft").and_then(spacing),
            ln_spc: node.child("lnSpc").and_then(spacing),
            run: node
                .child("defRPr")
                .map(|r| self.run_style(r))
                .unwrap_or_default(),
        }
    }

    /// `a:lstStyle`, `p:titleStyle`, `p:defaultTextStyle` and the like.
    fn list_style(&self, node: Option<&Node>) -> ListStyle {
        let mut out = ListStyle::default();
        let Some(node) = node else { return out };
        for c in &node.children {
            let level = c
                .name
                .strip_prefix("lvl")
                .and_then(|r| r.strip_suffix("pPr"))
                .and_then(|n| n.parse::<usize>().ok());
            if let Some(n) = level
                && (1..=9).contains(&n)
            {
                out.levels[n - 1] = self.para_style(c);
            }
        }
        out
    }
}

fn auto_number(scheme: &str, n: u32) -> String {
    let alpha = |upper: bool| {
        let mut n = n.max(1);
        let mut s = Vec::new();
        while n > 0 {
            n -= 1;
            s.push((if upper { b'A' } else { b'a' } + (n % 26) as u8) as char);
            n /= 26;
        }
        s.iter().rev().collect::<String>()
    };
    let roman = |upper: bool| {
        let mut n = n.clamp(1, 3999);
        let mut s = String::new();
        for (v, r) in [
            (1000, "m"),
            (900, "cm"),
            (500, "d"),
            (400, "cd"),
            (100, "c"),
            (90, "xc"),
            (50, "l"),
            (40, "xl"),
            (10, "x"),
            (9, "ix"),
            (5, "v"),
            (4, "iv"),
            (1, "i"),
        ] {
            while n >= v {
                s.push_str(r);
                n -= v;
            }
        }
        if upper { s.to_uppercase() } else { s }
    };
    let body = if scheme.starts_with("alphaLc") {
        alpha(false)
    } else if scheme.starts_with("alphaUc") {
        alpha(true)
    } else if scheme.starts_with("romanLc") {
        roman(false)
    } else if scheme.starts_with("romanUc") {
        roman(true)
    } else {
        n.to_string()
    };
    if scheme.ends_with("ParenBoth") {
        format!("({body})")
    } else if scheme.ends_with("ParenR") {
        format!("{body})")
    } else if scheme.ends_with("Plain") {
        body
    } else {
        format!("{body}.")
    }
}

// ------------------------------------------------------------ placeholders

#[derive(Debug, Clone, PartialEq)]
struct Ph {
    /// The `type` attribute as written, if any.
    kind: Option<String>,
    idx: Option<String>,
}

/// The `p:nvSpPr` / `p:nvPicPr` / … child of a shape.
fn nv(shape: &Node) -> Option<&Node> {
    shape
        .children
        .iter()
        .find(|c| c.name.starts_with("nv") && c.name.ends_with("Pr"))
}

fn ph_of(shape: &Node) -> Option<Ph> {
    let ph = nv(shape)?.child("nvPr")?.child("ph")?;
    Some(Ph {
        kind: ph.attr("type").map(str::to_string),
        idx: ph.attr("idx").map(str::to_string),
    })
}

/// Placeholder types that stand for each other when looking one up.
fn ph_family(kind: Option<&str>) -> &str {
    match kind {
        Some("title" | "ctrTitle") => "title",
        Some("body" | "subTitle" | "obj") | None => "body",
        Some(other) => other,
    }
}

fn sp_tree(root: &Node) -> Option<&Node> {
    root.path(&["cSld", "spTree"])
}

fn find_placeholder<'n>(root: &'n Node, ph: &Ph, by_idx: bool) -> Option<&'n Node> {
    let tree = sp_tree(root)?;
    let all = || {
        tree.children
            .iter()
            .filter_map(|s| ph_of(s).map(|p| (s, p)))
    };
    if by_idx
        && let Some(idx) = &ph.idx
        && let Some((s, _)) = all().find(|(_, p)| p.idx.as_ref() == Some(idx))
    {
        return Some(s);
    }
    // A slide placeholder that gives only an index has no type to go by.
    if by_idx && ph.kind.is_none() && ph.idx.is_some() {
        return None;
    }
    let family = ph_family(ph.kind.as_deref());
    all()
        .find(|(_, p)| ph_family(p.kind.as_deref()) == family)
        .map(|(s, _)| s)
}

// ---------------------------------------------------------------- geometry

#[derive(Debug, Clone, Copy)]
struct Xf {
    sx: f64,
    sy: f64,
    tx: f64,
    ty: f64,
}

impl Xf {
    const IDENTITY: Xf = Xf {
        sx: 1.,
        sy: 1.,
        tx: 0.,
        ty: 0.,
    };
}

#[derive(Debug, Clone, Copy, Default)]
struct Frame {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    rot: f32,
    flip_h: bool,
    flip_v: bool,
}

fn frame_of(xfrm: &Node) -> Option<Frame> {
    let off = xfrm.child("off")?;
    let ext = xfrm.child("ext")?;
    Some(Frame {
        x: off.num("x")?,
        y: off.num("y")?,
        w: ext.num("cx")?.max(0.),
        h: ext.num("cy")?.max(0.),
        rot: xfrm.num("rot").map(|r| (r / 60_000.) as f32).unwrap_or(0.),
        flip_h: xfrm.flag("flipH").unwrap_or(false),
        flip_v: xfrm.flag("flipV").unwrap_or(false),
    })
}

fn polygon(points: &[(f32, f32)]) -> Geometry {
    let mut cmds = Vec::with_capacity(points.len() + 1);
    for (i, &(x, y)) in points.iter().enumerate() {
        cmds.push(if i == 0 {
            PathCmd::Move(x, y)
        } else {
            PathCmd::Line(x, y)
        });
    }
    cmds.push(PathCmd::Close);
    Geometry::Path(cmds)
}

/// A preset shape's outline for a box of `w` × `h` points.
fn preset_geometry(prst: &str, w: f32, h: f32, adj: Option<f32>) -> Geometry {
    let min = w.min(h);
    // An adjustment measured along the short side, as a fraction of the width
    // or height.
    let ox = |default: f32| {
        if w > 0. {
            (adj.unwrap_or(default) * min / w).clamp(0., 1.)
        } else {
            0.
        }
    };
    let oy = |default: f32| {
        if h > 0. {
            (adj.unwrap_or(default) * min / h).clamp(0., 1.)
        } else {
            0.
        }
    };
    match prst {
        "roundRect"
        | "flowChartAlternateProcess"
        | "round1Rect"
        | "round2SameRect"
        | "round2DiagRect"
        | "snipRoundRect"
        | "wedgeRoundRectCallout" => {
            Geometry::RoundRect(min * adj.unwrap_or(0.16667).clamp(0., 0.5))
        }
        "flowChartTerminator" => Geometry::RoundRect(min / 2.),
        "ellipse"
        | "flowChartConnector"
        | "donut"
        | "pie"
        | "chord"
        | "wedgeEllipseCallout"
        | "cloud"
        | "cloudCallout" => Geometry::Ellipse,
        "line" | "straightConnector1" => {
            Geometry::Path(vec![PathCmd::Move(0., 0.), PathCmd::Line(1., 1.)])
        }
        "bentConnector2" => Geometry::Path(vec![
            PathCmd::Move(0., 0.),
            PathCmd::Line(1., 0.),
            PathCmd::Line(1., 1.),
        ]),
        "bentConnector3" | "bentConnector4" | "bentConnector5" => {
            let a = adj.unwrap_or(0.5);
            Geometry::Path(vec![
                PathCmd::Move(0., 0.),
                PathCmd::Line(a, 0.),
                PathCmd::Line(a, 1.),
                PathCmd::Line(1., 1.),
            ])
        }
        "curvedConnector2" | "curvedConnector3" | "curvedConnector4" | "curvedConnector5" => {
            Geometry::Path(vec![
                PathCmd::Move(0., 0.),
                PathCmd::Cubic(0.5, 0., 0.5, 1., 1., 1.),
            ])
        }
        "triangle" | "flowChartExtract" => {
            let a = adj.unwrap_or(0.5).clamp(0., 1.);
            polygon(&[(a, 0.), (1., 1.), (0., 1.)])
        }
        "flowChartMerge" => polygon(&[(0., 0.), (1., 0.), (0.5, 1.)]),
        "rtTriangle" => polygon(&[(0., 0.), (1., 1.), (0., 1.)]),
        "diamond" | "flowChartDecision" => polygon(&[(0.5, 0.), (1., 0.5), (0.5, 1.), (0., 0.5)]),
        "parallelogram" | "flowChartInputOutput" => {
            let o = ox(0.25);
            polygon(&[(o, 0.), (1., 0.), (1. - o, 1.), (0., 1.)])
        }
        "trapezoid" => {
            let o = ox(0.25);
            polygon(&[(o, 0.), (1. - o, 0.), (1., 1.), (0., 1.)])
        }
        "pentagon" => polygon(&[
            (0.5, 0.),
            (1., 0.382),
            (0.809, 1.),
            (0.191, 1.),
            (0., 0.382),
        ]),
        "hexagon" | "flowChartPreparation" => {
            let o = ox(0.25);
            polygon(&[
                (o, 0.),
                (1. - o, 0.),
                (1., 0.5),
                (1. - o, 1.),
                (o, 1.),
                (0., 0.5),
            ])
        }
        "octagon" => {
            let (x, y) = (ox(0.29289), oy(0.29289));
            polygon(&[
                (x, 0.),
                (1. - x, 0.),
                (1., y),
                (1., 1. - y),
                (1. - x, 1.),
                (x, 1.),
                (0., 1. - y),
                (0., y),
            ])
        }
        "chevron" => {
            let o = ox(0.5);
            polygon(&[
                (0., 0.),
                (1. - o, 0.),
                (1., 0.5),
                (1. - o, 1.),
                (0., 1.),
                (o, 0.5),
            ])
        }
        "homePlate" | "flowChartOffpageConnector" => {
            let o = ox(0.5);
            polygon(&[(0., 0.), (1. - o, 0.), (1., 0.5), (1. - o, 1.), (0., 1.)])
        }
        "rightArrow" => {
            let o = ox(0.5);
            polygon(&[
                (0., 0.25),
                (1. - o, 0.25),
                (1. - o, 0.),
                (1., 0.5),
                (1. - o, 1.),
                (1. - o, 0.75),
                (0., 0.75),
            ])
        }
        "leftArrow" => {
            let o = ox(0.5);
            polygon(&[
                (1., 0.25),
                (o, 0.25),
                (o, 0.),
                (0., 0.5),
                (o, 1.),
                (o, 0.75),
                (1., 0.75),
            ])
        }
        "downArrow" => {
            let o = oy(0.5);
            polygon(&[
                (0.25, 0.),
                (0.25, 1. - o),
                (0., 1. - o),
                (0.5, 1.),
                (1., 1. - o),
                (0.75, 1. - o),
                (0.75, 0.),
            ])
        }
        "upArrow" => {
            let o = oy(0.5);
            polygon(&[
                (0.25, 1.),
                (0.25, o),
                (0., o),
                (0.5, 0.),
                (1., o),
                (0.75, o),
                (0.75, 1.),
            ])
        }
        "plus" | "mathPlus" => {
            let (x, y) = (ox(0.25), oy(0.25));
            polygon(&[
                (x, 0.),
                (1. - x, 0.),
                (1. - x, y),
                (1., y),
                (1., 1. - y),
                (1. - x, 1. - y),
                (1. - x, 1.),
                (x, 1.),
                (x, 1. - y),
                (0., 1. - y),
                (0., y),
                (x, y),
            ])
        }
        "star5" => {
            let mut pts = Vec::with_capacity(10);
            for i in 0..10 {
                let r = if i % 2 == 0 { 0.5 } else { 0.19 };
                let a = std::f32::consts::PI * (i as f32 / 5. - 0.5);
                pts.push((0.5 + r * a.cos(), 0.5 + r * a.sin() + 0.02));
            }
            polygon(&pts)
        }
        // Open outlines this crate has no path for: better nothing than a box.
        "arc" | "leftBracket" | "rightBracket" | "leftBrace" | "rightBrace" | "bracketPair"
        | "bracePair" => Geometry::Path(Vec::new()),
        _ => Geometry::Rect,
    }
}

/// `a:custGeom`: its paths, each scaled to the unit box.
fn custom_geometry(node: &Node, frame: &Frame) -> Geometry {
    let mut cmds = Vec::new();
    let Some(list) = node.child("pathLst") else {
        return Geometry::Rect;
    };
    for path in list.children_named("path") {
        let pw = path.num("w").filter(|v| *v > 0.).unwrap_or(frame.w.max(1.));
        let ph = path.num("h").filter(|v| *v > 0.).unwrap_or(frame.h.max(1.));
        let point = |n: &Node| -> (f64, f64) {
            (n.num("x").unwrap_or(0.) / pw, n.num("y").unwrap_or(0.) / ph)
        };
        let mut cur = (0f64, 0f64);
        for c in &path.children {
            let pts: Vec<(f64, f64)> = c.children_named("pt").map(point).collect();
            match (c.name.as_str(), pts.as_slice()) {
                ("moveTo", [p, ..]) => {
                    cmds.push(PathCmd::Move(p.0 as f32, p.1 as f32));
                    cur = *p;
                }
                ("lnTo", [p, ..]) => {
                    cmds.push(PathCmd::Line(p.0 as f32, p.1 as f32));
                    cur = *p;
                }
                ("cubicBezTo", [a, b, p, ..]) => {
                    cmds.push(PathCmd::Cubic(
                        a.0 as f32, a.1 as f32, b.0 as f32, b.1 as f32, p.0 as f32, p.1 as f32,
                    ));
                    cur = *p;
                }
                ("quadBezTo", [q, p, ..]) => {
                    let a = (
                        cur.0 + 2. / 3. * (q.0 - cur.0),
                        cur.1 + 2. / 3. * (q.1 - cur.1),
                    );
                    let b = (p.0 + 2. / 3. * (q.0 - p.0), p.1 + 2. / 3. * (q.1 - p.1));
                    cmds.push(PathCmd::Cubic(
                        a.0 as f32, a.1 as f32, b.0 as f32, b.1 as f32, p.0 as f32, p.1 as f32,
                    ));
                    cur = *p;
                }
                ("arcTo", _) => {
                    let (Some(wr), Some(hr), Some(st), Some(sw)) =
                        (c.num("wR"), c.num("hR"), c.num("stAng"), c.num("swAng"))
                    else {
                        continue;
                    };
                    cur = arc(&mut cmds, cur, (wr / pw, hr / ph), st, sw);
                }
                ("close", _) => cmds.push(PathCmd::Close),
                _ => {}
            }
        }
        if cmds.len() > 100_000 {
            break;
        }
    }
    Geometry::Path(cmds)
}

/// An elliptical arc from `cur` as cubic segments; angles are DrawingML's
/// (60000ths of a degree, clockwise). Returns the end point.
fn arc(
    cmds: &mut Vec<PathCmd>,
    cur: (f64, f64),
    (rx, ry): (f64, f64),
    start: f64,
    sweep: f64,
) -> (f64, f64) {
    let rad = |a: f64| (a / 60_000.).to_radians();
    // DrawingML gives the angle of the ray from the centre; on an ellipse the
    // matching parameter differs unless the radii are equal.
    let param = |a: f64| (rx * a.sin()).atan2(ry * a.cos());
    let t0 = param(rad(start));
    let total = rad(sweep);
    if rx <= 0. || ry <= 0. || total == 0. {
        return cur;
    }
    let (cx, cy) = (cur.0 - rx * t0.cos(), cur.1 - ry * t0.sin());
    let steps = (total.abs() / std::f64::consts::FRAC_PI_2)
        .ceil()
        .clamp(1., 8.) as usize;
    let dt = total / steps as f64;
    let k = 4. / 3. * (dt / 4.).tan();
    let mut t = t0;
    let mut end = cur;
    for _ in 0..steps {
        let (s0, c0) = t.sin_cos();
        let (s1, c1) = (t + dt).sin_cos();
        let p1 = (cx + rx * (c0 - k * s0), cy + ry * (s0 + k * c0));
        let p2 = (cx + rx * (c1 + k * s1), cy + ry * (s1 - k * c1));
        end = (cx + rx * c1, cy + ry * s1);
        cmds.push(PathCmd::Cubic(
            p1.0 as f32,
            p1.1 as f32,
            p2.0 as f32,
            p2.1 as f32,
            end.0 as f32,
            end.1 as f32,
        ));
        t += dt;
    }
    end
}

fn flip(geometry: Geometry, h: bool, v: bool) -> Geometry {
    let Geometry::Path(mut cmds) = geometry else {
        return geometry;
    };
    if h || v {
        let fx = |x: f32| if h { 1. - x } else { x };
        let fy = |y: f32| if v { 1. - y } else { y };
        for c in &mut cmds {
            *c = match *c {
                PathCmd::Move(x, y) => PathCmd::Move(fx(x), fy(y)),
                PathCmd::Line(x, y) => PathCmd::Line(fx(x), fy(y)),
                PathCmd::Cubic(a, b, c2, d, x, y) => {
                    PathCmd::Cubic(fx(a), fy(b), fx(c2), fy(d), fx(x), fy(y))
                }
                PathCmd::Close => PathCmd::Close,
            };
        }
    }
    Geometry::Path(cmds)
}

// ----------------------------------------------------------------- builder

pub(crate) struct SceneBuilder {
    parts: HashMap<String, Option<Rc<Part>>>,
    masters: HashMap<String, Option<Rc<Master>>>,
    themes: HashMap<String, Rc<Theme>>,
    default_text: Option<Node>,
    pub images: ImageSet,
    /// Slide size in points.
    pub size: (f32, f32),
}

impl SceneBuilder {
    /// `pres` is the text of `presentation.xml`.
    pub fn new(pres: &str) -> Self {
        let root = Node::parse(pres).ok();
        let size = root
            .as_ref()
            .and_then(|r| r.child("sldSz"))
            .and_then(|s| Some((pt(s.num("cx")?), pt(s.num("cy")?))))
            .filter(|(w, h)| *w >= 1. && *h >= 1.)
            .unwrap_or((960., 540.));
        Self {
            parts: HashMap::new(),
            masters: HashMap::new(),
            themes: HashMap::new(),
            default_text: root.and_then(|r| r.child("defaultTextStyle").cloned()),
            images: ImageSet::default(),
            size,
        }
    }

    fn part(&mut self, pkg: &mut Package<'_>, name: &str) -> Option<Rc<Part>> {
        if let Some(p) = self.parts.get(name) {
            return p.clone();
        }
        let loaded = (|| {
            let xml = pkg.read_xml_opt(name).ok()??;
            let root = Node::parse(&xml).ok()?;
            let rels = pkg.rels_for(name).ok()?;
            Some(Rc::new(Part { root, rels }))
        })();
        self.parts.insert(name.to_string(), loaded.clone());
        loaded
    }

    fn theme(&mut self, pkg: &mut Package<'_>, name: &str) -> Rc<Theme> {
        if let Some(t) = self.themes.get(name) {
            return t.clone();
        }
        let mut theme = Theme::default();
        if let Some(root) = pkg
            .read_xml_opt(name)
            .ok()
            .flatten()
            .and_then(|xml| Node::parse(&xml).ok())
            && let Some(el) = root.child("themeElements")
        {
            if let Some(scheme) = el.child("clrScheme") {
                for c in &scheme.children {
                    let color = c.children.iter().find_map(|v| match v.name.as_str() {
                        "srgbClr" => v.attr("val").and_then(hex_color),
                        "sysClr" => v.attr("lastClr").and_then(hex_color).or_else(|| {
                            Some(match v.attr("val")? {
                                "window" => Color::WHITE,
                                _ => Color::BLACK,
                            })
                        }),
                        _ => None,
                    });
                    if let Some(color) = color {
                        theme.colors.insert(c.name.clone(), color);
                    }
                }
            }
            let face = |which: &str| {
                el.path(&["fontScheme", which, "latin"])
                    .and_then(|l| l.attr("typeface"))
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
            };
            theme.major_font = face("majorFont");
            theme.minor_font = face("minorFont");
            if let Some(lines) = el.path(&["fmtScheme", "lnStyleLst"]) {
                theme.line_widths = lines
                    .children_named("ln")
                    .map(|l| l.num("w").map(pt).unwrap_or(0.75))
                    .collect();
            }
            if let Some(effects) = el.path(&["fmtScheme", "effectStyleLst"]) {
                theme.effects = effects.children_named("effectStyle").cloned().collect();
            }
        }
        let theme = Rc::new(theme);
        self.themes.insert(name.to_string(), theme.clone());
        theme
    }

    fn master(&mut self, pkg: &mut Package<'_>, name: &str) -> Option<Rc<Master>> {
        if let Some(m) = self.masters.get(name) {
            return m.clone();
        }
        let loaded = (|| {
            let part = self.part(pkg, name)?;
            let theme = match part.rels.first_of_type("/theme") {
                Some(rel) if !rel.external => self.theme(pkg, &rel.target.clone()),
                _ => Rc::new(Theme::default()),
            };
            let mut clr_map = HashMap::new();
            if let Some(map) = part.root.child("clrMap") {
                for key in [
                    "bg1", "tx1", "bg2", "tx2", "accent1", "accent2", "accent3", "accent4",
                    "accent5", "accent6", "hlink", "folHlink",
                ] {
                    if let Some(v) = map.attr(key) {
                        clr_map.insert(key.to_string(), v.to_string());
                    }
                }
            }
            let env = Env {
                theme: &theme,
                clr_map: &clr_map,
                rels: &part.rels,
                slide_no: 0,
            };
            let styles = part.root.child("txStyles");
            let style = |name: &str| env.list_style(styles.and_then(|s| s.child(name)));
            let master = Master {
                default_text: env.list_style(self.default_text.as_ref()),
                title: style("titleStyle"),
                body: style("bodyStyle"),
                other: style("otherStyle"),
                part: part.clone(),
                theme: theme.clone(),
                clr_map,
            };
            Some(Rc::new(master))
        })();
        self.masters.insert(name.to_string(), loaded.clone());
        loaded
    }

    /// Lays out the slide stored in `part`; `None` when its layout or master
    /// cannot be read.
    pub fn build(&mut self, pkg: &mut Package<'_>, part: &str, slide_no: usize) -> Option<Scene> {
        let slide = self.part(pkg, part)?;
        let layout_name = slide
            .rels
            .first_of_type("/slideLayout")
            .filter(|r| !r.external)?
            .target
            .clone();
        let layout = self.part(pkg, &layout_name)?;
        let master_name = layout
            .rels
            .first_of_type("/slideMaster")
            .filter(|r| !r.external)?
            .target
            .clone();
        let master = self.master(pkg, &master_name)?;

        // A layout or slide may replace the master's colour map.
        let override_map = |root: &Node| {
            let o = root.path(&["clrMapOvr", "overrideClrMapping"])?;
            let mut map = master.clr_map.clone();
            for key in ["bg1", "tx1", "bg2", "tx2"] {
                if let Some(v) = o.attr(key) {
                    map.insert(key.to_string(), v.to_string());
                }
            }
            Some(map)
        };
        let layout_map = override_map(&layout.root).unwrap_or_else(|| master.clr_map.clone());
        let slide_map = override_map(&slide.root).unwrap_or_else(|| layout_map.clone());

        let mut scene = Scene::default();

        // Background: the slide's own, else its layout's, else its master's.
        let sources: [(&Rc<Part>, &HashMap<String, String>); 3] = [
            (&slide, &slide_map),
            (&layout, &layout_map),
            (&master.part, &master.clr_map),
        ];
        scene.background = Fill::Solid(Color::WHITE);
        for (part, map) in sources {
            let env = Env {
                theme: &master.theme,
                clr_map: map,
                rels: &part.rels,
                slide_no,
            };
            if let Some(bg) = part.root.path(&["cSld", "bg"])
                && let Some(fill) = self.background(pkg, bg, &env)
            {
                scene.background = fill;
                break;
            }
        }

        let show = |root: &Node| root.flag("showMasterSp").unwrap_or(true);
        let chain = Chain {
            layout: &layout,
            master: &master,
        };
        if show(&slide.root) {
            if show(&layout.root) {
                let env = Env {
                    theme: &master.theme,
                    clr_map: &master.clr_map,
                    rels: &master.part.rels,
                    slide_no,
                };
                self.tree(
                    pkg,
                    &master.part.root,
                    &env,
                    &chain,
                    Source::Inherited,
                    &mut scene,
                );
            }
            let env = Env {
                theme: &master.theme,
                clr_map: &layout_map,
                rels: &layout.rels,
                slide_no,
            };
            self.tree(
                pkg,
                &layout.root,
                &env,
                &chain,
                Source::Inherited,
                &mut scene,
            );
        }
        let env = Env {
            theme: &master.theme,
            clr_map: &slide_map,
            rels: &slide.rels,
            slide_no,
        };
        self.tree(pkg, &slide.root, &env, &chain, Source::Slide, &mut scene);
        Some(scene)
    }

    fn background(&mut self, pkg: &mut Package<'_>, bg: &Node, env: &Env<'_>) -> Option<Fill> {
        if let Some(pr) = bg.child("bgPr") {
            return self.fill(pkg, pr, env);
        }
        let r = bg.child("bgRef")?;
        env.color(r).map(Fill::Solid)
    }

    /// The fill a properties element (`p:spPr`, `p:bgPr`, `a:tcPr`) names;
    /// `None` when it names none, which is not the same as "no fill".
    fn fill(&mut self, pkg: &mut Package<'_>, node: &Node, env: &Env<'_>) -> Option<Fill> {
        for c in &node.children {
            match c.name.as_str() {
                "noFill" => return Some(Fill::None),
                "solidFill" => return env.color(c).map(Fill::Solid),
                "pattFill" => {
                    return c.child("fgClr").and_then(|f| env.color(f)).map(Fill::Solid);
                }
                "gradFill" => {
                    let mut stops: Vec<(f64, Color)> = c
                        .child("gsLst")
                        .into_iter()
                        .flat_map(|l| l.children_named("gs"))
                        .filter_map(|gs| Some((gs.num("pos").unwrap_or(0.), env.color(gs)?)))
                        .collect();
                    stops.sort_by(|a, b| a.0.total_cmp(&b.0));
                    let (first, last) = (stops.first()?, stops.last()?);
                    if stops.iter().all(|s| s.1 == first.1) {
                        return Some(Fill::Solid(first.1));
                    }
                    let angle = c
                        .child("lin")
                        .and_then(|l| l.num("ang"))
                        .map(|a| (a / 60_000.) as f32)
                        .unwrap_or(90.);
                    let span = (last.0 - first.0).max(1.);
                    let mid = stops[1..stops.len() - 1]
                        .iter()
                        .take(16)
                        .map(|(pos, color)| (((pos - first.0) / span) as f32, *color))
                        .collect();
                    return Some(Fill::Gradient {
                        angle,
                        from: first.1,
                        to: last.1,
                        mid,
                        radial: c.has("path"),
                    });
                }
                "blipFill" => {
                    let name = self.blip(pkg, c, env)?;
                    return Some(Fill::Image(name));
                }
                _ => {}
            }
        }
        None
    }

    /// The image a `p:blipFill` / `a:blipFill` embeds.
    fn blip(&mut self, pkg: &mut Package<'_>, blip_fill: &Node, env: &Env<'_>) -> Option<String> {
        let id = blip_fill.child("blip")?.attr_prefixed("embed")?;
        let rel = env.rels.get(id).filter(|r| !r.external)?;
        self.images.add(pkg, &rel.target)
    }

    fn tree(
        &mut self,
        pkg: &mut Package<'_>,
        root: &Node,
        env: &Env<'_>,
        chain: &Chain<'_>,
        source: Source,
        scene: &mut Scene,
    ) {
        if let Some(tree) = sp_tree(root) {
            self.group(pkg, tree, Xf::IDENTITY, env, chain, source, 0, scene);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn group(
        &mut self,
        pkg: &mut Package<'_>,
        group: &Node,
        xf: Xf,
        env: &Env<'_>,
        chain: &Chain<'_>,
        source: Source,
        depth: usize,
        scene: &mut Scene,
    ) {
        for node in &group.children {
            if scene.shapes.len() >= MAX_SHAPES {
                return;
            }
            let hidden = nv(node)
                .and_then(|n| n.child("cNvPr"))
                .and_then(|c| c.flag("hidden"))
                .unwrap_or(false);
            if hidden {
                continue;
            }
            // A layout's and a master's placeholders are only patterns.
            if source == Source::Inherited && ph_of(node).is_some() {
                continue;
            }
            match node.name.as_str() {
                "sp" | "cxnSp" => {
                    if let Some(shape) = self.shape(pkg, node, xf, env, chain, source) {
                        scene.shapes.push(shape);
                    }
                }
                "pic" => {
                    if let Some(shape) = self.picture(pkg, node, xf, env, chain, source) {
                        scene.shapes.push(shape);
                    }
                }
                "graphicFrame" => {
                    if let Some(shape) = self.graphic_frame(pkg, node, xf, env, chain, source) {
                        scene.shapes.push(shape);
                    }
                }
                "grpSp" if depth < MAX_GROUP_DEPTH => {
                    let Some(x) = node.path(&["grpSpPr", "xfrm"]) else {
                        self.group(pkg, node, xf, env, chain, source, depth + 1, scene);
                        continue;
                    };
                    let num = |el: &str, a: &str| x.child(el).and_then(|n| n.num(a));
                    let (ox, oy) = (num("off", "x").unwrap_or(0.), num("off", "y").unwrap_or(0.));
                    let (ew, eh) = (
                        num("ext", "cx").unwrap_or(0.),
                        num("ext", "cy").unwrap_or(0.),
                    );
                    let (cx, cy) = (
                        num("chOff", "x").unwrap_or(ox),
                        num("chOff", "y").unwrap_or(oy),
                    );
                    let (cw, ch) = (
                        num("chExt", "cx").unwrap_or(ew),
                        num("chExt", "cy").unwrap_or(eh),
                    );
                    let kx = if cw > 0. { ew / cw } else { 1. };
                    let ky = if ch > 0. { eh / ch } else { 1. };
                    // child → group → outer
                    let inner = Xf {
                        sx: xf.sx * kx,
                        sy: xf.sy * ky,
                        tx: xf.tx + xf.sx * (ox - cx * kx),
                        ty: xf.ty + xf.sy * (oy - cy * ky),
                    };
                    self.group(pkg, node, inner, env, chain, source, depth + 1, scene);
                }
                _ => {}
            }
        }
    }

    /// The placeholders a slide's shape inherits from: layout first, then master.
    fn ancestors<'c>(
        &self,
        node: &Node,
        chain: &Chain<'c>,
        source: Source,
    ) -> (Option<Ph>, Vec<&'c Node>) {
        let Some(ph) = ph_of(node) else {
            return (None, Vec::new());
        };
        let mut out = Vec::new();
        if source != Source::Slide {
            return (Some(ph), out);
        }
        let on_layout = find_placeholder(&chain.layout.root, &ph, true);
        // The layout's placeholder says what kind this one is when the slide
        // only gives an index.
        let effective = match (&ph.kind, on_layout.and_then(ph_of)) {
            (None, Some(l)) => Ph {
                kind: l.kind,
                idx: ph.idx.clone(),
            },
            _ => ph.clone(),
        };
        out.extend(on_layout);
        out.extend(find_placeholder(&chain.master.part.root, &effective, false));
        (Some(effective), out)
    }

    fn base(&self, node: &Node, xf: Xf, inherited: &[&Node]) -> Option<(Shape, Frame)> {
        let frame = std::iter::once(node)
            .chain(inherited.iter().copied())
            .find_map(|n| {
                n.child("spPr")
                    .and_then(|p| p.child("xfrm"))
                    .or_else(|| n.child("xfrm"))
                    .and_then(frame_of)
            })?;
        let c = nv(node).and_then(|n| n.child("cNvPr"));
        let shape = Shape {
            name: c
                .and_then(|c| c.attr("name"))
                .unwrap_or_default()
                .to_string(),
            description: c
                .and_then(|c| c.attr("descr").or_else(|| c.attr("title")))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            x: pt(xf.tx + xf.sx * frame.x),
            y: pt(xf.ty + xf.sy * frame.y),
            w: pt(xf.sx * frame.w),
            h: pt(xf.sy * frame.h),
            rotation: frame.rot,
            geometry: Geometry::Rect,
            fill: Fill::None,
            stroke: None,
            shadow: None,
            kind: ShapeKind::Shape,
            text: None,
        };
        Some((shape, frame))
    }

    fn geometry(&self, props: &[&Node], shape: &Shape, frame: &Frame) -> Geometry {
        for p in props {
            if let Some(g) = p.child("prstGeom") {
                let adj = g
                    .child("avLst")
                    .and_then(|l| l.child("gd"))
                    .and_then(|gd| gd.attr("fmla"))
                    .and_then(|f| f.trim().strip_prefix("val "))
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .map(|v| v / 100_000.);
                let prst = g.attr("prst").unwrap_or("rect");
                return flip(
                    preset_geometry(prst, shape.w, shape.h, adj),
                    frame.flip_h,
                    frame.flip_v,
                );
            }
            if let Some(g) = p.child("custGeom") {
                return flip(custom_geometry(g, frame), frame.flip_h, frame.flip_v);
            }
        }
        Geometry::Rect
    }

    /// The outline: the first `a:ln` that says how it is painted decides, the
    /// shape's style reference is the fallback.
    fn stroke(&self, props: &[&Node], style: Option<&Node>, env: &Env<'_>) -> Option<Stroke> {
        let lines: Vec<&Node> = props.iter().filter_map(|p| p.child("ln")).collect();
        let mut color = None;
        for ln in &lines {
            if ln.has("noFill") {
                return None;
            }
            if let Some(f) = ln.child("solidFill") {
                color = env.color(f);
                break;
            }
            if let Some(f) = ln.child("gradFill") {
                color = f
                    .child("gsLst")
                    .and_then(|l| l.child("gs"))
                    .and_then(|gs| env.color(gs));
                break;
            }
            if let Some(f) = ln.child("pattFill") {
                color = f.child("fgClr").and_then(|c| env.color(c));
                break;
            }
        }
        let style_ref = style
            .and_then(|s| s.child("lnRef"))
            .filter(|r| r.num("idx").unwrap_or(0.) >= 1.);
        let color = color.or_else(|| style_ref.and_then(|r| env.color(r)))?;
        let width = lines
            .iter()
            .find_map(|l| l.num("w"))
            .map(pt)
            .or_else(|| {
                let ix = style_ref?.num("idx")? as usize;
                Some(
                    *env.theme
                        .line_widths
                        .get(ix.checked_sub(1)?)
                        .unwrap_or(&0.75),
                )
            })
            .unwrap_or(0.75);
        let dash = lines
            .iter()
            .find_map(|l| l.child("prstDash"))
            .and_then(|d| d.attr("val"))
            .map(|v| match v {
                "solid" => Dash::Solid,
                "dot" | "sysDot" => Dash::Dot,
                _ => Dash::Dash,
            })
            .unwrap_or_default();
        let arrow = |name: &str| {
            lines
                .iter()
                .find_map(|l| l.child(name))
                .and_then(|e| e.attr("type"))
                .is_some_and(|t| t != "none")
        };
        Some(Stroke {
            color,
            width,
            dash,
            start_arrow: arrow("headEnd"),
            end_arrow: arrow("tailEnd"),
        })
    }

    /// The shadow of a shape's effects: one cast behind it, a glow (a shadow
    /// all round), or one cast inside it.
    ///
    /// A shape that lists no effects of its own takes the theme's effect
    /// style its `style` names (the soft shadow under shapes drawn in the
    /// older Office themes), in the colour the reference carries.
    fn shadow(&self, props: Option<&Node>, style: Option<&Node>, env: &Env<'_>) -> Option<Shadow> {
        let own = props.and_then(|p| p.child("effectLst"));
        let effects = match own {
            Some(own) => own,
            None => {
                let r = style?.child("effectRef")?;
                let ix = r.num("idx").filter(|ix| *ix >= 1.)? as usize;
                env.theme.effects.get(ix - 1)?.child("effectLst")?
            }
        };
        if let Some(glow) = effects.child("glow") {
            return Some(Shadow {
                color: env.color(glow)?,
                blur: glow.num("rad").map(pt).unwrap_or(0.),
                dx: 0.,
                dy: 0.,
                inset: false,
            });
        }
        let (s, inset) = match effects.child("outerShdw") {
            Some(s) => (s, false),
            None => (effects.child("innerShdw")?, true),
        };
        let color = env.color(s)?;
        let dist = s.num("dist").map(pt).unwrap_or(0.);
        let dir = (s.num("dir").unwrap_or(0.) / 60_000.).to_radians() as f32;
        Some(Shadow {
            color,
            blur: s.num("blurRad").map(pt).unwrap_or(0.),
            dx: dist * dir.cos(),
            dy: dist * dir.sin(),
            inset,
        })
    }

    fn shape(
        &mut self,
        pkg: &mut Package<'_>,
        node: &Node,
        xf: Xf,
        env: &Env<'_>,
        chain: &Chain<'_>,
        source: Source,
    ) -> Option<Shape> {
        let (ph, inherited) = self.ancestors(node, chain, source);
        let (mut shape, frame) = self.base(node, xf, &inherited)?;
        // Own properties first, then the layout's and the master's.
        let props: Vec<&Node> = std::iter::once(node)
            .chain(inherited.iter().copied())
            .filter_map(|n| n.child("spPr"))
            .collect();
        let style = node.child("style");

        shape.geometry = self.geometry(&props, &shape, &frame);
        let mut fill = None;
        for (i, p) in props.iter().enumerate() {
            // A placeholder's inherited picture fill lives in another part's
            // relationships; only the slide's own can be followed here.
            if i > 0 && p.has("blipFill") {
                continue;
            }
            fill = self.fill(pkg, p, env);
            if fill.is_some() {
                break;
            }
        }
        shape.fill = fill
            .or_else(|| {
                let r = style?.child("fillRef")?;
                (r.num("idx").unwrap_or(0.) >= 1.)
                    .then(|| env.color(r))
                    .flatten()
                    .map(Fill::Solid)
            })
            .unwrap_or_default();
        shape.stroke = self.stroke(&props, style, env);
        // Nothing drawn casts nothing: a bare frame of text has no shadow.
        if !matches!(shape.fill, Fill::None) || shape.stroke.is_some() {
            shape.shadow = self.shadow(props.first().copied(), style, env);
        }

        if let Some(tx) = node.child("txBody") {
            let mut styles = chain.master.default_text.clone();
            let class = match ph.as_ref().map(|p| p.kind.as_deref()) {
                Some(Some("title" | "ctrTitle")) => &chain.master.title,
                Some(Some("dt" | "ftr" | "sldNum" | "hdr")) | None => &chain.master.other,
                Some(_) => &chain.master.body,
            };
            styles.over(class);
            // Master's placeholder, then the layout's, then the shape's own.
            for n in inherited.iter().rev() {
                // Their colours were written against the same theme.
                styles.over(&env.list_style(n.path(&["txBody", "lstStyle"])));
            }
            if let Some(c) = style
                .and_then(|s| s.child("fontRef"))
                .and_then(|r| env.color(r))
            {
                for level in &mut styles.levels {
                    level.run.color = Some(c);
                }
            }
            styles.over(&env.list_style(tx.child("lstStyle")));
            let body_props: Vec<&Node> = std::iter::once(node)
                .chain(inherited.iter().copied())
                .filter_map(|n| n.path(&["txBody", "bodyPr"]))
                .collect();
            shape.text = self.text_body(tx, &body_props, &styles, env);
            // Where its outline curves in, a preset keeps its words inside
            // the curve: the box they are set in is smaller than the shape's.
            if let Some(body) = &mut shape.text {
                let (dx, dy) = match &shape.geometry {
                    Geometry::RoundRect(r) => (r * 0.29289, r * 0.29289),
                    Geometry::Ellipse => (shape.w * 0.14645, shape.h * 0.14645),
                    _ => (0., 0.),
                };
                body.insets[0] += dx;
                body.insets[1] += dy;
                body.insets[2] += dx;
                body.insets[3] += dy;
            }
        }

        let drawn = shape.fill != Fill::None || shape.stroke.is_some() || shape.text.is_some();
        drawn.then_some(shape)
    }

    fn picture(
        &mut self,
        pkg: &mut Package<'_>,
        node: &Node,
        xf: Xf,
        env: &Env<'_>,
        chain: &Chain<'_>,
        source: Source,
    ) -> Option<Shape> {
        let (_, inherited) = self.ancestors(node, chain, source);
        let (mut shape, frame) = self.base(node, xf, &inherited)?;
        let blip_fill = node.child("blipFill")?;
        let image = self.blip(pkg, blip_fill, env)?;
        let crop = blip_fill
            .child("srcRect")
            .map(|r| {
                let side = |n: &str| (r.num(n).unwrap_or(0.) / 100_000.).clamp(0., 0.95) as f32;
                [side("l"), side("t"), side("r"), side("b")]
            })
            .unwrap_or_default();
        let props: Vec<&Node> = node.child("spPr").into_iter().collect();
        shape.geometry = self.geometry(&props, &shape, &frame);
        shape.stroke = self.stroke(&props, node.child("style"), env);
        shape.shadow = self.shadow(props.first().copied(), node.child("style"), env);
        shape.kind = ShapeKind::Picture(Picture { image, crop });
        Some(shape)
    }

    fn graphic_frame(
        &mut self,
        pkg: &mut Package<'_>,
        node: &Node,
        xf: Xf,
        env: &Env<'_>,
        chain: &Chain<'_>,
        source: Source,
    ) -> Option<Shape> {
        let (_, inherited) = self.ancestors(node, chain, source);
        let (mut shape, _) = self.base(node, xf, &inherited)?;
        let data = node.path(&["graphic", "graphicData"])?;
        if let Some(tbl) = data.child("tbl") {
            shape.kind = ShapeKind::Table(self.table(pkg, tbl, &shape, env, chain));
            return Some(shape);
        }
        // An embedded object usually carries a picture of itself.
        if let Some(pic) = data.child("oleObj").and_then(|o| o.child("pic"))
            && let Some(mut inner) = self.picture(pkg, pic, xf, env, chain, source)
        {
            (inner.x, inner.y, inner.w, inner.h) = (shape.x, shape.y, shape.w, shape.h);
            return Some(inner);
        }
        let uri = data.attr("uri").unwrap_or_default();
        // A chart: worked out into shapes from the numbers its part caches.
        if uri.ends_with("/chart")
            && let Some(rel) = data
                .child("chart")
                .and_then(|c| c.attr_prefixed("id"))
                .and_then(|id| env.rels.get(id))
                .filter(|r| !r.external)
            && let Ok(Some(xml)) = pkg.read_xml_opt(&rel.target)
            && let Ok(root) = Node::parse(&xml)
            && let Some((shapes, title)) =
                crate::chart::build(&root, (shape.x, shape.y, shape.w, shape.h), env)
        {
            shape.kind = ShapeKind::Drawing(Drawing {
                what: "chart".to_string(),
                label: title,
                shapes,
            });
            return Some(shape);
        }
        // A diagram (SmartArt): the application saves it drawn, as ordinary
        // shapes in a part of their own, placed from the frame's corner.
        if uri.ends_with("/diagram")
            && let Some(drawing) = self.diagram(pkg, node, data, xf, env, chain)
        {
            shape.kind = ShapeKind::Drawing(drawing);
            return Some(shape);
        }
        let what = if uri.ends_with("/chart") {
            "Chart"
        } else if uri.ends_with("/diagram") {
            "Diagram"
        } else {
            "Object"
        };
        shape.kind = ShapeKind::Unsupported(what.to_string());
        Some(shape)
    }

    /// The saved drawing of the diagram in `frame`, as shapes on the slide.
    fn diagram(
        &mut self,
        pkg: &mut Package<'_>,
        frame: &Node,
        data: &Node,
        xf: Xf,
        env: &Env<'_>,
        chain: &Chain<'_>,
    ) -> Option<Drawing> {
        // The diagram's data names its drawing; a slide with one diagram has
        // one drawing, which is found without it.
        let named = data
            .child("relIds")
            .and_then(|r| r.attr_prefixed("dm"))
            .and_then(|id| env.rels.get(id))
            .filter(|r| !r.external)
            .and_then(|rel| pkg.read_xml_opt(&rel.target).ok().flatten())
            .and_then(|xml| Node::parse(&xml).ok())
            .and_then(|model| {
                fn find(n: &Node, depth: usize) -> Option<&Node> {
                    if n.name == "dataModelExt" {
                        return Some(n);
                    }
                    if depth > 8 {
                        return None;
                    }
                    n.children.iter().find_map(|c| find(c, depth + 1))
                }
                find(&model, 0)?.attr("relId").map(str::to_string)
            })
            .and_then(|id| env.rels.get(&id))
            .or_else(|| env.rels.first_of_type("/diagramDrawing"))
            .filter(|r| !r.external)?
            .target
            .clone();
        let part = self.part(pkg, &named)?;
        let tree = part.root.child("spTree")?;
        let at = frame.child("xfrm").and_then(frame_of)?;
        let inner = Xf {
            sx: xf.sx,
            sy: xf.sy,
            tx: xf.tx + xf.sx * at.x,
            ty: xf.ty + xf.sy * at.y,
        };
        let env = Env {
            theme: env.theme,
            clr_map: env.clr_map,
            rels: &part.rels,
            slide_no: env.slide_no,
        };
        let mut drawn = Scene::default();
        self.group(pkg, tree, inner, &env, chain, Source::Slide, 1, &mut drawn);
        if drawn.shapes.is_empty() {
            return None;
        }
        // It goes by what its first few boxes say.
        let label = drawn
            .shapes
            .iter()
            .map(Shape::plain_text)
            .filter(|t| !t.is_empty())
            .take(4)
            .collect::<Vec<_>>()
            .join(", ");
        Some(Drawing {
            what: "diagram".to_string(),
            label,
            shapes: drawn.shapes,
        })
    }

    fn table(
        &mut self,
        pkg: &mut Package<'_>,
        tbl: &Node,
        shape: &Shape,
        env: &Env<'_>,
        chain: &Chain<'_>,
    ) -> Table {
        let mut columns: Vec<f32> = tbl
            .child("tblGrid")
            .into_iter()
            .flat_map(|g| g.children_named("gridCol"))
            .map(|c| c.num("w").map(pt).unwrap_or(0.))
            .take(crate::MAX_COLS)
            .collect();
        // The frame is the truth about the table's width.
        let total: f32 = columns.iter().sum();
        if total > 0. && shape.w > 0. {
            for c in &mut columns {
                *c *= shape.w / total;
            }
        }
        let pr = tbl.child("tblPr");
        let styled = pr.is_some_and(|p| p.has("tableStyleId") || p.has("tableStyle"));
        let first_row = pr.and_then(|p| p.flag("firstRow")).unwrap_or(false);
        let banded = pr.and_then(|p| p.flag("bandRow")).unwrap_or(false);
        let accent = env.scheme("accent1").unwrap_or(Color::rgb(68, 114, 196));
        let tint = |c: Color, keep: f32| {
            let mix = |ch: u8| (ch as f32 * keep + 255. * (1. - keep)).round() as u8;
            Color::rgb(mix(c.r), mix(c.g), mix(c.b))
        };

        let mut styles = chain.master.default_text.clone();
        styles.over(&chain.master.other);

        let mut rows = Vec::new();
        for (ri, tr) in tbl.children_named("tr").take(crate::MAX_ROWS).enumerate() {
            let header = styled && first_row && ri == 0;
            let mut cells = Vec::new();
            for tc in tr.children_named("tc").take(columns.len().max(1)) {
                let under_left = tc.flag("hMerge").unwrap_or(false);
                let covered = under_left || tc.flag("vMerge").unwrap_or(false);
                let tc_pr = tc.child("tcPr");
                let own_fill = tc_pr.and_then(|p| self.fill(pkg, p, env));
                let fill = own_fill.unwrap_or_else(|| {
                    if !styled {
                        Fill::None
                    } else if header {
                        Fill::Solid(accent)
                    } else if banded {
                        let body_row = if first_row { ri.saturating_sub(1) } else { ri };
                        Fill::Solid(tint(accent, if body_row % 2 == 0 { 0.2 } else { 0.1 }))
                    } else {
                        Fill::Solid(tint(accent, 0.1))
                    }
                });
                let border = tc_pr
                    .and_then(|p| {
                        ["lnB", "lnT", "lnL", "lnR"].iter().find_map(|side| {
                            let ln = p.child(side)?;
                            if ln.has("noFill") {
                                return None;
                            }
                            let color = env.color(ln.child("solidFill")?)?;
                            Some(Stroke {
                                color,
                                width: ln.num("w").map(pt).unwrap_or(1.),
                                dash: Dash::Solid,
                                start_arrow: false,
                                end_arrow: false,
                            })
                        })
                    })
                    .or_else(|| {
                        styled.then_some(Stroke {
                            color: Color::WHITE,
                            width: 1.,
                            dash: Dash::Solid,
                            start_arrow: false,
                            end_arrow: false,
                        })
                    });
                let mut cell_styles = styles.clone();
                if header {
                    for level in &mut cell_styles.levels {
                        level.run.bold = Some(true);
                        level.run.color = Some(Color::WHITE);
                    }
                }
                let text = tc.child("txBody").and_then(|tx| {
                    let mut body = self.text_body(tx, &[], &cell_styles, env)?;
                    if let Some(p) = tc_pr {
                        let side = |n: &str, default: f32| p.num(n).map(pt).unwrap_or(default);
                        body.insets = [
                            side("marL", 7.2),
                            side("marT", 3.6),
                            side("marR", 7.2),
                            side("marB", 3.6),
                        ];
                        body.anchor = match p.attr("anchor") {
                            Some("ctr") => Anchor::Middle,
                            Some("b") => Anchor::Bottom,
                            _ => Anchor::Top,
                        };
                    }
                    Some(body)
                });
                cells.push(TableCell {
                    col_span: if under_left {
                        0
                    } else {
                        tc.num("gridSpan").map(|v| v as usize).unwrap_or(1).max(1)
                    },
                    row_span: tc.num("rowSpan").map(|v| v as usize).unwrap_or(1).max(1),
                    covered,
                    fill,
                    border,
                    text,
                });
            }
            rows.push(TableRow {
                height: tr.num("h").map(pt).unwrap_or(0.),
                cells,
            });
        }
        Table { columns, rows }
    }

    /// `body_props` are the `a:bodyPr` to read, the shape's own first.
    fn text_body(
        &self,
        tx: &Node,
        body_props: &[&Node],
        styles: &ListStyle,
        env: &Env<'_>,
    ) -> Option<TextBody> {
        let own = tx.child("bodyPr");
        let props: Vec<&Node> = if body_props.is_empty() {
            own.into_iter().collect()
        } else {
            body_props.to_vec()
        };
        let attr = |name: &str| props.iter().find_map(|p| p.attr(name));
        let inset = |name: &str, default: f64| {
            pt(attr(name)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(default))
        };
        let fit = own.and_then(|b| b.child("normAutofit"));
        let pct = |n: &Node, name: &str| {
            let raw = n.attr(name)?.trim();
            Some(match raw.strip_suffix('%') {
                Some(p) => p.parse::<f32>().ok()? / 100.,
                None => raw.parse::<f32>().ok()? / 100_000.,
            })
        };
        let font_scale = fit.and_then(|f| pct(f, "fontScale")).unwrap_or(1.);
        let line_cut = fit.and_then(|f| pct(f, "lnSpcReduction")).unwrap_or(0.);

        let default_color = env.scheme("tx1").unwrap_or(Color::BLACK);
        let mut numbers = [0u32; 9];
        let mut paragraphs = Vec::new();
        let mut any_text = false;

        for p in tx.children_named("p") {
            let ppr = p.child("pPr");
            let level = ppr
                .and_then(|n| n.num("lvl"))
                .map(|v| v as usize)
                .unwrap_or(0)
                .min(8);
            let mut style = styles.levels[level].clone();
            if let Some(n) = ppr {
                style.over(&env.para_style(n));
            }
            let finish = |rs: &RunStyle, text: String, link: Option<String>| Run {
                text: if rs.caps == Some(true) {
                    text.to_uppercase()
                } else {
                    text
                },
                size: rs.size.unwrap_or(DEFAULT_FONT_SIZE) * font_scale,
                bold: rs.bold.unwrap_or(false),
                italic: rs.italic.unwrap_or(false),
                underline: rs.underline.unwrap_or(false),
                strike: rs.strike.unwrap_or(false),
                color: rs.color.unwrap_or(default_color),
                font: rs.font.clone(),
                link,
            };
            let mut runs: Vec<Run> = Vec::new();
            for c in &p.children {
                let mut rs = style.run.clone();
                let rpr = c.child("rPr");
                if let Some(r) = rpr {
                    rs.over(&env.run_style(r));
                }
                let link = rpr
                    .and_then(|r| r.child("hlinkClick"))
                    .and_then(|h| h.attr_prefixed("id"))
                    .and_then(|id| env.rels.get(id))
                    .filter(|r| r.external)
                    .and_then(|r| crate::docx::safe_url(&r.target));
                if link.is_some() && rs.color == style.run.color {
                    rs.color = env.scheme("hlink").or(rs.color);
                    rs.underline = Some(true);
                }
                let text = match c.name.as_str() {
                    "r" => c.child("t").map(|t| t.text.clone()).unwrap_or_default(),
                    "fld" => {
                        if c.attr("type") == Some("slidenum") && env.slide_no > 0 {
                            env.slide_no.to_string()
                        } else {
                            c.child("t").map(|t| t.text.clone()).unwrap_or_default()
                        }
                    }
                    "br" => "\n".to_string(),
                    _ => continue,
                };
                if text.is_empty() {
                    continue;
                }
                // Tabs and vertical tabs have no stops here.
                let text = text.replace('\u{b}', "\n").replace('\t', "    ");
                runs.push(finish(&rs, text, link));
            }
            let has_text = runs.iter().any(|r| !r.text.trim().is_empty());
            any_text |= has_text;

            let mut end = style.run.clone();
            if let Some(e) = p.child("endParaRPr") {
                end.over(&env.run_style(e));
            }
            let size = runs
                .first()
                .map(|r| r.size)
                .unwrap_or(end.size.unwrap_or(DEFAULT_FONT_SIZE) * font_scale);

            let bullet = match (&style.bullet, has_text) {
                (Some(BulletStyle::Char(c)), true) => {
                    numbers[level..].fill(0);
                    Some(c.clone())
                }
                (Some(BulletStyle::AutoNum { scheme, start }), true) => {
                    numbers[level] = if numbers[level] == 0 {
                        *start
                    } else {
                        numbers[level] + 1
                    };
                    numbers[level + 1..].fill(0);
                    Some(auto_number(scheme, numbers[level]))
                }
                (_, true) => {
                    numbers[level..].fill(0);
                    None
                }
                (_, false) => None,
            }
            .map(|text| Bullet {
                // A numbered item is set in the text's own font.
                font: match &style.bullet {
                    Some(BulletStyle::Char(_)) => style.bullet_font.clone(),
                    _ => None,
                },
                text,
                color: style.bullet_color,
                scale: style.bullet_size.unwrap_or(1.).clamp(0.25, 4.),
            });

            let gap = |s: Option<Spacing>| match s {
                Some(Spacing::Pct(p)) => p * size * 1.2,
                Some(Spacing::Pts(v)) => v * font_scale,
                None => 0.,
            };
            paragraphs.push(Paragraph {
                align: style.align.unwrap_or_default(),
                margin_left: style.mar_l.unwrap_or(0.).max(0.),
                indent: style.indent.unwrap_or(0.),
                bullet,
                space_before: gap(style.spc_bef),
                space_after: gap(style.spc_aft),
                line_spacing: match style.ln_spc {
                    Some(Spacing::Pts(v)) => LineSpacing::Exact(v * font_scale),
                    Some(Spacing::Pct(p)) => LineSpacing::Multiple((p - line_cut).max(0.5)),
                    None => LineSpacing::Multiple((1. - line_cut).max(0.5)),
                },
                runs,
                size,
            });
        }
        if !any_text {
            return None;
        }
        Some(TextBody {
            insets: [
                inset("lIns", 91_440.),
                inset("tIns", 45_720.),
                inset("rIns", 91_440.),
                inset("bIns", 45_720.),
            ],
            anchor: match attr("anchor") {
                Some("ctr") => Anchor::Middle,
                Some("b") => Anchor::Bottom,
                _ => Anchor::Top,
            },
            wrap: attr("wrap") != Some("none"),
            turn: match attr("vert") {
                Some("vert" | "eaVert" | "wordArtVert" | "mongolianVert") => 90.,
                Some("vert270" | "wordArtVertRtl") => 270.,
                _ => 0.,
            } + attr("rot")
                .and_then(|r| r.trim().parse::<f32>().ok())
                .map(|r| r / 60_000.)
                .unwrap_or(0.),
            paragraphs,
        })
    }
}

struct Chain<'c> {
    layout: &'c Rc<Part>,
    master: &'c Rc<Master>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// A shape of the layout or the master, shown behind the slide's own.
    Inherited,
    Slide,
}
