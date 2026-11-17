//! Spreadsheets via calamine, plus CSV/TSV.

use std::io::Cursor;

use anyhow::{Context as _, anyhow};
use calamine::{
    Data, DataRef, ExcelDateTime, Ods, Range, Reader, SheetType, SheetVisible, Xls, Xlsb, Xlsx,
};

use crate::{Cell, MAX_COLS, MAX_ROWS, Sheet, Workbook};

pub fn parse(ext: &str, bytes: &[u8]) -> anyhow::Result<Workbook> {
    match ext {
        "csv" => Ok(parse_delimited(bytes, b',')),
        "tsv" | "tab" => Ok(parse_delimited(bytes, b'\t')),
        "xlsx" | "xlsm" | "xltx" | "xltm" | "xlam" => {
            parse_xlsx(bytes).or_else(|e| parse_any(bytes).map_err(|_| e))
        }
        "xlsb" => parse_xlsb(bytes).or_else(|e| parse_any(bytes).map_err(|_| e)),
        "xls" | "xla" => {
            let rs = Cursor::new(bytes);
            match Xls::new(rs) {
                Ok(mut book) => Ok(from_ranges(&mut book)),
                // Files named .xls are often really xlsx or HTML; try the rest.
                Err(e) => parse_any(bytes).map_err(|_| anyhow!("not a readable .xls file: {e}")),
            }
        }
        "ods" => {
            let mut book: Ods<_> = Ods::new(Cursor::new(bytes))
                .map_err(|e| anyhow!("not a readable OpenDocument spreadsheet: {e}"))?;
            Ok(from_ranges(&mut book))
        }
        _ => parse_any(bytes),
    }
}

fn parse_any(bytes: &[u8]) -> anyhow::Result<Workbook> {
    if let Ok(book) = parse_xlsx(bytes) {
        return Ok(book);
    }
    if let Ok(book) = parse_xlsb(bytes) {
        return Ok(book);
    }
    if let Ok(mut book) = Xls::new(Cursor::new(bytes)) {
        return Ok(from_ranges(&mut book));
    }
    if let Ok(mut book) = Ods::<_>::new(Cursor::new(bytes)) {
        return Ok(from_ranges(&mut book));
    }
    Err(anyhow!("not a readable spreadsheet"))
}

/// Accumulates cells streamed in any order into a capped, A1-anchored grid.
struct Grid {
    rows: Vec<Vec<Cell>>,
    total_rows: usize,
    total_cols: usize,
}

impl Grid {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            total_rows: 0,
            total_cols: 0,
        }
    }

    fn set(&mut self, row: usize, col: usize, cell: Cell) {
        if matches!(cell, Cell::Empty) {
            return;
        }
        self.total_rows = self.total_rows.max(row + 1);
        self.total_cols = self.total_cols.max(col + 1);
        if row >= MAX_ROWS || col >= MAX_COLS {
            return;
        }
        if self.rows.len() <= row {
            self.rows.resize_with(row + 1, Vec::new);
        }
        let r = &mut self.rows[row];
        if r.len() <= col {
            r.resize(col + 1, Cell::Empty);
        }
        r[col] = cell;
    }

    fn into_sheet(self, name: String, hidden: bool) -> Sheet {
        let width = self.total_cols.min(MAX_COLS);
        let mut rows = self.rows;
        rows.resize_with(self.total_rows.min(MAX_ROWS), Vec::new);
        for r in &mut rows {
            r.resize(width, Cell::Empty);
        }
        Sheet {
            name,
            rows,
            total_rows: self.total_rows,
            total_cols: self.total_cols,
            truncated: self.total_rows > MAX_ROWS || self.total_cols > MAX_COLS,
            hidden,
        }
    }
}

fn is_hidden(v: SheetVisible) -> bool {
    !matches!(v, SheetVisible::Visible)
}

/// Worksheets only: chart sheets and VBA modules carry no cells.
fn wanted(typ: SheetType) -> bool {
    matches!(
        typ,
        SheetType::WorkSheet | SheetType::DialogSheet | SheetType::MacroSheet
    )
}

fn parse_xlsx(bytes: &[u8]) -> anyhow::Result<Workbook> {
    let mut book: Xlsx<_> =
        Xlsx::new(Cursor::new(bytes)).map_err(|e| anyhow!("not a readable Excel workbook: {e}"))?;
    let metas = book.sheets_metadata().to_vec();
    let mut sheets = Vec::new();
    for meta in metas.into_iter().filter(|m| wanted(m.typ)) {
        let mut grid = Grid::new();
        let mut reader = book
            .worksheet_cells_reader(&meta.name)
            .with_context(|| format!("cannot read sheet {:?}", meta.name))?;
        while let Some(cell) = reader
            .next_cell()
            .map_err(|e| anyhow!("malformed sheet {:?}: {e}", meta.name))?
        {
            let (r, c) = cell.get_position();
            grid.set(r as usize, c as usize, from_data_ref(cell.get_value()));
        }
        drop(reader);
        sheets.push(grid.into_sheet(meta.name, is_hidden(meta.visible)));
    }
    Ok(Workbook { sheets })
}

