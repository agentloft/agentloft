//! Feeds truncated and corrupted parts through every parser and checks that
//! nothing panics, not even inside `load_bytes`' own `catch_unwind` guard.
//! Kept in its own test binary because the panic hook is process-wide.

mod common;

use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use loft_docs::{DocKind, load_bytes};

static PANICS: AtomicUsize = AtomicUsize::new(0);

fn parts_of(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    (0..zip.len())
        .map(|i| {
            let mut f = zip.by_index(i).unwrap();
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            (f.name().unwrap().to_string(), data)
        })
        .collect()
}

fn rezip(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in parts {
        w.start_file(name.as_str(), zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// Truncates every XML part at many offsets and splices markup characters in
/// at pseudo-random positions, parsing each variant.
fn torture(kind: DocKind, ext: &str, original: &[u8]) -> usize {
    let parts = parts_of(original);
    let mut runs = 0;
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    for (idx, (name, data)) in parts.iter().enumerate() {
        if !(name.ends_with(".xml") || name.ends_with(".rels")) {
            continue;
        }
        let step = (data.len() / 40).max(1);
        for cut in (0..data.len()).step_by(step) {
            let mut variant = parts.clone();
            variant[idx].1.truncate(cut);
            let _ = load_bytes(kind, ext, &rezip(&variant));
            runs += 1;
        }
        for _ in 0..40 {
            let mut variant = parts.clone();
            let bytes = &mut variant[idx].1;
            for _ in 0..3 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let pos = (seed >> 33) as usize % bytes.len().max(1);
                let junk = b"<>\"&/\xff09-";
                if let Some(b) = bytes.get_mut(pos) {
                    *b = junk[(seed >> 13) as usize % junk.len()];
                }
            }
            let _ = load_bytes(kind, ext, &rezip(&variant));
            runs += 1;
        }
    }
    // Truncating the archive itself.
    for cut in (0..original.len()).step_by((original.len() / 50).max(1)) {
        let _ = load_bytes(kind, ext, &original[..cut]);
        runs += 1;
    }
    runs
}

#[test]
fn corrupted_documents_never_panic() {
    std::panic::set_hook(Box::new(|info| {
        PANICS.fetch_add(1, Ordering::SeqCst);
        eprintln!("panic: {info}");
    }));

    let body = [
        p("Title", &r("T")),
        p("Heading1", &r("H")),
        li(1, 0, &r("a")),
        li(2, 1, &r("b")),
        p("", &format!(r#"<w:hyperlink r:id="rId3">{}</w:hyperlink>{}"#, r("l"), drawing("rId4", "x"))),
        r#"<w:tbl><w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>c</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#.to_string(),
        p("", r#"<w:r><w:t>n</w:t></w:r><w:r><w:footnoteReference w:id="1"/></w:r>"#),
    ]
    .concat();
    let mut runs = torture(DocKind::Word, "docx", &docx(&body));

    let data = r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>4</v></c><c r="C1" s="1"><v>45000</v></c><c r="D1" t="b"><v>0</v></c></row>"#;
    runs += torture(
        DocKind::Spreadsheet,
        "xlsx",
        &xlsx(&[("A", "", data), ("B", "hidden", data)], &["s"], true),
    );

    let slide = slide_xml(
        &[
            shape(2, "title", &ap(0, "", &ar("T"))),
            shape(
                3,
                "body",
                &[ap(0, "", &ar("a")), ap(1, "<a:buNone/>", &ar("b"))].concat(),
            ),
        ]
        .concat(),
    );
    let pptx = zip(&[
        ("_rels/.rels", rels(&[("rId1", "officeDocument", "ppt/presentation.xml", false)])),
        (
            "ppt/presentation.xml",
            format!(r#"<p:presentation {P_NS}><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#).into_bytes(),
        ),
        ("ppt/_rels/presentation.xml.rels", rels(&[("rId1", "slide", "slides/slide1.xml", false)])),
        ("ppt/slides/slide1.xml", slide),
        ("ppt/slides/_rels/slide1.xml.rels", rels(&[("rId9", "notesSlide", "../notesSlides/n1.xml", false)])),
        ("ppt/notesSlides/n1.xml", shape(1, "body", &ap(0, "", &ar("note"))).into_bytes()),
    ]);
    runs += torture(DocKind::Slides, "pptx", &pptx);

    for junk in [
        &b""[..],
        b"PK",
        b"PK\x05\x06",
        b"\0\0\0\0",
        b"<xml>",
        b"a,b\n\"unterminated",
    ] {
        for (kind, ext) in [
            (DocKind::Word, "docx"),
            (DocKind::Slides, "pptx"),
            (DocKind::Spreadsheet, "xlsx"),
            (DocKind::Spreadsheet, "xls"),
            (DocKind::Spreadsheet, "xlsb"),
            (DocKind::Spreadsheet, "ods"),
            (DocKind::Spreadsheet, "csv"),
        ] {
            let _ = load_bytes(kind, ext, junk);
            runs += 1;
        }
    }

    assert!(runs > 1000, "only {runs} variants");
    assert_eq!(
        PANICS.load(Ordering::SeqCst),
        0,
        "a parser panicked; see stderr"
    );
}
