//! Slides as drawn: what a shape inherits from its layout, master and theme.

mod common;

use common::*;
use loft_docs::scene::*;
use loft_docs::{Deck, DocKind, Document, load_bytes};

fn deck(bytes: &[u8]) -> Deck {
    match load_bytes(DocKind::Slides, "pptx", bytes).expect("pptx parses") {
        Document::Slides(d) => d,
        other => panic!("expected slides, got {other:?}"),
    }
}

const TREE_HEAD: &str =
    r#"<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>"#;

fn xfrm(x: i64, y: i64, w: i64, h: i64) -> String {
    format!(r#"<a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{w}" cy="{h}"/></a:xfrm>"#)
}

/// A deck of one slide (`shapes` is its shape tree) on a layout and a master
/// that each hold a title placeholder, with a dark theme whose colour map
/// swaps light and dark.
fn package(shapes: &str) -> Vec<u8> {
    let presentation = format!(
        r#"<p:presentation {P_NS}><p:sldMasterIdLst><p:sldMasterId id="1" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst><p:sldSz cx="9144000" cy="5143500"/><p:defaultTextStyle><a:lvl1pPr><a:defRPr sz="1800"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/></a:defRPr></a:lvl1pPr></p:defaultTextStyle></p:presentation>"#
    );
    let theme = r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements><a:clrScheme name="t"><a:dk1><a:srgbClr val="101010"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FAFAFA"/></a:lt1><a:dk2><a:srgbClr val="202020"/></a:dk2><a:lt2><a:srgbClr val="E0E0E0"/></a:lt2><a:accent1><a:srgbClr val="336699"/></a:accent1><a:accent2><a:srgbClr val="FF0000"/></a:accent2></a:clrScheme><a:fontScheme name="f"><a:majorFont><a:latin typeface="Heading Face"/></a:majorFont><a:minorFont><a:latin typeface="Body Face"/></a:minorFont></a:fontScheme><a:fmtScheme><a:lnStyleLst><a:ln w="6350"/><a:ln w="12700"/><a:ln w="19050"/></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst><a:outerShdw blurRad="38100" dist="25400" dir="5400000"><a:srgbClr val="000000"><a:alpha val="50000"/></a:srgbClr></a:outerShdw></a:effectLst></a:effectStyle></a:effectStyleLst></a:fmtScheme></a:themeElements></a:theme>"#;
    // The master: a title placeholder with a place and a style, a logo box
    // that every slide shows, and a colour map where text is light.
    let master = format!(
        r#"<p:sldMaster {P_NS}><p:cSld><p:bg><p:bgPr><a:solidFill><a:schemeClr val="bg1"/></a:solidFill></p:bgPr></p:bg><p:spTree>{TREE_HEAD}
        <p:sp><p:nvSpPr><p:cNvPr id="2" name="Master title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr>{}</p:spPr><p:txBody><a:bodyPr anchor="b" lIns="0"/><a:lstStyle/><a:p/></p:txBody></p:sp>
        <p:sp><p:nvSpPr><p:cNvPr id="3" name="Logo"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:solidFill><a:schemeClr val="accent2"/></a:solidFill></p:spPr></p:sp>
        </p:spTree></p:cSld><p:clrMap bg1="dk1" tx1="lt1" bg2="dk2" tx2="lt2" accent1="accent1" accent2="accent2"/>
        <p:txStyles><p:titleStyle><a:lvl1pPr algn="ctr"><a:defRPr sz="4400" b="1"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mj-lt"/></a:defRPr></a:lvl1pPr></p:titleStyle>
        <p:bodyStyle><a:lvl1pPr marL="342900" indent="-342900"><a:buChar char="•"/><a:defRPr sz="3200"/></a:lvl1pPr><a:lvl2pPr marL="742950" indent="-285750"><a:buChar char="–"/><a:defRPr sz="2800"/></a:lvl2pPr></p:bodyStyle></p:txStyles></p:sldMaster>"#,
        xfrm(457200, 0, 8229600, 914400),
        xfrm(0, 0, 127000, 127000),
    );
    // The layout moves the title and makes it smaller.
    let layout = format!(
        r#"<p:sldLayout {P_NS}><p:cSld><p:spTree>{TREE_HEAD}
        <p:sp><p:nvSpPr><p:cNvPr id="2" name="Layout title"/><p:cNvSpPr/><p:nvPr><p:ph type="title" idx="7"/></p:nvPr></p:nvSpPr><p:spPr>{}</p:spPr><p:txBody><a:bodyPr/><a:lstStyle><a:lvl1pPr><a:defRPr sz="3600"/></a:lvl1pPr></a:lstStyle><a:p/></p:txBody></p:sp>
        <p:sp><p:nvSpPr><p:cNvPr id="3" name="Layout body"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:spPr>{}</p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody></p:sp>
        </p:spTree></p:cSld></p:sldLayout>"#,
        xfrm(127000, 254000, 2540000, 635000),
        xfrm(127000, 1270000, 2540000, 2540000),
    );
    let slide = format!(
        r#"<p:sld {P_NS}><p:cSld><p:spTree>{TREE_HEAD}{shapes}</p:spTree></p:cSld></p:sld>"#
    );
    zip(&[
        ("[Content_Types].xml", content_types(&[])),
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
            ]),
        ),
        ("ppt/slides/slide1.xml", slide.into_bytes()),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            rels(&[
                (
                    "rId1",
                    "slideLayout",
                    "../slideLayouts/slideLayout1.xml",
                    false,
                ),
                ("rId2", "image", "../media/image1.png", false),
            ]),
        ),
        ("ppt/slideLayouts/slideLayout1.xml", layout.into_bytes()),
        (
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            rels(&[(
                "rId1",
                "slideMaster",
                "../slideMasters/slideMaster1.xml",
                false,
            )]),
        ),
        ("ppt/slideMasters/slideMaster1.xml", master.into_bytes()),
        (
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            rels(&[("rId1", "theme", "../theme/theme1.xml", false)]),
        ),
        ("ppt/theme/theme1.xml", theme.as_bytes().to_vec()),
        ("ppt/media/image1.png", PNG.to_vec()),
    ])
}

