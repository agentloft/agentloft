//! A small element tree over quick-xml, for the parts of a document that are
//! read with look-ups rather than in one pass (a slide's shapes inherit from
//! its layout, master and theme).
//!
//! Elements are matched by local name, as in [`crate::xml`]. An
//! `mc:AlternateContent` is replaced by the children of its `mc:Fallback`
//! (plain DrawingML, usually a picture), or of its first `mc:Choice` when there
//! is no fallback.

use quick_xml::events::Event;

use crate::xml::{MAX_DEPTH, Xml, local, strip_prefix, text_of};

/// A part with more elements than this is refused.
const MAX_NODES: usize = 2_000_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    /// Local name (`a:off` → `off`).
    pub name: String,
    attrs: Vec<Attr>,
    pub children: Vec<Node>,
    /// Character data directly inside this element.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Attr {
    name: String,
    prefixed: bool,
    value: String,
}

impl Node {
    /// Parses a whole part; the returned node is its root element.
    pub fn parse(xml: &str) -> anyhow::Result<Node> {
        let mut x = Xml::new(xml);
        let mut stack: Vec<Node> = vec![Node::default()];
        let mut count = 0usize;
        loop {
            let ev = x.next()?;
            match &ev {
                Event::Start(e) => {
                    count += 1;
                    if count > MAX_NODES || stack.len() > MAX_DEPTH {
                        anyhow::bail!("the document is too large or too deeply nested");
                    }
                    let mut attrs = Vec::new();
                    for a in e.attributes().with_checks(false).flatten() {
                        let q = a.key.0;
                        if q.starts_with("xmlns") {
                            continue;
                        }
                        if let Ok(value) = a.normalized_value(quick_xml::XmlVersion::Implicit1_0) {
                            attrs.push(Attr {
                                name: strip_prefix(q).to_string(),
                                prefixed: q.contains(':'),
                                value: value.into_owned(),
                            });
                        }
                    }
                    stack.push(Node {
                        name: local(e).to_string(),
                        attrs,
                        children: Vec::new(),
                        text: String::new(),
                    });
                }
                Event::End(_) => {
                    if stack.len() < 2 {
                        continue;
                    }
                    let Some(node) = stack.pop() else { continue };
                    let Some(parent) = stack.last_mut() else {
                        continue;
                    };
                    if node.name == "AlternateContent" {
                        let pick = node
                            .children
                            .iter()
                            .position(|c| c.name == "Fallback")
                            .or_else(|| node.children.iter().position(|c| c.name == "Choice"));
                        if let Some(ix) = pick {
                            let mut node = node;
                            parent.children.append(&mut node.children[ix].children);
                        }
                    } else {
                        parent.children.push(node);
                    }
                }
                Event::Eof => break,
                _ => {
                    if let (Some(t), Some(top)) = (text_of(&ev), stack.last_mut()) {
                        top.text.push_str(&t);
                    }
                }
            }
        }
        // Unclosed elements (malformed input): fold them into their parents.
        while stack.len() > 1 {
            if let Some(node) = stack.pop()
                && let Some(parent) = stack.last_mut()
            {
                parent.children.push(node);
            }
        }
        stack
            .pop()
            .and_then(|mut root| root.children.pop())
            .ok_or_else(|| anyhow::anyhow!("the part has no root element"))
    }

    /// The attribute with this local name; an unprefixed one wins over a
    /// prefixed one of the same name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name == name && !a.prefixed)
            .or_else(|| self.attrs.iter().find(|a| a.name == name))
            .map(|a| a.value.as_str())
    }

    /// A namespace-prefixed attribute, such as `r:embed`.
    pub fn attr_prefixed(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name == name && a.prefixed)
            .map(|a| a.value.as_str())
    }

    pub fn num(&self, name: &str) -> Option<f64> {
        self.attr(name)?.trim().parse().ok()
    }

    /// `1`/`true` → true, `0`/`false` → false.
    pub fn flag(&self, name: &str) -> Option<bool> {
        match self.attr(name)?.trim() {
            "1" | "true" | "on" => Some(true),
            "0" | "false" | "off" => Some(false),
            _ => None,
        }
    }

    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn has(&self, name: &str) -> bool {
        self.child(name).is_some()
    }

    /// Follows a chain of child names.
    pub fn path(&self, names: &[&str]) -> Option<&Node> {
        let mut node = self;
        for name in names {
            node = node.child(name)?;
        }
        Some(node)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_tree() {
        let n = Node::parse(
            r#"<p:sp xmlns:p="x"><p:nvPr id="3" r:id="rId1"/><a:t>a &amp; b</a:t><a:t>c</a:t></p:sp>"#,
        )
        .unwrap();
        assert_eq!(n.name, "sp");
        assert_eq!(n.child("nvPr").unwrap().attr("id"), Some("3"));
        assert_eq!(n.child("nvPr").unwrap().attr_prefixed("id"), Some("rId1"));
        assert_eq!(n.child("t").unwrap().text, "a & b");
        assert_eq!(n.children_named("t").count(), 2);
    }

    #[test]
    fn alternate_content_prefers_the_fallback() {
        let n = Node::parse(
            "<r><mc:AlternateContent><mc:Choice><new/></mc:Choice><mc:Fallback><pic/></mc:Fallback></mc:AlternateContent></r>",
        )
        .unwrap();
        assert_eq!(n.children.len(), 1);
        assert_eq!(n.children[0].name, "pic");
        let n = Node::parse(
            "<r><mc:AlternateContent><mc:Choice><new/></mc:Choice></mc:AlternateContent></r>",
        )
        .unwrap();
        assert_eq!(n.children[0].name, "new");
    }

    #[test]
    fn malformed_does_not_panic() {
        assert!(Node::parse("").is_err());
        let _ = Node::parse("<a><b></a>");
        let _ = Node::parse("<a><b>");
    }
}
