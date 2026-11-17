//! Markdown building blocks shared by the Word and slide converters: inline
//! runs with formatting, escaping, list indentation and GitHub tables.

/// Character formatting of a run of text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Marks {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Inline {
    Text {
        text: String,
        marks: Marks,
        link: Option<String>,
    },
    LineBreak,
    Image {
        alt: String,
        url: String,
        link: Option<String>,
    },
    /// Markdown emitted verbatim, such as a footnote reference.
    Raw(String),
}

/// The inline content of one paragraph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InlineBuf {
    items: Vec<Inline>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOpts {
    /// Rendering into a table cell: breaks become `<br>` and pipes are escaped
    /// even inside code spans.
    pub in_table: bool,
    /// Drop bold markers (used for headings, which are bold anyway).
    pub no_bold: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Delim {
    Link(String),
    Bold,
    Italic,
    Strike,
}

impl Delim {
    fn open(&self) -> &'static str {
        match self {
            Delim::Link(_) => "[",
            Delim::Bold => "**",
            Delim::Italic => "*",
            Delim::Strike => "~~",
        }
    }

    fn close(&self, out: &mut String) {
        match self {
            Delim::Link(url) => {
                out.push_str("](");
                out.push_str(&link_destination(url));
                out.push(')');
            }
            other => out.push_str(other.open()),
        }
    }
}

impl InlineBuf {
    pub fn push_text(&mut self, text: &str, marks: Marks, link: Option<&str>) {
        if text.is_empty() {
            return;
        }
        if let Some(Inline::Text {
            text: prev,
            marks: pm,
            link: pl,
        }) = self.items.last_mut()
            && *pm == marks
            && pl.as_deref() == link
        {
            prev.push_str(text);
            return;
        }
        self.items.push(Inline::Text {
            text: text.to_string(),
            marks,
            link: link.map(str::to_string),
        });
    }

    pub fn push_break(&mut self) {
        self.items.push(Inline::LineBreak);
    }

    pub fn push_image(&mut self, alt: &str, url: &str, link: Option<&str>) {
        self.items.push(Inline::Image {
            alt: alt.to_string(),
            url: url.to_string(),
            link: link.map(str::to_string),
        });
    }

    pub fn push_raw(&mut self, raw: String) {
        self.items.push(Inline::Raw(raw));
    }

    /// True when there is nothing visible: no non-blank text, no images.
    pub fn is_blank(&self) -> bool {
        self.items.iter().all(|i| match i {
            Inline::Text { text, .. } => text.trim().is_empty(),
            Inline::LineBreak => true,
            Inline::Image { .. } | Inline::Raw(_) => false,
        })
    }

