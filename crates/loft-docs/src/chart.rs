//! A chart (`c:chartSpace`) worked out into plain shapes: bars, lines,
//! slices, axes, gridlines, labels and a legend, each placed in the chart's
//! frame on the slide. A renderer that draws shapes draws charts.
//!
//! The numbers are the ones cached in the chart part, as the presentation
//! application last saw them; the workbook embedded with the chart is not
//! read. Drawn: bar and column (clustered, stacked, 100%), line, stock (as
//! lines), area, pie, doughnut, scatter and radar; bars, areas and lines also
//! combined on one pair of axes. Text widths are estimated, since nothing is
//! measured here.
//!
//! Not drawn (the caller shows a stand-in): surface charts, and the newer
//! chart kinds (`cx:`: waterfall, treemap, sunburst, …), which are not
//! `c:chartSpace` parts at all.

use crate::dom::Node;
use crate::scene::*;
use crate::slide_scene::Env;

/// More series or points than this are cut off.
const MAX_SERIES: usize = 64;
const MAX_POINTS: usize = 2_000;
/// The width of a character as a fraction of the font size, on average.
const CHAR: f32 = 0.55;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stack {
    None,
    Stacked,
    Percent,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Bar {
        horizontal: bool,
        stack: Stack,
    },
    Line,
    Area {
        stack: Stack,
    },
    /// `hole` is the inner radius as a fraction of the outer one.
    Pie {
        hole: f32,
    },
    Scatter {
        lines: bool,
    },
    Radar {
        filled: bool,
    },
}

#[derive(Debug, Default)]
struct Series {
    name: String,
    categories: Vec<String>,
    /// Scatter only: where each point is along the horizontal axis.
    xs: Vec<Option<f64>>,
    values: Vec<Option<f64>>,
    fill: Option<Color>,
    line: Option<Color>,
    line_width: Option<f32>,
    /// Points with a colour of their own.
    points: Vec<(usize, Color)>,
    markers: bool,
}

struct Plot {
    kind: Kind,
    series: Vec<Series>,
    /// The gap between groups of bars, as a fraction of a group's width.
    gap: f32,
    show_values: bool,
    show_percent: bool,
}

/// The cached points of a reference (`c:cat`, `c:val`, `c:tx`, …), by index.
fn cache(node: &Node) -> Vec<Option<String>> {
    // strRef/strCache, numRef/numCache, strLit, numLit, or the first level
    // of multiLvlStrRef/multiLvlStrCache.
    fn find(node: &Node, depth: usize) -> Option<&Node> {
        if node.has("pt") || node.has("ptCount") {
            return Some(node);
        }
        if depth > 4 {
            return None;
        }
        node.children.iter().find_map(|c| find(c, depth + 1))
    }
    let Some(list) = find(node, 0) else {
        return Vec::new();
    };
    let count = list
        .child("ptCount")
        .and_then(|c| c.num("val"))
        .map(|v| v as usize)
        .unwrap_or(0);
    let mut out: Vec<Option<String>> = vec![None; count.min(MAX_POINTS)];
    for pt in list.children_named("pt") {
        let Some(ix) = pt.num("idx").map(|v| v as usize) else {
            continue;
        };
        if ix >= MAX_POINTS {
            continue;
        }
        if ix >= out.len() {
            out.resize(ix + 1, None);
        }
        out[ix] = pt.child("v").map(|v| v.text.trim().to_string());
    }
    out
}

fn numbers(node: Option<&Node>) -> Vec<Option<f64>> {
    node.map(cache)
        .unwrap_or_default()
        .into_iter()
        .map(|v| {
            v.and_then(|s| s.parse::<f64>().ok())
                .filter(|n| n.is_finite())
        })
        .collect()
}

/// `group_markers`: whether the series' chart marks its points unless a
/// series says otherwise.
fn series(ser: &Node, group_markers: bool, env: &Env<'_>) -> Series {
    let name = ser
        .child("tx")
        .map(|tx| {
            let cached = cache(tx)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            if cached.is_empty() {
                tx.child("v")
                    .map(|v| v.text.trim().to_string())
                    .unwrap_or_default()
            } else {
                cached
            }
        })
        .unwrap_or_default();
    let sp = ser.child("spPr");
    let line = sp.and_then(|p| p.child("ln"));
    let marker = ser.child("marker");
    Series {
        name,
        categories: ser
            .child("cat")
            .map(cache)
            .unwrap_or_default()
            .into_iter()
            .map(Option::unwrap_or_default)
            .collect(),
        xs: numbers(ser.child("xVal")),
        values: numbers(ser.child("val").or_else(|| ser.child("yVal"))),
        fill: sp
            .and_then(|p| p.child("solidFill"))
            .and_then(|f| env.color(f)),
        line: line
            .filter(|l| !l.has("noFill"))
            .and_then(|l| l.child("solidFill"))
            .and_then(|f| env.color(f)),
        line_width: line.and_then(|l| l.num("w")).map(|w| (w / 12_700.) as f32),
        points: ser
            .children_named("dPt")
            .filter_map(|d| {
                let ix = d.child("idx")?.num("val")? as usize;
                let color = env.color(d.child("spPr")?.child("solidFill")?)?;
                Some((ix, color))
            })
            .collect(),
        markers: marker
            .and_then(|m| m.child("symbol"))
            .and_then(|s| s.attr("val"))
            .map(|v| v != "none")
            .unwrap_or(group_markers),
    }
}

