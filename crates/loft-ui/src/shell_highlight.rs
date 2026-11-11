//! Colours for a shell command in the timeline: commands, flags, strings,
//! variables, `$(…)`, operators and comments, from the syntax theme. A small
//! tokenizer rather than a grammar, so the desktop and browser builds colour
//! commands the same way (the browser build has no shell grammar).

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, HighlightStyle};
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Command,
    Keyword,
    Flag,
    String,
    Variable,
    Operator,
    Number,
    Comment,
}

impl Kind {
    /// Syntax-theme names to colour it with, best first (themes leave some
    /// out, and the plainer ones, like `function`, are near the text colour).
    fn theme_names(self) -> &'static [&'static str] {
        match self {
            Kind::Command => &["keyword", "function"],
            Kind::Keyword => &["tag", "keyword"],
            Kind::Flag => &["constant", "number", "attribute"],
            Kind::String => &["string"],
            Kind::Variable => &["variable.special", "tag", "variable"],
            Kind::Operator => &["operator", "type", "punctuation.special"],
            Kind::Number => &["number", "constant"],
            Kind::Comment => &["comment"],
        }
    }
}

/// Highlights for `command`, as byte ranges.
pub fn highlights(command: &str, cx: &App) -> Vec<(Range<usize>, HighlightStyle)> {
    let theme = &cx.theme().highlight_theme;
    tokens(command)
        .into_iter()
        .filter_map(|(range, kind)| {
            let style = kind.theme_names().iter().find_map(|n| theme.style(n))?;
            Some((range, style))
        })
        .collect()
}

const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "time", "!",
];

/// Words after which a new command starts.
const OPENS_COMMAND: &[&str] = &[
    "if", "then", "else", "elif", "while", "until", "do", "!", "time", "sudo", "exec", "env",
    "nohup", "xargs", "command", "builtin",
];

/// Characters that end a word.
fn breaks_word(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '\'' | '"' | '`'
        )
}

fn tokens(s: &str) -> Vec<(Range<usize>, Kind)> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut at_command = true;
    let mut i = 0;
    // Whether the previous character was whitespace (or the start).
    let mut after_space = true;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap_or(' ');
        let len = c.len_utf8();
        match c {
            '\n' => {
                at_command = true;
                after_space = true;
                i += 1;
                continue;
            }
            c if c.is_whitespace() => {
                after_space = true;
                i += len;
                continue;
            }
            '#' if after_space => {
                let end = s[i..].find('\n').map_or(s.len(), |n| i + n);
                out.push((i..end, Kind::Comment));
                i = end;
                continue;
            }
            '\\' => {
                // A line continuation stands out; an escaped character is
                // part of its word.
                if bytes.get(i + 1) == Some(&b'\n') {
                    out.push((i..i + 1, Kind::Operator));
                    i += 2;
                } else {
                    let next = s[i + 1..].chars().next().map_or(0, char::len_utf8);
                    i += 1 + next;
                }
                after_space = false;
                continue;
            }
            '\'' => {
                let end = s[i + 1..].find('\'').map_or(s.len(), |n| i + 1 + n + 1);
                out.push((i..end, Kind::String));
                i = end;
                at_command = false;
                after_space = false;
                continue;
            }
            '"' => {
                let end = double_quoted_end(s, i);
                out.push((i..end, Kind::String));
                i = end;
                at_command = false;
                after_space = false;
                continue;
            }
            '$' => {
                let rest = &s[i + 1..];
                let end = if rest.starts_with('(') {
                    out.push((i..i + 2, Kind::Operator));
                    i += 2;
                    at_command = true;
                    after_space = true;
                    continue;
                } else if rest.starts_with('{') {
                    rest.find('}').map_or(s.len(), |n| i + 1 + n + 1)
                } else {
                    let n = rest
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(rest.len());
                    // `$?`, `$@`, `$1`…
                    let special = rest.starts_with(['?', '@', '*', '#', '!', '$', '-']);
                    i + 1 + if n == 0 && special { 1 } else { n }
                };
                if end > i + 1 {
                    out.push((i..end, Kind::Variable));
                }
                i = end.max(i + 1);
                at_command = false;
                after_space = false;
                continue;
            }
            '|' | '&' | ';' | '(' | ')' | '<' | '>' | '`' => {
                let two = s.get(i..i + 2).unwrap_or("");
                let n = if matches!(two, "&&" | "||" | ";;" | ">>" | "<<" | ">&" | "&>" | "|&") {
                    2
                } else {
                    1
                };
                out.push((i..i + n, Kind::Operator));
                at_command = !matches!(c, '<' | '>' | ')');
                i += n;
                after_space = true;
                continue;
            }
            _ => {}
        }
        // A word.
        let start = i;
        let mut end = i;
        for (ix, c) in s[i..].char_indices() {
            if breaks_word(c) || c == '$' && ix > 0 {
                break;
            }
            end = i + ix + c.len_utf8();
        }
        if end == start {
            end = start + len;
        }
        let word = &s[start..end];
        // `2>&1`: a file descriptor before a redirect.
        let redirect_fd = word.chars().all(|c| c.is_ascii_digit()) && s[end..].starts_with('>');
        let kind = if redirect_fd {
            Some(Kind::Operator)
        } else if at_command && KEYWORDS.contains(&word) {
            Some(Kind::Keyword)
        } else if at_command && is_assignment(word) {
            let eq = word.find('=').unwrap_or(0);
            out.push((start..start + eq, Kind::Variable));
            None
        } else if at_command {
            at_command = OPENS_COMMAND.contains(&word);
            Some(Kind::Command)
        } else if word.starts_with('-') && word.len() > 1 {
            // `--format=x`: only the flag's name.
            let name_end = word.find('=').map_or(end, |eq| start + eq);
            out.push((start..name_end, Kind::Flag));
            None
        } else if word.chars().all(|c| c.is_ascii_digit()) {
            Some(Kind::Number)
        } else {
            None
        };
        if let Some(kind) = kind {
            if kind == Kind::Keyword {
                at_command = OPENS_COMMAND.contains(&word);
            }
            out.push((start..end, kind));
        }
        i = end;
        after_space = false;
    }
    out
}