    /// Unformatted text; line breaks become `\n`.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            match item {
                Inline::Text { text, .. } => out.push_str(text),
                Inline::LineBreak => out.push('\n'),
                Inline::Image { .. } | Inline::Raw(_) => {}
            }
        }
        out
    }

    /// Plain text with whitespace (including breaks) collapsed to single spaces.
    pub fn plain_line(&self) -> String {
        self.plain_text()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Splits at line breaks, for code blocks.
    pub fn plain_lines(&self) -> Vec<String> {
        self.plain_text().split('\n').map(str::to_string).collect()
    }

    /// Renders to Markdown inline text. Formatting is opened and closed
    /// minimally, with whitespace kept outside the delimiters so emphasis is
    /// recognised.
    pub fn render(&self, opts: RenderOpts) -> String {
        let mut out = String::new();
        let mut open: Vec<Delim> = Vec::new();
        let mut pending_ws = String::new();
        let close_from = |open: &mut Vec<Delim>, keep: usize, out: &mut String| {
            while open.len() > keep {
                if let Some(d) = open.pop() {
                    d.close(out);
                }
            }
        };
        let flush_ws = |pending: &mut String, out: &mut String| {
            if !(out.is_empty() || out.ends_with('\n') || out.ends_with("<br>")) {
                out.push_str(pending);
            }
            pending.clear();
        };
        for item in &self.items {
            match item {
                Inline::Text { text, marks, link } => {
                    let text = normalize_ws(text);
                    let core = text.trim_matches(is_md_ws);
                    if core.is_empty() {
                        pending_ws.push_str(&text);
                        continue;
                    }
                    let lead = &text[..text.len() - text.trim_start_matches(is_md_ws).len()];
                    let trail = &text[text.trim_end_matches(is_md_ws).len()..];
                    let mut desired = Vec::with_capacity(4);
                    if let Some(url) = link {
                        desired.push(Delim::Link(url.clone()));
                    }
                    if marks.bold && !opts.no_bold {
                        desired.push(Delim::Bold);
                    }
                    if marks.italic {
                        desired.push(Delim::Italic);
                    }
                    if marks.strike {
                        desired.push(Delim::Strike);
                    }
                    let keep = open
                        .iter()
                        .zip(&desired)
                        .take_while(|(a, b)| a == b)
                        .count();
                    close_from(&mut open, keep, &mut out);
                    pending_ws.push_str(lead);
                    flush_ws(&mut pending_ws, &mut out);
                    for d in desired.into_iter().skip(keep) {
                        out.push_str(d.open());
                        open.push(d);
                    }
                    if marks.code {
                        out.push_str(&code_span(core, opts.in_table));
                    } else {
                        escape_into(core, &mut out);
                    }
                    pending_ws.push_str(trail);
                }
                Inline::LineBreak => {
                    close_from(&mut open, 0, &mut out);
                    pending_ws.clear();
                    if !(out.is_empty() || out.ends_with('\n') || out.ends_with("<br>")) {
                        out.push_str(if opts.in_table { "<br>" } else { "\\\n" });
                    }
                }
                Inline::Image { alt, url, link } => {
                    let keep = match (open.first(), link) {
                        (Some(Delim::Link(a)), Some(b)) if a == b => 1,
                        _ => 0,
                    };
                    close_from(&mut open, keep, &mut out);
                    flush_ws(&mut pending_ws, &mut out);
                    if keep == 0
                        && let Some(url) = link
                    {
                        out.push('[');
                        open.push(Delim::Link(url.clone()));
                    }
                    out.push_str("![");
                    escape_into(&normalize_ws(alt), &mut out);
                    out.push_str("](");
                    out.push_str(&link_destination(url));
                    out.push(')');
                }
                Inline::Raw(raw) => {
                    flush_ws(&mut pending_ws, &mut out);
                    out.push_str(raw);
                }
            }
        }
        close_from(&mut open, 0, &mut out);
        // A trailing hard break would render as a literal backslash.
        while let Some(s) = out
            .strip_suffix("\\\n")
            .or_else(|| out.strip_suffix("<br>"))
        {
            out.truncate(s.len());
        }
        if opts.in_table {
            out = out.replace('\n', " ");
        }
        out
    }
}

fn is_md_ws(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Tabs and stray newlines inside a run become spaces.
fn normalize_ws(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\t' | '\n' | '\r' | '\u{b}' | '\u{c}' => ' ',
            c => c,
        })
        .collect()
}

/// Backslash-escapes characters that Markdown would otherwise interpret
/// inline. Line-start constructs are handled by [`escape_line_starts`].
pub fn escape_into(text: &str, out: &mut String) {
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let prev = if i > 0 {
            chars.get(i - 1).copied()
        } else {
            None
        };
        let next = chars.get(i + 1).copied();
        let needs = match c {
            '\\' | '`' | '*' | '[' | ']' | '<' | '|' | '~' => true,
            // Intraword underscores can never start or end emphasis.
            '_' => {
                !(prev.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric))
            }
            // Only something that reads as an entity (`&amp;`, `&#65;`) needs protecting.
            '&' => looks_like_entity(&chars[i + 1..]),
            _ => false,
        };
        if needs {
            out.push('\\');
        }
        out.push(c);
    }
}

fn looks_like_entity(rest: &[char]) -> bool {
    if rest.first() == Some(&'#') {
        return true;
    }
    let name = rest
        .iter()
        .take(32)
        .take_while(|c| c.is_ascii_alphanumeric())
        .count();
    name > 0 && rest.get(name) == Some(&';')
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    escape_into(text, &mut out);
    out
}

/// Escapes a single line of plain text completely, including line-start
/// constructs.
pub fn escape_line(text: &str) -> String {
    escape_line_starts(&escape(text.trim()))
}

/// Protects the start of each line from being read as a block construct
/// (heading, list item, block quote, thematic break or setext underline).
pub fn escape_line_starts(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&escape_line_start(line));
    }
    out
}

