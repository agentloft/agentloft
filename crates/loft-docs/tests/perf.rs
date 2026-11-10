//! Timing check for large inputs. Run with
//! `cargo test --release -p loft-docs --test perf -- --ignored --nocapture`.

mod common;

use std::time::Instant;

use common::*;
use loft_docs::{DocKind, Document, load_bytes};

/// A text-heavy docx of at least `target` compressed bytes: headings,
/// formatted runs, links, nested lists and tables of pseudo-random words.
fn big_docx(target: usize) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "agent",
        "loft",
        "report",
        "quarter",
        "revenue",
        "growth",
        "the",
        "of",
        "and",
        "pipeline",
        "latency",
        "throughput",
        "customer",
        "sheet",
        "slide",
        "markdown",
        "parser",
        "zip",
        "stream",
        "render",
        "budget",
        "forecast",
        "risk",
        "mitigation",
        "owner",
        "deadline",
    ];
    let mut seed: u64 = 42;
    let mut word = || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let w = WORDS[(seed >> 33) as usize % WORDS.len()];
        let n = (seed >> 20) % 1000;
        format!("{w}{n}")
    };
    let mut body = String::new();
    let mut i = 0usize;
    loop {
        if i.is_multiple_of(50) {
            body.push_str(&p("Heading2", &r(&format!("Section {i} {}", word()))));
        }
        let mut runs = String::new();
        for k in 0..12 {
            let text: Vec<String> = (0..6).map(|_| word()).collect();
            let text = format!("{} ", text.join(" "));
            runs.push_str(&match k % 4 {
                0 => r_fmt("<w:b/>", &text),
                1 => r_fmt("<w:i/>", &text),
                2 => format!(r#"<w:hyperlink r:id="rId3">{}</w:hyperlink>"#, r(&text)),
                _ => r(&text),
            });
        }
        body.push_str(&p("", &runs));
        body.push_str(&li(1, (i % 3) as u32, &r(&word())));
        if i.is_multiple_of(20) {
            let row = format!(
                "<w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr>",
                p("", &r(&word())),
                p("", &r(&word())),
                p("", &r(&word()))
            );
            body.push_str(&format!("<w:tbl>{}</w:tbl>", row.repeat(5)));
        }
        i += 1;
        // Check the size only now and then; zipping is the slow part here.
        if i.is_multiple_of(2000) {
            let bytes = docx(&body);
            if bytes.len() >= target {
                return bytes;
            }
        }
    }
}

#[test]
#[ignore = "timing test; run in release mode"]
fn five_megabyte_docx() {
    let bytes = big_docx(5 * 1024 * 1024);
    let start = Instant::now();
    let Document::Word(doc) = load_bytes(DocKind::Word, "docx", &bytes).unwrap() else {
        panic!()
    };
    let elapsed = start.elapsed();
    println!(
        "{:.1} MB docx -> {:.1} MB markdown in {elapsed:?}",
        bytes.len() as f64 / 1048576.0,
        doc.markdown.len() as f64 / 1048576.0
    );
    if !cfg!(debug_assertions) {
        assert!(elapsed.as_secs_f64() < 1.0, "took {elapsed:?}");
    }
}