fn plots(area: &Node, env: &Env<'_>) -> Vec<Plot> {
    let mut out = Vec::new();
    for group in &area.children {
        let val = |name: &str| group.child(name).and_then(|n| n.attr("val"));
        let stack = match val("grouping") {
            Some("stacked") => Stack::Stacked,
            Some("percentStacked") => Stack::Percent,
            _ => Stack::None,
        };
        let kind = match group.name.as_str() {
            "barChart" | "bar3DChart" => Kind::Bar {
                horizontal: val("barDir") == Some("bar"),
                stack,
            },
            // A stock chart's series (high, low, close) are lines.
            "lineChart" | "line3DChart" | "stockChart" => Kind::Line,
            "radarChart" => Kind::Radar {
                filled: val("radarStyle") == Some("filled"),
            },
            "areaChart" | "area3DChart" => Kind::Area { stack },
            "pieChart" | "pie3DChart" | "ofPieChart" => Kind::Pie { hole: 0. },
            "doughnutChart" => Kind::Pie {
                hole: group
                    .child("holeSize")
                    .and_then(|h| h.num("val"))
                    .map(|v| (v / 100.) as f32)
                    .unwrap_or(0.5)
                    .clamp(0.1, 0.9),
            },
            "scatterChart" | "bubbleChart" => Kind::Scatter {
                lines: val("scatterStyle").is_some_and(|s| s.to_lowercase().contains("line")),
            },
            _ => continue,
        };
        let labels = |name: &str| {
            group
                .child("dLbls")
                .and_then(|d| d.child(name))
                .and_then(|n| n.flag("val"))
                .unwrap_or(false)
        };
        let markers = group
            .child("marker")
            .and_then(|m| m.flag("val"))
            .unwrap_or(false);
        let series: Vec<Series> = group
            .children_named("ser")
            .take(MAX_SERIES)
            .map(|s| series(s, markers, env))
            .collect();
        if series.is_empty() {
            continue;
        }
        out.push(Plot {
            kind,
            series,
            gap: group
                .child("gapWidth")
                .and_then(|g| g.num("val"))
                .map(|v| (v / 100.) as f32)
                .unwrap_or(1.5)
                .clamp(0., 5.),
            show_values: labels("showVal"),
            show_percent: labels("showPercent"),
        });
    }
    out
}

/// Round numbers to label an axis with: the low end, the step and how many
/// steps, covering `lo..=hi`.
fn ticks(lo: f64, hi: f64) -> (f64, f64, usize) {
    let (lo, hi) = if hi > lo { (lo, hi) } else { (lo, lo + 1.) };
    let raw = (hi - lo) / 5.;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1., 2., 2.5, 5., 10.]
        .iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw)
        .unwrap_or(10. * mag);
    let first = (lo / step).floor() * step;
    let count = (((hi - first) / step) - 1e-9).ceil().max(1.) as usize;
    (first, step, count.min(50))
}

struct Format {
    percent: bool,
    grouped: bool,
    prefix: &'static str,
}

impl Format {
    fn of(code: Option<&str>, percent_axis: bool) -> Self {
        let code = code.unwrap_or_default();
        Self {
            percent: percent_axis || code.contains('%'),
            grouped: code.contains("#,#"),
            prefix: if code.contains('$') { "$" } else { "" },
        }
    }

    /// `v` written with as many decimals as `step` needs.
    fn show(&self, v: f64, step: f64) -> String {
        let (v, step) = if self.percent {
            (v * 100., step * 100.)
        } else {
            (v, step)
        };
        let decimals = if step >= 1. || step <= 0. {
            0
        } else {
            (-step.log10()).ceil().clamp(0., 4.) as usize
        };
        let mut text = format!("{:.*}", decimals, v.abs());
        if self.grouped || v.abs() >= 10_000. {
            let (int, frac) = match text.split_once('.') {
                Some((i, f)) => (i.to_string(), format!(".{f}")),
                None => (text.clone(), String::new()),
            };
            let mut grouped = String::new();
            for (i, ch) in int.chars().enumerate() {
                if i > 0 && (int.len() - i) % 3 == 0 {
                    grouped.push(',');
                }
                grouped.push(ch);
            }
            text = grouped + &frac;
        }
        let sign = if v < 0. && text.chars().any(|c| c != '0' && c != '.') {
            "−"
        } else {
            ""
        };
        format!(
            "{sign}{}{text}{}",
            self.prefix,
            if self.percent { "%" } else { "" }
        )
    }
}

/// The shapes being made, and what their text looks like.
struct Out<'e> {
    shapes: Vec<Shape>,
    ink: Color,
    size: f32,
    font: Option<String>,
    env: &'e Env<'e>,
}

fn with_alpha(c: Color, a: f32) -> Color {
    Color {
        a: (c.a as f32 * a).round() as u8,
        ..c
    }
}

