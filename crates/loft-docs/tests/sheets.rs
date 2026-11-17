mod common;

use common::*;
use loft_docs::{Cell, DocKind, Document, MAX_COLS, MAX_ROWS, Workbook, load, load_bytes};

fn book(ext: &str, bytes: &[u8]) -> Workbook {
    match load_bytes(DocKind::Spreadsheet, ext, bytes).expect("spreadsheet parses") {
        Document::Spreadsheet(b) => b,
        other => panic!("expected a workbook, got {other:?}"),
    }
}

#[test]
fn two_sheet_xlsx() {
    let data = concat!(
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="inlineStr"><is><t>Flag</t></is></c><c r="D1" t="inlineStr"><is><t>When</t></is></c><c r="E1" t="inlineStr"><is><t>Ratio</t></is></c></row>"#,
        r#"<row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>42</v></c><c r="C2" t="b"><v>1</v></c><c r="D2" s="1"><v>45366</v></c><c r="E2" t="e"><v>#DIV/0!</v></c></row>"#,
        // C3 is deliberately missing: an empty cell inside the used area.
        r#"<row r="3"><c r="A3" t="inlineStr"><is><t>beta</t></is></c><c r="B3"><v>3.25</v></c><c r="D3" s="1"><v>45366.5</v></c><c r="E3"><v>0.30000000000000004</v></c></row>"#,
    );
    let second = r#"<row r="2"><c r="B2"><v>7</v></c><c r="C2" t="str"><f>"x"&amp;"y"</f><v>xy</v></c></row>"#;
    let bytes = xlsx(
        &[("Data", "", data), ("Hidden calc", "hidden", second)],
        &["Name", "Value", "alpha"],
        true,
    );
    let wb = book("xlsx", &bytes);
    assert_eq!(wb.sheets.len(), 2);

    let s = &wb.sheets[0];
    assert_eq!(s.name, "Data");
    assert!(!s.hidden);
    assert_eq!((s.total_rows, s.total_cols, s.truncated), (3, 5, false));
    assert_eq!(s.rows.len(), 3);
    assert!(s.rows.iter().all(|r| r.len() == 5));
    assert_eq!(s.rows[0][0], Cell::Text("Name".into()));
    assert_eq!(s.rows[0][2], Cell::Text("Flag".into()));
    assert_eq!(s.rows[1][0], Cell::Text("alpha".into()));
    assert_eq!(s.rows[1][1], Cell::Number(42.0));
    assert_eq!(s.rows[1][1].to_string(), "42");
    assert_eq!(s.rows[1][2], Cell::Bool(true));
    assert_eq!(s.rows[1][3], Cell::Date("2024-03-15".into()));
    assert_eq!(s.rows[1][4], Cell::Error("#DIV/0!".into()));
    assert_eq!(s.rows[2][1].to_string(), "3.25");
    assert_eq!(s.rows[2][2], Cell::Empty);
    assert_eq!(s.rows[2][3], Cell::Date("2024-03-15T12:00:00".into()));
    assert_eq!(s.rows[2][4].to_string(), "0.3");

    let s = &wb.sheets[1];
    assert_eq!(s.name, "Hidden calc");
    assert!(s.hidden);
    // Anchored at A1 even though the data starts at B2.
    assert_eq!((s.total_rows, s.total_cols), (2, 3));
    assert_eq!(s.rows[0], vec![Cell::Empty; 3]);
    assert_eq!(s.rows[1][1], Cell::Number(7.0));
    assert_eq!(s.rows[1][2], Cell::Text("xy".into()));
}

