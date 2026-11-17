mod common;

use common::*;
use loft_docs::{DocKind, Document, WordDoc, load_bytes};

fn convert(body: &str) -> WordDoc {
    match load_bytes(DocKind::Word, "docx", &docx(body)).expect("docx parses") {
        Document::Word(doc) => doc,
        other => panic!("expected a Word document, got {other:?}"),
    }
}

#[test]
fn full_document() {
    let body = [
        p("Title", &r("Quarterly Report")),
        p("Heading1", &r("Overview")),
        p(
            "",
            &[
                r("Plain "),
                r_fmt("<w:b/>", "bold"),
                r(" and "),
                r_fmt("<w:i/>", "italic"),
                r(" and "),
                r_fmt("<w:strike/>", "gone"),
                r(", with *stars* and a "),
                format!(r#"<w:hyperlink r:id="rId3">{}</w:hyperlink>"#, r("link")),
                r("."),
            ]
            .concat(),
        ),
        p("Heading2", &r("Lists")),
        li(1, 0, &r("Apples")),
        li(1, 1, &r("Green")),
        li(1, 1, &r("Red")),
        li(1, 0, &r("Pears")),
        p("", &r("Steps:")),
        li(2, 0, &r("First")),
        li(2, 0, &r("Second")),
        li(2, 1, &r("Detail")),
        li(2, 0, &r("Third")),
        p("ListBullet", &r("Bullet from style")),
        p("", &r("Table:")),
        format!(
            r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/></w:tblPr><w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid>
              <w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr>
              <w:tr><w:tc><w:tcPr><w:tcW w:w="100"/></w:tcPr>{}{}</w:tc><w:tc>{}</w:tc></w:tr>
              <w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr>{}</w:tc></w:tr>
            </w:tbl>"#,
            p("", &r_fmt("<w:b/>", "Name")),
            p("", &r("Qty")),
            p("", &r("Widget | big")),
            p("", &r("second line")),
            p("", &r("3")),
            p("", &r("Total")),
        ),
        p("Heading3", &r("Ignored unknown style id")),
        p("berschrift3", &r("Localized")),
        p("ChapterHeading", &r("Inherited heading")),
    ]
    .concat();
    let doc = convert(&body);
    let md = &doc.markdown;
    println!("{md}");

    assert_eq!(doc.title.as_deref(), Some("Quarterly Report"));
    assert!(
        md.starts_with("# Quarterly Report\n\n# Overview\n\n"),
        "{md}"
    );
    assert!(
        md.contains(
            "Plain **bold** and *italic* and ~~gone~~, with \\*stars\\* and a [link](https://example.com/report?q=1)."
        ),
        "{md}"
    );
    assert!(
        md.contains("## Lists\n\n- Apples\n  - Green\n  - Red\n- Pears\n\nSteps:"),
        "{md}"
    );
    assert!(
        md.contains("1. First\n2. Second\n   1. Detail\n3. Third\n- Bullet from style"),
        "{md}"
    );
    assert!(
        md.contains(
            "| **Name** | Qty |\n| --- | --- |\n| Widget \\| big<br>second line | 3 |\n| Total | |"
        ),
        "{md}"
    );
    // Unknown style ids that look like headings still map by id.
    assert!(md.contains("### Ignored unknown style id"), "{md}");
    // Style names win over localized ids; basedOn chains are followed.
    assert!(md.contains("### Localized"), "{md}");
    assert!(md.contains("# Inherited heading"), "{md}");
}

#[test]
fn revisions_breaks_code_quotes_and_escaping() {
    let body = [
        p(
            "",
            &format!(
                r#"{}<w:del w:id="1" w:author="a"><w:r><w:delText>removed</w:delText></w:r></w:del><w:ins w:id="2" w:author="a">{}</w:ins>"#,
                r("Keep "),
                r("inserted"),
            ),
        ),
        p("", &format!(r#"{}<w:r><w:br/></w:r>{}"#, r("line one"), r("line two"))),
        p("", r#"<w:r><w:br w:type="page"/></w:r>"#),
        p("SourceCode", &r("fn main() {")),
        p("SourceCode", &r("    println!(\"*hi*\");")),
        p("SourceCode", &r("}")),
        p(
            "",
            &[
                r("Run "),
                r_fmt(r#"<w:rStyle w:val="VerbatimChar"/>"#, "cargo test"),
                r(" or "),
                r_fmt(r#"<w:rFonts w:ascii="Courier New" w:hAnsi="Courier New"/>"#, "x = 1"),
                r(" or "),
                r_fmt(r#"<w:rStyle w:val="Strong"/>"#, "strong style"),
            ]
            .concat(),
        ),
        p("Quote", &r("To be or not to be.")),
        p("", &r("1. Not a list")),
        p("", &r("# not a heading &amp; &lt;b&gt; tag")),
        p("", &r_fmt(r#"<w:vanish/>"#, "hidden text")),
        p(
            "",
            r#"<mc:AlternateContent><mc:Choice Requires="wps"><w:r><w:t>choice</w:t></w:r></mc:Choice><mc:Fallback><w:r><w:t>fallback</w:t></w:r></mc:Fallback></mc:AlternateContent>"#,
        ),
        p("", &r("tail")),
    ]
    .concat();
    let doc = convert(&body);
    let md = &doc.markdown;
    println!("{md}");
    assert!(
        md.starts_with("Keep inserted\n\nline one\\\nline two\n\n---\n\n"),
        "{md}"
    );
    assert!(!md.contains("removed"), "{md}");
    assert!(
        md.contains("```\nfn main() {\n    println!(\"*hi*\");\n}\n```"),
        "{md}"
    );
    assert!(
        md.contains("Run `cargo test` or `x = 1` or **strong style**"),
        "{md}"
    );
    assert!(md.contains("> To be or not to be."), "{md}");
    assert!(md.contains("\n1\\. Not a list\n"), "{md}");
    assert!(md.contains("\\# not a heading & \\<b> tag"), "{md}");
    assert!(!md.contains("hidden text"), "{md}");
    assert!(md.contains("choice") && !md.contains("fallback"), "{md}");
    // No Title paragraph: fall back to the core property.
    assert_eq!(doc.title.as_deref(), Some("Core Title"));
}

#[test]
fn images_footnotes_and_fields() {
    let body = [
        p("", &drawing("rId4", "Revenue chart")),
        p("", &drawing("rId4", "")),
        p(
            "",
            &[
                r("Claim"),
                r#"<w:r><w:rPr><w:vertAlign w:val="superscript"/></w:rPr><w:footnoteReference w:id="1"/></w:r>"#.to_string(),
                r(" stands."),
            ]
            .concat(),
        ),
        p(
            "",
            &[
                r("See "),
                r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> HYPERLINK "https://example.org/docs" </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r>"#.to_string(),
                r("the docs"),
                r#"<w:r><w:fldChar w:fldCharType="end"/></w:r>"#.to_string(),
                r(" and "),
                r#"<w:fldSimple w:instr=" HYPERLINK &quot;https://example.net&quot; "><w:r><w:t>simple</w:t></w:r></w:fldSimple>"#.to_string(),
                r(" and "),
                format!(r#"<w:hyperlink r:id="rId6">{}</w:hyperlink>"#, r("unsafe")),
                r(" and "),
                format!(r#"<w:hyperlink w:anchor="_Toc1">{}</w:hyperlink>"#, r("internal")),
            ]
            .concat(),
        ),
    ]
    .concat();
    let doc = convert(&body);
    let md = &doc.markdown;
    println!("{md}");
    assert!(
        md.starts_with("![Revenue chart](image1.png)\n\n![image1.png](image1.png)\n\n"),
        "{md}"
    );
    assert_eq!(doc.images.len(), 1, "the same image is listed once");
    assert_eq!(doc.images[0].name, "image1.png");
    assert_eq!(doc.images[0].mime, "image/png");
    assert_eq!(doc.images[0].bytes, PNG);
    assert!(md.contains("Claim[^1] stands."), "{md}");
    assert!(md.ends_with("[^1]: Source: the *annual* survey."), "{md}");
    assert!(
        md.contains(
            "See [the docs](https://example.org/docs) and [simple](https://example.net) and unsafe and internal"
        ),
        "{md}"
    );
}

#[test]
fn numbering_restarts_and_nesting_clamps() {
    let body = [
        li(2, 0, &r("one")),
        li(2, 0, &r("two")),
        p("", &r("break")),
        // Same abstract list continues numbering…
        li(2, 0, &r("three")),
        // …while a num with a start override restarts it.
        li(3, 0, &r("restart")),
        li(3, 0, &r("next")),
        p("", &r("break")),
        // Jumping from level 0 to level 2 is clamped to one level deeper.
        li(1, 0, &r("top")),
        li(1, 2, &r("deep")),
    ]
    .concat();
    let md = convert(&body).markdown;
    println!("{md}");
    assert!(
        md.starts_with("1. one\n2. two\n\nbreak\n\n3. three\n1. restart\n2. next"),
        "{md}"
    );
    assert!(md.ends_with("- top\n  - deep"), "{md}");
}

#[test]
fn works_without_styles_or_numbering_parts() {
    // Minimal package: only the document and the package relationships.
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:document {W_NS}><w:body>{}{}{}</w:body></w:document>"#,
        p("Heading2", &r("Bare")),
        li(7, 0, &r("unknown list")),
        p("", &r("text")),
    );
    let bytes = zip(&[
        ("[Content_Types].xml", content_types(&[])),
        (
            "_rels/.rels",
            rels(&[("rId1", "officeDocument", "word/document.xml", false)]),
        ),
        ("word/document.xml", document.into_bytes()),
    ]);
    let Document::Word(doc) = load_bytes(DocKind::Word, "docx", &bytes).unwrap() else {
        panic!()
    };
    assert_eq!(doc.markdown, "## Bare\n\n- unknown list\n\ntext");
    assert_eq!(doc.title, None);
}

#[test]
fn errors_instead_of_panics() {
    assert!(load_bytes(DocKind::Word, "docx", b"PK\x03\x04 truncated").is_err());
    let no_document = zip(&[("[Content_Types].xml", content_types(&[]))]);
    let err = load_bytes(DocKind::Word, "docx", &no_document).unwrap_err();
    assert!(err.to_string().contains("main document"), "{err}");
    let broken = zip(&[(
        "word/document.xml",
        b"<w:document xmlns:w=\"x\"><w:body><w:p></w:body></w:document>".to_vec(),
    )]);
    assert!(load_bytes(DocKind::Word, "docx", &broken).is_err());
}