impl Out<'_> {
    fn shape(&mut self, name: &str, (x, y, w, h): (f32, f32, f32, f32)) -> &mut Shape {
        self.shapes.push(Shape {
            name: name.to_string(),
            description: None,
            x,
            y,
            w,
            h,
            rotation: 0.,
            geometry: Geometry::Rect,
            fill: Fill::None,
            stroke: None,
            shadow: None,
            kind: ShapeKind::Shape,
            text: None,
        });
        let last = self.shapes.len() - 1;
        &mut self.shapes[last]
    }

    fn rect(&mut self, frame: (f32, f32, f32, f32), fill: Color) {
        self.shape("Bar", frame).fill = Fill::Solid(fill);
    }

    /// An open or closed outline through `points` (in points on the slide).
    fn path(
        &mut self,
        name: &str,
        points: &[(f32, f32)],
        close: bool,
        fill: Option<Color>,
        stroke: Option<(Color, f32)>,
    ) {
        if points.len() < 2 {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in points {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        // A level or upright line has no extent one way; give it a little so
        // its points can be told apart in its own box.
        let (w, h) = ((x1 - x0).max(0.01), (y1 - y0).max(0.01));
        let mut cmds: Vec<PathCmd> = points
            .iter()
            .enumerate()
            .map(|(i, &(x, y))| {
                let (u, v) = ((x - x0) / w, (y - y0) / h);
                if i == 0 {
                    PathCmd::Move(u, v)
                } else {
                    PathCmd::Line(u, v)
                }
            })
            .collect();
        if close {
            cmds.push(PathCmd::Close);
        }
        let shape = self.shape(name, (x0, y0, w, h));
        shape.geometry = Geometry::Path(cmds);
        shape.fill = fill.map(Fill::Solid).unwrap_or_default();
        shape.stroke = stroke.map(|(color, width)| Stroke {
            color,
            width,
            dash: Dash::Solid,
            start_arrow: false,
            end_arrow: false,
        });
    }

    fn line(&mut self, from: (f32, f32), to: (f32, f32), color: Color, width: f32) {
        self.path("Line", &[from, to], false, None, Some((color, width)));
    }

    fn dot(&mut self, (cx, cy): (f32, f32), r: f32, color: Color) {
        let shape = self.shape("Point", (cx - r, cy - r, 2. * r, 2. * r));
        shape.geometry = Geometry::Ellipse;
        shape.fill = Fill::Solid(color);
    }

    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        text: &str,
        frame: (f32, f32, f32, f32),
        align: Align,
        anchor: Anchor,
        size: f32,
        color: Color,
        bold: bool,
    ) {
        if text.trim().is_empty() {
            return;
        }
        let font = self.font.clone();
        self.shape("Label", frame).text = Some(TextBody {
            insets: [0.; 4],
            anchor,
            wrap: false,
            turn: 0.,
            paragraphs: vec![Paragraph {
                align,
                size,
                runs: vec![Run {
                    text: text.to_string(),
                    size,
                    bold,
                    italic: false,
                    underline: false,
                    strike: false,
                    color,
                    font,
                    link: None,
                }],
                ..Paragraph::default()
            }],
        });
    }

    /// The colour of series (or slice) `ix` when the chart names none: the
    /// theme's accents in turn, lighter the second time round.
    fn auto_color(&self, ix: usize) -> Color {
        let base = self
            .env
            .scheme(&format!("accent{}", ix % 6 + 1))
            .unwrap_or(Color::rgb(68, 114, 196));
        let round = (ix / 6) as f32;
        if round == 0. {
            return base;
        }
        let keep = (1. - 0.25 * round).max(0.35);
        let mix = |c: u8| (c as f32 * keep + 255. * (1. - keep)).round() as u8;
        Color::rgb(mix(base.r), mix(base.g), mix(base.b))
    }
}

/// Black or white, whichever reads on `on`.
fn contrast(on: Color) -> Color {
    let lum = 0.299 * on.r as f32 + 0.587 * on.g as f32 + 0.114 * on.b as f32;
    if lum < 150. {
        Color::WHITE
    } else {
        Color::rgb(40, 40, 40)
    }
}