#[test]
fn caps_rows_and_columns() {
    let mut rows = String::new();
    for r in 1..=MAX_ROWS + 5 {
        rows.push_str(&format!(r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c></row>"#));
    }
    // One far-away cell in column IP (250).
    rows.push_str(r#"<row r="10006"><c r="IP10006"><v>1</v></c></row>"#);
    let wb = book("xlsx", &xlsx(&[("Big", "", &rows)], &[], false));
    let s = &wb.sheets[0];
    assert!(s.truncated);
    assert_eq!(s.total_rows, MAX_ROWS + 6);
    assert_eq!(s.total_cols, 250);
    assert_eq!(s.rows.len(), MAX_ROWS);
    assert!(s.rows.iter().all(|r| r.len() == MAX_COLS));
    assert_eq!(s.rows[MAX_ROWS - 1][0].to_string(), MAX_ROWS.to_string());
}

#[test]
fn csv_and_tsv() {
    let csv =
        "name,qty,ok,price\r\nwidget,3,true,1.50\n\"quoted, \"\"comma\"\"\",,FALSE,2.5\nshort\n";
    let wb = book("csv", csv.as_bytes());
    let s = &wb.sheets[0];
    assert_eq!((s.total_rows, s.total_cols, s.truncated), (4, 4, false));
    assert_eq!(s.rows[0][3], Cell::Text("price".into()));
    assert_eq!(s.rows[1][1], Cell::Number(3.0));
    assert_eq!(s.rows[1][2], Cell::Bool(true));
    // Kept as text: showing 1.5 would change what the file says.
    assert_eq!(s.rows[1][3], Cell::Text("1.50".into()));
    assert_eq!(s.rows[2][0], Cell::Text("quoted, \"comma\"".into()));
    assert_eq!(s.rows[2][1], Cell::Empty);
    assert_eq!(s.rows[2][3], Cell::Number(2.5));
    assert_eq!(
        s.rows[3],
        vec![
            Cell::Text("short".into()),
            Cell::Empty,
            Cell::Empty,
            Cell::Empty
        ]
    );

    let wb = book(".TSV", "\u{feff}a\tb\n1\tx,y\n".as_bytes());
    let s = &wb.sheets[0];
    assert_eq!(s.rows[0][0], Cell::Text("a".into()));
    assert_eq!(s.rows[1][1], Cell::Text("x,y".into()));

    // Excel's "Unicode text" export is UTF-16LE with a BOM.
    let utf16: Vec<u8> = std::iter::once(0xFEFFu16)
        .chain("é,2\n".encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    let s = &book("csv", &utf16).sheets[0];
    assert_eq!(s.rows[0][0], Cell::Text("é".into()));
    assert_eq!(s.rows[0][1], Cell::Number(2.0));

    let s = &book("csv", b"").sheets[0];
    assert_eq!((s.total_rows, s.total_cols, s.rows.len()), (0, 0, 0));
}

#[test]
fn csv_caps_count_everything() {
    let mut csv = String::new();
    for i in 0..MAX_ROWS + 10 {
        csv.push_str(&format!("{i}\n"));
    }
    csv.push_str(&format!("{}x\n", ",".repeat(MAX_COLS + 9)));
    let s = &book("csv", csv.as_bytes()).sheets[0];
    assert!(s.truncated);
    assert_eq!(s.total_rows, MAX_ROWS + 11);
    assert_eq!(s.total_cols, MAX_COLS + 10);
    assert_eq!(s.rows.len(), MAX_ROWS);
}

#[test]
fn load_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Sales Q1.csv");
    std::fs::write(&path, "a,b\n1,2\n").unwrap();
    let Document::Spreadsheet(wb) = load(&path).unwrap() else {
        panic!()
    };
    assert_eq!(wb.sheets[0].name, "Sales Q1");

    let path = dir.path().join("book.xlsx");
    std::fs::write(
        &path,
        xlsx(
            &[("S", "", r#"<row r="1"><c r="A1"><v>1</v></c></row>"#)],
            &[],
            false,
        ),
    )
    .unwrap();
    let Document::Spreadsheet(wb) = load(&path).unwrap() else {
        panic!()
    };
    assert_eq!(wb.sheets[0].name, "S");

    // Over the limit: refused from metadata, before reading.
    let big = dir.path().join("huge.xlsx");
    let f = std::fs::File::create(&big).unwrap();
    f.set_len(51 * 1024 * 1024).unwrap();
    let err = load(&big).unwrap_err();
    assert!(format!("{err:#}").contains("too large"), "{err:#}");

    let err = load(&dir.path().join("notes.txt")).unwrap_err();
    assert!(
        err.to_string().contains("not a supported document type"),
        "{err}"
    );
    assert!(load(&dir.path().join("missing.docx")).is_err());
}

#[test]
fn bad_spreadsheets_are_errors() {
    for ext in ["xlsx", "xls", "xlsb", "ods", "xlsm"] {
        assert!(
            load_bytes(DocKind::Spreadsheet, ext, b"nope").is_err(),
            "{ext}"
        );
    }
    // An xlsx without a workbook part.
    let bytes = zip(&[("[Content_Types].xml", content_types(&[]))]);
    assert!(load_bytes(DocKind::Spreadsheet, "xlsx", &bytes).is_err());
}
