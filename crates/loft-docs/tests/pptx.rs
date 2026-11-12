mod common;

use common::*;
use loft_docs::{Deck, DocKind, Document, load_bytes};

fn deck(bytes: &[u8]) -> Deck {
    match load_bytes(DocKind::Slides, "pptx", bytes).expect("pptx parses") {
        Document::Slides(d) => d,
        other => panic!("expected slides, got {other:?}"),
    }
}

fn sample_deck() -> Vec<u8> {
    // slide2.xml is listed first in sldIdLst, so it is presented first.
    let welcome = slide_xml(
        &[
            shape(2, "ctrTitle", &ap(0, "", &ar("Welcome Deck"))),
            shape(3, "subTitle", &ap(0, "", &ar("An overview"))),
            shape(
                4,
                "body",
                &[
                    ap(
                        0,
                        "",
                        &[ar("Point "), r#"<a:r><a:rPr lang="en-US" b="1"/><a:t>one</a:t></a:r>"#.to_string()].concat(),
                    ),
                    ap(1, "", &ar("Sub point")),
                    ap(2, "", &ar("Sub sub point")),
                    ap(
                        0,
                        "",
                        &[
                            ar("Point two, see "),
                            r#"<a:r><a:rPr lang="en-US" i="1"><a:hlinkClick r:id="rId3"/></a:rPr><a:t>the site</a:t></a:r>"#.to_string(),
                        ]
                        .concat(),
                    ),
                    ap(0, "", ""),
                    ap(0, r#"<a:buAutoNum type="arabicPeriod"/>"#, &ar("Numbered")),
                    ap(0, r#"<a:buAutoNum type="arabicPeriod"/>"#, &ar("Numbered again")),
                ]
                .concat(),
            ),
            r#"<p:pic><p:nvPicPr><p:cNvPr id="5" name="Picture 4" descr="Company logo"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/></p:blipFill><p:spPr/></p:pic>"#.to_string(),
            shape(6, "sldNum", &ap(0, "", r#"<a:fld id="{1}" type="slidenum"><a:t>1</a:t></a:fld>"#)),
        ]
        .concat(),
    );
    let numbers = slide_xml(
        &[
            shape(2, "title", &ap(0, "", &ar("Numbers &amp; Facts"))),
            r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="3" name="Table 2"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl><a:tblPr firstRow="1"/><a:tblGrid><a:gridCol w="1"/><a:gridCol w="1"/></a:tblGrid>
              <a:tr h="1"><a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>Metric</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>Value</a:t></a:r></a:p></a:txBody></a:tc></a:tr>
              <a:tr h="1"><a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>Users</a:t></a:r></a:p><a:p><a:r><a:t>(monthly)</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>1,024 | up</a:t></a:r></a:p></a:txBody></a:tc></a:tr>
              <a:tr h="1"><a:tc gridSpan="2"><a:txBody><a:bodyPr/><a:p><a:r><a:t>Merged</a:t></a:r></a:p></a:txBody></a:tc><a:tc hMerge="1"><a:txBody><a:bodyPr/><a:p><a:r><a:t>hidden</a:t></a:r></a:p></a:txBody></a:tc></a:tr>
            </a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#.to_string(),
            shape(4, "", &ap(0, "<a:buNone/>", &ar("Plain caption"))),
            // A group with a nested text box.
            format!(
                r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr id="7" name="Group"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{}</p:grpSp>"#,
                shape(8, "", &ap(0, "", &ar("Grouped note")))
            ),
        ]
        .concat(),
    );
    let notes = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:notes {P_NS}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{}{}{}</p:spTree></p:cSld></p:notes>"#,
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Slide Image"/><p:cNvSpPr/><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr><p:spPr/></p:sp>"#,
        shape(
            3,
            "body",
            &[
                ap(0, "", &ar("Remember to smile.")),
                ap(0, "", &ar("Thank *everyone*"))
            ]
            .concat()
        ),
        shape(4, "sldNum", &ap(0, "", &ar("1"))),
    );
    let presentation = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:presentation {P_NS}><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst><p:sldId id="256" r:id="rId3"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#
    );
    zip(&[
        (
            "[Content_Types].xml",
            content_types(&[
                (
                    "/ppt/presentation.xml",
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
                ),
                (
                    "/ppt/slides/slide1.xml",
                    "application/vnd.openxmlformats-officedocument.presentationml.slide+xml",
                ),
                (
                    "/ppt/slides/slide2.xml",
                    "application/vnd.openxmlformats-officedocument.presentationml.slide+xml",
                ),
                (
                    "/ppt/notesSlides/notesSlide1.xml",
                    "application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml",
                ),
            ]),
        ),
        (
            "_rels/.rels",
            rels(&[("rId1", "officeDocument", "ppt/presentation.xml", false)]),
        ),
        ("ppt/presentation.xml", presentation.into_bytes()),
        (
            "ppt/_rels/presentation.xml.rels",
            rels(&[
                (
                    "rId1",
                    "slideMaster",
                    "slideMasters/slideMaster1.xml",
                    false,
                ),
                ("rId2", "slide", "slides/slide1.xml", false),
                ("rId3", "slide", "slides/slide2.xml", false),
            ]),
        ),
        ("ppt/slides/slide1.xml", numbers),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            rels(&[(
                "rId1",
                "slideLayout",
                "../slideLayouts/slideLayout1.xml",
                false,
            )]),
        ),
        ("ppt/slides/slide2.xml", welcome),
        (
            "ppt/slides/_rels/slide2.xml.rels",
            rels(&[
                (
                    "rId1",
                    "slideLayout",
                    "../slideLayouts/slideLayout1.xml",
                    false,
                ),
                ("rId2", "image", "../media/image1.png", false),
                ("rId3", "hyperlink", "https://example.com/", true),
                (
                    "rId4",
                    "notesSlide",
                    "../notesSlides/notesSlide1.xml",
                    false,
                ),
            ]),
        ),
        ("ppt/notesSlides/notesSlide1.xml", notes.into_bytes()),
        ("ppt/media/image1.png", PNG.to_vec()),
    ])
}

#[test]
fn two_slides_in_presentation_order() {
    let deck = deck(&sample_deck());
    assert_eq!(deck.slides.len(), 2);

    let s = &deck.slides[0];
    println!("{}", s.markdown);
    assert_eq!(s.index, 1);
    assert_eq!(s.title.as_deref(), Some("Welcome Deck"));
    assert!(
        !s.markdown.contains("Welcome Deck"),
        "title is not repeated"
    );
    assert_eq!(
        s.markdown,
        "An overview\n\n\
         - Point **one**\n  \
           - Sub point\n    \
             - Sub sub point\n\
         - Point two, see [*the site*](https://example.com/)\n\
         1. Numbered\n\
         2. Numbered again\n\n\
         ![Company logo](image1.png)"
    );
    assert_eq!(
        s.notes.as_deref(),
        Some("Remember to smile.\nThank *everyone*")
    );
    assert_eq!(deck.images.len(), 1);
    assert_eq!(deck.images[0].name, "image1.png");
    assert_eq!(deck.images[0].bytes, PNG);
    // No layout to place the shapes by: the Markdown is what there is.
    assert!(s.scene.is_none());

    let s = &deck.slides[1];
    println!("{}", s.markdown);
    assert_eq!(s.index, 2);
    assert_eq!(s.title.as_deref(), Some("Numbers & Facts"));
    assert_eq!(
        s.markdown,
        "| Metric | Value |\n| --- | --- |\n| Users<br>(monthly) | 1,024 \\| up |\n| Merged | |\n\n\
         Plain caption\n\n\
         - Grouped note"
    );
    assert_eq!(s.notes, None);

    let all = deck.to_markdown();
    assert!(
        all.starts_with("## 1. Welcome Deck\n\nAn overview"),
        "{all}"
    );
    assert!(
        all.contains("> Remember to smile.\n> Thank \\*everyone\\*"),
        "{all}"
    );
    assert!(all.contains("---\n\n## 2. Numbers & Facts"), "{all}");
}

#[test]
fn falls_back_to_slide_file_order() {
    let slide = |t: &str| slide_xml(&shape(2, "title", &ap(0, "", &ar(t))));
    let bytes = zip(&[
        (
            "_rels/.rels",
            rels(&[("rId1", "officeDocument", "ppt/presentation.xml", false)]),
        ),
        (
            "ppt/presentation.xml",
            format!(r#"<p:presentation {P_NS}></p:presentation>"#).into_bytes(),
        ),
        ("ppt/slides/slide10.xml", slide("ten")),
        ("ppt/slides/slide2.xml", slide("two")),
    ]);
    let titles: Vec<_> = deck(&bytes)
        .slides
        .into_iter()
        .map(|s| s.title.unwrap_or_default())
        .collect();
    assert_eq!(titles, ["two", "ten"]);
}

#[test]
fn broken_decks_are_errors() {
    assert!(load_bytes(DocKind::Slides, "pptx", b"not a zip").is_err());
    let bytes = zip(&[
        ("_rels/.rels", rels(&[("rId1", "officeDocument", "ppt/presentation.xml", false)])),
        ("ppt/presentation.xml", format!(r#"<p:presentation {P_NS}><p:sldIdLst><p:sldId id="1" r:id="rId1"/></p:sldIdLst></p:presentation>"#).into_bytes()),
        ("ppt/_rels/presentation.xml.rels", rels(&[("rId1", "slide", "slides/slide1.xml", false)])),
        ("ppt/slides/slide1.xml", b"<p:sld><p:cSld><p:spTree><p:sp></p:spTree>".to_vec()),
    ]);
    let err = load_bytes(DocKind::Slides, "pptx", &bytes).unwrap_err();
    assert!(format!("{err:#}").contains("slide1.xml"), "{err:#}");
}