/// Works `root` (a `c:chartSpace`) out into shapes inside `frame` (x, y,
/// width, height in points). Returns the shapes and the chart's title, or
/// `None` when it has nothing this module draws.
pub(crate) fn build(
    root: &Node,
    frame: (f32, f32, f32, f32),
    env: &Env<'_>,
) -> Option<(Vec<Shape>, String)> {
    let chart = root.child("chart")?;
    let area = chart.child("plotArea")?;
    let plots = plots(area, env);
    if plots.is_empty() {
        return None;
    }
    let (x, y, w, h) = frame;
    if w < 20. || h < 20. {
        return None;
    }

    // Text: the chart's own default, kept in proportion to its frame.
    let tx = root.child("txPr");
    let def = tx.and_then(|t| t.path(&["p", "pPr", "defRPr"]));
    let size = def
        .and_then(|d| d.num("sz"))
        .map(|v| v as f32 / 100.)
        .unwrap_or(10.)
        .clamp(6., (w.min(h) / 14.).max(8.));
    let ink = def
        .and_then(|d| d.child("solidFill"))
        .and_then(|f| env.color(f))
        .unwrap_or_else(|| {
            // PowerPoint's own: the text colour, softened towards the paper.
            let (t, b) = (
                env.scheme("tx1").unwrap_or(Color::BLACK),
                env.scheme("bg1").unwrap_or(Color::WHITE),
            );
            let mix = |a: u8, b: u8| (a as f32 * 0.65 + b as f32 * 0.35).round() as u8;
            Color::rgb(mix(t.r, b.r), mix(t.g, b.g), mix(t.b, b.b))
        });
    let mut out = Out {
        shapes: Vec::new(),
        ink,
        size,
        font: def
            .and_then(|d| d.child("latin"))
            .and_then(|l| l.attr("typeface"))
            .and_then(|t| env.font(t))
            .or_else(|| env.font("+mn-lt")),
        env,
    };

    let pad = 4.;
    let (mut left, mut top, mut right, mut bottom) = (x + pad, y + pad, x + w - pad, y + h - pad);

    // Title: its own text, or the only series' name when it has none.
    let single = plots.len() == 1 && plots[0].series.len() == 1;
    let title = chart.child("title").map(|t| {
        let mut text = String::new();
        fn gather(n: &Node, out: &mut String, depth: usize) {
            if n.name == "t" {
                out.push_str(&n.text);
            }
            if depth < 12 {
                for c in &n.children {
                    gather(c, out, depth + 1);
                }
            }
        }
        if let Some(tx) = t.child("tx") {
            gather(tx, &mut text, 0);
        }
        text.trim().to_string()
    });
    let title = match title {
        Some(t) if !t.is_empty() => t,
        Some(_) if single => plots[0].series[0].name.clone(),
        _ => String::new(),
    };
    if !title.is_empty() {
        let ts = size * 1.4;
        out.text(
            &title,
            (left, top, right - left, ts * 1.4),
            Align::Center,
            Anchor::Middle,
            ts,
            ink,
            false,
        );
        top += ts * 1.4 + 2.;
    }

    // Legend entries: the series, or for a pie its slices.
    let pie = matches!(plots[0].kind, Kind::Pie { .. });
    let entries: Vec<(String, Color)> = if pie {
        let s = &plots[0].series[0];
        (0..s.values.len())
            .map(|i| {
                let color = s
                    .points
                    .iter()
                    .find(|(ix, _)| *ix == i)
                    .map(|(_, c)| *c)
                    .unwrap_or_else(|| out.auto_color(i));
                (s.categories.get(i).cloned().unwrap_or_default(), color)
            })
            .collect()
    } else {
        let mut ix = 0;
        let mut list = Vec::new();
        for plot in &plots {
            for s in &plot.series {
                let color = match plot.kind {
                    Kind::Line | Kind::Scatter { .. } => s.line.or(s.fill),
                    _ => s.fill.or(s.line),
                }
                .unwrap_or_else(|| out.auto_color(ix));
                list.push((s.name.clone(), color));
                ix += 1;
            }
        }
        list
    };
    if let Some(legend) = chart.child("legend") {
        let pos = legend
            .child("legendPos")
            .and_then(|p| p.attr("val"))
            .unwrap_or("r");
        let swatch = size * 0.7;
        let widths: Vec<f32> = entries
            .iter()
            .map(|(name, _)| name.chars().count() as f32 * size * CHAR + swatch + size)
            .collect();
        let row = size * 1.5;
        if matches!(pos, "b" | "t") {
            let total: f32 = widths.iter().sum();
            let ly = if pos == "b" {
                bottom -= row + 2.;
                bottom + 2.
            } else {
                top += row + 2.;
                top - row - 2.
            };
            let mut lx = (left + right) / 2. - total.min(right - left) / 2.;
            for ((name, color), lw) in entries.iter().zip(&widths) {
                out.rect((lx, ly + (row - swatch) / 2., swatch, swatch), *color);
                out.text(
                    name,
                    (lx + swatch + size * 0.3, ly, lw - swatch, row),
                    Align::Left,
                    Anchor::Middle,
                    size,
                    ink,
                    false,
                );
                lx += lw;
            }
        } else {
            let lw = widths
                .iter()
                .copied()
                .fold(0f32, f32::max)
                .min((right - left) * 0.4);
            let lx = if pos == "l" {
                left += lw + 4.;
                left - lw - 4.
            } else {
                right -= lw + 4.;
                right + 4.
            };
            let total = row * entries.len() as f32;
            let mut ly = ((top + bottom) / 2. - total / 2.).max(top);
            for (name, color) in &entries {
                if ly + row > bottom + 1. {
                    break;
                }
                out.rect((lx, ly + (row - swatch) / 2., swatch, swatch), *color);
                out.text(
                    name,
                    (lx + swatch + size * 0.3, ly, lw - swatch, row),
                    Align::Left,
                    Anchor::Middle,
                    size,
                    ink,
                    false,
                );
                ly += row;
            }
        }
    }

    let room = (left, top, right, bottom);
    match plots[0].kind {
        Kind::Pie { hole } => draw_pie(&mut out, &plots[0], hole, room, &entries),
        Kind::Scatter { .. } => draw_scatter(&mut out, &plots, area, room, &entries),
        Kind::Radar { filled } => draw_radar(&mut out, &plots[0], filled, room, &entries),
        _ => draw_cartesian(&mut out, &plots, area, room, &entries),
    }
    Some((out.shapes, title))
}

