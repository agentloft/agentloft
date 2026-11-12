//! Document previews (loft-docs): Word as rendered Markdown with its images,
//! spreadsheets as a virtualized grid with sheet tabs, slide decks as the
//! slides themselves (drawn by [`super::slides`]) with speaker notes, PDFs as
//! their pages.
//!
//! Notes: with annotating on, a click on a shape, a cell or a spot on a page
//! (in a Word document, a selection) asks for a note about it. The note gets
//! a numbered pin here and is handed on as a [`DocEvent`] for the composer,
//! which sends it to the agent with the next message.

use crate::panel::slides::{self, Marks, PIN, Spot};
use crate::spinner::Spinner;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Bounds, ClickEvent, Context, CursorStyle, ElementId, Entity,
    EventEmitter, FontWeight, Image, ImageFormat, ImageSource, InteractiveElement as _,
    IntoElement, ListHorizontalSizingBehavior, MouseButton, ParentElement as _, Pixels, Render,
    RenderImage, ScrollHandle, SharedString, SharedUri, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, canvas, div, img,
    prelude::FluentBuilder as _, px, relative, uniform_list,
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
    /// A cell: the sheet's position among the visible ones, row and column
    /// from 0.
    Cell {
        sheet: usize,
        row: usize,
        col: usize,
    },
    /// A passage of a Word document; the note's `what` quotes it.
    Text,
    /// A point on a PDF page (from 0), as fractions of its width and height.
    Page { page: usize, x: f32, y: f32 },
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

