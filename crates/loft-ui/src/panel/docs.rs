//! Office document previews (loft-docs): Word as rendered Markdown with its
//! images, spreadsheets as a virtualized grid with sheet tabs, slide decks as
//! slide cards with speaker notes.

use crate::spinner::Spinner;
use crate::theme::Palette;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, Image, ImageFormat, ImageSource,
    InteractiveElement as _, IntoElement, ListHorizontalSizingBehavior, ParentElement as _, Render,
    SharedString, SharedUri, StatefulInteractiveElement as _, Styled as _, UniformListScrollHandle,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px, uniform_list,
};
use loft_docs::{Cell, Document, EmbeddedImage, Sheet};
use std::collections::HashMap;
use std::sync::Arc;

pub use loft_docs::DocKind;

pub fn doc_kind(path: &str) -> Option<DocKind> {
    loft_docs::kind_for(std::path::Path::new(path))
}

const COL_WIDTH: f32 = 132.;
const ROW_HEIGHT: f32 = 26.;
const GUTTER: f32 = 52.;

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
        slides: Vec<(loft_docs::Slide, Entity<TextViewState>)>,
        images: Arc<HashMap<String, Arc<Image>>>,
    },
    Error(String),
}

pub struct DocPreview {
    state: State,
    grid_scroll: UniformListScrollHandle,
}

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
        Self {
            state: State::Loading,
            grid_scroll: UniformListScrollHandle::new(),
        }
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
                let mut all = HashMap::new();
                let slides = deck
                    .slides
                    .into_iter()
                    .map(|s| {
                        all.extend(image_map(&s.images));
                        let md =
                            cx.new(|cx| TextViewState::markdown(&s.markdown, cx).selectable(true));
                        (s, md)
                    })
                    .collect();
                State::Slides {
                    slides,
                    images: Arc::new(all),
                }
            }
        }
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
        let header = grid_row(None, (0..cols).map(column_name).collect(), p, true);
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
                                    range
                                        .filter_map(|r| sheet.rows.get(r).map(|row| (r, row)))
                                        .map(|(r, row)| {
                                            grid_row(
                                                Some(r + 1),
                                                row.iter().map(cell_text).collect(),
                                                &p,
                                                false,
                                            )
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

fn grid_row(
    number: Option<usize>,
    cells: Vec<String>,
    p: &Palette,
    header: bool,
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
    for text in cells {
        let numeric = !header && text.parse::<f64>().is_ok();
        row = row.child(
            div()
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
                .child(div().truncate().child(text)),
        );
    }
    row.into_any_element()
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        match &self.state {
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
            State::Slides { slides, images } => {
                let mut list = v_flex().gap_4().p_4();
                for (slide, md) in slides {
                    list = list.child(
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.faint_foreground)
                                    .child(format!("Slide {}", slide.index)),
                            )
                            .child(
                                v_flex()
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
                                        d.child(
                                            div().text_xl().font_weight(FontWeight::BOLD).child(t),
                                        )
                                    })
                                    .child(
                                        TextView::new(md).image_source(resolver(images.clone())),
                                    ),
                            )
                            .when_some(
                                slide.notes.clone().filter(|n| !n.trim().is_empty()),
                                |d, notes| {
                                    d.child(
                                        div()
                                            .px_3()
                                            .py_2()
                                            .rounded(px(8.))
                                            .bg(p.muted)
                                            .text_xs()
                                            .text_color(p.muted_foreground)
                                            .child(format!("Notes: {notes}")),
                                    )
                                },
                            ),
                    );
                }
                div()
                    .id("slides-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(list)
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::column_name;

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
