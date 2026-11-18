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
            check_xls(bytes)?;
            let rs = Cursor::new(bytes);
            match Xls::new(rs) {
                Ok(mut book) => Ok(from_ranges(&mut book)),
                // Files named .xls are often really xlsx or HTML; try the rest.
                Err(e) => parse_any(bytes).map_err(|_| anyhow!("not a readable .xls file: {e}")),
            }
        }
        "ods" => {
            check_ods(bytes)?;
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
    check_xls(bytes)?;
    if let Ok(mut book) = Xls::new(Cursor::new(bytes)) {
        return Ok(from_ranges(&mut book));
    }
    check_ods(bytes)?;
    if let Ok(mut book) = Ods::<_>::new(Cursor::new(bytes)) {
        return Ok(from_ranges(&mut book));
    }
    Err(anyhow!("not a readable spreadsheet"))
}

/// Most cells a sheet read whole may span. calamine reads `.xls` and `.ods`
/// sheets whole, into a grid as large as the cells it finds are far apart
/// (and, for `.xls`, as the sheet says it is): a few kilobytes naming a
/// cell in the far corner asked for a grid of billions, and an allocation
/// that fails ends the whole app. What is shown is capped far lower anyway.
const MAX_SHEET_CELLS: u64 = 10_000_000;

fn too_large() -> anyhow::Error {
    anyhow!("this spreadsheet is too large to preview")
}

/// Refuse an `.xls` whose sheets span more than [`MAX_SHEET_CELLS`], before
/// calamine reads it. Not an `.xls` (no workbook stream): `Ok`, for the
/// next reader to try.
fn check_xls(bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Read as _;
    let Ok(mut file) = cfb::CompoundFile::open(Cursor::new(bytes)) else {
        return Ok(());
    };
    let Some(mut stream) = ["/Workbook", "/Book"]
        .iter()
        .find_map(|name| file.open_stream(name).ok())
    else {
        return Ok(());
    };
    let mut data = Vec::new();
    // A stream can't be longer than the file holding it.
    stream
        .by_ref()
        .take(bytes.len() as u64)
        .read_to_end(&mut data)?;
    if xls_span(&data) > MAX_SHEET_CELLS {
        return Err(too_large());
    }
    Ok(())
}

/// The most cells any sheet of a BIFF workbook stream spans: what its
/// Dimensions record says, or the box around its cells, whichever is more.
fn xls_span(data: &[u8]) -> u64 {
    let u16_at = |d: &[u8], at: usize| d.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |d: &[u8], at: usize| {
        d.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut most = 0u64;
    // (min row, max row, min col, max col) of the sheet being read.
    let mut bounds: Option<(u64, u64, u64, u64)> = None;
    let mut depth = 0usize;
    let mut at = 0usize;
    let close = |bounds: &mut Option<(u64, u64, u64, u64)>, most: &mut u64| {
        if let Some((r0, r1, c0, c1)) = bounds.take() {
            *most = (*most).max((r1 - r0 + 1).saturating_mul(c1 - c0 + 1));
        }
    };
    while let (Some(typ), Some(len)) = (u16_at(data, at), u16_at(data, at + 2)) {
        let start = at + 4;
        let end = (start + usize::from(len)).min(data.len());
        let record = &data[start..end];
        at = start + usize::from(len);
        let mut cell = |row: u64, first: u64, last: u64| {
            let b = bounds.get_or_insert((row, row, first, last));
            *b = (b.0.min(row), b.1.max(row), b.2.min(first), b.3.max(last));
        };
        match typ {
            // BOF: a sheet starts (or a chart inside one).
            0x0809 | 0x0409 | 0x0209 | 0x0009 => {
                if depth == 0 {
                    close(&mut bounds, &mut most);
                }
                depth += 1;
            }
            // EOF
            0x000A => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close(&mut bounds, &mut most);
                }
            }
            // Dimensions: BIFF8 has 32-bit rows, older ones 16-bit.
            0x0200 => {
                let (r0, r1, c0, c1) = if record.len() >= 14 {
                    (
                        u32_at(record, 0),
                        u32_at(record, 4),
                        u16_at(record, 8),
                        u16_at(record, 10),
                    )
                        .into()
                } else {
                    (
                        u16_at(record, 0).map(u32::from),
                        u16_at(record, 2).map(u32::from),
                        u16_at(record, 4),
                        u16_at(record, 6),
                    )
                        .into()
                };
                if let (Some(r0), Some(r1), Some(c0), Some(c1)) = (r0, r1, c0, c1) {
                    let rows = u64::from(r1.saturating_sub(r0)) + 1;
                    let cols = u64::from(c1.saturating_sub(c0)) + 1;
                    most = most.max(rows.saturating_mul(cols));
                }
            }
            // A run of cells in one row: first column, …, last column.
            0x00BD | 0x00BE => {
                if let (Some(row), Some(first), Some(last)) = (
                    u16_at(record, 0),
                    u16_at(record, 2),
                    record.len().checked_sub(2).and_then(|l| u16_at(record, l)),
                ) {
                    cell(row.into(), first.into(), u64::from(last.max(first)));
                }
            }
            // One cell: row, column.
            0x0006 | 0x0201 | 0x0203 | 0x0204 | 0x0205 | 0x00D6 | 0x027E | 0x00FD => {
                if let (Some(row), Some(col)) = (u16_at(record, 0), u16_at(record, 2)) {
                    cell(row.into(), col.into(), col.into());
                }
            }
            _ => {}
        }
    }
    close(&mut bounds, &mut most);
    most
}

