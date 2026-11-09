//! Builders for tiny OOXML packages, written by hand so tests need no binary fixtures.
#![allow(dead_code)]

use std::io::{Cursor, Write};

use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

pub const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR fake image bytes";

pub fn zip(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, data) in parts {
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

const PKG_RELS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// A relationships part. Each entry is (id, type suffix, target, external).
pub fn rels(entries: &[(&str, &str, &str, bool)]) -> Vec<u8> {
    let mut s = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{PKG_RELS_NS}">"#
    );
    for (id, ty, target, external) in entries {
        let ty = if ty.starts_with("http") {
            ty.to_string()
        } else {
            format!("{REL}/{ty}")
        };
        let mode = if *external {
            r#" TargetMode="External""#
        } else {
            ""
        };
        s.push_str(&format!(
            r#"<Relationship Id="{id}" Type="{ty}" Target="{target}"{mode}/>"#
        ));
    }
    s.push_str("</Relationships>");
    s.into_bytes()
}

pub fn content_types(overrides: &[(&str, &str)]) -> Vec<u8> {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/>"#,
    );
    for (part, ty) in overrides {
        s.push_str(&format!(
            r#"<Override PartName="{part}" ContentType="{ty}"/>"#
        ));
    }
    s.push_str("</Types>");
    s.into_bytes()
}

// ---------------------------------------------------------------------------
// docx

pub const W_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#;