fn scene(shapes: &str) -> (Deck, Scene) {
    let deck = deck(&package(shapes));
    let scene = deck.slides[0].scene.clone().expect("the slide has a scene");
    (deck, scene)
}

fn named<'s>(scene: &'s Scene, name: &str) -> &'s Shape {
    scene
        .shapes
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no shape named {name}: {:?}", scene.shapes))
}

#[test]
fn a_placeholder_inherits_place_and_text_style() {
    let (deck, scene) = scene(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr/><p:nvPr><p:ph type="title" idx="7"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>Hello</a:t></a:r></a:p></p:txBody></p:sp>"#,
    );
    assert_eq!(deck.size, (720., 405.));
    // The master's background, through its colour map: bg1 is the dark one.
    assert_eq!(scene.background, Fill::Solid(Color::rgb(0x10, 0x10, 0x10)));

    let title = named(&scene, "Title 1");
    // Placed by the layout, not the master.
    assert_eq!((title.x, title.y, title.w, title.h), (10., 20., 200., 50.));
    let body = title.text.as_ref().expect("text");
    // The anchor and the zero left inset come from the master's placeholder.
    assert_eq!(body.anchor, Anchor::Bottom);
    assert_eq!(body.insets[0], 0.);
    assert_eq!(body.insets[2], 7.2);
    let p = &body.paragraphs[0];
    assert_eq!(p.align, Align::Center, "from the master's title style");
    assert!(p.bullet.is_none());
    let run = &p.runs[0];
    assert_eq!(run.text, "Hello");
    assert_eq!(run.size, 36., "the layout's size beats the master's 44");
    assert!(run.bold);
    assert_eq!(run.color, Color::rgb(0xFA, 0xFA, 0xFA), "tx1 maps to lt1");
    assert_eq!(run.font.as_deref(), Some("Heading Face"));

    // The master's logo is drawn behind the slide's own shapes; neither the
    // master's nor the layout's placeholders are.
    assert_eq!(scene.shapes[0].name, "Logo");
    assert_eq!(scene.shapes[0].fill, Fill::Solid(Color::rgb(255, 0, 0)));
    assert_eq!(scene.shapes.len(), 2);
}