/// `NAME=value` at a command's start.
fn is_assignment(word: &str) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let name = &word[..eq];
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The end of a `"…"` string starting at `start` (backslash escapes skip).
fn double_quoted_end(s: &str, start: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<(&str, Kind)> {
        tokens(s).into_iter().map(|(r, k)| (&s[r], k)).collect()
    }

    #[test]
    fn a_pipeline_with_flags_strings_and_substitution() {
        use Kind::*;
        assert_eq!(
            kinds(r#"git add src/a.tsx && git commit -q -m "Fix it" | head -5"#),
            vec![
                ("git", Command),
                ("&&", Operator),
                ("git", Command),
                ("-q", Flag),
                ("-m", Flag),
                ("\"Fix it\"", String),
                ("|", Operator),
                ("head", Command),
                ("-5", Flag),
            ]
        );
        assert_eq!(
            kinds("remote=$(ssh vm 'cd /srv && ls') && echo \"$remote\" 2>&1"),
            vec![
                ("remote", Variable),
                ("$(", Operator),
                ("ssh", Command),
                ("'cd /srv && ls'", String),
                (")", Operator),
                ("&&", Operator),
                ("echo", Command),
                ("\"$remote\"", String),
                ("2", Operator),
                (">&", Operator),
                ("1", Number),
            ]
        );
    }

    #[test]
    fn keywords_comments_variables_and_continuations() {
        use Kind::*;
        assert_eq!(
            kinds("for f in $FILES; do cat --x=1 \"$f\"; done # all\nsudo rm -rf ${DIR} \\\n  x"),
            vec![
                ("for", Keyword),
                ("$FILES", Variable),
                (";", Operator),
                ("do", Keyword),
                ("cat", Command),
                ("--x", Flag),
                ("\"$f\"", String),
                (";", Operator),
                ("done", Keyword),
                ("# all", Comment),
                ("sudo", Command),
                ("rm", Command),
                ("-rf", Flag),
                ("${DIR}", Variable),
                ("\\", Operator),
            ]
        );
    }

    #[test]
    fn odd_input_does_not_panic() {
        for s in [
            "", "$", "\"", "'", "\\", "a\\", "${", "$(", "é$é", "#", "2>",
        ] {
            let _ = tokens(s);
        }
    }
}
