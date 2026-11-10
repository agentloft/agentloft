//! The page outline an agent reads. `agent.js` walks the rendered DOM and
//! returns a [`PageSnapshot`] as JSON; [`format_snapshot`] turns it into
//! compact text with `[ref=N]` handles and enforces the size cap.

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// The outline is capped at about this many characters.
pub const MAX_SNAPSHOT_CHARS: usize = 25_000;

const MAX_NAME_CHARS: usize = 100;
const MAX_VALUE_CHARS: usize = 100;
const MAX_TEXT_CHARS: usize = 600;
const MAX_PRE_CHARS: usize = 1_500;
const MAX_HREF_CHARS: usize = 120;
const MAX_OPTIONS: usize = 12;

/// Raw page outline as produced by the snapshot script.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageSnapshot {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub viewport: Viewport,
    #[serde(default)]
    pub blocks: Vec<Block>,
    /// The script stopped collecting early (very large page).
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Viewport {
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
    #[serde(default)]
    pub scroll_y: f64,
    #[serde(default)]
    pub scroll_height: f64,
}

/// One line of the outline: a paragraph, heading, list item, table row, …
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Block {
    #[serde(default)]
    pub kind: BlockKind,
    /// Heading level (1–6) for [`BlockKind::Heading`].
    #[serde(default)]
    pub level: u8,
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Heading,
    Item,
    Quote,
    Pre,
    /// Paragraphs, table rows, and any kind this version doesn't know.
    #[default]
    #[serde(other)]
    Text,
}

/// Inline content: plain text or an interactive element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Part {
    Text(String),
    Element(ElementInfo),
}

/// An interactive element tagged with `data-loft-ref`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementInfo {
    #[serde(rename = "ref")]
    pub ref_id: u32,
    /// `link`, `button`, `input`, `textarea`, `select`, `checkbox`, `radio`,
    /// `editable`, or an ARIA widget role (`tab`, `menuitem`, `switch`, …).
    pub role: String,
    /// Accessible name (label, aria-label, text content, title).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub href: Option<String>,
    /// Input type (`text`, `email`, `password`, …).
    #[serde(default, rename = "type")]
    pub input_type: Option<String>,
    /// The form field's `name` attribute.
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub selected: Option<bool>,
    #[serde(default)]
    pub expanded: Option<bool>,
    #[serde(default)]
    pub disabled: bool,
    /// `<select>` option labels (the script sends at most a few dozen).
    #[serde(default)]
    pub options: Vec<String>,
    /// Total `<select>` option count.
    #[serde(default)]
    pub option_count: Option<u32>,
}

/// [`format_snapshot`]'s output.
#[derive(Debug, Clone, PartialEq)]
pub struct FormattedSnapshot {
    pub text: String,
    pub truncated: bool,
    /// Interactive elements in `text`.
    pub refs_shown: usize,
    /// Interactive elements the script found.
    pub refs_total: usize,
}

impl PageSnapshot {
    pub fn ref_count(&self) -> usize {
        self.blocks
            .iter()
            .flat_map(|b| &b.parts)
            .filter(|p| matches!(p, Part::Element(_)))
            .count()
    }
}

/// Renders the outline, keeping it under about `max_chars` characters.
pub fn format_snapshot(snapshot: &PageSnapshot, max_chars: usize) -> FormattedSnapshot {
    let mut text = String::new();
    let _ = writeln!(text, "Title: {}", one_line(&snapshot.title, 200));
    let _ = writeln!(text, "URL: {}", snapshot.url);
    let v = &snapshot.viewport;
    if v.width > 0.0 && v.height > 0.0 {
        let _ = writeln!(
            text,
            "Viewport: {}x{}, scrolled {} of {}px",
            v.width.round(),
            v.height.round(),
            v.scroll_y.round(),
            v.scroll_height.round()
        );
    }
    text.push('\n');

    let refs_total = snapshot.ref_count();
    let mut used = text.chars().count();
    let mut refs_shown = 0;
    let mut cut_at = None;
    for (ix, block) in snapshot.blocks.iter().enumerate() {
        let line = format_block(block, &snapshot.url);
        if line.is_empty() {
            continue;
        }
        let len = line.chars().count() + 1;
        if used + len > max_chars {
            cut_at = Some(ix);
            break;
        }
        used += len;
        text.push_str(&line);
        text.push('\n');
        refs_shown += block
            .parts
            .iter()
            .filter(|p| matches!(p, Part::Element(_)))
            .count();
    }

    if snapshot.blocks.is_empty() {
        text.push_str("(no visible content)\n");
    }

    let truncated = cut_at.is_some() || snapshot.truncated;
    if let Some(ix) = cut_at {
        let hidden_blocks = snapshot.blocks.len() - ix;
        let _ = writeln!(
            text,
            "\n[Truncated: the snapshot is capped at ~{max_chars} characters. {hidden_blocks} more \
             lines with {} more interactive elements are not shown. Scroll and snapshot again, or \
             use eval to query specific parts of the page.]",
            refs_total - refs_shown
        );
    } else if snapshot.truncated {
        text.push_str(
            "\n[Truncated: the page is very large and the snapshot stopped collecting early. \
             Scroll and snapshot again, or use eval to query specific parts of the page.]\n",
        );
    }

    FormattedSnapshot {
        text,
        truncated,
        refs_shown,
        refs_total,
    }
}