#[test]
fn body_text_gets_bullets_levels_and_numbers() {
    let para = |lvl: u32, ppr: &str, text: &str| {
        format!(
            r#"<a:p><a:pPr lvl="{lvl}">{ppr}</a:pPr><a:r><a:rPr lang="en-US"/><a:t>{text}</a:t></a:r></a:p>"#
        )
    };
    let (_, scene) = scene(&format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Body"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr><a:normAutofit fontScale="50000"/></a:bodyPr><a:lstStyle/>{}{}{}{}<a:p><a:endParaRPr lang="en-US"/></a:p><a:p><a:r><a:rPr cap="all" sz="2000"/><a:t>Page </a:t></a:r><a:fld type="slidenum"><a:t>9</a:t></a:fld></a:p></p:txBody></p:sp>"#,
        para(0, "", "One"),
        para(1, "", "Two"),
        para(0, r#"<a:buAutoNum type="alphaLcParenR"/>"#, "First"),
        para(0, r#"<a:buAutoNum type="alphaLcParenR"/>"#, "Second"),
    ));
    let body = named(&scene, "Body");
    // The slide only gives an index; the layout's placeholder with that index
    // places it and says it is body text.
    assert_eq!((body.x, body.y), (10., 100.));
    let ps = &body.text.as_ref().unwrap().paragraphs;
    assert_eq!(ps[0].bullet.as_ref().unwrap().text, "•");
    assert_eq!(ps[0].margin_left, 27.);
    assert_eq!(ps[0].indent, -27.);
    assert_eq!(ps[0].runs[0].size, 16., "32 pt, shrunk to fit by half");
    assert_eq!(ps[1].bullet.as_ref().unwrap().text, "–");
    assert_eq!(ps[1].runs[0].size, 14.);
    assert_eq!(ps[2].bullet.as_ref().unwrap().text, "a)");
    assert_eq!(ps[3].bullet.as_ref().unwrap().text, "b)");
    assert!(ps[4].runs.is_empty(), "a blank line keeps its place");
    assert!(ps[4].bullet.is_none());
    assert_eq!(ps[5].runs[0].text, "PAGE ");
    assert_eq!(
        ps[5].runs[1].text, "1",
        "the slide's number, not the stored one"
    );
}

#[test]
fn shapes_fills_lines_groups_and_pictures() {
    let (deck, scene) = scene(&format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Card"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:prstGeom prst="roundRect"><a:avLst><a:gd name="adj" fmla="val 10000"/></a:avLst></a:prstGeom><a:solidFill><a:schemeClr val="accent1"><a:alpha val="50000"/></a:schemeClr></a:solidFill><a:ln w="25400"><a:solidFill><a:srgbClr val="00FF00"/></a:solidFill><a:prstDash val="dash"/></a:ln><a:effectLst><a:outerShdw blurRad="50800" dist="25400" dir="5400000"><a:srgbClr val="000000"><a:alpha val="40000"/></a:srgbClr></a:outerShdw></a:effectLst></p:spPr></p:sp>
        <p:cxnSp><p:nvCxnSpPr><p:cNvPr id="3" name="Arrow"/><p:cNvCxnSpPr/><p:nvPr/></p:nvCxnSpPr><p:spPr><a:xfrm flipV="1"><a:off x="0" y="0"/><a:ext cx="1270000" cy="635000"/></a:xfrm><a:prstGeom prst="line"/><a:ln><a:tailEnd type="triangle"/></a:ln></p:spPr><p:style><a:lnRef idx="2"><a:schemeClr val="accent1"/></a:lnRef><a:fillRef idx="0"><a:schemeClr val="accent1"/></a:fillRef></p:style></p:cxnSp>
        <p:grpSp><p:nvGrpSpPr><p:cNvPr id="4" name="Group"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="1270000" y="1270000"/><a:ext cx="2540000" cy="1270000"/><a:chOff x="0" y="0"/><a:chExt cx="1270000" cy="1270000"/></a:xfrm></p:grpSpPr>
          <p:sp><p:nvSpPr><p:cNvPr id="5" name="Inner"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:prstGeom prst="ellipse"/><a:solidFill><a:srgbClr val="000000"><a:tint val="75000"/></a:srgbClr></a:solidFill></p:spPr></p:sp>
        </p:grpSp>
        <p:pic><p:nvPicPr><p:cNvPr id="6" name="Photo" descr="A photo"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/><a:srcRect l="25000" r="10000"/></p:blipFill><p:spPr>{}</p:spPr></p:pic>
        <p:sp><p:nvSpPr><p:cNvPr id="7" name="Hidden" hidden="1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill></p:spPr></p:sp>
        <p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="8" name="Chart 1"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="0" y="0"/><a:ext cx="127000" cy="127000"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"/></a:graphic></p:graphicFrame>"#,
        xfrm(127000, 127000, 2540000, 1270000),
        xfrm(635000, 0, 635000, 635000),
        xfrm(0, 0, 1270000, 1270000),
        xfrm(0, 0, 12700, 12700),
    ));

    let card = named(&scene, "Card");
    assert_eq!(
        card.geometry,
        Geometry::RoundRect(10.),
        "a tenth of the short side"
    );
    assert_eq!(
        card.fill,
        Fill::Solid(Color {
            r: 0x33,
            g: 0x66,
            b: 0x99,
            a: 128
        })
    );
    let stroke = card.stroke.unwrap();
    assert_eq!(
        (stroke.color, stroke.width, stroke.dash),
        (Color::rgb(0, 255, 0), 2., Dash::Dash)
    );
    let shadow = card.shadow.unwrap();
    assert_eq!(shadow.blur, 4.);
    assert!(
        shadow.dx.abs() < 0.001 && (shadow.dy - 2.).abs() < 0.001,
        "straight down"
    );

    // A connector's line comes from its style; flipped, it runs bottom left
    // to top right.
    let arrow = named(&scene, "Arrow");
    let stroke = arrow.stroke.unwrap();
    assert_eq!(stroke.color, Color::rgb(0x33, 0x66, 0x99));
    assert_eq!(stroke.width, 1., "the theme's second line width");
    assert!(stroke.end_arrow && !stroke.start_arrow);
    assert_eq!(
        arrow.geometry,
        Geometry::Path(vec![PathCmd::Move(0., 1.), PathCmd::Line(1., 0.)])
    );
    assert_eq!(arrow.fill, Fill::None);

    // The group is twice as wide as its children's space.
    let inner = named(&scene, "Inner");
    assert_eq!(
        (inner.x, inner.y, inner.w, inner.h),
        (200., 100., 100., 50.)
    );
    assert_eq!(inner.geometry, Geometry::Ellipse);
    // 75% tint of black is a mid grey, not a dark one.
    assert_eq!(inner.fill, Fill::Solid(Color::rgb(137, 137, 137)));

    let photo = named(&scene, "Photo");
    assert_eq!(photo.description.as_deref(), Some("A photo"));
    let ShapeKind::Picture(pic) = &photo.kind else {
        panic!("not a picture: {photo:?}")
    };
    assert_eq!(pic.crop, [0.25, 0., 0.1, 0.]);
    assert!(deck.images.iter().any(|i| i.name == pic.image));

    assert!(scene.shapes.iter().all(|s| s.name != "Hidden"));
    assert_eq!(
        named(&scene, "Chart 1").kind,
        ShapeKind::Unsupported("Chart".into())
    );
}