fn escape_line_start(line: &str) -> String {
    let trimmed = line.trim_start_matches(is_md_ws);
    let indent = &line[..line.len() - trimmed.len()];
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return line.to_string();
    };
    let second = chars.next();
    let body = trimmed.trim_end();
    let needs_prefix = match first {
        '#' | '>' => true,
        '-' | '+' => second.is_none_or(char::is_whitespace) || body.chars().all(|c| c == first),
        '=' => body.chars().all(|c| c == '='),
        _ => false,
    };
    if needs_prefix {
        return format!("{indent}\\{trimmed}");
    }
    // `1.` or `1)` followed by a space would start an ordered list.
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let rest = &trimmed[digits..];
        let mut rc = rest.chars();
        if let Some(p @ ('.' | ')')) = rc.next()
            && rc.next().is_none_or(char::is_whitespace)
        {
            return format!("{indent}{}\\{p}{}", &trimmed[..digits], &rest[1..]);
        }
    }
    line.to_string()
}

/// Escapes a trailing `#` in a heading so it is not taken as a closing sequence.
pub fn heading_text(rendered: &str) -> String {
    let text = rendered.replace("\\\n", " ").replace('\n', " ");
    let text = text.trim();
    if text.ends_with('#') && !text.ends_with("\\#") {
        let mut s = text[..text.len() - 1].to_string();
        s.push_str("\\#");
        s
    } else {
        text.to_string()
    }
}

/// An inline code span with a fence longer than any backtick run inside.
fn code_span(text: &str, in_table: bool) -> String {
    let text = if in_table {
        text.replace('|', "\\|")
    } else {
        text.to_string()
    };
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    let pad = text.starts_with('`') || text.ends_with('`');
    if pad {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// A link or image destination, wrapped in `<…>` when it contains characters
/// that would end a bare destination.
pub fn link_destination(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        return "<>".to_string();
    }
    let needs_angle = url
        .chars()
        .any(|c| c.is_whitespace() || c == '(' || c == ')' || c == '<' || c == '>');
    if needs_angle {
        let escaped = url.replace('<', "%3C").replace('>', "%3E");
        format!("<{escaped}>")
    } else {
        url.to_string()
    }
}

/// A fenced code block whose fence cannot collide with the content.
pub fn code_block(lines: &[String]) -> String {
    let mut longest = 0;
    for line in lines {
        let run = line.trim_start().chars().take_while(|&c| c == '`').count();
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest.max(2) + 1);
    let mut out = String::new();
    out.push_str(&fence);
    out.push('\n');
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&fence);
    out
}

/// Tracks list nesting so each nested item is indented to its parent's
/// content column, as CommonMark requires.
#[derive(Debug, Default)]
pub struct ListState {
    /// Content column for each open depth.
    columns: Vec<usize>,
}

impl ListState {
    pub fn reset(&mut self) {
        self.columns.clear();
    }

    /// Renders one list item at `level` (0-based; deeper levels than the
    /// current nesting allows are clamped). `marker` is `-` or `N.`.
    pub fn item(&mut self, level: usize, marker: &str, content: &str) -> String {
        let depth = level.min(self.columns.len());
        self.columns.truncate(depth);
        let indent = if depth == 0 {
            0
        } else {
            self.columns[depth - 1]
        };
        let column = indent + marker.len() + 1;
        self.columns.push(column);
        let pad = " ".repeat(indent);
        let cont = " ".repeat(column);
        let mut out = String::new();
        for (i, line) in content.split('\n').enumerate() {
            if i == 0 {
                out.push_str(&pad);
                out.push_str(marker);
                out.push(' ');
            } else {
                out.push('\n');
                out.push_str(&cont);
            }
            out.push_str(line);
        }
        out
    }
}

/// Renders a GitHub table. The first row is the header; rows are padded to
/// the widest row. Cells must already be single-line Markdown.
pub fn table(rows: &[Vec<String>]) -> Option<String> {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if rows.is_empty() || width == 0 {
        return None;
    }
    let mut out = String::new();
    let write_row = |out: &mut String, row: &[String]| {
        out.push('|');
        for i in 0..width {
            let cell = row.get(i).map(|c| c.trim()).unwrap_or("");
            out.push(' ');
            out.push_str(cell);
            if !cell.is_empty() {
                out.push(' ');
            }
            out.push('|');
        }
    };
    write_row(&mut out, &rows[0]);
    out.push('\n');
    out.push('|');
    for _ in 0..width {
        out.push_str(" --- |");
    }
    for row in &rows[1..] {
        out.push('\n');
        write_row(&mut out, row);
    }
    Some(out)
}

