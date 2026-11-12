//! PDF: the size of every page, any page as a picture, and what a page
//! says and links to.
//!
//! Drawing is [hayro](https://github.com/LaurenzV/hayro)'s, on the CPU and in
//! Rust throughout, so it is the same on the desktop and in a browser. Fonts
//! a file does not embed are replaced by hayro's built-in ones.
//! Password-protected files are refused.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};

/// Files with more pages than this are cut off.
const MAX_PAGES: usize = 20_000;
/// A page is never drawn larger than this on either side.
pub const MAX_PAGE_PIXELS: f32 = 8_192.;

/// A PDF file, ready to have its pages drawn.
#[derive(Clone, PartialEq)]
pub struct PdfDoc {
    /// Width and height of each page in points, as shown (rotation applied).
    pub pages: Vec<(f32, f32)>,
    /// True when the file has more pages than [`pages`](Self::pages) lists.
    pub truncated: bool,
    data: Arc<Vec<u8>>,
}

impl fmt::Debug for PdfDoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PdfDoc")
            .field("pages", &self.pages.len())
            .field("data", &format_args!("<{} bytes>", self.data.len()))
            .finish()
    }
}

/// A drawn page: `width` × `height` pixels, four bytes each (red, green, blue,
/// alpha), on white, so every pixel is opaque.
#[derive(Clone, PartialEq, Eq)]
pub struct PageImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl fmt::Debug for PageImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PageImage({}×{})", self.width, self.height)
    }
}

fn open(data: &Arc<Vec<u8>>) -> anyhow::Result<Pdf> {
    Pdf::new(data.clone()).map_err(|err| match err {
        hayro::hayro_syntax::LoadPdfError::Decryption(_) => {
            anyhow::anyhow!("the PDF is password-protected")
        }
        _ => anyhow::anyhow!("the file is not a PDF this preview can read"),
    })
}

pub fn parse(bytes: Vec<u8>) -> anyhow::Result<PdfDoc> {
    let data = Arc::new(bytes);
    let pdf = open(&data)?;
    let all = pdf.pages();
    let pages: Vec<(f32, f32)> = all
        .iter()
        .take(MAX_PAGES)
        .map(|p| {
            let (w, h) = p.render_dimensions();
            (w.max(1.), h.max(1.))
        })
        .collect();
    if pages.is_empty() {
        bail!("the PDF has no pages");
    }
    Ok(PdfDoc {
        truncated: all.len() > pages.len(),
        pages,
        data,
    })
}

/// A piece of a page's text that reads in one go: a line, or the part of one
/// between wide gaps. Where it is, in points from the page's top left.
#[derive(Debug, Clone, PartialEq)]
pub struct TextSpan {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Where each character of `text` starts, from `x`, and last where the
    /// text ends: one more entry than `text` has characters.
    pub stops: Vec<f32>,
}

/// A place on a page that goes somewhere when clicked.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub to: LinkTo,
}

/// Where a link goes.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkTo {
    /// A web address.
    Url(String),
    /// A page of the same document, from 0.
    Page(usize),
}

/// What a page says and links to, apart from how it looks.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PageText {
    /// In the order the page draws them, which is usually reading order.
    pub spans: Vec<TextSpan>,
    pub links: Vec<Link>,
}

/// A page keeps at most this many spans of text.
const MAX_SPANS: usize = 20_000;

/// One glyph as the page draws it.
struct Placed {
    text: String,
    /// Where it starts and ends along its baseline, and the baseline's height.
    x: f64,
    end: f64,
    y: f64,
    size: f64,
}

/// Takes the page's drawing and keeps only its text.
#[derive(Default)]
struct TextCollector {
    glyphs: Vec<Placed>,
}

impl<'a> hayro::hayro_interpret::Device<'a> for TextCollector {
    fn draw_path(
        &mut self,
        _: &hayro::kurbo::BezPath,
        _: hayro::hayro_interpret::DrawProps<'a>,
        _: &hayro::hayro_interpret::DrawMode,
    ) {
    }
    fn push_clip_path(&mut self, _: &hayro::hayro_interpret::ClipPath) {}
    fn push_transparency_group(
        &mut self,
        _: f32,
        _: Option<hayro::hayro_interpret::SoftMask<'a>>,
        _: hayro::hayro_interpret::BlendMode,
    ) {
    }
    fn draw_image(
        &mut self,
        _: hayro::hayro_interpret::Image<'a, '_>,
        _: hayro::hayro_interpret::ImageDrawProps<'a>,
    ) {
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}