fn format_block(block: &Block, page_url: &str) -> String {
    if block.kind == BlockKind::Pre {
        let body: String = block
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                Part::Element(_) => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let body = body.trim_matches('\n');
        if body.trim().is_empty() {
            return String::new();
        }
        return format!("```\n{}\n```", truncate(body, MAX_PRE_CHARS));
    }

    let mut line = String::new();
    for part in &block.parts {
        let piece = match part {
            Part::Text(t) => one_line(t, MAX_TEXT_CHARS),
            Part::Element(e) => format_element(e, page_url),
        };
        if piece.is_empty() {
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&piece);
    }
    if line.is_empty() {
        return line;
    }
    let prefix = match block.kind {
        BlockKind::Heading => format!("{} ", "#".repeat(block.level.clamp(1, 6) as usize)),
        BlockKind::Item => "- ".into(),
        BlockKind::Quote => "> ".into(),
        BlockKind::Text | BlockKind::Pre => String::new(),
    };
    prefix + &line
}

/// `[ref=3] link "More information" -> https://iana.org/…`
pub fn format_element(e: &ElementInfo, page_url: &str) -> String {
    let mut out = format!("[ref={}] {}", e.ref_id, e.role);
    let name = one_line(&e.name, MAX_NAME_CHARS);
    if !name.is_empty() {
        let _ = write!(out, " {}", quoted(&name));
    }
    if let Some(t) = e
        .input_type
        .as_deref()
        .filter(|t| !t.is_empty() && *t != "text")
    {
        let _ = write!(out, " type={t}");
    }
    if let Some(field) = e.field.as_deref().filter(|f| !f.is_empty()) {
        let _ = write!(out, " name={}", quoted(&one_line(field, 60)));
    }
    if let Some(p) = e
        .placeholder
        .as_deref()
        .filter(|p| !p.is_empty() && *p != e.name)
    {
        let _ = write!(out, " placeholder={}", quoted(&one_line(p, 60)));
    }
    if let Some(v) = e.value.as_deref().filter(|v| !v.is_empty()) {
        let _ = write!(out, " value={}", quoted(&one_line(v, MAX_VALUE_CHARS)));
    }
    if !e.options.is_empty() {
        let shown: Vec<String> = e
            .options
            .iter()
            .take(MAX_OPTIONS)
            .map(|o| quoted(&one_line(o, 40)))
            .collect();
        let total = e.option_count.map_or(e.options.len(), |c| c as usize);
        let more = total.saturating_sub(shown.len());
        let _ = write!(out, " options=[{}", shown.join(", "));
        if more > 0 {
            let _ = write!(out, ", … {more} more");
        }
        out.push(']');
    }
    let mut flags = Vec::new();
    match e.checked {
        Some(true) => flags.push("checked"),
        Some(false) => flags.push("unchecked"),
        None => {}
    }
    if e.selected == Some(true) {
        flags.push("selected");
    }
    match e.expanded {
        Some(true) => flags.push("expanded"),
        Some(false) => flags.push("collapsed"),
        None => {}
    }
    if e.disabled {
        flags.push("disabled");
    }
    if !flags.is_empty() {
        let _ = write!(out, " ({})", flags.join(", "));
    }
    if let Some(href) = e.href.as_deref().and_then(|h| display_href(h, page_url)) {
        let _ = write!(out, " -> {href}");
    }
    out
}