fn draw_pie(
    out: &mut Out<'_>,
    plot: &Plot,
    hole: f32,
    (left, top, right, bottom): (f32, f32, f32, f32),
    entries: &[(String, Color)],
) {
    let s = &plot.series[0];
    let total: f64 = s.values.iter().flatten().filter(|v| **v > 0.).sum();
    if total <= 0. {
        return;
    }
    let r = ((right - left).min(bottom - top) / 2. - 2.).max(4.);
    let (cx, cy) = ((left + right) / 2., (top + bottom) / 2.);
    let frame = (cx - r, cy - r, 2. * r, 2. * r);
    // A point on the circle of radius `rr` (in the unit box) at angle `a`,
    // clockwise from twelve o'clock.
    let at = |a: f32, rr: f32| (0.5 + rr * a.sin(), 0.5 - rr * a.cos());
    let arc = |cmds: &mut Vec<PathCmd>, from: f32, to: f32, rr: f32| {
        let steps = ((to - from).abs() / std::f32::consts::FRAC_PI_2)
            .ceil()
            .max(1.) as usize;
        let d = (to - from) / steps as f32;
        let k = 4. / 3. * (d / 4.).tan() * rr;
        for i in 0..steps {
            let (a, b) = (from + d * i as f32, from + d * (i + 1) as f32);
            let (p0, p3) = (at(a, rr), at(b, rr));
            cmds.push(PathCmd::Cubic(
                p0.0 + k * a.cos(),
                p0.1 + k * a.sin(),
                p3.0 - k * b.cos(),
                p3.1 - k * b.sin(),
                p3.0,
                p3.1,
            ));
        }
    };
    let mut angle = 0f32;
    for (i, v) in s.values.iter().enumerate() {
        let Some(v) = v.filter(|v| *v > 0.) else {
            continue;
        };
        let share = (v / total) as f32;
        let sweep = share * std::f32::consts::TAU;
        let (from, to) = (angle, angle + sweep);
        angle = to;
        let color = entries.get(i).map(|e| e.1).unwrap_or(out.ink);
        let mut cmds = Vec::new();
        if hole > 0. {
            let start = at(from, 0.5);
            cmds.push(PathCmd::Move(start.0, start.1));
            arc(&mut cmds, from, to, 0.5);
            let inner = at(to, 0.5 * hole);
            cmds.push(PathCmd::Line(inner.0, inner.1));
            arc(&mut cmds, to, from, 0.5 * hole);
        } else if share > 0.999 {
            let start = at(0., 0.5);
            cmds.push(PathCmd::Move(start.0, start.1));
            arc(&mut cmds, 0., std::f32::consts::TAU, 0.5);
        } else {
            cmds.push(PathCmd::Move(0.5, 0.5));
            let start = at(from, 0.5);
            cmds.push(PathCmd::Line(start.0, start.1));
            arc(&mut cmds, from, to, 0.5);
        }
        cmds.push(PathCmd::Close);
        let paper = out.env.scheme("bg1").unwrap_or(Color::WHITE);
        let shape = out.shape("Slice", frame);
        shape.geometry = Geometry::Path(cmds);
        shape.fill = Fill::Solid(color);
        // The paper shows between slices, as it does in the application.
        shape.stroke = Some(Stroke {
            color: paper,
            width: 0.75,
            dash: Dash::Solid,
            start_arrow: false,
            end_arrow: false,
        });
        if (plot.show_values || plot.show_percent) && share > 0.04 {
            let label = if plot.show_percent {
                format!("{}%", (share * 100.).round() as i64)
            } else {
                Format::of(None, false).show(v, 1.)
            };
            let mid = (from + to) / 2.;
            let rr = r * (if hole > 0. { (1. + hole) / 2. } else { 0.65 });
            let (lx, ly) = (cx + rr * mid.sin(), cy - rr * mid.cos());
            let lw = out.size * 4.;
            out.text(
                &label,
                (lx - lw / 2., ly - out.size * 0.7, lw, out.size * 1.4),
                Align::Center,
                Anchor::Middle,
                out.size,
                contrast(color),
                false,
            );
        }
    }
}

/// The value axis of a chart with categories: its range, labels and grid.
struct ValueAxis {
    lo: f64,
    step: f64,
    count: usize,
    format: Format,
    shown: bool,
    grid: bool,
}

impl ValueAxis {
    fn hi(&self) -> f64 {
        self.lo + self.step * self.count as f64
    }

    fn label(&self, i: usize) -> String {
        self.format.show(self.lo + self.step * i as f64, self.step)
    }

    /// Where `v` is along the axis: 0 at its low end, 1 at its high end.
    fn frac(&self, v: f64) -> f32 {
        (((v - self.lo) / (self.hi() - self.lo)) as f32).clamp(0., 1.)
    }

    /// From the data's range, and whatever the axis element pins down.
    fn new(axis: Option<&Node>, data: (f64, f64), from_zero: bool, percent: bool) -> Self {
        let (mut lo, mut hi) = data;
        if percent {
            (lo, hi) = (lo.min(0.), 1.);
        } else if from_zero || lo < 0. || (hi - lo) > hi.abs() * (1. / 6.) {
            // Away from zero only when the values sit close together far
            // from it, as the application does.
            lo = lo.min(0.);
            hi = hi.max(0.);
        }
        let scaling = axis.and_then(|a| a.child("scaling"));
        let pinned = |name: &str| {
            scaling
                .and_then(|s| s.child(name))
                .and_then(|n| n.num("val"))
        };
        let (min, max) = (pinned("min"), pinned("max"));
        let (mut first, mut step, mut count) = ticks(min.unwrap_or(lo), max.unwrap_or(hi));
        if percent {
            (first, step, count) = (lo, 0.2, ((1. - lo) / 0.2).round() as usize);
        }
        if let Some(unit) = axis
            .and_then(|a| a.child("majorUnit"))
            .and_then(|u| u.num("val"))
            .filter(|u| *u > 0.)
        {
            let top = max.unwrap_or(first + step * count as f64);
            first = min.unwrap_or(first);
            let n = ((top - first) / unit).ceil();
            if (1. ..=50.).contains(&n) {
                (step, count) = (unit, n as usize);
            }
        }
        let deleted = axis
            .and_then(|a| a.child("delete"))
            .and_then(|d| d.flag("val"))
            .unwrap_or(false);
        Self {
            lo: first,
            step,
            count,
            format: Format::of(
                axis.and_then(|a| a.child("numFmt"))
                    .and_then(|f| f.attr("formatCode")),
                percent,
            ),
            shown: !deleted,
            grid: axis.is_some_and(|a| a.has("majorGridlines")),
        }
    }
}