    fn draw_glyph_run(
        &mut self,
        run: &hayro::hayro_interpret::font::GlyphRun<'_, 'a>,
        props: hayro::hayro_interpret::DrawProps<'a>,
        // Text drawn invisibly is still text: it is how a scanned page
        // carries what it says.
        _: &hayro::hayro_interpret::DrawMode,
    ) {
        use hayro::hayro_interpret::font::Glyph;
        use hayro::hayro_interpret::hayro_cmap::BfString;
        for glyph in run.glyphs() {
            if self.glyphs.len() >= MAX_SPANS * 40 {
                return;
            }
            let Some(unicode) = glyph.as_unicode() else {
                continue;
            };
            let text = match unicode {
                BfString::Char(c) => c.to_string(),
                BfString::String(s) => s,
            };
            // Glyph space is a thousandth of the font size.
            let [a, b, c, d, e, f] = (props.transform * glyph.transform()).as_coeffs();
            let size = (c * c + d * d).sqrt() * 1000.;
            // Only text that runs left to right on a level line is kept.
            if size < 0.5 || a <= 0. || b.abs() > a.abs() * 0.2 {
                continue;
            }
            let advance = match &**glyph {
                Glyph::Outline(g) => g.advance_width(),
                Glyph::Type3(g) => g.advance_width(),
            }
            .map(|w| w as f64)
            .unwrap_or(500.);
            self.glyphs.push(Placed {
                text,
                x: e,
                end: e + a * advance,
                y: f,
                size,
            });
        }
    }
}

/// Glyphs in drawing order, joined into spans: glyphs on one baseline that
/// follow each other closely, with a space where the page leaves one.
fn spans(glyphs: Vec<Placed>) -> Vec<TextSpan> {
    struct Open {
        text: String,
        /// Where each character starts along the baseline.
        starts: Vec<f64>,
        x: f64,
        end: f64,
        y: f64,
        size: f64,
    }
    impl Open {
        /// Adds a glyph's characters, sharing its width between them.
        fn push(&mut self, text: &str, from: f64, to: f64) {
            let n = text.chars().count().max(1) as f64;
            for (i, _) in text.chars().enumerate() {
                self.starts.push(from + (to - from) * i as f64 / n);
            }
            self.text.push_str(text);
        }
    }
    let mut out = Vec::new();
    let mut open: Option<Open> = None;
    let close = |open: Option<Open>, out: &mut Vec<TextSpan>| {
        let Some(mut o) = open else { return };
        // Spaces at the end are not part of the line.
        while o.text.ends_with(char::is_whitespace) {
            o.text.pop();
            o.end = o.starts.pop().unwrap_or(o.end);
        }
        if o.text.trim().is_empty() || out.len() >= MAX_SPANS {
            return;
        }
        let mut stops: Vec<f32> = o.starts.iter().map(|s| (s - o.x) as f32).collect();
        stops.push((o.end - o.x) as f32);
        out.push(TextSpan {
            text: o.text,
            x: o.x as f32,
            // A line of text reaches about four fifths of its size above the
            // baseline and a fifth below.
            y: (o.y - o.size * 0.8) as f32,
            w: (o.end - o.x).max(1.) as f32,
            h: o.size as f32,
            stops,
        });
    };
    for g in glyphs {
        let joins = open.as_ref().is_some_and(|o| {
            let gap = g.x - o.end;
            (g.y - o.y).abs() < o.size * 0.25
                && (g.size / o.size - 1.).abs() < 0.25
                && gap > -o.size * 0.5
                && gap < o.size * 1.5
        });
        if !joins {
            close(open.take(), &mut out);
            let mut o = Open {
                text: String::new(),
                starts: Vec::new(),
                x: g.x,
                end: g.end.max(g.x),
                y: g.y,
                size: g.size,
            };
            o.push(&g.text, g.x, g.end.max(g.x));
            open = Some(o);
            continue;
        }
        if let Some(o) = &mut open {
            // A gap the width of a space is one, where the page wrote none.
            if g.x - o.end > o.size * 0.18 && !o.text.ends_with(' ') && !g.text.starts_with(' ') {
                let from = o.end;
                o.push(" ", from, g.x);
            }
            let from = g.x.max(o.starts.last().copied().unwrap_or(g.x));
            o.push(&g.text, from, g.end.max(from));
            o.end = o.end.max(g.end);
        }
    }
    close(open, &mut out);
    out
}