/// Same-origin links print as a path; `javascript:` links print nothing.
fn display_href(href: &str, page_url: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.to_ascii_lowercase().starts_with("javascript:") {
        return None;
    }
    let shown = match (url::Url::parse(href), url::Url::parse(page_url)) {
        (Ok(target), Ok(page))
            if target.origin() == page.origin() && target.origin().is_tuple() =>
        {
            let mut path = target.path().to_owned();
            if let Some(q) = target.query() {
                path.push('?');
                path.push_str(q);
            }
            if let Some(f) = target.fragment() {
                path.push('#');
                path.push_str(f);
            }
            path
        }
        _ => href.to_owned(),
    };
    Some(truncate(&shown, MAX_HREF_CHARS))
}

/// Collapses whitespace and truncates to `max` characters.
fn one_line(s: &str, max: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, max)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn example_com() -> PageSnapshot {
        serde_json::from_value(json!({
            "url": "https://example.com/",
            "title": "Example Domain",
            "viewport": {"width": 1024, "height": 700, "scrollY": 0, "scrollHeight": 700},
            "blocks": [
                {"kind": "heading", "level": 1, "parts": ["Example Domain"]},
                {"kind": "text", "parts": ["This domain is for use in   illustrative\n examples."]},
                {"kind": "text", "parts": [
                    {"ref": 1, "role": "link", "name": "More information...",
                     "href": "https://www.iana.org/domains/example"}
                ]}
            ],
            "truncated": false
        }))
        .unwrap()
    }

    #[test]
    fn parses_script_output() {
        let s = example_com();
        assert_eq!(s.title, "Example Domain");
        assert_eq!(s.blocks.len(), 3);
        assert_eq!(s.blocks[0].kind, BlockKind::Heading);
        assert_eq!(s.ref_count(), 1);
        let Part::Element(link) = &s.blocks[2].parts[0] else {
            panic!("expected element");
        };
        assert_eq!(link.ref_id, 1);
        assert_eq!(link.role, "link");
    }

    #[test]
    fn tolerates_unknown_kinds_and_missing_fields() {
        let s: PageSnapshot = serde_json::from_value(json!({
            "blocks": [{"kind": "something-new", "parts": ["hi", {"ref": 2, "role": "button"}]}],
            "extra": 1
        }))
        .unwrap();
        assert_eq!(s.blocks[0].kind, BlockKind::Text);
        let out = format_snapshot(&s, MAX_SNAPSHOT_CHARS);
        assert!(out.text.contains("hi [ref=2] button\n"), "{}", out.text);
    }

    #[test]
    fn formats_example_com() {
        let out = format_snapshot(&example_com(), MAX_SNAPSHOT_CHARS);
        assert_eq!(
            out.text,
            "Title: Example Domain\n\
             URL: https://example.com/\n\
             Viewport: 1024x700, scrolled 0 of 700px\n\
             \n\
             # Example Domain\n\
             This domain is for use in illustrative examples.\n\
             [ref=1] link \"More information...\" -> https://www.iana.org/domains/example\n"
        );
        assert!(!out.truncated);
        assert_eq!((out.refs_shown, out.refs_total), (1, 1));
    }

    #[test]
    fn formats_inline_rows_and_form_controls() {
        let s: PageSnapshot = serde_json::from_value(json!({
            "url": "https://news.ycombinator.com/news",
            "title": "Hacker News",
            "blocks": [
                {"kind": "text", "parts": [
                    "1.",
                    {"ref": 4, "role": "link", "name": "upvote", "href": "https://news.ycombinator.com/vote?id=1&how=up"},
                    {"ref": 5, "role": "link", "name": "Show \"HN\": A thing", "href": "https://thing.dev/"},
                    "(",
                    {"ref": 6, "role": "link", "name": "thing.dev", "href": "javascript:void(0)"},
                    ")"
                ]},
                {"kind": "item", "parts": [
                    {"ref": 7, "role": "input", "name": "Email", "type": "email", "field": "email",
                     "placeholder": "you@example.com", "value": "a@b.c"},
                    {"ref": 8, "role": "input", "name": "Search", "type": "text", "placeholder": "Search"},
                    {"ref": 9, "role": "checkbox", "name": "Remember me", "checked": true},
                    {"ref": 10, "role": "select", "name": "Size", "field": "size", "value": "M",
                     "options": ["S", "M", "L"], "optionCount": 3},
                    {"ref": 11, "role": "button", "name": "Save", "disabled": true},
                    {"ref": 12, "role": "tab", "name": "Code", "selected": true},
                    {"ref": 13, "role": "button", "name": "Menu", "expanded": false}
                ]}
            ]
        }))
        .unwrap();
        let out = format_snapshot(&s, MAX_SNAPSHOT_CHARS);
        let lines: Vec<&str> = out.text.lines().collect();
        assert_eq!(
            lines[3],
            "1. [ref=4] link \"upvote\" -> /vote?id=1&how=up \
             [ref=5] link \"Show \\\"HN\\\": A thing\" -> https://thing.dev/ \
             ( [ref=6] link \"thing.dev\" )"
        );
        assert_eq!(
            lines[4],
            "- [ref=7] input \"Email\" type=email name=\"email\" placeholder=\"you@example.com\" \
             value=\"a@b.c\" [ref=8] input \"Search\" [ref=9] checkbox \"Remember me\" (checked) \
             [ref=10] select \"Size\" name=\"size\" value=\"M\" options=[\"S\", \"M\", \"L\"] \
             [ref=11] button \"Save\" (disabled) [ref=12] tab \"Code\" (selected) \
             [ref=13] button \"Menu\" (collapsed)"
        );
    }

    #[test]
    fn select_options_are_capped() {
        let options: Vec<String> = (0..40).map(|i| format!("opt{i}")).collect();
        let e = ElementInfo {
            ref_id: 1,
            role: "select".into(),
            options: options[..20].to_vec(),
            option_count: Some(40),
            ..Default::default()
        };
        let s = format_element(&e, "https://a.b/");
        assert!(s.ends_with("\"opt11\", … 28 more]"), "{s}");
    }

    #[test]
    fn long_text_and_names_are_shortened() {
        let e = ElementInfo {
            ref_id: 3,
            role: "link".into(),
            name: "x".repeat(500),
            href: Some(format!("https://other.example/{}", "p".repeat(300))),
            ..Default::default()
        };
        let s = format_element(&e, "https://a.b/");
        assert!(s.chars().count() < 260, "{}", s.chars().count());
        assert!(s.contains('…'));
    }

    #[test]
    fn pre_blocks_keep_newlines() {
        let s = PageSnapshot {
            blocks: vec![Block {
                kind: BlockKind::Pre,
                level: 0,
                parts: vec![Part::Text("fn main() {\n    println!(\"hi\");\n}\n".into())],
            }],
            ..Default::default()
        };
        let out = format_snapshot(&s, MAX_SNAPSHOT_CHARS);
        assert!(
            out.text
                .contains("```\nfn main() {\n    println!(\"hi\");\n}\n```\n")
        );
    }

    #[test]
    fn caps_size_with_a_truncation_note() {
        let blocks = (0..2_000)
            .map(|i| Block {
                kind: BlockKind::Item,
                level: 0,
                parts: vec![
                    Part::Text(format!("Story number {i} with a reasonably long title")),
                    Part::Element(ElementInfo {
                        ref_id: i + 1,
                        role: "link".into(),
                        name: format!("{i} comments"),
                        href: Some(format!("https://news.ycombinator.com/item?id={i}")),
                        ..Default::default()
                    }),
                ],
            })
            .collect();
        let s = PageSnapshot {
            url: "https://news.ycombinator.com/".into(),
            title: "HN".into(),
            blocks,
            ..Default::default()
        };
        let out = format_snapshot(&s, MAX_SNAPSHOT_CHARS);
        assert!(out.truncated);
        let len = out.text.chars().count();
        assert!(len <= MAX_SNAPSHOT_CHARS + 400, "{len}");
        assert!(len >= MAX_SNAPSHOT_CHARS - 200, "{len}");
        assert_eq!(out.refs_total, 2_000);
        assert!(out.refs_shown > 100 && out.refs_shown < 2_000);
        let note = out.text.lines().last().unwrap();
        assert!(note.starts_with("[Truncated:"), "{note}");
        assert!(note.contains(&format!("{} more interactive", 2_000 - out.refs_shown)));
    }

    #[test]
    fn script_side_truncation_is_reported() {
        let mut s = example_com();
        s.truncated = true;
        let out = format_snapshot(&s, MAX_SNAPSHOT_CHARS);
        assert!(out.truncated);
        assert!(out.text.contains("stopped collecting early"));
    }

    #[test]
    fn empty_page() {
        let out = format_snapshot(
            &PageSnapshot {
                url: "about:blank".into(),
                ..Default::default()
            },
            MAX_SNAPSHOT_CHARS,
        );
        assert_eq!(
            out.text,
            "Title: \nURL: about:blank\n\n(no visible content)\n"
        );
    }
}
