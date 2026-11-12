//! What a slide looks like: its background and its shapes, placed.
//!
//! Lengths are in points (1/72 inch) on a slide of [`crate::Deck::size`];
//! everything a shape inherits from its layout, master and theme is already
//! resolved, so a renderer only has to scale and draw.

/// An sRGB colour with alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Fill {
    #[default]
    None,
    Solid(Color),
    /// A linear gradient from `from` to `to`, through the colours of `mid`
    /// (each with where it is along the way, between 0 and 1). `angle` is in
    /// degrees, clockwise, 0 pointing right (as DrawingML has it).
    Gradient {
        angle: f32,
        from: Color,
        to: Color,
        mid: Vec<(f32, Color)>,
        /// Spreading from the middle outwards (`from` at the centre) instead
        /// of running along `angle`.
        radial: bool,
    },
    /// A picture stretched over the area; names an entry of [`crate::Deck::images`].
    Image(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dash {
    #[default]
    Solid,
    Dash,
    Dot,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    pub color: Color,
    pub width: f32,
    pub dash: Dash,
    /// An arrowhead where the outline starts / ends (lines and connectors).
    pub start_arrow: bool,
    pub end_arrow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub color: Color,
    pub blur: f32,
    pub dx: f32,
    pub dy: f32,
    /// Cast inwards from the shape's edge instead of behind it.
    pub inset: bool,
}

/// One step of an outline, in the shape's own box: (0, 0) is its top left,
/// (1, 1) its bottom right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathCmd {
    Move(f32, f32),
    Line(f32, f32),
    /// Two control points, then the end point.
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Geometry {
    #[default]
    Rect,
    /// Corner radius in points.
    RoundRect(f32),
    Ellipse,
    /// Any other outline; flips are already applied.
    Path(Vec<PathCmd>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    /// The shape's name in the file ("Title 1", "Picture 4").
    pub name: String,
    /// Its alternative text, if any.
    pub description: Option<String>,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Degrees, clockwise, about the centre.
    pub rotation: f32,
    pub geometry: Geometry,
    pub fill: Fill,
    pub stroke: Option<Stroke>,
    pub shadow: Option<Shadow>,
    pub kind: ShapeKind,
    pub text: Option<TextBody>,
}

impl Shape {
    /// The shape's text as one line, for naming it.
    pub fn plain_text(&self) -> String {
        // A chart or a diagram goes by its title, not by its axis labels.
        if let ShapeKind::Drawing(d) = &self.kind {
            return d.label.trim().to_string();
        }
        let mut out = String::new();
        let mut push = |body: &TextBody| {
            for p in &body.paragraphs {
                let line: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
                if !line.is_empty() {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&line);
                }
            }
        };
        if let Some(body) = &self.text {
            push(body);
        }
        if let ShapeKind::Table(t) = &self.kind {
            for cell in t.rows.iter().flat_map(|r| &r.cells) {
                if let Some(body) = &cell.text {
                    push(body);
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ShapeKind {
    /// A drawn shape: geometry, fill, outline, maybe text.
    #[default]
    Shape,
    Picture(Picture),
    Table(Table),
    /// A chart or a diagram, worked out into shapes of its own.
    Drawing(Drawing),
    /// Something this crate does not draw (an unusual chart, an embedded
    /// object without a picture), named for a stand-in.
    Unsupported(String),
}

/// Something made of shapes that counts as one thing on the slide.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Drawing {
    /// What it is, as a word: "chart", "diagram".
    pub what: String,
    /// Its title, if it has one.
    pub label: String,
    /// Its parts, placed on the slide (not relative to the drawing), back to
    /// front.
    pub shapes: Vec<Shape>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    /// Names an entry of [`crate::Deck::images`].
    pub image: String,
    /// The part of the image cut away on each side (left, top, right,
    /// bottom), as fractions of its size.
    pub crop: [f32; 4],
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Table {
    /// Column widths.
    pub columns: Vec<f32>,
    pub rows: Vec<TableRow>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TableRow {
    pub height: f32,
    /// One cell per column, including the ones a merged cell covers.
    pub cells: Vec<TableCell>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TableCell {
    /// How many columns the cell is wide; 0 for one covered by a cell that
    /// spans over it from the left.
    pub col_span: usize,
    pub row_span: usize,
    /// Hidden under a merged cell that starts further left or above: it keeps
    /// its place but shows no text.
    pub covered: bool,
    pub fill: Fill,
    /// The outline all round the cell.
    pub border: Option<Stroke>,
    pub text: Option<TextBody>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    #[default]
    Top,
    Middle,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextBody {
    /// Space kept free inside the shape: left, top, right, bottom.
    pub insets: [f32; 4],
    pub anchor: Anchor,
    /// False when lines are not broken at the shape's width.
    pub wrap: bool,
    /// Degrees the text is turned inside its shape, clockwise: 90 or 270 for
    /// text that runs down or up the shape, 0 otherwise. Turned a quarter,
    /// its lines are as long as the shape is tall.
    pub turn: f32,
    pub paragraphs: Vec<Paragraph>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Paragraph {
    pub align: Align,
    /// Where the text starts, from the left inset.
    pub margin_left: f32,
    /// Where the bullet sits relative to `margin_left` (negative: hanging).
    pub indent: f32,
    pub bullet: Option<Bullet>,
    pub space_before: f32,
    pub space_after: f32,
    /// Line height as a multiple of the font's own.
    pub line_spacing: LineSpacing,
    /// Empty for a blank line, which is as tall as text of `size`.
    pub runs: Vec<Run>,
    /// The font size of the paragraph mark.
    pub size: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineSpacing {
    /// A multiple of the font's natural line height.
    Multiple(f32),
    /// An exact height in points.
    Exact(f32),
}

impl Default for LineSpacing {
    fn default() -> Self {
        LineSpacing::Multiple(1.)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bullet {
    /// The character, or the number with its punctuation ("3.").
    pub text: String,
    /// `None`: the colour of the text it leads.
    pub color: Option<Color>,
    /// `None`: the font of the text it leads.
    pub font: Option<String>,
    /// Its size as a multiple of the text's.
    pub scale: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// A line break inside a paragraph is a run of `"\n"`.
    pub text: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub color: Color,
    pub font: Option<String>,
    pub link: Option<String>,
}

/// A slide as drawn: the background, then the shapes from back to front
/// (master, layout, slide).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scene {
    pub background: Fill,
    pub shapes: Vec<Shape>,
}