/// A numbered mark on a page: where (fractions of the page's width and
/// height, from its top left) and the number shown in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pin {
    pub x: f32,
    pub y: f32,
    pub n: u32,
}

impl PdfDoc {
    fn draw(&self, index: usize, scale: f32) -> anyhow::Result<hayro::vello_cpu::Pixmap> {
        let &(w, h) = self.pages.get(index).context("no such page")?;
        let scale = scale
            .min(MAX_PAGE_PIXELS / w)
            .min(MAX_PAGE_PIXELS / h)
            .max(0.01);
        let data = self.data.clone();
        // hayro reads whatever a file claims to be a PDF; a panic on a broken
        // one is an error here, as for the other document kinds.
        let drawn = std::panic::catch_unwind(move || -> anyhow::Result<_> {
            let pdf = open(&data)?;
            let page = pdf.pages().get(index).context("no such page")?;
            let cache = RenderCache::new();
            Ok(hayro::render(
                page,
                &cache,
                &InterpreterSettings::default(),
                &RenderSettings::default(),
                &PixmapSettings {
                    x_scale: scale,
                    y_scale: scale,
                    bg_color: WHITE,
                },
            ))
        });
        match drawn {
            Ok(pixmap) => pixmap,
            Err(_) => bail!("the page could not be drawn"),
        }
    }

