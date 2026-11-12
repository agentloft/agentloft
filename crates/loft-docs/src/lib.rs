//! Pure-Rust reading of documents into models, and pictures, a UI can show.
//!
//! * Word (`.docx` and friends) becomes Markdown plus the embedded images.
//! * Spreadsheets (`.xlsx`, `.xlsm`, `.xls`, `.xlsb`, `.ods`, `.csv`, `.tsv`) become
//!   typed cell grids, capped at [`MAX_ROWS`] × [`MAX_COLS`] per sheet.
//! * Slide decks (`.pptx`) become, per slide, a [`scene::Scene`] (every shape
//!   placed and styled, ready to draw) and a Markdown body, with the title and
//!   speaker notes split out.
//! * PDF files ([`pdf`]) give their page sizes, and any page as a picture.
//!
//! Apart from PDF pages this crate draws nothing, and it does not depend on
//! GPUI. Every entry point returns an error instead of panicking on malformed
//! input.

use std::fmt;
use std::path::Path;

use anyhow::{Context as _, bail};

mod chart;
mod docx;
mod dom;
mod md;
mod opc;
pub mod pdf;
mod pptx;
pub mod scene;
mod sheets;
mod slide_scene;
mod xml;

/// Files larger than this are refused by [`load`] and [`load_bytes`].
pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// At most this many rows are kept per sheet; see [`Sheet::truncated`].
pub const MAX_ROWS: usize = 10_000;
/// At most this many columns are kept per sheet; see [`Sheet::truncated`].
pub const MAX_COLS: usize = 200;

/// The broad family a file belongs to, decided by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocKind {
    /// `.docx` (also `.docm`, `.dotx`, `.dotm`).
    Word,
    /// `.xlsx`, `.xlsm`, `.xls`, `.xlsb`, `.ods`, `.csv`, `.tsv` (also `.xltx`, `.xltm`).
    Spreadsheet,
    /// `.pptx` (also `.pptm`, `.ppsx`, `.ppsm`, `.potx`, `.potm`).
    Slides,
    /// `.pdf`.
    Pdf,
}

/// A parsed document.
#[derive(Debug, Clone, PartialEq)]
pub enum Document {
    Word(WordDoc),
    Spreadsheet(Workbook),
    Slides(Deck),
    Pdf(pdf::PdfDoc),
}

impl Document {
    pub fn kind(&self) -> DocKind {
        match self {
            Document::Word(_) => DocKind::Word,
            Document::Spreadsheet(_) => DocKind::Spreadsheet,
            Document::Slides(_) => DocKind::Slides,
            Document::Pdf(_) => DocKind::Pdf,
        }
    }
}

/// A Word document converted to GitHub-flavoured Markdown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WordDoc {
    /// The text of the first `Title`-styled paragraph, falling back to the
    /// `dc:title` document property. The title paragraph is also part of
    /// [`markdown`](Self::markdown) as a level-1 heading.
    pub title: Option<String>,
    /// The document body. Images are referenced as `![alt](name)` where `name`
    /// matches an entry in [`images`](Self::images).
    pub markdown: String,
    /// Images referenced from the Markdown, each listed once.
    pub images: Vec<EmbeddedImage>,
}

/// An image stored inside a document package.
#[derive(Clone, PartialEq, Eq)]
pub struct EmbeddedImage {
    /// File name used as the Markdown image URL, unique within its document or slide.
    pub name: String,
    /// MIME type guessed from the file extension, such as `image/png`.
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for EmbeddedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmbeddedImage")
            .field("name", &self.name)
            .field("mime", &self.mime)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .finish()
    }
}

/// All sheets of a spreadsheet, in workbook order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Workbook {
    pub sheets: Vec<Sheet>,
}

/// One sheet. `rows` always starts at cell A1 and is rectangular: every row has
/// the same number of cells (`min(total_cols, MAX_COLS)`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<Cell>>,
    /// Number of rows in the sheet's used area (counted from row 1), before capping.
    pub total_rows: usize,
    /// Number of columns in the sheet's used area (counted from column A), before capping.
    pub total_cols: usize,
    /// True when `rows` holds fewer rows or columns than the sheet has.
    pub truncated: bool,
    /// True for sheets hidden in the spreadsheet application.
    pub hidden: bool,
}