/// A note being written.
struct Draft {
    place: Place,
    chip: String,
    what: String,
    input: Entity<InputState>,
    _sub: Subscription,
}

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
            let Place::Page { page, x, y } = note.place else {
                continue;
            };
            let pin = loft_docs::pdf::Pin { x, y, n: note.n };
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

    /// Opens the note field for `place`.
    fn begin_note(
        &mut self,
        place: Place,
        chip: String,
        what: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.annotating {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Note for the agent…"));
        let sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.commit_note(cx);
            }
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        self.draft = Some(Draft {
            place,
            chip,
            what,
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

    /// The note being written is done: pin it and hand it on.
    fn commit_note(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        let text = draft.input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            self.draft = Some(draft);
            return;
        }
        let note = DocNote {
            n: self.next_n,
            place: draft.place,
            chip: draft.chip,
            what: draft.what,
            note: text,
        };
        self.next_n += 1;
        self.notes.push(note.clone());
        cx.emit(DocEvent::Note(note));
        cx.notify();
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

    /// Pins, the cell a note is being written on and what a click does, for
    /// row `row` of sheet `sheet`; `None` when the row has none of them.
    fn row_marks(
        &self,
        sheet: usize,
        row: usize,
        sheet_name: &str,
        cells: &[String],
        view: &WeakEntity<Self>,
        p: &Palette,
    ) -> Option<RowMarks> {
        let here = |place: &Place| match place {
            Place::Cell {
                sheet: s,
                row: r,
                col,
            } if *s == sheet && *r == row => Some(*col),
            _ => None,
        };
        let pins: Vec<(usize, u32)> = self
            .notes
            .iter()
            .filter_map(|n| Some((here(&n.place)?, n.n)))
            .collect();
        let active = self.draft.as_ref().and_then(|d| here(&d.place));
        if !self.annotating && pins.is_empty() {
            return None;
        }
        let pick = self.annotating.then(|| {
            let view = view.clone();
            let sheet_name = sheet_name.to_owned();
            let cells = cells.to_vec();
            Rc::new(move |col: usize, window: &mut Window, cx: &mut App| {
                let at = format!("{}{}", column_name(col), row + 1);
                let mut what = format!("cell {at} on sheet “{sheet_name}”");
                match cells.get(col).map(|c| c.trim()).filter(|c| !c.is_empty()) {
                    Some(value) => what.push_str(&format!(" (“{}”)", clip(value, 80))),
                    None => what.push_str(" (empty)"),
                }
                let place = Place::Cell { sheet, row, col };
                let _ = view.update(cx, |this, cx| this.begin_note(place, at, what, window, cx));
            }) as PickCell
        });
        Some(RowMarks {
            row,
            pins,
            active,
            pick,
            accent: p.accent,
            on_accent: p.accent_foreground,
        })
    }

    /// What a click on a slide is a note about.
    fn slide_place(slide: &SlideView, spot: Spot) -> (Place, String, String) {
        use loft_docs::scene::ShapeKind;
        let n = slide.index;
        let chip = format!("Slide {n}");
        match spot {
            Spot::Shape(ix) => {
                let shape = slide.scene.as_ref().and_then(|s| s.shapes.get(ix));
                let mut what = format!("slide {n}");
                if let Some(shape) = shape {
                    let kind = match &shape.kind {
                        ShapeKind::Picture(_) => "picture".to_owned(),
                        ShapeKind::Table(_) => "table".to_owned(),
                        ShapeKind::Drawing(d) => d.what.clone(),
                        ShapeKind::Unsupported(what) => what.to_lowercase(),
                        ShapeKind::Shape => "shape".to_owned(),
                    };
                    what.push_str(&format!(", the {kind}"));
                    if !shape.name.trim().is_empty() {
                        what.push_str(&format!(" “{}”", shape.name.trim()));
                    }
                    let text = shape.plain_text();
                    if !text.is_empty() {
                        what.push_str(&format!(" (“{}”)", clip(&text, 80)));
                    } else if let Some(alt) = &shape.description {
                        what.push_str(&format!(" (described as “{}”)", clip(alt, 80)));
                    }
                }
                (
                    Place::Shape {
                        slide: n,
                        shape: ix,
                    },
                    chip,
                    what,
                )
            }
            Spot::At(x, y) => (
                Place::Slide { slide: n, x, y },
                chip,
                format!("slide {n}, {}", point_in_words(x, y)),
            ),
        }
    }

    /// The bar under the document while notes are being left: the note field,
    /// or what to click.
    fn note_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        if !self.annotating {
            return None;
        }
        let bar = h_flex()
            .flex_none()
            .w_full()
            .min_h(px(40.))
            .px_3()
            .py_1p5()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(p.border)
            .bg(p.accent.opacity(0.06))
            .text_xs();
        let button = |id: &'static str, label: &'static str, strong: bool| {
            div()
                .id(id)
                .flex_none()
                .px_2()
                .py(px(3.))
                .rounded(px(6.))
                .cursor_pointer()
                .when(strong, |d| {
                    d.bg(p.accent)
                        .text_color(p.accent_foreground)
                        .font_weight(FontWeight::MEDIUM)
                        .hover(|d| d.opacity(0.9))
                })
                .when(!strong, |d| {
                    d.text_color(p.muted_foreground)
                        .hover(|d| d.bg(p.hover).text_color(p.foreground))
                })
                .child(label)
        };
        let bar = match &self.draft {
            Some(draft) => bar
                .child(slides::pin(Some(self.next_n), p.accent, p.accent_foreground))
                .child(
                    div()
                        .flex_none()
                        .max_w(px(160.))
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(draft.chip.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&draft.input).appearance(false).small()),
                )
                .child(
                    button("note-add", "Add note", true)
                        .on_click(cx.listener(|this, _, _, cx| this.commit_note(cx))),
                )
                .child(
                    button("note-cancel", "Cancel", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.draft = None;
                            cx.notify();
                        },
                    )),
                ),
            None => bar
                .child(
                    Icon::new(IconName::MessageSquarePlus)
                        .xsmall()
                        .text_color(p.accent),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(p.muted_foreground)
                        .child(match &self.state {
                            State::Slides { .. } => {
                                "Click a shape, or anywhere on a slide, to leave a note for the agent"
                            }
                            State::Sheets { .. } => "Click a cell to leave a note for the agent",
                            State::Word { .. } => "Select some text to leave a note on it",
                            _ => "Click a spot on a page to leave a note for the agent",
                        }),
                )
                .child(
                    button("note-done", "Done", false)
                        .on_click(cx.listener(|this, _, _, cx| this.set_annotating(false, cx))),
                ),
        };
        Some(bar.into_any_element())
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
                                    let name = sheet.name.clone();
                                    range
                                        .filter_map(|r| sheet.rows.get(r).map(|row| (r, row)))
                                        .map(|(r, row)| {
                                            let cells: Vec<String> =
                                                row.iter().map(cell_text).collect();
                                            let marks =
                                                this.row_marks(active, r, &name, &cells, &view, &p);
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

/// What a click on a cell does while notes can be left: called with the
/// cell's column.
type PickCell = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// Notes on one row of a sheet.
struct RowMarks {
    row: usize,
    /// Column and pin number.
    pins: Vec<(usize, u32)>,
    /// The column of the cell a note is being written on.
    active: Option<usize>,
    pick: Option<PickCell>,
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
        let pin = marks.pins.iter().find(|(c, _)| *c == col).map(|(_, n)| *n);
        let cell = cell
            .relative()
            .when(marks.active == Some(col), |d| {
                d.bg(accent.opacity(0.14)).border_1().border_color(accent)
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
        row = match &marks.pick {
            Some(pick) => {
                let pick = pick.clone();
                // Rows share a parent: the id has to tell their cells apart.
                let id = ElementId::NamedInteger("cell".into(), (marks.row * 256 + col) as u64);
                row.child(
                    cell.id(id)
                        .cursor(CursorStyle::Crosshair)
                        .hover(|d| d.bg(accent.opacity(0.1)))
                        .on_click(move |_, window, cx| pick(col, window, cx)),
                )
            }
            None => row.child(cell),
        };
    }
    row.into_any_element()
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
                        cx.listener(|this, _, window, cx| {
                            let quote = gpui_kit::base::TextSelection::selected_text(window, cx);
                            let quote = quote.trim();
                            if quote.is_empty() {
                                return;
                            }
                            let what = format!("the text “{}”", clip(quote, 300));
                            this.begin_note(Place::Text, clip(quote, 24), what, window, cx);
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
                        _ => None,
                    };
                    let pins: Vec<(u32, Spot)> = self
                        .notes
                        .iter()
                        .filter_map(|note| Some((note.n, spot(&note.place)?)))
                        .collect();
                    let marks = (annotating || !pins.is_empty()).then(|| Marks {
                        accent: p.accent,
                        on_accent: p.accent_foreground,
                        pins,
                        active: self.draft.as_ref().and_then(|d| spot(&d.place)),
                        pick: annotating.then(|| {
                            let view = view.clone();
                            Rc::new(move |spot: Spot, window: &mut Window, cx: &mut App| {
                                let _ = view.update(cx, |this, cx| {
                                    let State::Slides { slides, .. } = &this.state else {
                                        return;
                                    };
                                    let Some(slide) = slides.get(ix) else {
                                        return;
                                    };
                                    let (place, chip, what) = Self::slide_place(slide, spot);
                                    this.begin_note(place, chip, what, window, cx);
                                });
                            }) as slides::Pick
                        }),
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
                    let here = |place: &Place| match *place {
                        Place::Page { page, x, y } if page == ix => Some((x, y)),
                        _ => None,
                    };
                    let at = |(x, y): (f32, f32), n: Option<u32>| {
                        slides::pin(n, p.accent, p.accent_foreground)
                            .absolute()
                            .left(relative(x))
                            .top(relative(y))
                            .ml(px(-PIN / 2.))
                            .mt(px(-PIN / 2.))
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
                    for note in &self.notes {
                        if let Some(spot) = here(&note.place) {
                            page = page.child(at(spot, Some(note.n)));
                        }
                    }
                    if let Some(spot) = self.draft.as_ref().and_then(|d| here(&d.place)) {
                        page = page.child(at(spot, None));
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
                        let bounds_cell = slot.bounds.clone();
                        page = page.cursor(CursorStyle::Crosshair).on_click(cx.listener(
                            move |this, event: &ClickEvent, window, cx| {
                                let Some(bounds) = bounds_cell.get() else {
                                    return;
                                };
                                let pos = event.position();
                                let (w, h) = (
                                    f32::from(bounds.size.width).max(1.),
                                    f32::from(bounds.size.height).max(1.),
                                );
                                let x = (f32::from(pos.x - bounds.origin.x) / w).clamp(0., 1.);
                                let y = (f32::from(pos.y - bounds.origin.y) / h).clamp(0., 1.);
                                let n = ix + 1;
                                this.begin_note(
                                    Place::Page { page: ix, x, y },
                                    format!("Page {n}"),
                                    format!("page {n}, {}", point_in_words(x, y)),
                                    window,
                                    cx,
                                );
                            },
                        ));
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
        v_flex()
            .size_full()
            .children(self.find_bar(&p, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(div().absolute().inset_0().child(body)),
            )
            .children(self.note_bar(&p, cx))
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
    fn spreadsheet_column_names() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(27), "AB");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
    }
}