/// Refuse an `.ods` whose sheets come to more than [`MAX_SHEET_CELLS`] once
/// repeated rows and cells are counted, before calamine reads it.
fn check_ods(bytes: &[u8]) -> anyhow::Result<()> {
    use quick_xml::events::Event;
    let Ok(mut package) = crate::opc::Package::open(bytes) else {
        return Ok(());
    };
    let Ok(Some(content)) = package.read_opt("content.xml") else {
        return Ok(());
    };
    let repeat = |e: &quick_xml::events::BytesStart<'_>, name: &str| -> u64 {
        e.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .and_then(|a| a.value.trim().parse().ok())
            .unwrap_or(1u64)
            .max(1)
    };
    let mut reader = quick_xml::Reader::from_reader(content.as_slice());
    let mut buf = Vec::new();
    let (mut sheet, mut row_repeat, mut row_cells) = (0u64, 1u64, 0u64);
    loop {
        let event = reader.read_event_into(&mut buf);
        let Ok(event) = event else {
            return Ok(());
        };
        match &event {
            Event::Start(e) | Event::Empty(e) => match e.name().as_ref() {
                "table:table" => sheet = 0,
                "table:table-row" => {
                    row_repeat = repeat(e, "table:number-rows-repeated");
                    row_cells = 0;
                    if matches!(event, Event::Empty(_)) {
                        sheet = sheet.saturating_add(row_repeat);
                    }
                }
                "table:table-cell" | "table:covered-table-cell" => {
                    row_cells =
                        row_cells.saturating_add(repeat(e, "table:number-columns-repeated"));
                }
                _ => {}
            },
            Event::End(e) if e.name().as_ref() == "table:table-row" => {
                sheet = sheet.saturating_add(row_repeat.saturating_mul(row_cells.max(1)));
                if sheet > MAX_SHEET_CELLS {
                    return Err(too_large());
                }
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
        buf.clear();
    }
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

#[cfg(test)]
mod size_tests {
    use super::*;
    use std::io::Write as _;

    fn record(typ: u16, data: &[u8]) -> Vec<u8> {
        let mut out = typ.to_le_bytes().to_vec();
        out.extend((data.len() as u16).to_le_bytes());
        out.extend(data);
        out
    }

    fn number(row: u16, col: u16) -> Vec<u8> {
        let mut data = row.to_le_bytes().to_vec();
        data.extend(col.to_le_bytes());
        data.extend([0u8; 10]);
        record(0x0203, &data)
    }

    /// An `.xls` (a compound file with a Workbook stream) of `records`.
    fn xls(records: &[Vec<u8>]) -> Vec<u8> {
        let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        let mut stream = file.create_stream("/Workbook").unwrap();
        stream.write_all(&records.concat()).unwrap();
        drop(stream);
        file.into_inner().into_inner()
    }

    fn sheet(cells: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut out = vec![record(0x0809, &[0; 16])];
        out.extend(cells);
        out.push(record(0x000A, &[]));
        out
    }

    #[test]
    fn a_sheet_spanning_billions_of_cells_is_refused_before_it_is_read() {
        // Its Dimensions record says 2^32 rows by 65535 columns.
        let mut dims = 0u32.to_le_bytes().to_vec();
        dims.extend(u32::MAX.to_le_bytes());
        dims.extend(0u16.to_le_bytes());
        dims.extend(u16::MAX.to_le_bytes());
        dims.extend([0, 0]);
        let declared = xls(&sheet(vec![record(0x0200, &dims), number(0, 0)]));
        // Two cells in opposite corners.
        let sparse = xls(&sheet(vec![number(0, 0), number(65535, 255)]));
        for bytes in [&declared, &sparse] {
            for ext in ["xls", "bin"] {
                let err = parse(ext, bytes).unwrap_err();
                assert!(err.to_string().contains("too large"), "{ext}: {err}");
            }
            // Named as another kind: refused all the same (that kind's own
            // error is the one told).
            for ext in ["xlsx", "xlsb"] {
                assert!(parse(ext, bytes).is_err(), "{ext}");
            }
        }
        // An ordinary sheet is measured as it is.
        let small = sheet(vec![number(0, 0), number(99, 9)]).concat();
        assert_eq!(xls_span(&small), 1000);
    }

    #[test]
    fn an_ods_repeating_rows_into_billions_is_refused() {
        let content = r#"<?xml version="1.0"?><office:document-content><office:body><office:spreadsheet>
            <table:table table:name="S"><table:table-row table:number-rows-repeated="1048576">
            <table:table-cell table:number-columns-repeated="16384" office:value-type="float" office:value="1"/>
            </table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("mimetype", options).unwrap();
        zip.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
            .unwrap();
        zip.start_file("content.xml", options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let err = parse("ods", &bytes).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }
}
