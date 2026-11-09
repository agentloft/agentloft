//! Thin helpers over quick-xml's streaming reader.
//!
//! Elements are matched by local name (`w:p` → `p`) so documents that use
//! unusual namespace prefixes still parse.

use std::borrow::Cow;

use anyhow::anyhow;
use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

/// Deepest element nesting we follow. Real documents stay far below this;
/// it keeps hostile input from exhausting the stack in recursive parsers.
pub const MAX_DEPTH: usize = 256;

pub struct Xml<'a> {
    reader: Reader<&'a [u8]>,
}

impl<'a> Xml<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut reader = Reader::from_str(text);
        // Every element yields Start and End, so parsers need not special-case
        // `<w:b/>`.
        reader.config_mut().expand_empty_elements = true;
        Self { reader }
    }

    pub fn next(&mut self) -> anyhow::Result<Event<'a>> {
        self.reader.read_event().map_err(|e| {
            anyhow!(
                "malformed XML near byte {}: {e}",
                self.reader.error_position()
            )
        })
    }

    /// Skips the rest of the element whose Start event was just read.
    pub fn skip(&mut self) -> anyhow::Result<()> {
        let mut depth = 0usize;
        loop {
            match self.next()? {
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    if depth == 0 {
                        return Ok(());
                    }
                    depth -= 1;
                }
                Event::Eof => return Ok(()),
                _ => {}
            }
        }
    }

    /// Collects all character data up to the end of the element whose Start
    /// event was just read, including text in descendants.
    pub fn text_until_end(&mut self) -> anyhow::Result<String> {
        let mut out = String::new();
        let mut depth = 0usize;
        loop {
            let ev = self.next()?;
            match &ev {
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    if depth == 0 {
                        return Ok(out);
                    }
                    depth -= 1;
                }
                Event::Eof => return Ok(out),
                _ => {
                    if let Some(t) = text_of(&ev) {
                        out.push_str(&t);
                    }
                }
            }
        }
    }
}

/// The character data carried by a Text, CData or entity-reference event.
pub fn text_of<'e>(ev: &'e Event<'_>) -> Option<Cow<'e, str>> {
    match ev {
        Event::Text(t) => Some(t.xml10_content()),
        Event::CData(c) => Some(Cow::Borrowed(&**c)),
        Event::GeneralRef(r) => {
            if r.is_char_ref() {
                let c = r.resolve_char_ref().ok().flatten()?;
                Some(Cow::Owned(c.to_string()))
            } else {
                resolve_predefined_entity(r).map(Cow::Borrowed)
            }
        }
        _ => None,
    }
}

/// Local name of an element (`w:p` → `p`).
pub fn local<'b>(e: &'b BytesStart<'_>) -> &'b str {
    strip_prefix(e.name().0)
}

pub fn strip_prefix(qname: &str) -> &str {
    qname.rsplit(':').next().unwrap_or(qname)
}

/// Value of the attribute with the given local name, unescaped.
pub fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    for a in e.attributes().with_checks(false).flatten() {
        if strip_prefix(a.key.0) == name {
            return a
                .normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(Cow::into_owned);
        }
    }
    None
}

/// Value of a namespace-prefixed attribute with the given local name, such as
/// `r:id` when the element also carries an unprefixed `id`.
pub fn attr_prefixed(e: &BytesStart<'_>, name: &str) -> Option<String> {
    for a in e.attributes().with_checks(false).flatten() {
        let q = a.key.0;
        if q.contains(':') && strip_prefix(q) == name && !q.starts_with("xmlns") {
            return a
                .normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(Cow::into_owned);
        }
    }
    None
}

/// Reads an OOXML on/off attribute (`w:val`): absent means on; `0`, `false`,
/// `off` and `none` mean off.
pub fn on_off(e: &BytesStart<'_>, name: &str) -> bool {
    match attr(e, name) {
        None => true,
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "none"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_with_entities() {
        let mut x = Xml::new("<a>x &amp; y &#65;<b>c</b><![CDATA[<d>]]></a>");
        assert!(matches!(x.next().unwrap(), Event::Start(_)));
        assert_eq!(x.text_until_end().unwrap(), "x & y Ac<d>");
    }

    #[test]
    fn attributes_by_local_name() {
        let mut x = Xml::new(r#"<p:sldId id="256" r:id="rId2" w:val="a&amp;b"/>"#);
        let Event::Start(e) = x.next().unwrap() else {
            panic!("expected start")
        };
        assert_eq!(local(&e), "sldId");
        assert_eq!(attr(&e, "id").as_deref(), Some("256"));
        assert_eq!(attr_prefixed(&e, "id").as_deref(), Some("rId2"));
        assert_eq!(attr(&e, "val").as_deref(), Some("a&b"));
    }

    #[test]
    fn malformed_is_error() {
        let mut x = Xml::new("<a><b></a>");
        let mut saw_err = false;
        for _ in 0..10 {
            match x.next() {
                Err(_) => {
                    saw_err = true;
                    break;
                }
                Ok(Event::Eof) => break,
                Ok(_) => {}
            }
        }
        assert!(saw_err);
    }
}