/// A single cell value. `Display` renders it the way a viewer should show it:
/// integers without `.0`, at most 15 significant digits, dates in ISO 8601.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Cell {
    #[default]
    Empty,
    Text(String),
    Number(f64),
    Bool(bool),
    /// An ISO 8601 date (`2024-03-15`), date-time (`2024-03-15T14:30:00`),
    /// time (`14:30:00`) or duration.
    Date(String),
    /// A formula error such as `#DIV/0!`.
    Error(String),
}

impl fmt::Display for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cell::Empty => Ok(()),
            Cell::Text(s) | Cell::Date(s) | Cell::Error(s) => f.write_str(s),
            Cell::Number(n) => f.write_str(&sheets::format_number(*n)),
            Cell::Bool(true) => f.write_str("TRUE"),
            Cell::Bool(false) => f.write_str("FALSE"),
        }
    }
}

/// A slide deck in presentation order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Deck {
    pub slides: Vec<Slide>,
    /// Slide width and height in points.
    pub size: (f32, f32),
    /// Every picture the slides show, each listed once.
    pub images: Vec<EmbeddedImage>,
}

impl Deck {
    /// The whole deck as one Markdown document: a `## N. Title` heading per
    /// slide, its body, and its speaker notes as a block quote, separated by
    /// thematic breaks. Image URLs refer to [`images`](Self::images).
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        for (i, slide) in self.slides.iter().enumerate() {
            if i > 0 {
                out.push_str("\n\n---\n\n");
            }
            out.push_str(&format!("## {}.", slide.index));
            if let Some(title) = &slide.title {
                out.push(' ');
                out.push_str(&md::escape_line(title));
            }
            if !slide.markdown.is_empty() {
                out.push_str("\n\n");
                out.push_str(&slide.markdown);
            }
            if let Some(notes) = &slide.notes {
                out.push_str("\n\n");
                let quoted: Vec<String> = notes
                    .lines()
                    .map(|l| {
                        if l.trim().is_empty() {
                            ">".to_string()
                        } else {
                            format!("> {}", md::escape_line(l))
                        }
                    })
                    .collect();
                out.push_str(&quoted.join("\n"));
            }
        }
        out
    }
}

/// One slide.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Slide {
    /// 1-based position in the deck.
    pub index: usize,
    /// Plain text of the title (or centred title) placeholder. It is *not*
    /// repeated in [`markdown`](Self::markdown).
    pub title: Option<String>,
    /// The slide body: text frames as (nested) bullets, tables as GitHub
    /// tables, pictures as `![alt](name)` referring to [`Deck::images`].
    pub markdown: String,
    /// Speaker notes as plain text, one paragraph per line.
    pub notes: Option<String>,
    /// The slide as drawn; `None` when its layout could not be read, in which
    /// case [`markdown`](Self::markdown) is what there is to show.
    pub scene: Option<scene::Scene>,
}

/// Decides how a file should be parsed from its extension (case-insensitive).
/// Returns `None` for unsupported files, including legacy `.doc` and `.ppt`.
pub fn kind_for(path: &Path) -> Option<DocKind> {
    let ext = path.extension()?.to_str()?;
    kind_for_ext(ext)
}

fn kind_for_ext(ext: &str) -> Option<DocKind> {
    let ext = normalize_ext(ext);
    Some(match ext.as_str() {
        "docx" | "docm" | "dotx" | "dotm" => DocKind::Word,
        "xlsx" | "xlsm" | "xltx" | "xltm" | "xls" | "xlsb" | "ods" | "csv" | "tsv" => {
            DocKind::Spreadsheet
        }
        "pptx" | "pptm" | "ppsx" | "ppsm" | "potx" | "potm" => DocKind::Slides,
        "pdf" => DocKind::Pdf,
        _ => return None,
    })
}

fn normalize_ext(ext: &str) -> String {
    ext.trim().trim_start_matches('.').to_ascii_lowercase()
}

