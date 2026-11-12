//! Text put into the composer from elsewhere: a selection in the
//! conversation as a Markdown quote, a selection in the terminal as a
//! fenced code block. Either goes after what is already typed.

/// `text` as a Markdown quote: `> ` before each line, a bare `>` for a
/// blank one. Blank lines around the selection are dropped (a selection
/// often ends with the line break after it). `None` when nothing is left.
pub(crate) fn markdown_quote(text: &str) -> Option<String> {
    let lines = trimmed_lines(text)?;
    let quoted: Vec<String> = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                ">".to_owned()
            } else {
                format!("> {line}")
            }
        })
        .collect();
    Some(quoted.join("\n"))
}

/// `text` as a fenced code block. The fence is one backtick longer than
/// the longest run of backticks in the text (three at least), so a
/// selection holding a fence of its own stays inside the block.
pub(crate) fn fenced_block(text: &str) -> Option<String> {
    let lines = trimmed_lines(text)?;
    let body = lines.join("\n");
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    Some(format!("{fence}\n{body}\n{fence}"))
}

/// What to add at the end of `typed` so `block` stands on its own after
/// it, followed by `after` (the blank line that ends a quote, the line
/// break after a code block) where the caret goes.
pub(crate) fn appended(typed: &str, block: &str, after: &str) -> String {
    let gap = if typed.is_empty() || typed.ends_with("\n\n") {
        ""
    } else if typed.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{gap}{block}{after}")
}

/// The lines of `text` without blank ones at either end and without
/// spaces at the ends of lines (a terminal pads its rows).
fn trimmed_lines(text: &str) -> Option<Vec<&str>> {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let first = lines.iter().position(|l| !l.is_empty())?;
    let last = lines.iter().rposition(|l| !l.is_empty())?;
    Some(lines[first..=last].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_each_line() {
        assert_eq!(markdown_quote("one line").as_deref(), Some("> one line"));
        assert_eq!(
            markdown_quote("first\nsecond\n\nafter a gap").as_deref(),
            Some("> first\n> second\n>\n> after a gap")
        );
    }

    #[test]
    fn a_trailing_line_break_is_not_quoted() {
        assert_eq!(markdown_quote("picked\n").as_deref(), Some("> picked"));
        assert_eq!(
            markdown_quote("\n\n  indented stays\r\nnext  \n\n").as_deref(),
            Some(">   indented stays\n> next")
        );
        assert_eq!(markdown_quote(" \n\t\n"), None);
        assert_eq!(markdown_quote(""), None);
    }

    #[test]
    fn fences_a_selection() {
        assert_eq!(
            fenced_block("$ cargo test\nok\n").as_deref(),
            Some("```\n$ cargo test\nok\n```")
        );
        assert_eq!(fenced_block("\n \n"), None);
    }

    #[test]
    fn a_selection_with_backticks_gets_a_longer_fence() {
        // Inline code fits in the usual fence.
        assert_eq!(
            fenced_block("run `ls` now").as_deref(),
            Some("```\nrun `ls` now\n```")
        );
        // A fence inside needs a longer one around it.
        assert_eq!(
            fenced_block("```rust\nfn main() {}\n```").as_deref(),
            Some("````\n```rust\nfn main() {}\n```\n````")
        );
        assert_eq!(
            fenced_block("a ````` b ``` c").as_deref(),
            Some("``````\na ````` b ``` c\n``````")
        );
    }

    #[test]
    fn goes_after_what_is_typed() {
        // An empty composer: just the block, the caret after it.
        assert_eq!(appended("", "> q", "\n\n"), "> q\n\n");
        // What was typed stays, a blank line apart.
        assert_eq!(appended("look:", "> q", "\n\n"), "\n\n> q\n\n");
        assert_eq!(appended("look:\n", "> q", "\n\n"), "\n> q\n\n");
        assert_eq!(appended("look:\n\n", "```\nx\n```", "\n"), "```\nx\n```\n");
    }
}