fn draw_cartesian(
    out: &mut Out<'_>,
    plots: &[Plot],
    area: &Node,
    (left, top, right, bottom): (f32, f32, f32, f32),
    entries: &[(String, Color)],
) {
    let size = out.size;
    let ink = out.ink;
    let horizontal = plots.iter().any(|p| {
        matches!(
            p.kind,
            Kind::Bar {
                horizontal: true,
                ..
            }
        )
    });
    let categories: Vec<String> = plots
        .iter()
        .flat_map(|p| &p.series)
        .map(|s| &s.categories)
        .max_by_key(|c| c.len())
        .cloned()
        .unwrap_or_default();
    let n = plots
        .iter()
        .flat_map(|p| &p.series)
        .map(|s| s.values.len())
        .max()
        .unwrap_or(0)
        .max(categories.len());
    if n == 0 {
        return;
    }

    // What each plot reaches to, stacking where it stacks.
    let stack_of = |kind: Kind| match kind {
        Kind::Bar { stack, .. } | Kind::Area { stack } => stack,
        _ => Stack::None,
    };
    let percent = plots.iter().any(|p| stack_of(p.kind) == Stack::Percent);
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for plot in plots {
        if stack_of(plot.kind) == Stack::None {
            for v in plot.series.iter().flat_map(|s| s.values.iter().flatten()) {
                (lo, hi) = (lo.min(*v), hi.max(*v));
            }
        } else {
            for i in 0..n {
                let (mut up, mut down) = (0f64, 0f64);
                for v in plot
                    .series
                    .iter()
                    .filter_map(|s| s.values.get(i).copied().flatten())
                {
                    if v >= 0. { up += v } else { down += v }
                }
                (lo, hi) = (lo.min(down), hi.max(up));
            }
        }
    }
    if lo > hi {
        return;
    }
    let from_zero = plots
        .iter()
        .any(|p| matches!(p.kind, Kind::Bar { .. } | Kind::Area { .. }));
    let axis = ValueAxis::new(area.child("valAx"), (lo, hi), from_zero, percent);
    let cat_shown = !area
        .child("catAx")
        .or_else(|| area.child("dateAx"))
        .and_then(|a| a.child("delete"))
        .and_then(|d| d.flag("val"))
        .unwrap_or(false);

    // Room for the labels of both axes.
    let widest = |labels: &mut dyn Iterator<Item = usize>| labels.max().unwrap_or(0) as f32;
    let value_chars = widest(&mut (0..=axis.count).map(|i| axis.label(i).chars().count()));
    let cat_chars = widest(&mut categories.iter().map(|c| c.chars().count()));
    let line = size * 1.5;
    let (mut pl, pt, mut pr, mut pb) = (left, top + size * 0.6, right - size * 0.6, bottom);
    if horizontal {
        if cat_shown {
            pl += (cat_chars * size * CHAR + 6.).min((right - left) * 0.4);
        }
        if axis.shown {
            pb -= line;
        }
        pr -= value_chars * size * CHAR / 2.;
    } else {
        if axis.shown {
            pl += value_chars * size * CHAR + 6.;
        }
        if cat_shown {
            pb -= line;
        }
    }
    if pr - pl < 10. || pb - pt < 10. {
        return;
    }
    // A value's place along its axis, and a category's band across the other.
    let value_at = |v: f64| {
        let f = axis.frac(v);
        if horizontal {
            pl + (pr - pl) * f
        } else {
            pb - (pb - pt) * f
        }
    };
    let band = if horizontal {
        (pb - pt) / n as f32
    } else {
        (pr - pl) / n as f32
    };
    // Categories run left to right, and bottom to top on a bar chart.
    let band_start = |i: usize| {
        if horizontal {
            pb - band * (i + 1) as f32
        } else {
            pl + band * i as f32
        }
    };
    let faint = with_alpha(ink, 0.22);

    for i in 0..=axis.count {
        let at = value_at(axis.lo + axis.step * i as f64);
        if axis.grid {
            if horizontal {
                out.line((at, pt), (at, pb), faint, 0.75);
            } else {
                out.line((pl, at), (pr, at), faint, 0.75);
            }
        }
        if axis.shown {
            let label = axis.label(i);
            if horizontal {
                let w = (value_chars * size * CHAR + 8.).max(band.min(60.));
                out.text(
                    &label,
                    (at - w / 2., pb + 2., w, line - 2.),
                    Align::Center,
                    Anchor::Top,
                    size,
                    ink,
                    false,
                );
            } else {
                out.text(
                    &label,
                    (left, at - line / 2., pl - left - 5., line),
                    Align::Right,
                    Anchor::Middle,
                    size,
                    ink,
                    false,
                );
            }
        }
    }
    if cat_shown {
        // Fewer labels when they would run into each other.
        let need = if horizontal {
            line
        } else {
            cat_chars * size * CHAR + 4.
        };
        let every = (need / band).ceil().max(1.) as usize;
        for (i, name) in categories.iter().enumerate().take(n) {
            if i % every != 0 {
                continue;
            }
            let start = band_start(i);
            if horizontal {
                out.text(
                    name,
                    (left, start, pl - left - 5., band),
                    Align::Right,
                    Anchor::Middle,
                    size,
                    ink,
                    false,
                );
            } else {
                let w = band * every as f32;
                out.text(
                    name,
                    (start + band / 2. - w / 2., pb + 2., w, line - 2.),
                    Align::Center,
                    Anchor::Top,
                    size,
                    ink,
                    false,
                );
            }
        }
    }

    // Bars and areas under lines, each series in its legend colour.
    let mut order: Vec<(usize, &Plot)> = Vec::new();
    let mut first = 0;
    for plot in plots {
        order.push((first, plot));
        first += plot.series.len();
    }
    order.sort_by_key(|(_, p)| match p.kind {
        Kind::Area { .. } => 0,
        Kind::Bar { .. } => 1,
        _ => 2,
    });
    let zero = value_at(0f64.clamp(axis.lo, axis.hi()));
    for (first, plot) in order {
        let color = |s: usize| entries.get(first + s).map(|e| e.1).unwrap_or(ink);
        let stack = stack_of(plot.kind);
        // Each point's share of its category's total, for 100% charts.
        let totals: Vec<f64> = (0..n)
            .map(|i| {
                plot.series
                    .iter()
                    .filter_map(|s| s.values.get(i).copied().flatten())
                    .map(f64::abs)
                    .sum()
            })
            .collect();
        let scaled = |v: f64, i: usize| {
            if stack == Stack::Percent {
                if totals[i] > 0. { v / totals[i] } else { 0. }
            } else {
                v
            }
        };
        match plot.kind {
            Kind::Bar { .. } => {
                // The gap between groups is measured in bars: 150% is a bar
                // and a half.
                let count = if stack == Stack::None {
                    plot.series.len()
                } else {
                    1
                };
                let thick = band / (count as f32 + plot.gap);
                let group = thick * count as f32;
                for i in 0..n {
                    let (mut up, mut down) = (0f64, 0f64);
                    for (si, s) in plot.series.iter().enumerate() {
                        let Some(v) = s.values.get(i).copied().flatten() else {
                            continue;
                        };
                        let v = scaled(v, i);
                        let (from, to) = if stack == Stack::None {
                            (0., v)
                        } else if v >= 0. {
                            up += v;
                            (up - v, up)
                        } else {
                            down += v;
                            (down - v, down)
                        };
                        let slot = if stack == Stack::None { si } else { 0 };
                        let across = band_start(i) + (band - group) / 2. + thick * slot as f32;
                        let (a, b) = (value_at(from), value_at(to));
                        let fill = s
                            .points
                            .iter()
                            .find(|(ix, _)| *ix == i)
                            .map(|(_, c)| *c)
                            .unwrap_or_else(|| color(si));
                        let frame = if horizontal {
                            (a.min(b), across, (a - b).abs(), thick)
                        } else {
                            (across, a.min(b), thick, (a - b).abs())
                        };
                        out.rect(frame, fill);
                        if plot.show_values {
                            let label = axis.format.show(v, axis.step.min(1.));
                            let lw = (label.chars().count() as f32 * size * CHAR + 6.).max(thick);
                            if horizontal {
                                out.text(
                                    &label,
                                    (a.max(b) + 3., across, lw, thick),
                                    Align::Left,
                                    Anchor::Middle,
                                    size,
                                    ink,
                                    false,
                                );
                            } else if stack == Stack::None {
                                out.text(
                                    &label,
                                    (across + thick / 2. - lw / 2., a.min(b) - line, lw, line),
                                    Align::Center,
                                    Anchor::Bottom,
                                    size,
                                    ink,
                                    false,
                                );
                            } else {
                                out.text(
                                    &label,
                                    (across + thick / 2. - lw / 2., a.min(b), lw, (a - b).abs()),
                                    Align::Center,
                                    Anchor::Middle,
                                    size,
                                    contrast(fill),
                                    false,
                                );
                            }
                        }
                    }
                }
            }
            Kind::Line | Kind::Area { .. } => {
                let area_chart = matches!(plot.kind, Kind::Area { .. });
                let mut base = vec![0f64; n];
                // Areas stack from the bottom up, so the last is drawn first
                // when they do not stack (the first stays in front).
                // Each series: which one, its line, and what it stands on.
                type Outline = Vec<(f32, f32)>;
                let mut drawn: Vec<(usize, Outline, Outline)> = Vec::new();
                for (si, s) in plot.series.iter().enumerate() {
                    let mut top_line = Vec::new();
                    let mut floor = Vec::new();
                    for (i, below) in base.iter_mut().enumerate() {
                        let Some(v) = s.values.get(i).copied().flatten() else {
                            continue;
                        };
                        let v = scaled(v, i);
                        let (from, to) = if stack == Stack::None {
                            (0f64.clamp(axis.lo, axis.hi()), v)
                        } else {
                            *below += v;
                            (*below - v, *below)
                        };
                        // Lines sit at the middle of each band; areas reach
                        // from the first band's middle to the last one's.
                        let across = band_start(i) + band / 2.;
                        top_line.push((across, value_at(to)));
                        floor.push((across, value_at(from)));
                    }
                    drawn.push((si, top_line, floor));
                }
                if area_chart && stack == Stack::None {
                    drawn.reverse();
                }
                for (si, top_line, floor) in drawn {
                    let s = &plot.series[si];
                    if area_chart {
                        let mut outline = top_line.clone();
                        outline.extend(floor.iter().rev());
                        out.path("Area", &outline, true, Some(color(si)), None);
                    } else {
                        let width = s.line_width.unwrap_or(2.25).clamp(0.5, 6.);
                        out.path("Series", &top_line, false, None, Some((color(si), width)));
                        if s.markers || top_line.len() == 1 {
                            for p in &top_line {
                                out.dot(*p, width * 1.4, color(si));
                            }
                        }
                    }
                    if plot.show_values {
                        for (i, p) in top_line.iter().enumerate() {
                            let v = s.values.iter().flatten().nth(i).copied().unwrap_or(0.);
                            let label = axis.format.show(v, axis.step.min(1.));
                            let lw = label.chars().count() as f32 * size * CHAR + 8.;
                            out.text(
                                &label,
                                (p.0 - lw / 2., p.1 - line - 2., lw, line),
                                Align::Center,
                                Anchor::Bottom,
                                size,
                                ink,
                                false,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // The line the categories stand on, over the bars' feet.
    let strong = with_alpha(ink, 0.55);
    if horizontal {
        out.line((zero, pt), (zero, pb), strong, 0.75);
    } else {
        out.line((pl, zero), (pr, zero), strong, 0.75);
    }
}

fn draw_scatter(
    out: &mut Out<'_>,
    plots: &[Plot],
    area: &Node,
    (left, top, right, bottom): (f32, f32, f32, f32),
    entries: &[(String, Color)],
) {
    let size = out.size;
    let ink = out.ink;
    let series: Vec<&Series> = plots.iter().flat_map(|p| &p.series).collect();
    let lines = plots
        .iter()
        .any(|p| matches!(p.kind, Kind::Scatter { lines: true }));
    // A point's place along x: its own, or its position when it has none.
    let x_of = |s: &Series, i: usize| s.xs.get(i).copied().flatten().unwrap_or((i + 1) as f64);
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for s in &series {
        for (i, v) in s.values.iter().enumerate() {
            let Some(v) = v else { continue };
            let xv = x_of(s, i);
            (x0, x1, y0, y1) = (x0.min(xv), x1.max(xv), y0.min(*v), y1.max(*v));
        }
    }
    if x0 > x1 {
        return;
    }
    // The two value axes, in the order the file lists them: x, then y.
    let mut axes = area.children_named("valAx");
    let (x_node, y_node) = (axes.next(), axes.next());
    let xa = ValueAxis::new(x_node, (x0, x1), false, false);
    let ya = ValueAxis::new(y_node, (y0, y1), false, false);
    let line = size * 1.5;
    let chars = (0..=ya.count)
        .map(|i| ya.label(i).chars().count())
        .max()
        .unwrap_or(1) as f32;
    let (pl, pt, pr, pb) = (
        left + chars * size * CHAR + 6.,
        top + size * 0.6,
        right - size,
        bottom - line,
    );
    if pr - pl < 10. || pb - pt < 10. {
        return;
    }
    let px = |v: f64| pl + (pr - pl) * xa.frac(v);
    let py = |v: f64| pb - (pb - pt) * ya.frac(v);
    let faint = with_alpha(ink, 0.22);
    for i in 0..=ya.count {
        let at = py(ya.lo + ya.step * i as f64);
        out.line((pl, at), (pr, at), faint, 0.75);
        out.text(
            &ya.label(i),
            (left, at - line / 2., pl - left - 5., line),
            Align::Right,
            Anchor::Middle,
            size,
            ink,
            false,
        );
    }
    for i in 0..=xa.count {
        let at = px(xa.lo + xa.step * i as f64);
        let w = (pr - pl) / xa.count.max(1) as f32;
        out.text(
            &xa.label(i),
            (at - w / 2., pb + 2., w, line - 2.),
            Align::Center,
            Anchor::Top,
            size,
            ink,
            false,
        );
    }
    out.line((pl, pb), (pr, pb), with_alpha(ink, 0.55), 0.75);
    for (si, s) in series.iter().enumerate() {
        let color = entries.get(si).map(|e| e.1).unwrap_or(ink);
        let points: Vec<(f32, f32)> = s
            .values
            .iter()
            .enumerate()
            .filter_map(|(i, v)| Some((px(x_of(s, i)), py((*v)?))))
            .collect();
        if lines && s.line.is_some() {
            out.path(
                "Series",
                &points,
                false,
                None,
                Some((color, s.line_width.unwrap_or(1.5))),
            );
        }
        for p in points {
            out.dot(p, (size * 0.3).clamp(2., 4.), color);
        }
    }
}

fn draw_radar(
    out: &mut Out<'_>,
    plot: &Plot,
    filled: bool,
    (left, top, right, bottom): (f32, f32, f32, f32),
    entries: &[(String, Color)],
) {
    let size = out.size;
    let ink = out.ink;
    let n = plot
        .series
        .iter()
        .map(|s| s.values.len())
        .max()
        .unwrap_or(0);
    let hi = plot
        .series
        .iter()
        .flat_map(|s| s.values.iter().flatten())
        .fold(0f64, |a, v| a.max(*v));
    if n < 3 || hi <= 0. {
        return;
    }
    let (_, step, count) = ticks(0., hi);
    let top_value = step * count as f64;
    // Room for the category names round the web.
    let r = ((right - left).min(bottom - top) / 2. - size * 1.6).max(8.);
    let (cx, cy) = ((left + right) / 2., (top + bottom) / 2.);
    // Spoke `i` at `frac` of the way out, the first one pointing up.
    let at = |i: usize, frac: f32| {
        let a = std::f32::consts::TAU * i as f32 / n as f32;
        (cx + r * frac * a.sin(), cy - r * frac * a.cos())
    };
    let faint = with_alpha(ink, 0.22);
    for ring in 1..=count {
        let frac = ring as f32 / count as f32;
        let web: Vec<(f32, f32)> = (0..n).map(|i| at(i, frac)).collect();
        out.path("Ring", &web, true, None, Some((faint, 0.75)));
    }
    let categories = plot
        .series
        .iter()
        .map(|s| &s.categories)
        .max_by_key(|c| c.len());
    for i in 0..n {
        out.line((cx, cy), at(i, 1.), faint, 0.75);
        if let Some(name) = categories.and_then(|c| c.get(i)) {
            let (lx, ly) = at(i, 1.);
            let w = name.chars().count() as f32 * size * CHAR + 6.;
            // Beside the spoke's end, on the side away from the centre.
            let (dx, dy) = ((lx - cx) / r, (ly - cy) / r);
            let frame = (
                lx + dx * (w / 2. + 2.) - w / 2.,
                ly + dy * size - size * 0.7,
                w,
                size * 1.4,
            );
            out.text(name, frame, Align::Center, Anchor::Middle, size, ink, false);
        }
    }
    for (si, s) in plot.series.iter().enumerate() {
        let color = entries.get(si).map(|e| e.1).unwrap_or(ink);
        let web: Vec<(f32, f32)> = (0..n)
            .map(|i| {
                let v = s.values.get(i).copied().flatten().unwrap_or(0.);
                at(i, (v / top_value).clamp(0., 1.) as f32)
            })
            .collect();
        out.path(
            "Series",
            &web,
            true,
            filled.then(|| with_alpha(color, 0.45)),
            Some((color, s.line_width.unwrap_or(2.).clamp(0.5, 5.))),
        );
    }
}