/// Reads and parses a document. Files over [`MAX_FILE_BYTES`] are refused
/// before being read.
pub fn load(path: &Path) -> anyhow::Result<Document> {
    let kind = kind_for(path).with_context(|| {
        format!(
            "{} is not a supported document type (expected docx, pptx, pdf, xlsx, xlsm, xls, xlsb, ods, csv or tsv)",
            path.display()
        )
    })?;
    let meta =
        std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    check_size(meta.len())?;
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    let mut doc =
        load_bytes(kind, ext, &bytes).with_context(|| format!("cannot open {}", path.display()))?;
    // Plain-text tables carry no sheet name of their own; use the file name.
    if let Document::Spreadsheet(book) = &mut doc
        && matches!(normalize_ext(ext).as_str(), "csv" | "tsv")
        && let (Some(sheet), Some(stem)) = (
            book.sheets.first_mut(),
            path.file_stem().and_then(|s| s.to_str()),
        )
    {
        sheet.name = stem.to_string();
    }
    Ok(doc)
}

/// Parses a document already in memory. `ext` (with or without a leading dot)
/// picks the spreadsheet reader and the CSV/TSV delimiter; for Word and Slides
/// it is ignored.
pub fn load_bytes(kind: DocKind, ext: &str, bytes: &[u8]) -> anyhow::Result<Document> {
    check_size(bytes.len() as u64)?;
    let ext = normalize_ext(ext);
    // The parsers here are written not to panic, but calamine and zip are
    // third-party code fed untrusted input: turn any panic into an error.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match kind {
        DocKind::Word => docx::parse(bytes).map(Document::Word),
        DocKind::Spreadsheet => sheets::parse(&ext, bytes).map(Document::Spreadsheet),
        DocKind::Slides => pptx::parse(bytes).map(Document::Slides),
        DocKind::Pdf => pdf::parse(bytes.to_vec()).map(Document::Pdf),
    }));
    match result {
        Ok(r) => r,
        Err(_) => bail!("the document is malformed and could not be parsed"),
    }
}

fn check_size(len: u64) -> anyhow::Result<()> {
    if len > MAX_FILE_BYTES {
        bail!(
            "the file is too large to preview ({:.1} MB; the limit is {} MB)",
            len as f64 / (1024.0 * 1024.0),
            MAX_FILE_BYTES / (1024 * 1024)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_extension() {
        assert_eq!(kind_for(Path::new("a/b.DOCX")), Some(DocKind::Word));
        assert_eq!(kind_for(Path::new("x.xlsm")), Some(DocKind::Spreadsheet));
        assert_eq!(kind_for(Path::new("x.tsv")), Some(DocKind::Spreadsheet));
        assert_eq!(kind_for(Path::new("x.ods")), Some(DocKind::Spreadsheet));
        assert_eq!(kind_for(Path::new("deck.pptx")), Some(DocKind::Slides));
        assert_eq!(kind_for(Path::new("paper.PDF")), Some(DocKind::Pdf));
        assert_eq!(kind_for(Path::new("old.doc")), None);
        assert_eq!(kind_for(Path::new("noext")), None);
    }

    #[test]
    fn cell_display() {
        assert_eq!(Cell::Number(3.0).to_string(), "3");
        assert_eq!(Cell::Number(-0.0).to_string(), "0");
        assert_eq!(Cell::Number(0.1 + 0.2).to_string(), "0.3");
        assert_eq!(Cell::Number(1234.5).to_string(), "1234.5");
        assert_eq!(Cell::Number(1e20).to_string(), "1E+20");
        assert_eq!(Cell::Bool(true).to_string(), "TRUE");
        assert_eq!(Cell::Empty.to_string(), "");
        assert_eq!(Cell::Date("2024-01-02".into()).to_string(), "2024-01-02");
    }

    #[test]
    fn rejects_oversized_input() {
        let err = check_size(MAX_FILE_BYTES + 1).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        for kind in [
            DocKind::Word,
            DocKind::Slides,
            DocKind::Spreadsheet,
            DocKind::Pdf,
        ] {
            for ext in ["docx", "pptx", "xlsx", "xls", "xlsb", "ods", "pdf"] {
                assert!(load_bytes(kind, ext, b"definitely not a zip").is_err());
                assert!(load_bytes(kind, ext, b"").is_err());
            }
        }
    }
}
