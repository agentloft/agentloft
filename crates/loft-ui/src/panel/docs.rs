//! Office document previews (Word, spreadsheets, slides).
//! Placeholder until `loft-docs` is integrated.

use crate::theme::Palette;
use gpui_kit::component::v_flex;
use gpui_kit::{Context, IntoElement, ParentElement as _, Render, Styled as _, Window};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Word,
    Spreadsheet,
    Slides,
}

pub fn doc_kind(path: &str) -> Option<DocKind> {
    match crate::panel::viewer::extension(path).as_str() {
        "docx" => Some(DocKind::Word),
        "xlsx" | "xlsm" | "xls" | "xlsb" | "ods" | "csv" | "tsv" => Some(DocKind::Spreadsheet),
        "pptx" => Some(DocKind::Slides),
        _ => None,
    }
}

pub struct DocPreview {
    kind: DocKind,
}

impl DocPreview {
    pub fn new(
        kind: DocKind,
        _name: &str,
        _bytes: Vec<u8>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self { kind }
    }
}

impl Render for DocPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(p.muted_foreground)
            .child(format!("{:?} preview is loading…", self.kind))
    }
}
