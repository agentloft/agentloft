//! Document previews (loft-docs): Word as rendered Markdown with its images,
//! spreadsheets as a virtualized grid with sheet tabs, slide decks as the
//! slides themselves (drawn by [`super::slides`]) with speaker notes, PDFs as
//! their pages.
//!
//! Notes, as in the browser panel: with annotating on, what is under the
//! pointer lights up; a click on it (a shape, a cell, a line or a spot on a
//! page) or a drag over an area (in a Word document, a selection) opens a
//! note box next to it. The note gets a numbered pin here and is handed on
//! as a [`DocEvent`] for the composer, which sends it to the agent with the
//! next message.

use crate::panel::slides::{self, Marks, PIN, Pointer, Spot};
use crate::spinner::Spinner;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, CursorStyle, DispatchPhase, ElementId, Entity,
    EventEmitter, FontWeight, Image, ImageFormat, ImageSource, InteractiveElement as _,
    IntoElement, KeyDownEvent, ListHorizontalSizingBehavior, MouseButton, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, RenderImage, ScrollHandle,
    SharedString, SharedUri, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, canvas, div, img, point,
    prelude::FluentBuilder as _, px, relative, size, uniform_list,
};
use loft_docs::pdf::PdfDoc;
use loft_docs::{Cell, Document, EmbeddedImage, Sheet};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

pub use loft_docs::DocKind;

pub fn doc_kind(path: &str) -> Option<DocKind> {
    loft_docs::kind_for(std::path::Path::new(path))
}

const COL_WIDTH: f32 = 132.;
const ROW_HEIGHT: f32 = 26.;
const GUTTER: f32 = 52.;
/// A PDF keeps at most this many of its pages drawn: a page at a wide panel's
/// width on a 2× display is some 15 MB.
const MAX_DRAWN_PAGES: usize = 6;
/// How wide the picture of a PDF page sent with its notes is, in pixels.
const NOTE_PICTURE_WIDTH: u32 = 1100;

/// Where in a document a note points.
#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    /// A shape on a slide: the slide's number (from 1) and the shape's
    /// position in its scene.
    Shape { slide: usize, shape: usize },
    /// A point on a slide, as fractions of its width and height.
    Slide { slide: usize, x: f32, y: f32 },
    /// An area of a slide, as fractions of its width and height, and the
    /// shapes in it.
    SlideArea {
        slide: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        shapes: Vec<usize>,
    },
    /// A cell: the sheet's position among the visible ones, row and column
    /// from 0.
    Cell {
        sheet: usize,
        row: usize,
        col: usize,
    },
    /// A block of cells, from `row`, `col` to `to_row`, `to_col`, both
    /// counted in.
    Cells {
        sheet: usize,
        row: usize,
        col: usize,
        to_row: usize,
        to_col: usize,
    },
    /// A passage of a Word document; the note's `what` quotes it.
    Text,
    /// A point on a PDF page (from 0), as fractions of its width and height.
    Page { page: usize, x: f32, y: f32 },
    /// An area of a PDF page (a line of its text, or one dragged out), as
    /// fractions of its width and height.
    PageArea {
        page: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
}

/// A note left on a place in a document.
#[derive(Debug, Clone, PartialEq)]
pub struct DocNote {
    /// The number on its pin, counted per document.
    pub n: u32,
    pub place: Place,
    /// The place in a word or two, for a chip: "Slide 3", "B4", "Page 2".
    pub chip: String,
    /// The place as part of a sentence: "slide 3, the shape “Card” (“Hello”)".
    pub what: String,
    pub note: String,
}

/// Pictures of a PDF's pages: each page's number (from 1) and its PNG.
pub type PagePictures = Vec<(usize, Vec<u8>)>;

pub enum DocEvent {
    /// The user finished a note.
    Note(DocNote),
}

enum State {
    Loading,
    Word {
        md: Entity<TextViewState>,
        images: Arc<HashMap<String, Arc<Image>>>,
    },
    Sheets {
        sheets: Vec<Sheet>,
        active: usize,
    },
    Slides {
        slides: Vec<SlideView>,
        /// Slide width and height in points.
        size: (f32, f32),
        images: Arc<HashMap<String, Arc<Image>>>,
    },
    Pdf {
        doc: Arc<PdfDoc>,
        pages: Vec<PageSlot>,
    },
    Error(String),
}

/// One slide: drawn from its scene, or as Markdown when it has none.
struct SlideView {
    index: usize,
    title: Option<String>,
    notes: Option<String>,
    scene: Option<Arc<loft_docs::scene::Scene>>,
    md: Option<Entity<TextViewState>>,
    /// Where the slide is on screen, once drawn with notes on.
    bounds: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
}

/// One PDF page: its picture once drawn, and where it is on screen.
#[derive(Default)]
struct PageSlot {
    /// The picture and the width in pixels it was drawn for.
    image: Option<(Arc<RenderImage>, u32)>,
    /// The width a picture is being drawn for right now.
    drawing: Option<u32>,
    /// When the page was last on screen, counted in looks.
    seen: u64,
    bounds: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    /// What the page says and links to, once read.
    text: Option<Arc<loft_docs::pdf::PageText>>,
    reading: bool,
}

/// Looking for text in a PDF.
struct Find {
    input: Entity<InputState>,
    _sub: Subscription,
    query: String,
    matches: Vec<Found>,
    /// Which match is the one in turn.
    current: usize,
    /// Pages are still being read.
    reading: bool,
}

/// One place the text looked for occurs: the page, and the bytes of each
/// span of its text the match covers (more than one when it runs over a
/// line's end).
#[derive(Debug, Clone, PartialEq)]
struct Found {
    page: usize,
    parts: Vec<(usize, std::ops::Range<usize>)>,
}

/// Where `query` occurs in a page's text, read as its spans one after
/// another with a space between.
fn find_on_page(spans: &[loft_docs::pdf::TextSpan], query: &str, page: usize) -> Vec<Found> {
    let mut all = String::new();
    // Where each span starts in `all`.
    let mut starts = Vec::with_capacity(spans.len());
    for span in spans {
        if !all.is_empty() {
            all.push(' ');
        }
        starts.push(all.len());
        all.push_str(&span.text);
    }
    let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
    occurrences(&all, &query)
        .into_iter()
        .map(|found| Found {
            page,
            parts: spans
                .iter()
                .zip(&starts)
                .enumerate()
                .filter_map(|(ix, (span, &start))| {
                    let from = found.start.max(start);
                    let to = found.end.min(start + span.text.len());
                    (from < to).then(|| (ix, from - start..to - start))
                })
                .collect(),
        })
        .filter(|f| !f.parts.is_empty())
        .collect()
}

/// Where `query` occurs in `text`, whatever the case of either.
fn occurrences(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let (hay, needle) = (text.to_lowercase(), query.to_lowercase());
    // Lowering the case can change how many bytes a letter takes; then the
    // places found would not be places in `text`, so look as written.
    let (hay, needle) = if hay.len() == text.len() {
        (hay, needle)
    } else {
        (text.to_owned(), query.to_owned())
    };
    if needle.is_empty() {
        return Vec::new();
    }
    hay.match_indices(&needle)
        .map(|(at, m)| at..at + m.len())
        .filter(|r| text.is_char_boundary(r.start) && text.is_char_boundary(r.end))
        .collect()
}

/// What a note is about.
struct About {
    place: Place,
    /// For its chip in the composer: "Slide 3", "B4", "Page 2".
    chip: String,
    /// The line over the note box: "picture · Logo", "Area · 3 shapes".
    label: String,
    /// As part of a sentence, for the agent.
    what: String,
}

/// A note being written.
struct Draft {
    about: About,
    /// What the note box sits next to, in the window.
    anchor: Bounds<Pixels>,
    input: Entity<TextareaState>,
    _sub: Subscription,
}

/// Something of a document the pointer picks things on.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Surface {
    /// A slide, by its position in the deck (from 0).
    Slide(usize),
    /// A PDF page (from 0).
    Page(usize),
}

/// The button is down on a slide or a page: a click, or a drag over an area.
struct Drag {
    on: Surface,
    from: Point<Pixels>,
    to: Point<Pixels>,
    /// It has gone far enough to be a drag.
    moved: bool,
}

/// The button is down on a sheet: the cells from `from` to `to` (row and
/// column) are being picked.
struct CellDrag {
    sheet: usize,
    from: (usize, usize),
    to: (usize, usize),
}

/// How far the pointer goes with the button down before it is a drag.
const DRAG_FROM: f32 = 6.;
/// How wide the note box is, and how much room it wants under what it is
/// about before it goes above instead.
const NOTE_WIDTH: f32 = 290.;
const NOTE_ROOM: f32 = 168.;

pub struct DocPreview {
    state: State,
    grid_scroll: UniformListScrollHandle,
    /// Where a Word document, a deck or a PDF is scrolled to.
    page_scroll: ScrollHandle,
    annotating: bool,
    /// Notes left and not sent yet: their pins show.
    notes: Vec<DocNote>,
    next_n: u32,
    draft: Option<Draft>,
    /// The shape or the line of text under the pointer.
    hover: Option<(Surface, usize)>,
    drag: Option<Drag>,
    cell_drag: Option<CellDrag>,
    /// Where the document's view is in the window.
    view_bounds: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    /// Where the top left of a sheet's first row is in the window, scrolled
    /// as it is.
    grid_origin: Rc<std::cell::Cell<Option<Point<Pixels>>>>,
    /// Counts looks at PDF pages, to tell the ones not seen for longest.
    looks: u64,
    find: Option<Find>,
}