/// Joins rendered blocks: list items that follow each other stay tight,
/// everything else is separated by a blank line.
#[derive(Debug, Default)]
pub struct BlockWriter {
    pub out: String,
    last_was_list: bool,
}

impl BlockWriter {
    pub fn block(&mut self, text: &str) {
        self.push(text, false);
    }

    pub fn list_item(&mut self, text: &str) {
        self.push(text, true);
    }

    /// Ends the current list so the next list item starts a new one.
    pub fn end_list(&mut self) {
        self.last_was_list = false;
    }

    fn push(&mut self, text: &str, is_list: bool) {
        if text.trim().is_empty() {
            return;
        }
        if !self.out.is_empty() {
            self.out.push_str(if is_list && self.last_was_list {
                "\n"
            } else {
                "\n\n"
            });
        }
        self.out.push_str(text);
        self.last_was_list = is_list;
    }

    pub fn finish(self) -> String {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bold() -> Marks {
        Marks {
            bold: true,
            ..Marks::default()
        }
    }

    #[test]
    fn nested_and_adjacent_marks() {
        let mut b = InlineBuf::default();
        b.push_text("plain ", Marks::default(), None);
        b.push_text("bold ", bold(), None);
        b.push_text(
            "both",
            Marks {
                bold: true,
                italic: true,
                ..Marks::default()
            },
            None,
        );
        b.push_text(" end", Marks::default(), None);
        assert_eq!(b.render(RenderOpts::default()), "plain **bold *both*** end");
    }

    #[test]
    fn whitespace_moves_outside_delimiters() {
        let mut b = InlineBuf::default();
        b.push_text("a", Marks::default(), None);
        b.push_text(" b ", bold(), None);
        b.push_text("c", Marks::default(), None);
        assert_eq!(b.render(RenderOpts::default()), "a **b** c");
    }

    #[test]
    fn links_and_code() {
        let mut b = InlineBuf::default();
        b.push_text("see ", Marks::default(), None);
        b.push_text("the", Marks::default(), Some("https://x.y/a b"));
        b.push_text(" docs", bold(), Some("https://x.y/a b"));
        b.push_text(" and ", Marks::default(), None);
        b.push_text(
            "a`b",
            Marks {
                code: true,
                ..Marks::default()
            },
            None,
        );
        assert_eq!(
            b.render(RenderOpts::default()),
            "see [the **docs**](<https://x.y/a b>) and ``a`b``"
        );
    }

    #[test]
    fn escapes() {
        assert_eq!(
            escape("a*b_c [x] snake_case _y_ <t> AT&T &amp;"),
            "a\\*b_c \\[x\\] snake_case \\_y\\_ \\<t> AT&T \\&amp;"
        );
        assert_eq!(
            escape_line_starts("# no\n- no\n1. no\n---\nfine - ok"),
            "\\# no\n\\- no\n1\\. no\n\\---\nfine - ok"
        );
    }

    #[test]
    fn breaks() {
        let mut b = InlineBuf::default();
        b.push_text("one", Marks::default(), None);
        b.push_break();
        b.push_text("two", Marks::default(), None);
        b.push_break();
        assert_eq!(b.render(RenderOpts::default()), "one\\\ntwo");
        assert_eq!(
            b.render(RenderOpts {
                in_table: true,
                ..RenderOpts::default()
            }),
            "one<br>two"
        );
    }

    #[test]
    fn list_indentation() {
        let mut l = ListState::default();
        assert_eq!(l.item(0, "1.", "a"), "1. a");
        assert_eq!(l.item(1, "-", "b"), "   - b");
        assert_eq!(l.item(5, "-", "c"), "     - c");
        assert_eq!(l.item(0, "10.", "d\ne"), "10. d\n    e");
    }

    #[test]
    fn tables() {
        let t = table(&[vec!["A".into(), "B".into()], vec!["1".into()]]).unwrap();
        assert_eq!(t, "| A | B |\n| --- | --- |\n| 1 | |");
    }
}
