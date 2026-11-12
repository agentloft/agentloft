//! PDF: page sizes and drawn pages.

use loft_docs::{DocKind, Document, load_bytes};

/// A PDF written by hand: each page is (width, height, rotation, content).
fn pdf(pages: &[(u32, u32, u32, &str)]) -> Vec<u8> {
    let mut objects: Vec<String> = Vec::new();
    let kids: Vec<String> = (0..pages.len())
        .map(|i| format!("{} 0 R", 3 + 2 * i))
        .collect();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        pages.len()
    ));
    for (i, (w, h, rotate, content)) in pages.iter().enumerate() {
        // Every page can write in Helvetica and carries one link.
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Rotate {rotate} /Contents {} 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Annots [ << /Type /Annot /Subtype /Link /Rect [20 50 150 80] /A << /S /URI /URI (https://example.com/a) >> >> << /Type /Annot /Subtype /Link /Rect [0 0 10 10] /Dest [3 0 R /Fit] >> ] >>",
            4 + 2 * i
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
    }
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
    }
    let xref = out.len();
    out.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for o in offsets {
        out.push_str(&format!("{o:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    out.into_bytes()
}

fn open(bytes: &[u8]) -> loft_docs::pdf::PdfDoc {
    match load_bytes(DocKind::Pdf, "pdf", bytes).expect("pdf opens") {
        Document::Pdf(doc) => doc,
        other => panic!("expected a PDF, got {other:?}"),
    }
}

#[test]
fn pages_have_their_shown_size() {
    let doc = open(&pdf(&[(200, 100, 0, ""), (200, 100, 90, "")]));
    // The second page is turned on its side.
    assert_eq!(doc.pages, vec![(200., 100.), (100., 200.)]);
    assert!(!doc.truncated);
}

#[test]
fn a_page_is_drawn_on_white_at_the_scale_asked() {
    // A red box over the bottom left quarter (PDF's origin is bottom left).
    let doc = open(&pdf(&[(200, 100, 0, "1 0 0 rg 0 0 100 50 re f")]));
    let image = doc.render(0, 2.).expect("the page draws");
    assert_eq!((image.width, image.height), (400, 200));
    assert_eq!(image.rgba.len(), 400 * 200 * 4);
    let at = |x: usize, y: usize| {
        let i = (y * 400 + x) * 4;
        [
            image.rgba[i],
            image.rgba[i + 1],
            image.rgba[i + 2],
            image.rgba[i + 3],
        ]
    };
    assert_eq!(at(10, 10), [255, 255, 255, 255], "top left is paper");
    assert_eq!(at(10, 190), [255, 0, 0, 255], "bottom left is the box");
    assert_eq!(at(390, 190), [255, 255, 255, 255], "bottom right is paper");

    assert!(doc.render(5, 1.).is_err(), "no such page");
    // A huge scale is brought down, not refused.
    let big = doc.render(0, 1_000.).expect("still draws");
    assert!(big.width <= loft_docs::pdf::MAX_PAGE_PIXELS as u32);
}

#[test]
fn what_is_not_a_pdf_is_an_error() {
    for bytes in [&b""[..], b"not a pdf", b"%PDF-1.7\nbut nothing else"] {
        assert!(load_bytes(DocKind::Pdf, "pdf", bytes).is_err());
    }
}

#[test]
fn a_page_picture_carries_its_pins() {
    let doc = open(&pdf(&[(200, 100, 0, "")]));
    let pins = [loft_docs::pdf::Pin {
        x: 0.5,
        y: 0.5,
        n: 12,
        area: None,
    }];
    let png = doc.render_marked_png(0, 600, &pins).expect("a picture");
    assert!(png.starts_with(b"\x89PNG"));
    let plain = doc.render_marked_png(0, 600, &[]).expect("a picture");
    assert_ne!(png, plain, "the pin is drawn");
    // A pin off the page's edge is clipped, not a panic.
    let edge = [loft_docs::pdf::Pin {
        x: 1.,
        y: 0.,
        n: 3,
        area: None,
    }];
    assert!(doc.render_marked_png(0, 600, &edge).is_ok());
    // An area is outlined as well; one that runs off the page is clipped.
    let area = [loft_docs::pdf::Pin {
        area: Some([0.2, 0.2, 0.3, 0.4]),
        ..pins[0]
    }];
    let outlined = doc.render_marked_png(0, 600, &area).expect("a picture");
    assert_ne!(outlined, png, "the area is outlined");
    let off = [loft_docs::pdf::Pin {
        area: Some([0.8, -0.5, 0.9, 2.]),
        ..pins[0]
    }];
    assert!(doc.render_marked_png(0, 600, &off).is_ok());
}

#[test]
fn a_page_gives_its_text_and_links() {
    // Two lines, the second written as two pieces with a word gap between
    // them and no space character.
    let content = "BT /F1 24 Tf 20 60 Td (Hello world) Tj ET \
                   BT /F1 10 Tf 20 30 Td (Second) Tj 40 0 Td (line) Tj ET";
    let doc = open(&pdf(&[(200, 100, 0, content)]));
    let text = doc.page_text(0).expect("the page reads");
    let said: Vec<&str> = text.spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(said, ["Hello world", "Second line"]);
    let hello = &text.spans[0];
    // The baseline is 60 pt from the bottom of a 100 pt page; the line's top
    // is four fifths of its size above that.
    assert!((hello.x - 20.).abs() < 0.01 && (hello.y - (40. - 19.2)).abs() < 0.01);
    assert_eq!(hello.h, 24.);
    assert!(
        (110. ..135.).contains(&hello.w),
        "about as wide as the words are in Helvetica: {}",
        hello.w
    );

    // A place for every character and one for the end, left to right.
    assert_eq!(hello.stops.len(), "Hello world".chars().count() + 1);
    assert_eq!(hello.stops[0], 0.);
    assert!(hello.stops.windows(2).all(|w| w[0] <= w[1]));
    assert!((hello.stops[11] - hello.w).abs() < 0.01);
    // "Second line": the gap the page left is the space's place.
    let second = &text.spans[1];
    assert!(second.stops[7] - second.stops[6] > 2., "{:?}", second.stops);

    use loft_docs::pdf::LinkTo;
    assert_eq!(text.links.len(), 2);
    let link = &text.links[0];
    assert_eq!(link.to, LinkTo::Url("https://example.com/a".into()));
    assert_eq!((link.x, link.y, link.w, link.h), (20., 20., 130., 30.));
    // The other goes to a page of this document: object 3 is the first page.
    assert_eq!(text.links[1].to, LinkTo::Page(0));

    assert!(doc.page_text(3).is_err());
}