impl EventEmitter<DocEvent> for DocPreview {}

impl DocPreview {
    pub fn new(
        kind: DocKind,
        name: &str,
        bytes: Vec<u8>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ext = crate::panel::viewer::extension(name);
        // Parse off the main thread; big workbooks take a moment.
        let task = cx
            .background_executor()
            .spawn(async move { loft_docs::load_bytes(kind, &ext, &bytes) });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(doc) => this.build(doc, cx),
                    Err(err) => State::Error(format!("{err:#}")),
                };
                cx.notify();
            });
        })
        .detach();
        // Each page picture is a texture of its own: let them go with the view.
        cx.on_release(|this, cx| {
            if let State::Pdf { pages, .. } = &mut this.state {
                for slot in pages {
                    if let Some((image, _)) = slot.image.take() {
                        cx.drop_image(image, None);
                    }
                }
            }
        })
        .detach();
        Self {
            state: State::Loading,
            grid_scroll: UniformListScrollHandle::new(),
            page_scroll: ScrollHandle::new(),
            annotating: false,
            notes: Vec::new(),
            next_n: 1,
            draft: None,
            hover: None,
            drag: None,
            cell_drag: None,
            view_bounds: Rc::default(),
            grid_origin: Rc::default(),
            looks: 0,
            find: None,
        }
    }

    /// Scrolls a Word document, a deck or a PDF to `y` from its top.
    pub fn scroll_to(&self, y: gpui_kit::Pixels) {
        self.page_scroll.set_offset(gpui_kit::point(px(0.), -y));
    }

    /// Whether there is a document to leave notes on.
    pub fn can_annotate(&self) -> bool {
        !matches!(self.state, State::Loading | State::Error(_))
    }

    pub fn annotating(&self) -> bool {
        self.annotating
    }

    pub fn set_annotating(&mut self, on: bool, cx: &mut Context<Self>) {
        self.annotating = on && self.can_annotate();
        if !self.annotating {
            self.draft = None;
            self.hover = None;
            self.drag = None;
            self.cell_drag = None;
        }
        cx.notify();
    }

    /// Takes the pin of note `n` away (it was removed from the composer).
    pub fn remove_note(&mut self, n: u32, cx: &mut Context<Self>) {
        self.notes.retain(|note| note.n != n);
        cx.notify();
    }

    /// Takes every pin away (the notes were sent).
    pub fn clear_notes(&mut self, cx: &mut Context<Self>) {
        self.notes.clear();
        self.next_n = 1;
        cx.notify();
    }

    /// For a PDF with notes: each page that has some, as a PNG with their
    /// pins drawn on it. Pages are numbered from 1.
    pub fn note_pictures(&self, cx: &App) -> Option<Task<PagePictures>> {
        let State::Pdf { doc, .. } = &self.state else {
            return None;
        };
        let mut by_page: Vec<(usize, Vec<loft_docs::pdf::Pin>)> = Vec::new();
        for note in &self.notes {
            let (page, pin) = match note.place {
                Place::Page { page, x, y } => (
                    page,
                    loft_docs::pdf::Pin {
                        x,
                        y,
                        n: note.n,
                        area: None,
                    },
                ),
                // The pin goes on the area's top right corner, as here.
                Place::PageArea { page, x, y, w, h } => (
                    page,
                    loft_docs::pdf::Pin {
                        x: x + w,
                        y,
                        n: note.n,
                        area: Some([x, y, w, h]),
                    },
                ),
                _ => continue,
            };
            match by_page.iter_mut().find(|(p, _)| *p == page) {
                Some((_, pins)) => pins.push(pin),
                None => by_page.push((page, vec![pin])),
            }
        }
        if by_page.is_empty() {
            return None;
        }
        by_page.sort_by_key(|(page, _)| *page);
        let doc = doc.clone();
        Some(cx.background_executor().spawn(async move {
            by_page
                .into_iter()
                .filter_map(|(page, pins)| {
                    let png = doc
                        .render_marked_png(page, NOTE_PICTURE_WIDTH, &pins)
                        .ok()?;
                    Some((page + 1, png))
                })
                .collect()
        }))
    }

    /// Opens the note box for `about`, next to `anchor` (in the window).
    fn begin_note(
        &mut self,
        about: About,
        anchor: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.annotating {
            return;
        }
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .submit_on_enter(true)
                .placeholder("What should change here?")
        });
        let sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                this.commit_note(cx);
            }
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        self.hover = None;
        self.draft = Some(Draft {
            about,
            anchor,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    /// Finishes the note being written with `text`, as typing it and pressing
    /// Return would. Does nothing when no note is being written.
    pub fn finish_note(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.draft else { return };
        draft
            .input
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        self.commit_note(cx);
    }

    /// The note being written is done: pin it and hand it on. With nothing
    /// written, there is no note.
    fn commit_note(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        cx.notify();
        let text = draft.input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let note = DocNote {
            n: self.next_n,
            place: draft.about.place,
            chip: draft.about.chip,
            what: draft.about.what,
            note: text,
        };
        self.next_n += 1;
        self.notes.push(note.clone());
        cx.emit(DocEvent::Note(note));
    }

    /// Where a slide or a page was painted last.
    fn surface(&self, on: Surface) -> Option<Bounds<Pixels>> {
        match (&self.state, on) {
            (State::Slides { slides, .. }, Surface::Slide(ix)) => slides.get(ix)?.bounds.get(),
            (State::Pdf { pages, .. }, Surface::Page(ix)) => pages.get(ix)?.bounds.get(),
            _ => None,
        }
    }

    /// The boxes, in the window, of what can be picked on a slide (its
    /// shapes) or a page (the lines of its text), in the order they are
    /// drawn.
    fn things(&self, on: Surface) -> Vec<Bounds<Pixels>> {
        let Some(bounds) = self.surface(on) else {
            return Vec::new();
        };
        let wide = f32::from(bounds.size.width);
        let boxed = |x: f32, y: f32, w: f32, h: f32| Bounds {
            origin: point(bounds.origin.x + px(x), bounds.origin.y + px(y)),
            size: size(px(w), px(h)),
        };
        match (&self.state, on) {
            (State::Slides { slides, size, .. }, Surface::Slide(ix)) => {
                let scale = wide / size.0.max(1.);
                slides
                    .get(ix)
                    .and_then(|s| s.scene.as_ref())
                    .map(|scene| {
                        scene
                            .shapes
                            .iter()
                            .map(|shape| {
                                let (x, y, w, h) = slides::hit_box(shape, scale);
                                boxed(x, y, w, h)
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            (State::Pdf { doc, pages }, Surface::Page(ix)) => {
                let (Some(&(page_w, _)), Some(text)) = (
                    doc.pages.get(ix),
                    pages.get(ix).and_then(|slot| slot.text.as_ref()),
                ) else {
                    return Vec::new();
                };
                let scale = wide / page_w.max(1.);
                text.spans
                    .iter()
                    .map(|s| boxed(s.x * scale, s.y * scale, s.w * scale, s.h * scale))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// The pointer did something on a slide or a page while notes can be
    /// left.
    fn pointer(&mut self, on: Surface, pointer: Pointer, cx: &mut Context<Self>) {
        if !self.annotating {
            return;
        }
        match pointer {
            Pointer::Down(at) => {
                // The press is for a note, not for selecting text.
                gpui_kit::component::GlobalState::suppress_text_selection(cx);
                self.draft = None;
                self.hover = None;
                self.drag = Some(Drag {
                    on,
                    from: at,
                    to: at,
                    moved: false,
                });
                cx.notify();
            }
            Pointer::Over(at) => {
                // What a note is being written about stays the one lit.
                if self.draft.is_some() || self.drag.is_some() {
                    return;
                }
                // The one on top is the one drawn last.
                let hover = self
                    .things(on)
                    .iter()
                    .rposition(|b| b.contains(&at))
                    .map(|ix| (on, ix));
                if hover != self.hover {
                    self.hover = hover;
                    cx.notify();
                }
            }
            Pointer::Out => {
                if self.hover.take_if(|(over, _)| *over == on).is_some() {
                    cx.notify();
                }
            }
        }
    }

    /// The pointer moved with the button down.
    fn drag_to(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        // On a sheet, the cells picked run to the one nearest the pointer,
        // wherever it has gone.
        if let (Some(drag), State::Sheets { sheets, .. }, Some(origin)) =
            (&mut self.cell_drag, &self.state, self.grid_origin.get())
            && let Some(sheet) = sheets.get(drag.sheet)
        {
            let rows = sheet.rows.len();
            let cols = sheet.rows.first().map_or(0, Vec::len);
            let nearest = |from: f32, each: f32, count: usize| {
                ((from / each).floor().max(0.) as usize).min(count.saturating_sub(1))
            };
            let to = (
                nearest(f32::from(at.y - origin.y), ROW_HEIGHT, rows),
                nearest(f32::from(at.x - origin.x) - GUTTER, COL_WIDTH, cols),
            );
            if drag.to != to {
                drag.to = to;
                cx.notify();
            }
            return;
        }
        let Some(drag) = &mut self.drag else { return };
        if !drag.moved {
            let (dx, dy) = (f32::from(at.x - drag.from.x), f32::from(at.y - drag.from.y));
            if dx.hypot(dy) < DRAG_FROM {
                return;
            }
            drag.moved = true;
        }
        drag.to = at;
        cx.notify();
    }

    /// The button came up: what was clicked, or the area dragged over, gets
    /// its note box.
    fn release(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cells) = self.cell_drag.take() {
            cx.notify();
            self.note_cells(cells, at, window, cx);
            return;
        }
        let Some(drag) = self.drag.take() else { return };
        cx.notify();
        let Some(bounds) = self.surface(drag.on) else {
            return;
        };
        let things = self.things(drag.on);
        if !drag.moved {
            let hit = things.iter().rposition(|b| b.contains(&drag.from));
            let [x, y, ..] = fractions(bounds, drag.from, drag.from);
            let (about, anchor) = match hit {
                Some(ix) => (self.about_thing(drag.on, ix), things[ix]),
                // A point's pin stands on it, to its right.
                None => (
                    self.about_point(drag.on, x, y),
                    Bounds {
                        origin: point(drag.from.x, drag.from.y - px(PIN)),
                        size: size(px(PIN), px(PIN)),
                    },
                ),
            };
            if let Some(about) = about {
                self.begin_note(about, anchor, window, cx);
            }
            return;
        }
        let area = fractions(bounds, drag.from, drag.to);
        let dragged = Bounds::from_corners(
            point(drag.from.x.min(drag.to.x), drag.from.y.min(drag.to.y)),
            point(drag.from.x.max(drag.to.x), drag.from.y.max(drag.to.y)),
        )
        .intersect(&bounds);
        // A drag that left the slide or the page at once covers nothing.
        if dragged.size.width < px(3.) || dragged.size.height < px(3.) {
            return;
        }
        // What is mostly in the area is what the note is about: the drag
        // need not cover a thing exactly.
        let inside: Vec<usize> = things
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                let whole = f32::from(b.size.width) * f32::from(b.size.height);
                let part = b.intersect(&dragged);
                let part = f32::from(part.size.width).max(0.) * f32::from(part.size.height).max(0.);
                whole > 0. && part / whole >= 0.6
            })
            .map(|(ix, _)| ix)
            .collect();
        if let Some(about) = self.about_area(drag.on, area, inside, dragged) {
            self.begin_note(about, dragged, window, cx);
        }
    }

    /// What a note on thing `ix` of a slide or a page (a shape, a line of
    /// text) is about.
    fn about_thing(&self, on: Surface, ix: usize) -> Option<About> {
        match (&self.state, on) {
            (State::Slides { slides, .. }, Surface::Slide(at)) => {
                let slide = slides.get(at)?;
                let shape = slide.scene.as_ref()?.shapes.get(ix)?;
                let n = slide.index;
                let text = shape.plain_text();
                Some(About {
                    place: Place::Shape {
                        slide: n,
                        shape: ix,
                    },
                    chip: format!("Slide {n}"),
                    label: if text.is_empty() {
                        shape_tag(shape)
                    } else {
                        format!("{} · {}", shape_kind(shape), clip(&text, 80))
                    },
                    what: format!("slide {n}, {}", shape_words(shape)),
                })
            }
            (State::Pdf { doc, pages }, Surface::Page(page)) => {
                let span = pages.get(page)?.text.as_ref()?.spans.get(ix)?;
                let &(pw, ph) = doc.pages.get(page)?;
                let n = page + 1;
                Some(About {
                    place: Place::PageArea {
                        page,
                        x: span.x / pw,
                        y: span.y / ph,
                        w: span.w / pw,
                        h: span.h / ph,
                    },
                    chip: format!("Page {n}"),
                    label: format!("text · {}", clip(&span.text, 80)),
                    what: format!("page {n}, the text “{}”", clip(&span.text, 300)),
                })
            }
            _ => None,
        }
    }

    /// What a note on a point of a slide or a page is about.
    fn about_point(&self, on: Surface, x: f32, y: f32) -> Option<About> {
        let at = point_in_words(x, y);
        match (&self.state, on) {
            (State::Slides { slides, .. }, Surface::Slide(ix)) => {
                let n = slides.get(ix)?.index;
                Some(About {
                    place: Place::Slide { slide: n, x, y },
                    chip: format!("Slide {n}"),
                    label: format!("Slide {n} · {at}"),
                    what: format!("slide {n}, {at}"),
                })
            }
            (State::Pdf { .. }, Surface::Page(page)) => {
                let n = page + 1;
                Some(About {
                    place: Place::Page { page, x, y },
                    chip: format!("Page {n}"),
                    label: format!("Page {n} · {at}"),
                    what: format!("page {n}, {at}"),
                })
            }
            _ => None,
        }
    }

    /// What a note on an area of a slide or a page is about: `area` as
    /// fractions (left, top, width, height), `inside` the things in it.
    fn about_area(
        &self,
        on: Surface,
        [x, y, w, h]: [f32; 4],
        inside: Vec<usize>,
        dragged: Bounds<Pixels>,
    ) -> Option<About> {
        let across = area_in_words(x, y, w, h);
        let sized = format!(
            "Area · {} × {}",
            f32::from(dragged.size.width).round(),
            f32::from(dragged.size.height).round()
        );
        match (&self.state, on) {
            (State::Slides { slides, .. }, Surface::Slide(ix)) => {
                let slide = slides.get(ix)?;
                let n = slide.index;
                // At most this many are named.
                let shapes: Vec<usize> = inside.into_iter().take(8).collect();
                let named: Vec<String> = shapes
                    .iter()
                    .filter_map(|&ix| slide.scene.as_ref()?.shapes.get(ix))
                    .map(shape_words)
                    .collect();
                let mut what = format!("slide {n}, the area {across}");
                if !named.is_empty() {
                    what.push_str(&format!(", with {}", named.join("; ")));
                }
                Some(About {
                    label: match shapes.len() {
                        0 => sized,
                        1 => "Area · 1 shape".to_owned(),
                        n => format!("Area · {n} shapes"),
                    },
                    place: Place::SlideArea {
                        slide: n,
                        x,
                        y,
                        w,
                        h,
                        shapes,
                    },
                    chip: format!("Slide {n}"),
                    what,
                })
            }
            (State::Pdf { pages, .. }, Surface::Page(page)) => {
                let n = page + 1;
                let said: Vec<&str> = pages
                    .get(page)
                    .and_then(|slot| slot.text.as_ref())
                    .map(|text| {
                        inside
                            .iter()
                            .filter_map(|&ix| text.spans.get(ix))
                            .map(|span| span.text.trim())
                            .filter(|said| !said.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                let mut what = format!("page {n}, the area {across}");
                if !said.is_empty() {
                    what.push_str(&format!(", with the text “{}”", clip(&said.join(" "), 300)));
                }
                Some(About {
                    place: Place::PageArea { page, x, y, w, h },
                    chip: format!("Page {n}"),
                    label: match said.len() {
                        0 => sized,
                        1 => "Area · 1 line".to_owned(),
                        n => format!("Area · {n} lines"),
                    },
                    what,
                })
            }
            _ => None,
        }
    }

    /// The button went down on a cell (row and column).
    fn cell_press(&mut self, sheet: usize, cell: (usize, usize), cx: &mut Context<Self>) {
        if !self.annotating {
            return;
        }
        gpui_kit::component::GlobalState::suppress_text_selection(cx);
        self.draft = None;
        self.cell_drag = Some(CellDrag {
            sheet,
            from: cell,
            to: cell,
        });
        cx.notify();
    }

    /// The cells picked get their note box; the button came up at `at`.
    fn note_cells(
        &mut self,
        picked: CellDrag,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let State::Sheets { sheets, .. } = &self.state else {
            return;
        };
        let Some(sheet) = sheets.get(picked.sheet) else {
            return;
        };
        let (row, to_row) = (
            picked.from.0.min(picked.to.0),
            picked.from.0.max(picked.to.0),
        );
        let (col, to_col) = (
            picked.from.1.min(picked.to.1),
            picked.from.1.max(picked.to.1),
        );
        let value = |r: usize, c: usize| {
            sheet
                .rows
                .get(r)
                .and_then(|cells| cells.get(c))
                .map(cell_text)
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
        };
        let name = &sheet.name;
        let first = format!("{}{}", column_name(col), row + 1);
        let about = if (row, col) == (to_row, to_col) {
            let said = value(row, col);
            About {
                place: Place::Cell {
                    sheet: picked.sheet,
                    row,
                    col,
                },
                label: match &said {
                    Some(said) => format!("{first} · {}", clip(said, 80)),
                    None => format!("{first} · empty"),
                },
                what: match &said {
                    Some(said) => {
                        format!("cell {first} on sheet “{name}” (“{}”)", clip(said, 80))
                    }
                    None => format!("cell {first} on sheet “{name}” (empty)"),
                },
                chip: first,
            }
        } else {
            let range = format!("{first}:{}{}", column_name(to_col), to_row + 1);
            let count = (to_row - row + 1) * (to_col - col + 1);
            // The first few cells that say something, to tell the block by.
            let said: Vec<String> = (row..=to_row)
                .flat_map(|r| (col..=to_col).map(move |c| (r, c)))
                .filter_map(|(r, c)| value(r, c))
                .take(6)
                .map(|said| format!("“{}”", clip(&said, 40)))
                .collect();
            let mut what = format!("cells {range} on sheet “{name}”");
            if said.is_empty() {
                what.push_str(" (empty)");
            } else {
                what.push_str(&format!(" (starting {})", said.join(", ")));
            }
            About {
                place: Place::Cells {
                    sheet: picked.sheet,
                    row,
                    col,
                    to_row,
                    to_col,
                },
                label: format!("{range} · {count} cells"),
                what,
                chip: range,
            }
        };
        let anchor = match self.grid_origin.get() {
            Some(origin) => Bounds {
                origin: point(
                    origin.x + px(GUTTER + col as f32 * COL_WIDTH),
                    origin.y + px(row as f32 * ROW_HEIGHT),
                ),
                size: size(
                    px((to_col - col + 1) as f32 * COL_WIDTH),
                    px((to_row - row + 1) as f32 * ROW_HEIGHT),
                ),
            },
            None => Bounds {
                origin: at,
                size: size(px(0.), px(0.)),
            },
        };
        self.begin_note(about, anchor, window, cx);
    }

    /// Whether the document can be searched (a PDF can).
    pub fn can_find(&self) -> bool {
        matches!(self.state, State::Pdf { .. })
    }

    pub fn finding(&self) -> bool {
        self.find.is_some()
    }

    /// Opens or closes the find bar.
    pub fn set_finding(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !on || !self.can_find() {
            self.find = None;
            cx.notify();
            return;
        }
        if let Some(find) = &self.find {
            find.input.update(cx, |i, cx| i.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in document…"));
        let sub =
            cx.subscribe_in(
                &input,
                window,
                |this, input, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => {
                        let query = input.read(cx).value().to_string();
                        this.look_for(query, cx);
                    }
                    InputEvent::PressEnter { shift, .. } => this.step_find(!*shift, cx),
                    _ => {}
                },
            );
        input.update(cx, |i, cx| i.focus(window, cx));
        self.find = Some(Find {
            input,
            _sub: sub,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            reading: false,
        });
        // Every page's text is wanted now, not only the ones looked at.
        self.read_all_text(cx);
        cx.notify();
    }

    /// Opens the find bar looking for `query`, as typing it there would.
    pub fn find(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.set_finding(true, window, cx);
        if let Some(find) = &self.find {
            find.input
                .update(cx, |i, cx| i.set_value(query.to_owned(), window, cx));
        }
        self.look_for(query.to_owned(), cx);
    }

    /// Reads the text of the pages that have none yet, off the main thread.
    fn read_all_text(&mut self, cx: &mut Context<Self>) {
        let State::Pdf { doc, pages } = &mut self.state else {
            return;
        };
        let missing: Vec<usize> = pages
            .iter()
            .enumerate()
            .filter(|(_, s)| s.text.is_none() && !s.reading)
            .map(|(i, _)| i)
            .collect();
        if missing.is_empty() {
            return;
        }
        for &i in &missing {
            pages[i].reading = true;
        }
        if let Some(find) = &mut self.find {
            find.reading = true;
        }
        let doc = doc.clone();
        let read = cx.background_executor().spawn(async move {
            missing
                .into_iter()
                .map(|i| (i, doc.page_text(i).unwrap_or_default()))
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let texts = read.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(find) = &mut this.find {
                    find.reading = false;
                }
                this.take_text(texts, cx);
            });
        })
        .detach();
    }

    /// Pages' text has been read: keep it, and look through it if something
    /// is being looked for.
    fn take_text(&mut self, texts: Vec<(usize, loft_docs::pdf::PageText)>, cx: &mut Context<Self>) {
        let State::Pdf { pages, .. } = &mut self.state else {
            return;
        };
        for (ix, text) in texts {
            if let Some(slot) = pages.get_mut(ix) {
                slot.reading = false;
                slot.text = Some(Arc::new(text));
            }
        }
        if let Some(query) = self.find.as_ref().map(|f| f.query.clone()) {
            self.look_for(query, cx);
        }
        cx.notify();
    }

    /// Finds `query` in the text read so far; the match in turn stays the
    /// same place when it still matches, else it is the first.
    fn look_for(&mut self, query: String, cx: &mut Context<Self>) {
        let State::Pdf { pages, .. } = &self.state else {
            return;
        };
        let mut matches = Vec::new();
        if !query.trim().is_empty() {
            for (page, slot) in pages.iter().enumerate() {
                let Some(text) = &slot.text else { continue };
                matches.extend(find_on_page(&text.spans, &query, page));
            }
        }
        let Some(find) = &mut self.find else { return };
        let changed = find.query != query;
        let kept = find
            .matches
            .get(find.current)
            .and_then(|was| matches.iter().position(|m| m == was));
        find.query = query;
        find.matches = matches;
        find.current = kept.unwrap_or(0);
        if changed {
            self.show_found();
        }
        cx.notify();
    }

    /// Moves to the next match, or the one before.
    fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else { return };
        let n = find.matches.len();
        if n == 0 {
            return;
        }
        find.current = if forward {
            (find.current + 1) % n
        } else {
            (find.current + n - 1) % n
        };
        self.show_found();
        cx.notify();
    }

    /// Scrolls so the match in turn is a little below the top.
    fn show_found(&self) {
        let (Some(find), State::Pdf { doc, pages }) = (&self.find, &self.state) else {
            return;
        };
        let Some(found) = find.matches.get(find.current) else {
            return;
        };
        let (Some(slot), Some(&(page_w, _))) = (pages.get(found.page), doc.pages.get(found.page))
        else {
            return;
        };
        // Where the page was painted last, which every page has been once
        // the document has been on screen.
        let (Some(bounds), Some(span)) = (
            slot.bounds.get(),
            found
                .parts
                .first()
                .and_then(|(span, _)| slot.text.as_ref()?.spans.get(*span)),
        ) else {
            return;
        };
        let scale = f32::from(bounds.size.width) / page_w;
        self.scroll_window_y(bounds.origin.y + px(span.y * scale), 90.);
    }

    /// Scrolls so what is painted at `y` (in the window) comes `below`
    /// pixels under the top of the view.
    fn scroll_window_y(&self, y: Pixels, below: f32) {
        let view = self.page_scroll.bounds();
        let offset = self.page_scroll.offset();
        let wanted = view.origin.y + px(below);
        self.page_scroll
            .set_offset(gpui_kit::point(offset.x, offset.y - (y - wanted)));
    }

    /// Scrolls a PDF to the top of page `ix` (from 0).
    fn show_page(&self, ix: usize) {
        let State::Pdf { pages, .. } = &self.state else {
            return;
        };
        if let Some(bounds) = pages.get(ix).and_then(|slot| slot.bounds.get()) {
            self.scroll_window_y(bounds.origin.y, 12.);
        }
    }

    /// The bar over a PDF while something is looked for in it.
    fn find_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let find = self.find.as_ref()?;
        let count = if find.query.trim().is_empty() {
            String::new()
        } else if find.matches.is_empty() {
            if find.reading {
                "Reading…".to_owned()
            } else {
                "No results".to_owned()
            }
        } else {
            format!("{} of {}", find.current + 1, find.matches.len())
        };
        let button = |id: &'static str, icon: IconName| {
            div()
                .id(id)
                .flex_none()
                .p(px(4.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(p.muted_foreground)
                .hover(|d| d.bg(p.hover).text_color(p.foreground))
                .child(Icon::new(icon).small())
        };
        Some(
            h_flex()
                .flex_none()
                .w_full()
                .h(px(36.))
                .px_3()
                .gap_1p5()
                .items_center()
                .border_b_1()
                .border_color(p.border)
                .text_xs()
                .child(
                    Icon::new(IconName::Search)
                        .xsmall()
                        .text_color(p.muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&find.input).appearance(false).small()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(p.faint_foreground)
                        .child(count),
                )
                .child(
                    button("find-prev", IconName::ChevronUp)
                        .on_click(cx.listener(|this, _, _, cx| this.step_find(false, cx))),
                )
                .child(
                    button("find-next", IconName::ChevronDown)
                        .on_click(cx.listener(|this, _, _, cx| this.step_find(true, cx))),
                )
                .child(button("find-close", IconName::X).on_click(
                    cx.listener(|this, _, window, cx| this.set_finding(false, window, cx)),
                ))
                .into_any_element(),
        )
    }

    /// A PDF page is on screen `width` pixels wide: see that it has a picture
    /// for that width. Called while painting, so it must not ask for a redraw
    /// unless something changed.
    fn want_page(&mut self, ix: usize, width: u32, cx: &mut Context<Self>) {
        self.looks += 1;
        let looks = self.looks;
        let State::Pdf { doc, pages } = &mut self.state else {
            return;
        };
        let (Some(slot), Some(&(page_w, _))) = (pages.get_mut(ix), doc.pages.get(ix)) else {
            return;
        };
        slot.seen = looks;
        if slot.text.is_none() && !slot.reading {
            slot.reading = true;
            let doc = doc.clone();
            let read = cx
                .background_executor()
                .spawn(async move { doc.page_text(ix).unwrap_or_default() });
            cx.spawn(async move |this: WeakEntity<Self>, cx| {
                let text = read.await;
                let _ = this.update(cx, |this, cx| {
                    this.take_text(vec![(ix, text)], cx);
                });
            })
            .detach();
        }
        if slot.drawing.is_some() || width == 0 {
            return;
        }
        // A picture drawn for nearly this width is stretched the little that
        // is missing; a panel being dragged wider is not redrawn at every step.
        if let Some((_, have)) = &slot.image {
            let ratio = *have as f32 / width as f32;
            if (0.85..=1.2).contains(&ratio) {
                return;
            }
        }
        slot.drawing = Some(width);
        let doc = doc.clone();
        let drawn = cx.background_executor().spawn(async move {
            let page = doc.render(ix, width as f32 / page_w).ok()?;
            let mut pixels = image::RgbaImage::from_raw(page.width, page.height, page.rgba)?;
            // GPUI paints BGRA.
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            Some(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let image = drawn.await;
            let _ = this.update(cx, |this, cx| {
                let State::Pdf { pages, .. } = &mut this.state else {
                    return;
                };
                if let Some(slot) = pages.get_mut(ix) {
                    slot.drawing = None;
                    if let Some(image) = image
                        && let Some((old, _)) = slot.image.replace((image, width))
                    {
                        cx.drop_image(old, None);
                    }
                }
                // Keep the pages looked at most recently; let the rest go.
                let mut drawn: Vec<(u64, usize)> = pages
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.image.is_some())
                    .map(|(i, s)| (s.seen, i))
                    .collect();
                if drawn.len() > MAX_DRAWN_PAGES {
                    drawn.sort_unstable();
                    for &(_, i) in &drawn[..drawn.len() - MAX_DRAWN_PAGES] {
                        if let Some((old, _)) = pages[i].image.take() {
                            cx.drop_image(old, None);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn build(&mut self, doc: Document, cx: &mut Context<Self>) -> State {
        match doc {
            Document::Word(w) => State::Word {
                md: cx.new(|cx| TextViewState::markdown(&w.markdown, cx).selectable(true)),
                images: Arc::new(image_map(&w.images)),
            },
            Document::Spreadsheet(book) => State::Sheets {
                sheets: book.sheets.into_iter().filter(|s| !s.hidden).collect(),
                active: 0,
            },
            Document::Slides(deck) => {
                let images = Arc::new(image_map(&deck.images));
                let slides = deck
                    .slides
                    .into_iter()
                    .map(|s| {
                        let md = s.scene.is_none().then(|| {
                            cx.new(|cx| TextViewState::markdown(&s.markdown, cx).selectable(true))
                        });
                        SlideView {
                            index: s.index,
                            title: s.title,
                            notes: s.notes.filter(|n| !n.trim().is_empty()),
                            scene: s.scene.map(Arc::new),
                            md,
                            bounds: Rc::default(),
                        }
                    })
                    .collect();
                State::Slides {
                    slides,
                    size: deck.size,
                    images,
                }
            }
            Document::Pdf(doc) => State::Pdf {
                pages: doc.pages.iter().map(|_| PageSlot::default()).collect(),
                doc: Arc::new(doc),
            },
        }
    }

    /// Pins, the cells picked or noted and what the pointer does, for row
    /// `row` of sheet `sheet`; `None` when the row has none of them.
    fn row_marks(
        &self,
        sheet: usize,
        row: usize,
        view: &WeakEntity<Self>,
        p: &Palette,
    ) -> Option<RowMarks> {
        // The columns of a place's cells in this row, and whether the row is
        // its first.
        let here = |place: &Place| {
            let (rows, cols) = match *place {
                Place::Cell {
                    sheet: s,
                    row: r,
                    col,
                } if s == sheet => ((r, r), (col, col)),
                Place::Cells {
                    sheet: s,
                    row: r,
                    col,
                    to_row,
                    to_col,
                } if s == sheet => ((r, to_row), (col, to_col)),
                _ => return None,
            };
            (rows.0..=rows.1)
                .contains(&row)
                .then_some((cols, rows.0 == row))
        };
        let noted: Vec<((usize, usize), bool, u32)> = self
            .notes
            .iter()
            .filter_map(|n| here(&n.place).map(|(cols, first)| (cols, first, n.n)))
            .collect();
        let active = match &self.cell_drag {
            Some(drag) if drag.sheet == sheet => {
                let rows = drag.from.0.min(drag.to.0)..=drag.from.0.max(drag.to.0);
                rows.contains(&row)
                    .then_some((drag.from.1.min(drag.to.1), drag.from.1.max(drag.to.1)))
            }
            Some(_) => None,
            None => self
                .draft
                .as_ref()
                .and_then(|d| here(&d.about.place))
                .map(|(cols, _)| cols),
        };
        if !self.annotating && noted.is_empty() {
            return None;
        }
        let on = self.annotating.then(|| {
            let view = view.clone();
            Rc::new(move |col: usize, cx: &mut App| {
                let _ = view.update(cx, |this, cx| this.cell_press(sheet, (row, col), cx));
            }) as OnCell
        });
        Some(RowMarks {
            row,
            // The pin is on the last cell of a block's first row.
            pins: noted
                .iter()
                .filter(|(_, first, _)| *first)
                .map(|((_, to), _, n)| (*to, *n))
                .collect(),
            noted: noted.iter().map(|(cols, ..)| *cols).collect(),
            active,
            on,
            origin: self.grid_origin.clone(),
            accent: p.accent,
            on_accent: p.accent_foreground,
        })
    }

    /// The bar over the document while notes are being left: what to do, and
    /// the way out.
    fn note_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        if !self.annotating {
            return None;
        }
        let how = match &self.state {
            State::Slides { .. } => "Click a shape or drag over an area to leave a note.",
            State::Sheets { .. } => "Click a cell or drag over several to leave a note.",
            State::Word { .. } => "Select some text to leave a note on it.",
            _ => "Click a line or a spot, or drag over an area, to leave a note.",
        };
        Some(
            h_flex()
                .flex_none()
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(p.border)
                .bg(p.accent.opacity(0.12))
                .text_xs()
                .child(
                    Icon::new(loft_browser::ANNOTATE_ICON)
                        .xsmall()
                        .text_color(p.accent),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(format!("{how} Notes wait in the composer until you send.")),
                )
                .child(
                    div()
                        .id("note-done")
                        .flex_none()
                        .px_2()
                        .py(px(3.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(p.muted_foreground)
                        .hover(|d| d.bg(p.hover).text_color(p.foreground))
                        .child("Done")
                        .on_click(cx.listener(|this, _, _, cx| this.set_annotating(false, cx))),
                )
                .into_any_element(),
        )
    }

    /// The box a note is written in, next to what it is about: under it
    /// where there is room, else over it.
    fn note_box(&self, p: &Palette, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let draft = self.draft.as_ref()?;
        let view = self.view_bounds.get()?;
        let (wide, tall) = (f32::from(view.size.width), f32::from(view.size.height));
        let about = draft.anchor;
        let left = f32::from(about.left() - view.left());
        let top = f32::from(about.top() - view.top());
        let bottom = f32::from(about.bottom() - view.top());
        let x = left.clamp(8., (wide - NOTE_WIDTH - 8.).max(8.));
        let card = div()
            .id("doc-note")
            .absolute()
            .left(px(x))
            .w(px(NOTE_WIDTH.min((wide - 16.).max(120.))));
        let card = if bottom + NOTE_ROOM < tall {
            card.top(px(bottom.max(0.) + 8.))
        } else if top - NOTE_ROOM > 0. {
            card.bottom(px(tall - top + 8.))
        } else {
            card.top(px(8.))
        };
        Some(
            card
                // What is under the box gets no pointer: a click in it is
                // not a click on the document.
                .occlude()
                .p(px(10.))
                .rounded(px(12.))
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .shadow_lg()
                .text_size(px(13.))
                // (The field would otherwise keep the key to itself.)
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.draft = None;
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .mx(px(2.))
                        .mb(px(6.))
                        .truncate()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.accent)
                        .child(draft.about.label.clone()),
                )
                .child(
                    div()
                        .w_full()
                        .rounded(px(8.))
                        .border_1()
                        .border_color(p.border)
                        .bg(p.background)
                        .child(Textarea::new(&draft.input).appearance(false).small()),
                )
                .child(
                    h_flex()
                        .mt(px(8.))
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(p.faint_foreground)
                                .child("⏎ add · esc cancel"),
                        )
                        .child(
                            div()
                                .id("note-add")
                                .flex_none()
                                .px(px(10.))
                                .py(px(4.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .bg(p.accent)
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.accent_foreground)
                                .hover(|d| d.opacity(0.9))
                                .child("Add note")
                                .on_click(cx.listener(|this, _, _, cx| this.commit_note(cx))),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_sheets(&self, p: &Palette, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        // Only metadata is read here; rows are rendered lazily below.
        let State::Sheets { sheets, active } = &self.state else {
            return centered(p, String::new());
        };
        let active = *active;
        let names: Vec<String> = sheets.iter().map(|s| s.name.clone()).collect();
        let Some(sheet) = sheets.get(active) else {
            return centered(p, "This workbook has no visible sheets.".into());
        };
        let cols = sheet.rows.first().map(Vec::len).unwrap_or(0);
        let (total_rows, total_cols, truncated, rows) = (
            sheet.total_rows,
            sheet.total_cols,
            sheet.truncated,
            sheet.rows.len(),
        );
        let width = px(GUTTER + cols as f32 * COL_WIDTH);
        let mut tabs = h_flex()
            .id("sheet-tabs")
            .gap_1()
            .px_2()
            .h(px(34.))
            .items_center()
            .overflow_x_scroll();
        for (ix, name) in names.into_iter().enumerate() {
            let on = ix == active;
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("sheet-{ix}")))
                    .px_2()
                    .py(px(3.))
                    .rounded(px(6.))
                    .text_xs()
                    .cursor_pointer()
                    .when(on, |d| d.bg(p.selected).font_weight(FontWeight::MEDIUM))
                    .when(!on, |d| {
                        d.text_color(p.muted_foreground).hover(|d| d.bg(p.hover))
                    })
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let State::Sheets { active, .. } = &mut this.state {
                            *active = ix;
                        }
                        cx.notify();
                    })),
            );
        }
        let info = format!(
            "{total_rows} rows × {total_cols} columns{}",
            if truncated {
                " (showing the first 10,000 × 200)"
            } else {
                ""
            }
        );
        let header = grid_row(None, (0..cols).map(column_name).collect(), p, true, None);
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .border_b_1()
                    .border_color(p.border)
                    .child(div().flex_1().min_w_0().child(tabs))
                    .child(
                        div()
                            .px_3()
                            .text_xs()
                            .text_color(p.faint_foreground)
                            .child(info),
                    ),
            )
            .child(
                div()
                    .id("grid-x")
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .child(
                        v_flex().w(width).h_full().child(header).child(
                            uniform_list(
                                "sheet-rows",
                                rows,
                                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                                    let p = Palette::of(cx);
                                    let State::Sheets { sheets, active } = &this.state else {
                                        return vec![];
                                    };
                                    let Some(sheet) = sheets.get(*active) else {
                                        return vec![];
                                    };
                                    let active = *active;
                                    let view = cx.entity().downgrade();
                                    range
                                        .filter_map(|r| sheet.rows.get(r).map(|row| (r, row)))
                                        .map(|(r, row)| {
                                            let cells: Vec<String> =
                                                row.iter().map(cell_text).collect();
                                            let marks = this.row_marks(active, r, &view, &p);
                                            grid_row(Some(r + 1), cells, &p, false, marks)
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .with_horizontal_sizing_behavior(
                                ListHorizontalSizingBehavior::Unconstrained,
                            )
                            .track_scroll(&self.grid_scroll)
                            .flex_1(),
                        ),
                    ),
            )
            .into_any_element()
    }
}

fn image_map(images: &[EmbeddedImage]) -> HashMap<String, Arc<Image>> {
    images
        .iter()
        .filter_map(|img| {
            let format = match img.mime.as_str() {
                "image/png" => ImageFormat::Png,
                "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
                "image/gif" => ImageFormat::Gif,
                "image/webp" => ImageFormat::Webp,
                "image/svg+xml" => ImageFormat::Svg,
                "image/bmp" => ImageFormat::Bmp,
                "image/tiff" => ImageFormat::Tiff,
                _ => return None,
            };
            Some((
                img.name.clone(),
                Arc::new(Image::from_bytes(format, img.bytes.clone())),
            ))
        })
        .collect()
}

/// Resolve `![x](name)` references to the document's embedded images.
fn resolver(
    images: Arc<HashMap<String, Arc<Image>>>,
) -> impl Fn(&SharedUri) -> ImageSource + Send + Sync + 'static {
    move |uri: &SharedUri| {
        let key = uri.as_ref().trim_start_matches("./");
        match images.get(key) {
            Some(img) => ImageSource::Image(img.clone()),
            None => ImageSource::from(uri.clone()),
        }
    }
}

fn column_name(mut ix: usize) -> String {
    let mut name = String::new();
    loop {
        name.insert(0, (b'A' + (ix % 26) as u8) as char);
        if ix < 26 {
            break;
        }
        ix = ix / 26 - 1;
    }
    name
}

fn cell_text(c: &Cell) -> String {
    match c {
        Cell::Empty => String::new(),
        other => other.to_string(),
    }
}

/// What a press on a cell does while notes can be left: called with the
/// cell's column.
type OnCell = Rc<dyn Fn(usize, &mut App)>;

/// Notes on one row of a sheet.
struct RowMarks {
    row: usize,
    /// Column and pin number.
    pins: Vec<(usize, u32)>,
    /// The columns, first and last, of the noted blocks that cross the row.
    noted: Vec<(usize, usize)>,
    /// The columns of the cells being picked, or written about.
    active: Option<(usize, usize)>,
    on: Option<OnCell>,
    /// Told where the grid is: see `DocPreview::grid_origin`.
    origin: Rc<std::cell::Cell<Option<Point<Pixels>>>>,
    accent: gpui_kit::Hsla,
    on_accent: gpui_kit::Hsla,
}

fn grid_row(
    number: Option<usize>,
    cells: Vec<String>,
    p: &Palette,
    header: bool,
    marks: Option<RowMarks>,
) -> gpui_kit::AnyElement {
    let mut row = h_flex()
        .h(px(ROW_HEIGHT))
        .flex_shrink_0()
        .border_b_1()
        .border_color(p.border)
        .text_xs()
        .when(header, |d| d.bg(p.muted).font_weight(FontWeight::MEDIUM))
        .child(
            div()
                .w(px(GUTTER))
                .h_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_end()
                .pr_2()
                .bg(p.muted)
                .border_r_1()
                .border_color(p.border)
                .text_color(p.faint_foreground)
                .child(number.map(|n| n.to_string()).unwrap_or_default()),
        );
    // While notes can be left, the row tells where the grid is, for placing
    // the note box next to the cells picked.
    if let Some(marks) = marks.as_ref().filter(|m| m.on.is_some()) {
        let (origin, above) = (marks.origin.clone(), marks.row as f32 * ROW_HEIGHT);
        row = row.relative().child(
            canvas(
                move |bounds: Bounds<Pixels>, _: &mut Window, _: &mut App| {
                    origin.set(Some(point(bounds.origin.x, bounds.origin.y - px(above))));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_0(),
        );
    }
    for (col, text) in cells.into_iter().enumerate() {
        let numeric = !header && text.parse::<f64>().is_ok();
        let cell = div()
            .w(px(COL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .when(numeric, |d| d.justify_end())
            .when(header, |d| {
                d.justify_center().text_color(p.muted_foreground)
            })
            .px_2()
            .border_r_1()
            .border_color(p.border)
            .child(div().truncate().child(text));
        let Some(marks) = &marks else {
            row = row.child(cell);
            continue;
        };
        let accent = marks.accent;
        let within = |(from, to): &(usize, usize)| (*from..=*to).contains(&col);
        let pin = marks.pins.iter().find(|(c, _)| *c == col).map(|(_, n)| *n);
        let cell = cell
            .relative()
            .when(marks.noted.iter().any(within), |d| {
                d.bg(accent.opacity(0.08))
            })
            .when(marks.active.as_ref().is_some_and(within), |d| {
                d.bg(accent.opacity(0.18))
            })
            // On the side the cell's own text is not: numbers sit right.
            .when_some(pin, |d, n| {
                let pin = slides::pin(Some(n), accent, marks.on_accent)
                    .absolute()
                    .top(px((ROW_HEIGHT - PIN) / 2.));
                d.child(if numeric {
                    pin.left(px(3.))
                } else {
                    pin.right(px(3.))
                })
            });
        row = match &marks.on {
            Some(on) => {
                let on = on.clone();
                // Rows share a parent: the id has to tell their cells apart.
                let id = ElementId::NamedInteger("cell".into(), (marks.row * 256 + col) as u64);
                row.child(
                    cell.id(id)
                        .cursor(CursorStyle::Crosshair)
                        .hover(|d| d.bg(accent.opacity(0.12)))
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| on(col, cx)),
                )
            }
            None => row.child(cell),
        };
    }
    row.into_any_element()
}

/// What kind of thing a shape is, in a word or two.
fn shape_kind(shape: &loft_docs::scene::Shape) -> String {
    use loft_docs::scene::ShapeKind;
    match &shape.kind {
        ShapeKind::Picture(_) => "picture".to_owned(),
        ShapeKind::Table(_) => "table".to_owned(),
        ShapeKind::Drawing(d) => d.what.clone(),
        ShapeKind::Unsupported(what) => what.to_lowercase(),
        ShapeKind::Shape => "shape".to_owned(),
    }
}

/// A shape's label while the pointer is on it: its kind and its name.
fn shape_tag(shape: &loft_docs::scene::Shape) -> String {
    let (kind, name) = (shape_kind(shape), shape.name.trim());
    if name.is_empty() {
        kind
    } else {
        format!("{kind} · {}", clip(name, 40))
    }
}

/// A shape as part of a sentence: "the picture “Logo” (“…”)".
fn shape_words(shape: &loft_docs::scene::Shape) -> String {
    let mut what = format!("the {}", shape_kind(shape));
    if !shape.name.trim().is_empty() {
        what.push_str(&format!(" “{}”", shape.name.trim()));
    }
    let text = shape.plain_text();
    if !text.is_empty() {
        what.push_str(&format!(" (“{}”)", clip(&text, 80)));
    } else if let Some(alt) = &shape.description {
        what.push_str(&format!(" (described as “{}”)", clip(alt, 80)));
    }
    what
}

/// The rectangle between two places in the window, cut to `within`, as
/// fractions of it: left, top, width, height.
fn fractions(within: Bounds<Pixels>, a: Point<Pixels>, b: Point<Pixels>) -> [f32; 4] {
    let (w, h) = (
        f32::from(within.size.width).max(1.),
        f32::from(within.size.height).max(1.),
    );
    let fx = |x: Pixels| (f32::from(x - within.origin.x) / w).clamp(0., 1.);
    let fy = |y: Pixels| (f32::from(y - within.origin.y) / h).clamp(0., 1.);
    let (x0, x1) = (fx(a.x.min(b.x)), fx(a.x.max(b.x)));
    let (y0, y1) = (fy(a.y.min(b.y)), fy(a.y.max(b.y)));
    [x0, y0, x1 - x0, y1 - y0]
}

/// An area given as fractions, the way one would say it.
fn area_in_words(x: f32, y: f32, w: f32, h: f32) -> String {
    let pc = |f: f32| (f * 100.).round() as i32;
    format!(
        "from {}% across and {}% down to {}% across and {}% down",
        pc(x),
        pc(y),
        pc(x + w),
        pc(y + h)
    )
}

/// `text` cut to `max` characters, with an ellipsis when something was cut.
fn clip(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// A point given as fractions, the way one would say it.
fn point_in_words(x: f32, y: f32) -> String {
    format!(
        "{}% across and {}% down",
        (x * 100.).round() as i32,
        (y * 100.).round() as i32
    )
}

fn centered(p: &Palette, text: String) -> gpui_kit::AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .px_6()
        .text_sm()
        .text_color(p.muted_foreground)
        .child(text)
        .into_any_element()
}

impl Render for DocPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let view = cx.entity().downgrade();
        let annotating = self.annotating;
        let body = match &self.state {
            State::Loading => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(Spinner::new().color(p.muted_foreground))
                .child("Reading document…")
                .into_any_element(),
            State::Error(err) => centered(&p, format!("Couldn't read this document: {err}")),
            State::Word { md, images } => div()
                .id("word-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.page_scroll)
                // A selection made while annotating is what the note is about.
                .when(annotating, |d| {
                    d.on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, window, cx| {
                            let quote = gpui_kit::base::TextSelection::selected_text(window, cx);
                            let quote = quote.trim();
                            if quote.is_empty() {
                                return;
                            }
                            let about = About {
                                place: Place::Text,
                                chip: clip(quote, 24),
                                label: format!("text · {}", clip(quote, 80)),
                                what: format!("the text “{}”", clip(quote, 300)),
                            };
                            // Under the line the selection ended on.
                            let anchor = Bounds {
                                origin: point(event.position.x, event.position.y - px(10.)),
                                size: size(px(0.), px(22.)),
                            };
                            this.begin_note(about, anchor, window, cx);
                        }),
                    )
                })
                .child(
                    div().w_full().flex().justify_center().child(
                        div()
                            .w_full()
                            .max_w(px(820.))
                            .m_4()
                            .px_10()
                            .py_8()
                            .rounded(px(10.))
                            .border_1()
                            .border_color(p.border)
                            .bg(p.elevated)
                            .shadow_sm()
                            .child(TextView::new(md).image_source(resolver(images.clone()))),
                    ),
                )
                .into_any_element(),
            State::Sheets { .. } => self.render_sheets(&p, cx),
            State::Slides {
                slides,
                size,
                images,
            } => {
                let mut list = v_flex().gap_4().p_4().w_full().max_w(px(1280.));
                for (ix, slide) in slides.iter().enumerate() {
                    let n = slide.index;
                    // Pins and the place being written about, on this slide.
                    let spot = |place: &Place| match *place {
                        Place::Shape { slide, shape } if slide == n => Some(Spot::Shape(shape)),
                        Place::Slide { slide, x, y } if slide == n => Some(Spot::At(x, y)),
                        Place::SlideArea {
                            slide, x, y, w, h, ..
                        } if slide == n => Some(Spot::Area { x, y, w, h }),
                        _ => None,
                    };
                    let pins: Vec<(u32, Spot)> = self
                        .notes
                        .iter()
                        .filter_map(|note| Some((note.n, spot(&note.place)?)))
                        .collect();
                    let on = Surface::Slide(ix);
                    let about = self.draft.as_ref().map(|d| &d.about.place);
                    let marks = (annotating || !pins.is_empty()).then(|| Marks {
                        accent: p.accent,
                        on_accent: p.accent_foreground,
                        pins,
                        active: about.and_then(spot),
                        picked: match about {
                            Some(Place::SlideArea { slide, shapes, .. }) if *slide == n => {
                                shapes.clone()
                            }
                            _ => Vec::new(),
                        },
                        hover: self
                            .hover
                            .filter(|(over, _)| *over == on)
                            .and_then(|(_, shape)| {
                                let shape = slide.scene.as_ref()?.shapes.get(shape)?;
                                Some((self.hover?.1, shape_tag(shape).into()))
                            }),
                        marquee: self
                            .drag
                            .as_ref()
                            .filter(|drag| drag.on == on && drag.moved)
                            .and_then(|drag| {
                                Some(fractions(slide.bounds.get()?, drag.from, drag.to))
                            }),
                        on: annotating.then(|| {
                            let view = view.clone();
                            Rc::new(move |pointer: Pointer, _: &mut Window, cx: &mut App| {
                                let _ = view.update(cx, |this, cx| this.pointer(on, pointer, cx));
                            }) as slides::OnPointer
                        }),
                        bounds: slide.bounds.clone(),
                    });
                    let card = match (&slide.scene, &slide.md) {
                        // A slide is a rectangle: no rounded corners to cut into it.
                        (Some(scene), _) => div()
                            .w_full()
                            .border_1()
                            .border_color(p.border)
                            .shadow_sm()
                            .child(slides::slide(
                                ix,
                                scene.clone(),
                                *size,
                                images.clone(),
                                marks,
                            )),
                        (None, md) => v_flex()
                            .w_full()
                            .min_h(px(240.))
                            .gap_3()
                            .px_8()
                            .py_6()
                            .rounded(px(10.))
                            .border_1()
                            .border_color(p.border)
                            .bg(p.elevated)
                            .shadow_sm()
                            .when_some(slide.title.clone(), |d, t| {
                                d.child(div().text_xl().font_weight(FontWeight::BOLD).child(t))
                            })
                            .when_some(md.as_ref(), |d, md| {
                                d.child(TextView::new(md).image_source(resolver(images.clone())))
                            }),
                    };
                    list = list.child(
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.faint_foreground)
                                    .child(format!("Slide {}", slide.index)),
                            )
                            .child(card)
                            .when_some(slide.notes.clone(), |d, notes| {
                                d.child(
                                    div()
                                        .px_3()
                                        .py_2()
                                        .rounded(px(8.))
                                        .bg(p.muted)
                                        .text_xs()
                                        .text_color(p.muted_foreground)
                                        .child(gpui_kit::base::SelectableText::new(
                                            ("slide-notes", ix),
                                            format!("Notes: {notes}"),
                                        )),
                                )
                            }),
                    );
                }
                div()
                    .id("slides-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.page_scroll)
                    .child(div().w_full().flex().justify_center().child(list))
                    .into_any_element()
            }
            State::Pdf { doc, pages } => {
                let device = window.scale_factor();
                let text_font = gpui_kit::font(window.text_style().font_family.clone());
                let selection = gpui_kit::base::Theme::global(cx).tokens.colors.selection;
                let (found, found_current): (&[Found], usize) = match &self.find {
                    Some(find) => (&find.matches, find.current),
                    None => (&[], 0),
                };
                let mut list = v_flex().gap_3().p_4().w_full().max_w(px(1000.));
                for (ix, (slot, &(pw, ph))) in pages.iter().zip(&doc.pages).enumerate() {
                    // Tells, while painting, where the page is and that it is
                    // on screen: that is when it gets its picture.
                    let sensor = canvas(
                        {
                            let view = view.clone();
                            let bounds_cell = slot.bounds.clone();
                            move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
                                bounds_cell.set(Some(bounds));
                                if window.content_mask().bounds.intersects(&bounds) {
                                    let width = (f32::from(bounds.size.width) * device).round();
                                    let _ = view.update(cx, |this, cx| {
                                        this.want_page(ix, width as u32, cx)
                                    });
                                }
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full();
                    let on = Surface::Page(ix);
                    // A place on this page: its point, or its area.
                    let here = |place: &Place| match *place {
                        Place::Page { page, x, y } if page == ix => Some((x, y, None)),
                        Place::PageArea { page, x, y, w, h } if page == ix => {
                            Some((x, y, Some((w, h))))
                        }
                        _ => None,
                    };
                    let boxed = |d: gpui_kit::Div, [x, y, w, h]: [f32; 4]| {
                        d.left(relative(x))
                            .top(relative(y))
                            .w(relative(w))
                            .h(relative(h))
                    };
                    let mut page = div()
                        .id(("pdf-page", ix))
                        .relative()
                        .w_full()
                        .aspect_ratio(pw / ph)
                        .bg(gpui_kit::white())
                        .border_1()
                        .border_color(p.border)
                        .shadow_sm()
                        .when_some(slot.image.as_ref(), |d, (image, _)| {
                            d.child(img(image.clone()).absolute().top_0().left_0().size_full())
                        })
                        .child(sensor);
                    // What a note is about, outlined, with its pin on the top
                    // right corner; a point has only the pin, its tip on it.
                    for note in &self.notes {
                        let pin = slides::pin(Some(note.n), p.accent, p.accent_foreground);
                        match here(&note.place) {
                            Some((x, y, Some((w, h)))) => {
                                page = page
                                    .child(boxed(slides::mark(p.accent), [x, y, w, h]))
                                    .child(
                                        pin.absolute()
                                            .left(relative(x + w))
                                            .top(relative(y))
                                            .ml(px(-PIN / 2.))
                                            .mt(px(-PIN / 2.)),
                                    );
                            }
                            Some((x, y, None)) => {
                                page = page.child(
                                    pin.absolute()
                                        .left(relative(x))
                                        .top(relative(y))
                                        .mt(px(-PIN)),
                                );
                            }
                            None => {}
                        }
                    }
                    match self.draft.as_ref().and_then(|d| here(&d.about.place)) {
                        Some((x, y, Some((w, h)))) => {
                            page = page.child(boxed(slides::marquee(p.accent), [x, y, w, h]));
                        }
                        Some((x, y, None)) => {
                            page = page.child(
                                slides::pin(None, p.accent, p.accent_foreground)
                                    .absolute()
                                    .left(relative(x))
                                    .top(relative(y))
                                    .ml(px(-PIN / 2.))
                                    .mt(px(-PIN / 2.)),
                            );
                        }
                        None => {}
                    }
                    // The line of text under the pointer, lit and labelled.
                    if let Some((_, line)) = self.hover.filter(|(over, _)| *over == on)
                        && let Some(span) = slot.text.as_ref().and_then(|t| t.spans.get(line))
                    {
                        let (x, y) = (span.x / pw, span.y / ph);
                        page = page
                            .child(boxed(
                                div()
                                    .absolute()
                                    .rounded(px(4.))
                                    .border_2()
                                    .border_color(p.accent)
                                    .bg(p.accent.opacity(0.12)),
                                [x, y, span.w / pw, span.h / ph],
                            ))
                            .child(
                                slides::tag("text".into(), p.accent, p.accent_foreground)
                                    .left(relative(x))
                                    .top(relative(y))
                                    .mt(px(-slides::TAG)),
                            );
                    }
                    if let Some(drag) = self.drag.as_ref().filter(|d| d.on == on && d.moved)
                        && let Some(bounds) = slot.bounds.get()
                    {
                        page = page.child(boxed(
                            slides::marquee(p.accent),
                            fractions(bounds, drag.from, drag.to),
                        ));
                    }
                    // Over the picture: the page's words, unseen, where they
                    // are drawn, so they can be selected, copied and found;
                    // and its links. Not while leaving notes, when a click
                    // means something else.
                    if !annotating
                        && let (Some(text), Some(bounds)) = (&slot.text, slot.bounds.get())
                    {
                        let scale = f32::from(bounds.size.width) / pw;
                        for (si, span) in text.spans.iter().enumerate() {
                            let said: SharedString = span.text.clone().into();
                            let run = gpui_kit::TextRun {
                                len: said.len(),
                                font: text_font.clone(),
                                color: gpui_kit::transparent_black(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            };
                            // A size at which this font sets the words as
                            // wide as the page has them.
                            let (wide, tall) = (span.w * scale, span.h * scale);
                            let at_100 = window
                                .text_system()
                                .shape_line(
                                    said.clone(),
                                    px(100.),
                                    std::slice::from_ref(&run),
                                    None,
                                )
                                .width;
                            let fit = (100. * wide / f32::from(at_100).max(1.)).clamp(1., 600.);
                            let finds = found
                                .iter()
                                .enumerate()
                                .filter(|(_, m)| m.page == ix)
                                .flat_map(|(i, m)| {
                                    m.parts
                                        .iter()
                                        .filter(move |(span, _)| *span == si)
                                        .map(move |(_, range)| (range.clone(), i == found_current))
                                })
                                .collect();
                            page = page.child(
                                div()
                                    .absolute()
                                    .left(px(span.x * scale))
                                    .top(px(span.y * scale))
                                    .h(px(tall))
                                    .whitespace_nowrap()
                                    .text_size(px(fit))
                                    .line_height(px(tall))
                                    .child(slides::hidden_text(
                                        si as u64,
                                        said,
                                        run,
                                        selection,
                                        finds,
                                        span.stops.iter().map(|s| s * scale).collect(),
                                    )),
                            );
                        }
                        for (li, link) in text.links.iter().enumerate() {
                            use loft_docs::pdf::LinkTo;
                            let (to, says) = match &link.to {
                                LinkTo::Url(url) => (link.to.clone(), url.clone()),
                                LinkTo::Page(n) => (link.to.clone(), format!("Page {}", n + 1)),
                            };
                            page = page.child(
                                div()
                                    .id(("pdf-link", li))
                                    .absolute()
                                    .left(px(link.x * scale))
                                    .top(px(link.y * scale))
                                    .w(px(link.w * scale))
                                    .h(px(link.h * scale))
                                    .cursor_pointer()
                                    .hover(|d| d.bg(p.accent.opacity(0.12)))
                                    .tooltip(move |window, cx| {
                                        gpui_kit::component::tooltip::Tooltip::new(says.clone())
                                            .build(window, cx)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| match &to {
                                        LinkTo::Url(url) => cx.open_url(url),
                                        LinkTo::Page(n) => {
                                            this.show_page(*n);
                                            cx.notify();
                                        }
                                    })),
                            );
                        }
                    }
                    if annotating {
                        let pointer = |view: &WeakEntity<Self>| {
                            let view = view.clone();
                            move |pointer: Pointer, cx: &mut App| {
                                let _ = view.update(cx, |this, cx| this.pointer(on, pointer, cx));
                            }
                        };
                        let (down, over, out) = (pointer(&view), pointer(&view), pointer(&view));
                        page = page
                            .cursor(CursorStyle::Crosshair)
                            .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                                down(Pointer::Down(event.position), cx)
                            })
                            .on_mouse_move(move |event, _, cx| {
                                if event.pressed_button.is_none() {
                                    over(Pointer::Over(event.position), cx)
                                }
                            })
                            .on_hover(move |hovered, _, cx| {
                                if !*hovered {
                                    out(Pointer::Out, cx)
                                }
                            });
                    }
                    list = list.child(
                        v_flex()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.faint_foreground)
                                    .child(format!("Page {} of {}", ix + 1, doc.pages.len())),
                            )
                            .child(page),
                    );
                }
                if doc.truncated {
                    list = list.child(
                        div()
                            .text_xs()
                            .text_color(p.faint_foreground)
                            .child(format!("Showing the first {} pages.", doc.pages.len())),
                    );
                }
                div()
                    .id("pdf-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.page_scroll)
                    .child(div().w_full().flex().justify_center().child(list))
                    .into_any_element()
            }
        };
        // Tells where the view is, for placing the note box; and, while
        // notes can be left, follows a drag wherever the pointer goes and
        // sees the button come up.
        let sensor = canvas(
            {
                let seen = self.view_bounds.clone();
                move |bounds: Bounds<Pixels>, _: &mut Window, _: &mut App| seen.set(Some(bounds))
            },
            {
                let view = view.clone();
                move |_, _, window: &mut Window, _: &mut App| {
                    if !annotating {
                        return;
                    }
                    let moved = view.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                        if phase == DispatchPhase::Capture
                            && event.pressed_button == Some(MouseButton::Left)
                        {
                            let _ = moved.update(cx, |this, cx| this.drag_to(event.position, cx));
                        }
                    });
                    let released = view.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                        if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                            let _ = released
                                .update(cx, |this, cx| this.release(event.position, window, cx));
                        }
                    });
                }
            },
        )
        .absolute()
        .inset_0();
        v_flex()
            .size_full()
            .children(self.find_bar(&p, cx))
            .children(self.note_bar(&p, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(sensor)
                    .child(div().absolute().inset_0().child(body))
                    .children(self.note_box(&p, cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::column_name;

    #[test]
    fn text_is_found_whatever_its_case() {
        use super::occurrences;
        assert_eq!(
            occurrences("Agents and an agent", "agent"),
            vec![0..5, 14..19]
        );
        assert!(occurrences("Agents", "").is_empty());
        assert!(occurrences("no match", "xyz").is_empty());
        // Where lowering the case would move the bytes, the text is searched
        // as written: places found are always places in the text.
        // A phrase that runs over a line's end is found in both lines.
        let span = |text: &str| loft_docs::pdf::TextSpan {
            text: text.to_owned(),
            x: 0.,
            y: 0.,
            w: 1.,
            h: 1.,
            stops: Vec::new(),
        };
        let lines = [span("every coding"), span("agent, side by side")];
        let found = super::find_on_page(&lines, "Coding  Agent", 4);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].page, 4);
        assert_eq!(found[0].parts, vec![(0, 6..12), (1, 0..5)]);
        let odd = "İstanbul and istanbul";
        for r in occurrences(odd, "istanbul") {
            assert_eq!(&odd[r], "istanbul");
        }
    }

    #[test]
    fn a_drag_is_an_area_of_what_it_is_on() {
        use super::{area_in_words, fractions};
        use gpui_kit::{Bounds, point, px, size};
        let slide = Bounds {
            origin: point(px(100.), px(50.)),
            size: size(px(400.), px(200.)),
        };
        // Whichever way it was dragged, and cut to the slide.
        let area = fractions(slide, point(px(300.), px(150.)), point(px(200.), px(100.)));
        assert_eq!(area, [0.25, 0.25, 0.25, 0.25]);
        let off = fractions(slide, point(px(0.), px(0.)), point(px(900.), px(150.)));
        assert_eq!(off, [0., 0., 1., 0.5]);
        assert_eq!(
            area_in_words(0.25, 0.25, 0.25, 0.25),
            "from 25% across and 25% down to 50% across and 50% down"
        );
    }

    #[test]
    fn spreadsheet_column_names() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(27), "AB");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
    }
}