pub fn p(style: &str, runs: &str) -> String {
    let ppr = if style.is_empty() {
        String::new()
    } else {
        format!(r#"<w:pPr><w:pStyle w:val="{style}"/></w:pPr>"#)
    };
    format!("<w:p>{ppr}{runs}</w:p>")
}

pub fn li(num_id: u32, ilvl: u32, runs: &str) -> String {
    format!(
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="{num_id}"/></w:numPr></w:pPr>{runs}</w:p>"#
    )
}

pub fn r(text: &str) -> String {
    format!(r#"<w:r><w:t xml:space="preserve">{text}</w:t></w:r>"#)
}

pub fn r_fmt(rpr: &str, text: &str) -> String {
    format!(r#"<w:r><w:rPr>{rpr}</w:rPr><w:t xml:space="preserve">{text}</w:t></w:r>"#)
}

pub const DOCX_STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri"/></w:rPr></w:rPrDefault></w:docDefaults>
  <w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>
  <w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/></w:style>
  <w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:pPr><w:outlineLvl w:val="0"/></w:pPr><w:rPr><w:b/></w:rPr></w:style>
  <w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:pPr><w:outlineLvl w:val="1"/></w:pPr></w:style>
  <w:style w:type="paragraph" w:styleId="berschrift3"><w:name w:val="heading 3"/><w:basedOn w:val="Normal"/></w:style>
  <w:style w:type="paragraph" w:styleId="ChapterHeading"><w:name w:val="Chapter Heading"/><w:basedOn w:val="Heading1"/></w:style>
  <w:style w:type="paragraph" w:styleId="ListBullet"><w:name w:val="List Bullet"/><w:basedOn w:val="Normal"/><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style>
  <w:style w:type="paragraph" w:styleId="SourceCode"><w:name w:val="Source Code"/><w:basedOn w:val="Normal"/></w:style>
  <w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/><w:basedOn w:val="Normal"/></w:style>
  <w:style w:type="character" w:styleId="VerbatimChar"><w:name w:val="Verbatim Char"/><w:rPr><w:rFonts w:ascii="Consolas" w:hAnsi="Consolas"/></w:rPr></w:style>
  <w:style w:type="character" w:styleId="Strong"><w:name w:val="Strong"/><w:rPr><w:b/></w:rPr></w:style>
</w:styles>"#;

pub const DOCX_NUMBERING: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="0">
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/></w:lvl>
    <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="o"/></w:lvl>
    <w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="-"/></w:lvl>
  </w:abstractNum>
  <w:abstractNum w:abstractNumId="1">
    <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
    <w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/></w:lvl>
  </w:abstractNum>
  <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
  <w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>
  <w:num w:numId="3"><w:abstractNumId w:val="1"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="1"/></w:lvlOverride></w:num>
</w:numbering>"#;

pub const DOCX_FOOTNOTES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>
  <w:footnote w:id="1"><w:p><w:r><w:footnoteRef/></w:r><w:r><w:t xml:space="preserve"> Source: the </w:t></w:r><w:r><w:rPr><w:i/></w:rPr><w:t>annual</w:t></w:r><w:r><w:t xml:space="preserve"> survey.</w:t></w:r></w:p></w:footnote>
</w:footnotes>"#;

/// Assembles a docx package around a `w:body` fragment.
pub fn docx(body: &str) -> Vec<u8> {
    docx_with(body, DOCX_STYLES, DOCX_NUMBERING)
}

pub fn docx_with(body: &str, styles: &str, numbering: &str) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {W_NS}><w:body>{body}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr></w:body></w:document>"#
    );
    let core = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Core Title</dc:title></cp:coreProperties>"#;
    zip(&[
        (
            "[Content_Types].xml",
            content_types(&[
                (
                    "/word/document.xml",
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
                ),
                (
                    "/word/styles.xml",
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml",
                ),
                (
                    "/word/numbering.xml",
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml",
                ),
                (
                    "/word/footnotes.xml",
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml",
                ),
            ]),
        ),
        (
            "_rels/.rels",
            rels(&[
                ("rId1", "officeDocument", "word/document.xml", false),
                (
                    "rId2",
                    "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties",
                    "docProps/core.xml",
                    false,
                ),
            ]),
        ),
        ("docProps/core.xml", core.as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        (
            "word/_rels/document.xml.rels",
            rels(&[
                ("rId1", "styles", "styles.xml", false),
                ("rId2", "numbering", "numbering.xml", false),
                ("rId3", "hyperlink", "https://example.com/report?q=1", true),
                ("rId4", "image", "media/image1.png", false),
                ("rId5", "footnotes", "footnotes.xml", false),
                ("rId6", "hyperlink", "javascript:alert(1)", true),
            ]),
        ),
        ("word/styles.xml", styles.as_bytes().to_vec()),
        ("word/numbering.xml", numbering.as_bytes().to_vec()),
        ("word/footnotes.xml", DOCX_FOOTNOTES.as_bytes().to_vec()),
        ("word/media/image1.png", PNG.to_vec()),
    ])
}

pub fn drawing(rel_id: &str, descr: &str) -> String {
    format!(
        r#"<w:r><w:drawing><wp:inline><wp:extent cx="100" cy="100"/><wp:docPr id="1" name="Picture 1" descr="{descr}"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:nvPicPr><pic:cNvPr id="0" name="image1.png"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="{rel_id}"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
    )
}

// ---------------------------------------------------------------------------
// xlsx

pub fn xlsx(sheets: &[(&str, &str, &str)], shared: &[&str], date_style: bool) -> Vec<u8> {
    let mut workbook = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#,
    );
    let mut wb_rels: Vec<(String, String, String)> = Vec::new();
    let mut overrides: Vec<(String, String)> = vec![(
        "/xl/workbook.xml".into(),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml".into(),
    )];
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    for (i, (name, state, rows)) in sheets.iter().enumerate() {
        let n = i + 1;
        let state = if state.is_empty() {
            String::new()
        } else {
            format!(r#" state="{state}""#)
        };
        workbook.push_str(&format!(
            r#"<sheet name="{name}" sheetId="{n}"{state} r:id="rId{n}"/>"#
        ));
        wb_rels.push((
            format!("rId{n}"),
            "worksheet".into(),
            format!("worksheets/sheet{n}.xml"),
        ));
        overrides.push((
            format!("/xl/worksheets/sheet{n}.xml"),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml".into(),
        ));
        parts.push((
            format!("xl/worksheets/sheet{n}.xml"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows}</sheetData></worksheet>"#
            )
            .into_bytes(),
        ));
    }
    workbook.push_str("</sheets></workbook>");

    let mut sst = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{0}" uniqueCount="{0}">"#,
        shared.len()
    );
    for s in shared {
        sst.push_str(&format!("<si><t>{s}</t></si>"));
    }
    sst.push_str("</sst>");
    let k = sheets.len();
    wb_rels.push((
        format!("rId{}", k + 1),
        "sharedStrings".into(),
        "sharedStrings.xml".into(),
    ));
    wb_rels.push((
        format!("rId{}", k + 2),
        "styles".into(),
        "styles.xml".into(),
    ));
    let xfs = if date_style {
        r#"<cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="14" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/></cellXfs>"#
    } else {
        r#"<cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs>"#
    };
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>{xfs}</styleSheet>"#
    );
    overrides.push((
        "/xl/sharedStrings.xml".into(),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml".into(),
    ));
    overrides.push((
        "/xl/styles.xml".into(),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml".into(),
    ));

    let rel_refs: Vec<(&str, &str, &str, bool)> = wb_rels
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str(), false))
        .collect();
    let ov_refs: Vec<(&str, &str)> = overrides
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let mut all: Vec<(&str, Vec<u8>)> = vec![
        ("[Content_Types].xml", content_types(&ov_refs)),
        (
            "_rels/.rels",
            rels(&[("rId1", "officeDocument", "xl/workbook.xml", false)]),
        ),
        ("xl/workbook.xml", workbook.into_bytes()),
        ("xl/_rels/workbook.xml.rels", rels(&rel_refs)),
        ("xl/sharedStrings.xml", sst.into_bytes()),
        ("xl/styles.xml", styles.into_bytes()),
    ];
    for (name, data) in &parts {
        all.push((name.as_str(), data.clone()));
    }
    zip(&all)
}

// ---------------------------------------------------------------------------
// pptx

pub const P_NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;

pub fn slide_xml(shapes: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sld {P_NS}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#
    )
    .into_bytes()
}

/// A text shape. `ph` is a placeholder type (`""` for a plain text box,
/// `"body"` for a body placeholder without a type attribute).
pub fn shape(id: u32, ph: &str, paras: &str) -> String {
    let nvpr = match ph {
        "" => "<p:nvPr/>".to_string(),
        "body" => r#"<p:nvPr><p:ph idx="1"/></p:nvPr>"#.to_string(),
        t => format!(r#"<p:nvPr><p:ph type="{t}"/></p:nvPr>"#),
    };
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="Shape {id}"/><p:cNvSpPr/>{nvpr}</p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody></p:sp>"#
    )
}

pub fn ap(lvl: u32, ppr_children: &str, runs: &str) -> String {
    format!(
        r#"<a:p><a:pPr lvl="{lvl}">{ppr_children}</a:pPr>{runs}<a:endParaRPr lang="en-US"/></a:p>"#
    )
}

pub fn ar(text: &str) -> String {
    format!(r#"<a:r><a:rPr lang="en-US"/><a:t>{text}</a:t></a:r>"#)
}