    /// What page `index` (from 0) says and links to.
    pub fn page_text(&self, index: usize) -> anyhow::Result<PageText> {
        use hayro::hayro_interpret::util::TransformExt as _;
        use hayro::hayro_interpret::{Context, InterpreterCache, interpret_page};
        use hayro::hayro_syntax::object::{Array, Dict, MaybeRef, Name, Rect, String as PdfString};
        if index >= self.pages.len() {
            bail!("no such page");
        }
        let data = self.data.clone();
        let read = std::panic::catch_unwind(move || -> anyhow::Result<PageText> {
            let pdf = open(&data)?;
            let page = pdf.pages().get(index).context("no such page")?;
            // Points from the page's top left, as it is shown.
            let to_page = page.initial_transform(true).to_kurbo();
            let (w, h) = page.render_dimensions();
            let cache = InterpreterCache::new();
            let mut context = Context::new(
                to_page,
                hayro::kurbo::Rect::new(0., 0., w as f64, h as f64),
                &cache,
                page.xref(),
                InterpreterSettings::default(),
            );
            let mut collector = TextCollector::default();
            interpret_page(page, &mut context, &mut collector);

            // Pages by their object numbers, for links from one to another.
            let page_of: std::collections::HashMap<u32, usize> = pdf
                .pages()
                .iter()
                .enumerate()
                .filter_map(|(i, p)| Some((p.raw().obj_id()?.obj_number, i)))
                .collect();
            let mut links = Vec::new();
            for annot in page
                .raw()
                .get::<Array<'_>>(&b"Annots"[..])
                .into_iter()
                .flat_map(|a| a.iter::<Dict<'_>>().collect::<Vec<_>>())
                .take(2_000)
            {
                let is_link = annot
                    .get::<Name<'_>>(&b"Subtype"[..])
                    .is_some_and(|n| &*n == b"Link");
                let Some(rect) = annot.get::<Rect>(&b"Rect"[..]).filter(|_| is_link) else {
                    continue;
                };
                let action = annot.get::<Dict<'_>>(&b"A"[..]);
                let url = action
                    .as_ref()
                    .and_then(|a| a.get::<PdfString<'_>>(&b"URI"[..]))
                    .map(|u| String::from_utf8_lossy(u.as_bytes()).trim().to_string())
                    .and_then(|u| crate::docx::safe_url(&u))
                    .map(LinkTo::Url);
                // Or a place in this document: a destination whose first
                // entry is the page it is on.
                let page_link = || {
                    let dest = annot
                        .get::<Array<'_>>(&b"Dest"[..])
                        .or_else(|| action.as_ref()?.get::<Array<'_>>(&b"D"[..]))?;
                    match dest.raw_iter().next()? {
                        MaybeRef::Ref(r) => page_of.get(&r.obj_number).copied(),
                        MaybeRef::NotRef(_) => None,
                    }
                    .map(LinkTo::Page)
                };
                let Some(to) = url.or_else(page_link) else {
                    continue;
                };
                let a = to_page * hayro::kurbo::Point::new(rect.x0, rect.y0);
                let b = to_page * hayro::kurbo::Point::new(rect.x1, rect.y1);
                links.push(Link {
                    x: a.x.min(b.x) as f32,
                    y: a.y.min(b.y) as f32,
                    w: (a.x - b.x).abs() as f32,
                    h: (a.y - b.y).abs() as f32,
                    to,
                });
            }
            Ok(PageText {
                spans: spans(collector.glyphs),
                links,
            })
        });
        match read {
            Ok(text) => text,
            Err(_) => bail!("the page's text could not be read"),
        }
    }

    /// Draws page `index` (from 0) at `scale` pixels per point. The scale is
    /// lowered when the page would come out larger than [`MAX_PAGE_PIXELS`].
    pub fn render(&self, index: usize, scale: f32) -> anyhow::Result<PageImage> {
        let pixmap = self.draw(index, scale)?;
        Ok(PageImage {
            width: pixmap.width() as u32,
            height: pixmap.height() as u32,
            rgba: pixmap.data_as_u8_slice().to_vec(),
        })
    }

    /// Page `index` as a PNG `width` pixels wide with `pins` drawn on it: a
    /// picture to show someone where on the page a note points.
    pub fn render_marked_png(
        &self,
        index: usize,
        width: u32,
        pins: &[Pin],
    ) -> anyhow::Result<Vec<u8>> {
        let &(w, _) = self.pages.get(index).context("no such page")?;
        let mut pixmap = self.draw(index, width as f32 / w)?;
        let (pw, ph) = (pixmap.width() as usize, pixmap.height() as usize);
        let radius = (pw as f32 / 60.).clamp(9., 22.);
        let buf = pixmap.data_as_u8_slice_mut();
        for pin in pins {
            let cx = pin.x.clamp(0., 1.) * pw as f32;
            let cy = pin.y.clamp(0., 1.) * ph as f32;
            draw_pin(buf, (pw, ph), (cx, cy), radius, pin.n);
        }
        pixmap.into_png().context("cannot encode the page picture")
    }
}

/// The pin's colour: a red that shows on paper and on dark pages alike.
const PIN: [u8; 3] = [0xE5, 0x3E, 0x3E];

/// Digits 0–9, five rows of three bits each, top row first.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b010, 0b010, 0b010],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

/// A filled disc with a white rim and the number in white, onto opaque RGBA.
fn draw_pin(buf: &mut [u8], (w, h): (usize, usize), (cx, cy): (f32, f32), r: f32, n: u32) {
    let mut put = |x: i64, y: i64, rgb: [u8; 3], cover: f32| {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 || cover <= 0. {
            return;
        }
        let i = (y as usize * w + x as usize) * 4;
        let Some(px) = buf.get_mut(i..i + 4) else {
            return;
        };
        let a = cover.min(1.);
        for c in 0..3 {
            px[c] = (px[c] as f32 * (1. - a) + rgb[c] as f32 * a).round() as u8;
        }
        px[3] = 255;
    };
    let rim = (r * 0.14).max(1.5);
    let reach = (r + rim + 1.).ceil() as i64;
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let (x, y) = (cx.floor() as i64 + dx, cy.floor() as i64 + dy);
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            // White out to the rim, then the disc over it, edges softened.
            put(x, y, [255, 255, 255], r + rim + 0.5 - d);
            put(x, y, PIN, r + 0.5 - d);
        }
    }
    let digits: Vec<usize> = n.to_string().bytes().map(|b| (b - b'0') as usize).collect();
    // Each digit is 3 cells wide with one cell between; fit them in the disc.
    let cells = digits.len() * 4 - 1;
    let cell = ((r * 1.3) / cells.max(5) as f32).max(1.);
    let (tw, th) = (cells as f32 * cell, 5. * cell);
    let (left, top) = (cx - tw / 2., cy - th / 2.);
    for (i, d) in digits.iter().enumerate() {
        for (row, bits) in DIGITS[*d].iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) == 0 {
                    continue;
                }
                let x0 = left + (i * 4 + col) as f32 * cell;
                let y0 = top + row as f32 * cell;
                for y in y0.floor() as i64..(y0 + cell).ceil() as i64 {
                    for x in x0.floor() as i64..(x0 + cell).ceil() as i64 {
                        put(x, y, [255, 255, 255], 1.);
                    }
                }
            }
        }
    }
}