fn parse_xlsb(bytes: &[u8]) -> anyhow::Result<Workbook> {
    let mut book: Xlsb<_> = Xlsb::new(Cursor::new(bytes))
        .map_err(|e| anyhow!("not a readable Excel binary workbook: {e}"))?;
    let metas = book.sheets_metadata().to_vec();
    let mut sheets = Vec::new();
    for meta in metas.into_iter().filter(|m| wanted(m.typ)) {
        let mut grid = Grid::new();
        let mut reader = book
            .worksheet_cells_reader(&meta.name)
            .with_context(|| format!("cannot read sheet {:?}", meta.name))?;
        while let Some(cell) = reader
            .next_cell()
            .map_err(|e| anyhow!("malformed sheet {:?}: {e}", meta.name))?
        {
            let (r, c) = cell.get_position();
            grid.set(r as usize, c as usize, from_data_ref(cell.get_value()));
        }
        drop(reader);
        sheets.push(grid.into_sheet(meta.name, is_hidden(meta.visible)));
    }
    Ok(Workbook { sheets })
}

/// For readers without a streaming API (xls, ods). Sheets that fail to read
/// are kept, empty, so the tab list still matches the workbook.
fn from_ranges<R, RS>(book: &mut R) -> Workbook
where
    R: Reader<RS>,
    RS: std::io::Read + std::io::Seek,
{
    let metas = book.sheets_metadata().to_vec();
    let mut sheets = Vec::new();
    for meta in metas.into_iter().filter(|m| wanted(m.typ)) {
        let mut grid = Grid::new();
        if let Ok(range) = book.worksheet_range(&meta.name) {
            fill_from_range(&mut grid, &range);
        }
        sheets.push(grid.into_sheet(meta.name, is_hidden(meta.visible)));
    }
    Workbook { sheets }
}

fn fill_from_range(grid: &mut Grid, range: &Range<Data>) {
    let (r0, c0) = range.start().unwrap_or((0, 0));
    for (r, c, value) in range.used_cells() {
        grid.set(r + r0 as usize, c + c0 as usize, from_data(value));
    }
}

fn from_data(d: &Data) -> Cell {
    match d {
        Data::Empty => Cell::Empty,
        Data::String(s) if s.is_empty() => Cell::Empty,
        Data::String(s) => Cell::Text(s.clone()),
        Data::Int(i) => Cell::Number(*i as f64),
        Data::Float(f) => Cell::Number(*f),
        Data::Bool(b) => Cell::Bool(*b),
        Data::DateTime(dt) => Cell::Date(format_datetime(dt)),
        Data::DateTimeIso(s) | Data::DurationIso(s) => Cell::Date(s.clone()),
        Data::Error(e) => Cell::Error(e.to_string()),
    }
}

fn from_data_ref(d: &DataRef<'_>) -> Cell {
    match d {
        DataRef::Empty => Cell::Empty,
        DataRef::String(s) if s.is_empty() => Cell::Empty,
        DataRef::SharedString("") => Cell::Empty,
        DataRef::String(s) => Cell::Text(s.clone()),
        DataRef::SharedString(s) => Cell::Text((*s).to_string()),
        DataRef::Int(i) => Cell::Number(*i as f64),
        DataRef::Float(f) => Cell::Number(*f),
        DataRef::Bool(b) => Cell::Bool(*b),
        DataRef::DateTime(dt) => Cell::Date(format_datetime(dt)),
        DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => Cell::Date(s.clone()),
        DataRef::Error(e) => Cell::Error(e.to_string()),
    }
}

/// ISO 8601: `2024-03-15`, `2024-03-15T14:30:00`, `14:30:00` for pure times,
/// and `[h]:mm:ss` for durations.
fn format_datetime(dt: &ExcelDateTime) -> String {
    let value = dt.as_f64();
    if !value.is_finite() {
        return value.to_string();
    }
    if dt.is_duration() {
        let total = (value * 86_400.0).round();
        let sign = if total < 0.0 { "-" } else { "" };
        let total = total.abs().min(1e15) as u64;
        return format!(
            "{sign}{}:{:02}:{:02}",
            total / 3600,
            (total / 60) % 60,
            total % 60
        );
    }
    let (y, mo, d, h, mi, s, ms) = dt.to_ymd_hms_milli();
    let time = if ms > 0 {
        format!("{h:02}:{mi:02}:{s:02}.{ms:03}")
    } else {
        format!("{h:02}:{mi:02}:{s:02}")
    };
    if (0.0..1.0).contains(&value) {
        time
    } else if h == 0 && mi == 0 && s == 0 && ms == 0 {
        format!("{y:04}-{mo:02}-{d:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{time}")
    }
}