#[test]
fn a_table_keeps_its_grid() {
    let cell = |attrs: &str, text: &str, pr: &str| {
        format!(
            r#"<a:tc {attrs}><a:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></a:txBody>{pr}</a:tc>"#
        )
    };
    let (_, scene) = scene(&format!(
        r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="2" name="Table"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="0" y="0"/><a:ext cx="3810000" cy="1270000"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl><a:tblPr firstRow="1" bandRow="1"><a:tableStyleId>{{5C22544A-7EE6-4342-B048-85BB5F17C6F5}}</a:tableStyleId></a:tblPr><a:tblGrid><a:gridCol w="1270000"/><a:gridCol w="1270000"/></a:tblGrid>
        <a:tr h="381000">{}{}</a:tr><a:tr h="381000">{}{}</a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#,
        cell(r#"gridSpan="2""#, "Head", ""),
        cell(r#"hMerge="1""#, "", ""),
        cell(
            "",
            "A",
            r#"<a:tcPr anchor="ctr"><a:solidFill><a:srgbClr val="ABCDEF"/></a:solidFill></a:tcPr>"#
        ),
        cell("", "B", ""),
    ));
    let table = named(&scene, "Table");
    let ShapeKind::Table(t) = &table.kind else {
        panic!("not a table: {table:?}")
    };
    // The frame is 300 pt wide; the grid's 200 pt is stretched to it.
    assert_eq!(t.columns, vec![150., 150.]);
    assert_eq!(t.rows.len(), 2);
    assert_eq!(t.rows[0].height, 30.);
    let head = &t.rows[0].cells[0];
    assert_eq!(head.col_span, 2);
    assert_eq!(
        head.fill,
        Fill::Solid(Color::rgb(0x33, 0x66, 0x99)),
        "the header takes accent 1"
    );
    let run = &head.text.as_ref().unwrap().paragraphs[0].runs[0];
    assert!(run.bold);
    assert_eq!(run.color, Color::WHITE);
    assert_eq!(t.rows[0].cells[1].col_span, 0, "covered from the left");
    let a = &t.rows[1].cells[0];
    assert_eq!(
        a.fill,
        Fill::Solid(Color::rgb(0xAB, 0xCD, 0xEF)),
        "its own fill wins"
    );
    assert_eq!(a.text.as_ref().unwrap().anchor, Anchor::Middle);
    assert_eq!(table.plain_text(), "Head A B");
}

#[test]
fn a_slide_without_a_layout_has_no_scene() {
    // Only the slide: nothing to place its shapes by.
    let bytes = zip(&[
        ("[Content_Types].xml", content_types(&[])),
        (
            "_rels/.rels",
            rels(&[("rId1", "officeDocument", "ppt/presentation.xml", false)]),
        ),
        (
            "ppt/presentation.xml",
            format!(r#"<p:presentation {P_NS}><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#).into_bytes(),
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            rels(&[("rId1", "slide", "slides/slide1.xml", false)]),
        ),
        ("ppt/slides/slide1.xml", slide_xml("")),
    ]);
    let deck = deck(&bytes);
    assert_eq!(deck.slides.len(), 1);
    assert!(deck.slides[0].scene.is_none());
    assert_eq!(
        deck.size,
        (960., 540.),
        "the size a deck has when it names none"
    );
}

/// The one-slide deck of [`package`] with more parts, and relationships to
/// them from the slide: (id, type, target) and (part name, its XML).
fn package_with(shapes: &str, more: &[(&str, &str, &str)], added: &[(&str, &str)]) -> Vec<u8> {
    use std::io::{Cursor, Read as _};
    let base = package(shapes);
    let mut zip_in = zip::ZipArchive::new(Cursor::new(base)).unwrap();
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..zip_in.len() {
        let mut f = zip_in.by_index(i).unwrap();
        let mut data = Vec::new();
        f.read_to_end(&mut data).unwrap();
        let name = f.name().unwrap().to_string();
        parts.push((name, data));
    }
    let mut all = vec![
        (
            "rId1",
            "slideLayout",
            "../slideLayouts/slideLayout1.xml",
            false,
        ),
        ("rId2", "image", "../media/image1.png", false),
    ];
    all.extend(
        more.iter()
            .map(|(id, ty, target)| (*id, *ty, *target, false)),
    );
    for (name, data) in &mut parts {
        if name == "ppt/slides/_rels/slide1.xml.rels" {
            *data = rels(&all);
        }
    }
    for (name, xml) in added {
        parts.push((name.to_string(), xml.as_bytes().to_vec()));
    }
    let refs: Vec<(&str, Vec<u8>)> = parts.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
    zip(&refs)
}

#[test]
fn a_chart_becomes_shapes() {
    let cache = |tag: &str, kind: &str, values: &[&str]| {
        let pts: String = values
            .iter()
            .enumerate()
            .map(|(i, v)| format!(r#"<c:pt idx="{i}"><c:v>{v}</c:v></c:pt>"#))
            .collect();
        format!(
            r#"<c:{tag}><c:{kind}Ref><c:{kind}Cache><c:ptCount val="{}"/>{pts}</c:{kind}Cache></c:{kind}Ref></c:{tag}>"#,
            values.len()
        )
    };
    let chart = format!(
        r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart><c:title><c:tx><c:rich><a:p><a:r><a:t>Sales</a:t></a:r></a:p></c:rich></c:tx></c:title><c:plotArea><c:barChart><c:barDir val="col"/><c:grouping val="clustered"/>
        <c:ser>{}{}{}</c:ser>
        <c:ser>{}{}{}<c:spPr><a:solidFill><a:srgbClr val="00FF00"/></a:solidFill></c:spPr></c:ser>
        <c:gapWidth val="100"/></c:barChart><c:catAx><c:delete val="0"/></c:catAx><c:valAx><c:majorGridlines/></c:valAx></c:plotArea><c:legend><c:legendPos val="b"/></c:legend></c:chart></c:chartSpace>"#,
        cache("tx", "str", &["2024"]),
        cache("cat", "str", &["North", "South"]),
        cache("val", "num", &["10", "20"]),
        cache("tx", "str", &["2025"]),
        cache("cat", "str", &["North", "South"]),
        cache("val", "num", &["30", "40"]),
    );
    let frame = r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="2" name="Chart 1"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="1270000" y="1270000"/><a:ext cx="3810000" cy="2540000"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId9"/></a:graphicData></a:graphic></p:graphicFrame>"#;
    let bytes = package_with(
        frame,
        &[("rId9", "chart", "../charts/chart1.xml")],
        &[("ppt/charts/chart1.xml", &chart)],
    );
    let deck = deck(&bytes);
    let scene = deck.slides[0].scene.as_ref().expect("a scene");
    let shape = named(scene, "Chart 1");
    let ShapeKind::Drawing(drawing) = &shape.kind else {
        panic!("not a drawing: {:?}", shape.kind)
    };
    assert_eq!(drawing.what, "chart");
    assert_eq!(drawing.label, "Sales");
    assert_eq!(shape.plain_text(), "Sales", "a chart goes by its title");

    // Four bars: two series over two categories, inside the chart's frame.
    let bars: Vec<&Shape> = drawing.shapes.iter().filter(|s| s.name == "Bar").collect();
    let data: Vec<&&Shape> = bars.iter().filter(|b| b.h > 12.).collect();
    assert_eq!(data.len(), 4, "{bars:?}");
    for b in &data {
        assert!(b.x >= 100. && b.x + b.w <= 400. && b.y >= 100. && b.y + b.h <= 300.1);
    }
    // The first series takes the theme's first accent, the second its own
    // colour; twice the value is twice as tall; they stand on one line.
    let accent = Fill::Solid(Color::rgb(0x33, 0x66, 0x99));
    let green = Fill::Solid(Color::rgb(0, 255, 0));
    let of = |fill: &Fill| -> Vec<&&&Shape> { data.iter().filter(|b| b.fill == *fill).collect() };
    let (first, second) = (of(&accent), of(&green));
    assert_eq!((first.len(), second.len()), (2, 2));
    assert!((first[1].h / first[0].h - 2.).abs() < 0.01);
    assert!((first[0].y + first[0].h - (second[1].y + second[1].h)).abs() < 0.01);
    // A gap of 100% is one bar wide: each band holds two bars and one gap.
    assert!((first[1].x - first[0].x - 3. * first[0].w).abs() < 0.01);

    let texts: Vec<String> = drawing.shapes.iter().map(Shape::plain_text).collect();
    for label in ["Sales", "North", "South", "2024", "2025", "0", "40"] {
        assert!(
            texts.iter().any(|t| t == label),
            "no label {label}: {texts:?}"
        );
    }
}

#[test]
fn a_diagram_is_read_from_its_saved_drawing() {
    let frame = r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="2" name="Diagram 1"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="1270000" y="635000"/><a:ext cx="3810000" cy="2540000"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/diagram"><dgm:relIds xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" r:dm="rId5" r:lo="rId6" r:qs="rId7" r:cs="rId8"/></a:graphicData></a:graphic></p:graphicFrame>"#;
    let data = r#"<dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><dgm:ptLst/><dgm:extLst><a:ext uri="http://schemas.microsoft.com/office/drawing/2008/diagram"><dsp:dataModelExt xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram" relId="rId9" minVer="http://schemas.openxmlformats.org/drawingml/2006/diagram"/></a:ext></dgm:extLst></dgm:dataModel>"#;
    let step = |x: i64, text: &str| {
        format!(
            r#"<dsp:sp modelId="{{1}}"><dsp:nvSpPr><dsp:cNvPr id="0" name=""/><dsp:cNvSpPr/></dsp:nvSpPr><dsp:spPr><a:xfrm><a:off x="{x}" y="127000"/><a:ext cx="1270000" cy="635000"/></a:xfrm><a:prstGeom prst="roundRect"><a:avLst/></a:prstGeom><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></dsp:spPr><dsp:style><a:lnRef idx="2"><a:schemeClr val="lt1"/></a:lnRef><a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef><a:fontRef idx="minor"><a:schemeClr val="lt1"/></a:fontRef></dsp:style><dsp:txBody><a:bodyPr anchor="ctr"/><a:lstStyle/><a:p><a:pPr algn="ctr"/><a:r><a:rPr lang="en-US" sz="2000"/><a:t>{text}</a:t></a:r></a:p></dsp:txBody><dsp:txXfrm><a:off x="{x}" y="127000"/><a:ext cx="1270000" cy="635000"/></dsp:txXfrm></dsp:sp>"#
        )
    };
    let drawing = format!(
        r#"<dsp:drawing xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><dsp:spTree><dsp:nvGrpSpPr><dsp:cNvPr id="0" name=""/><dsp:cNvGrpSpPr/></dsp:nvGrpSpPr><dsp:grpSpPr/>{}{}</dsp:spTree></dsp:drawing>"#,
        step(0, "Plan"),
        step(2540000, "Ship"),
    );
    let bytes = package_with(
        frame,
        &[
            ("rId5", "diagramData", "../diagrams/data1.xml"),
            (
                "rId9",
                "http://schemas.microsoft.com/office/2007/relationships/diagramDrawing",
                "../diagrams/drawing1.xml",
            ),
        ],
        &[
            ("ppt/diagrams/data1.xml", data),
            ("ppt/diagrams/drawing1.xml", &drawing),
        ],
    );
    let deck = deck(&bytes);
    let scene = deck.slides[0].scene.as_ref().expect("a scene");
    let shape = named(scene, "Diagram 1");
    let ShapeKind::Drawing(d) = &shape.kind else {
        panic!("not a drawing: {:?}", shape.kind)
    };
    assert_eq!(d.what, "diagram");
    assert_eq!(d.label, "Plan, Ship");
    assert_eq!(d.shapes.len(), 2);
    // Placed from the frame's corner (100, 50 pt): the second box starts
    // 200 pt in and 10 pt down.
    let ship = &d.shapes[1];
    assert_eq!((ship.x, ship.y, ship.w, ship.h), (300., 60., 100., 50.));
    assert!(matches!(ship.geometry, Geometry::RoundRect(_)));
    assert_eq!(ship.fill, Fill::Solid(Color::rgb(0x33, 0x66, 0x99)));
    let body = ship.text.as_ref().expect("its text");
    assert_eq!(body.anchor, Anchor::Middle);
    let run = &body.paragraphs[0].runs[0];
    assert_eq!((run.text.as_str(), run.size), ("Ship", 20.));
    // The colour its style gives text: lt1.
    assert_eq!(run.color, Color::rgb(0xFA, 0xFA, 0xFA));
}

#[test]
fn gradients_inner_shadows_and_turned_text() {
    let (_, scene) = scene(&format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Banner"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:prstGeom prst="rect"/><a:gradFill><a:gsLst><a:gs pos="0"><a:srgbClr val="FF0000"/></a:gs><a:gs pos="50000"><a:srgbClr val="FFFFFF"/></a:gs><a:gs pos="100000"><a:srgbClr val="0000FF"/></a:gs></a:gsLst><a:lin ang="5400000"/></a:gradFill><a:effectLst><a:innerShdw blurRad="25400" dist="12700" dir="0"><a:srgbClr val="000000"/></a:innerShdw></a:effectLst></p:spPr><p:txBody><a:bodyPr vert="vert270"/><a:lstStyle/><a:p><a:r><a:t>Up the side</a:t></a:r></a:p></p:txBody></p:sp>
        <p:sp><p:nvSpPr><p:cNvPr id="3" name="Halo"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:solidFill><a:srgbClr val="111111"/></a:solidFill><a:effectLst><a:glow rad="63500"><a:srgbClr val="FFCC00"/></a:glow></a:effectLst></p:spPr></p:sp>"#,
        xfrm(0, 0, 1270000, 2540000),
        xfrm(2540000, 0, 1270000, 1270000),
    ));
    let banner = named(&scene, "Banner");
    assert_eq!(
        banner.fill,
        Fill::Gradient {
            angle: 90.,
            from: Color::rgb(255, 0, 0),
            to: Color::rgb(0, 0, 255),
            mid: vec![(0.5, Color::WHITE)],
            radial: false,
        }
    );
    let shadow = banner.shadow.unwrap();
    assert!(shadow.inset);
    assert_eq!((shadow.blur, shadow.dx, shadow.dy), (2., 1., 0.));
    assert_eq!(banner.text.as_ref().unwrap().turn, 270.);

    // A glow is a shadow all round: no offset, not inset.
    let halo = named(&scene, "Halo").shadow.unwrap();
    assert_eq!(halo.color, Color::rgb(0xFF, 0xCC, 0));
    assert_eq!(
        (halo.blur, halo.dx, halo.dy, halo.inset),
        (5., 0., 0., false)
    );
}

#[test]
fn a_shape_takes_the_shadow_its_style_names() {
    let style = r#"<p:style><a:lnRef idx="0"><a:schemeClr val="accent1"/></a:lnRef><a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef><a:effectRef idx="2"><a:schemeClr val="accent1"/></a:effectRef><a:fontRef idx="minor"><a:schemeClr val="lt1"/></a:fontRef></p:style>"#;
    let (_, scene) = scene(&format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Styled"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{a}<a:prstGeom prst="rect"/></p:spPr>{style}</p:sp>
        <p:sp><p:nvSpPr><p:cNvPr id="3" name="Plain"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{b}<a:prstGeom prst="rect"/><a:effectLst/></p:spPr>{style}</p:sp>
        <p:sp><p:nvSpPr><p:cNvPr id="4" name="Empty"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{c}<a:prstGeom prst="rect"/><a:noFill/></p:spPr>{style}<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>Just words</a:t></a:r></a:p></p:txBody></p:sp>"#,
        a = xfrm(0, 0, 1270000, 1270000),
        b = xfrm(2540000, 0, 1270000, 1270000),
        c = xfrm(5080000, 0, 1270000, 1270000),
    ));
    // The theme's second effect style: three points of blur, two points down.
    let shadow = named(&scene, "Styled").shadow.unwrap();
    assert_eq!(
        shadow.color,
        Color {
            r: 0,
            g: 0,
            b: 0,
            a: 128
        }
    );
    assert_eq!(shadow.blur, 3.);
    assert!(shadow.dx.abs() < 0.001 && (shadow.dy - 2.).abs() < 0.001);
    // Its own (empty) list of effects wins over the style's.
    assert!(named(&scene, "Plain").shadow.is_none());
    // With nothing drawn there is nothing to cast one.
    assert!(named(&scene, "Empty").shadow.is_none());
}

#[test]
fn words_stay_inside_a_rounded_corner() {
    let (_, scene) = scene(&format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Pill"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}<a:prstGeom prst="roundRect"><a:avLst><a:gd name="adj" fmla="val 50000"/></a:avLst></a:prstGeom><a:solidFill><a:srgbClr val="336699"/></a:solidFill></p:spPr><p:txBody><a:bodyPr lIns="0" tIns="0" rIns="0" bIns="0"/><a:lstStyle/><a:p><a:r><a:t>Inside</a:t></a:r></a:p></p:txBody></p:sp>"#,
        xfrm(0, 0, 2540000, 1270000),
    ));
    let pill = named(&scene, "Pill");
    assert_eq!(pill.geometry, Geometry::RoundRect(50.));
    // The corner's radius, less what a square set in its curve leaves.
    let inset = 50. * 0.29289;
    for side in pill.text.as_ref().unwrap().insets {
        assert!((side - inset).abs() < 0.001, "{side}");
    }
}