/// Formats a number the way a spreadsheet's General format would: integers
/// without a fraction, otherwise at most 15 significant digits.
pub fn format_number(n: f64) -> String {
    if !n.is_finite() {
        return n.to_string();
    }
    if n == 0.0 {
        return "0".to_string();
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    // Round to 15 significant digits, then print the shortest exact form.
    let rounded: f64 = format!("{n:.14e}").parse().unwrap_or(n);
    let abs = rounded.abs();
    if (1e-9..1e15).contains(&abs) {
        return rounded.to_string();
    }
    // Very large or small: scientific notation, as spreadsheets show it.
    let sci = format!("{rounded:e}");
    match sci.split_once('e') {
        Some((mantissa, exp)) => {
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp),
            };
            format!("{mantissa}E{sign}{digits:0>2}")
        }
        None => sci,
    }
}

// ---------------------------------------------------------------------------
// CSV / TSV

fn parse_delimited(bytes: &[u8], delimiter: u8) -> Workbook {
    let text = crate::opc::decode_text(bytes.to_vec());
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut grid = Grid::new();
    let mut record = csv::ByteRecord::new();
    let mut row = 0usize;
    // With `flexible` only I/O errors are possible, and the input is in memory.
    while let Ok(true) = reader.read_byte_record(&mut record) {
        for (col, field) in record.iter().enumerate() {
            if row < MAX_ROWS && col < MAX_COLS {
                grid.set(row, col, infer_cell(&String::from_utf8_lossy(field)));
            } else if !field.trim_ascii().is_empty() {
                // Beyond the caps: only count it.
                grid.total_rows = grid.total_rows.max(row + 1);
                grid.total_cols = grid.total_cols.max(col + 1);
            }
        }
        row += 1;
    }
    Workbook {
        sheets: vec![grid.into_sheet("Sheet1".to_string(), false)],
    }
}

/// CSV fields are text; numbers and booleans are recognised only when
/// displaying the parsed value would show exactly the same text.
fn infer_cell(raw: &str) -> Cell {
    let s = raw.trim();
    if s.is_empty() {
        return Cell::Empty;
    }
    let looks_numeric = s
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'.' | b'e' | b'E' | b'+'));
    if looks_numeric
        && let Ok(n) = s.parse::<f64>()
        && n.is_finite()
        && format_number(n) == s
    {
        return Cell::Number(n);
    }
    match s {
        "TRUE" | "true" | "True" => Cell::Bool(true),
        "FALSE" | "false" | "False" => Cell::Bool(false),
        _ => Cell::Text(raw.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(format_number(42.0), "42");
        assert_eq!(format_number(-7.0), "-7");
        assert_eq!(format_number(2.5), "2.5");
        assert_eq!(format_number(1.0 / 3.0), "0.333333333333333");
        assert_eq!(format_number(1e-7), "0.0000001");
        assert_eq!(format_number(f64::NAN), "NaN");
        assert_eq!(format_number(1e20), "1E+20");
        assert_eq!(format_number(1.5e-12), "1.5E-12");
    }

    #[test]
    fn dates() {
        use calamine::ExcelDateTimeType::{DateTime, TimeDelta};
        assert_eq!(
            format_datetime(&ExcelDateTime::new(45366.0, DateTime, false)),
            "2024-03-15"
        );
        assert_eq!(
            format_datetime(&ExcelDateTime::new(45366.5, DateTime, false)),
            "2024-03-15T12:00:00"
        );
        assert_eq!(
            format_datetime(&ExcelDateTime::new(0.75, DateTime, false)),
            "18:00:00"
        );
        assert_eq!(
            format_datetime(&ExcelDateTime::new(1.5, TimeDelta, false)),
            "36:00:00"
        );
    }

    #[test]
    fn csv_inference() {
        assert_eq!(infer_cell("42"), Cell::Number(42.0));
        assert_eq!(infer_cell("1.5"), Cell::Number(1.5));
        assert_eq!(infer_cell("1.50"), Cell::Text("1.50".into()));
        assert_eq!(infer_cell("007"), Cell::Text("007".into()));
        assert_eq!(infer_cell("true"), Cell::Bool(true));
        assert_eq!(infer_cell("hello"), Cell::Text("hello".into()));
        assert_eq!(infer_cell("  "), Cell::Empty);
    }
}
