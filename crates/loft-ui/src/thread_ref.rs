//! Another thread as context for a message. In the composer it is a chip
//! ("thread: Title"); sent, it is a line of text the agent can act on:
//!
//! ```text
//! [Agentloft thread "Title" (thread_id 0193…): read it with the agentloft read_thread tool if it is relevant]
//! ```
//!
//! `thread_id` is the argument Agentloft's `read_thread` tool takes
//! (`crates/loftd/src/mcp.rs`). A message already sent draws such lines as
//! chips again ([`split`]).

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex};
use gpui_kit::{ParentElement as _, Styled as _, div, px};
use loft_protocol::{ProjectId, ThreadId};
use std::borrow::Cow;

/// How many threads `@` offers at most.
pub(crate) const MAX_THREADS: usize = 5;

const OPEN: &str = "[Agentloft thread \"";
const ID: &str = "\" (thread_id ";
const CLOSE: &str = "): read it with the agentloft read_thread tool if it is relevant]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadRef {
    pub id: ThreadId,
    pub title: String,
}

impl ThreadRef {
    pub(crate) fn new(id: ThreadId, title: &str) -> Self {
        // One line, whatever the title holds.
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        Self { id, title }
    }

    /// The line sent to the agent.
    pub(crate) fn to_text(&self) -> String {
        format!("{OPEN}{}{ID}{}{CLOSE}", self.title, self.id.as_str())
    }

    /// What the chip says.
    pub(crate) fn label(&self) -> String {
        format!("thread: {}", self.title)
    }
}

/// `line` as a thread reference, if it is exactly one.
pub(crate) fn parse(line: &str) -> Option<ThreadRef> {
    let inner = line.trim().strip_prefix(OPEN)?.strip_suffix(CLOSE)?;
    // The id is last and has no quote in it; the title may.
    let (title, id) = inner.rsplit_once(ID)?;
    let plain = !id.is_empty() && !id.contains(|c: char| c.is_whitespace() || c == '"');
    plain.then(|| ThreadRef {
        id: ThreadId(id.to_owned()),
        title: title.to_owned(),
    })
}

/// A sent message without its thread references, and the references (in
/// order). Text with none comes back as it is.
pub(crate) fn split(text: &str) -> (Cow<'_, str>, Vec<ThreadRef>) {
    if !text.contains(OPEN) {
        return (Cow::Borrowed(text), Vec::new());
    }
    let mut refs = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        match parse(line) {
            Some(r) => {
                refs.push(r);
                // The blank line that set it apart goes with it.
                if kept.last().is_some_and(|l| l.trim().is_empty()) {
                    kept.pop();
                }
            }
            None => kept.push(line),
        }
    }
    if refs.is_empty() {
        return (Cow::Borrowed(text), refs);
    }
    (Cow::Owned(kept.join("\n").trim().to_owned()), refs)
}

/// A thread as the matcher sees it.
pub(crate) struct Listed<'a> {
    pub id: &'a ThreadId,
    pub title: &'a str,
    pub project: &'a ProjectId,
    pub project_name: &'a str,
    /// When it was last active.
    pub active_ms: i64,
    /// Not there to be read: archived, or nothing said in it yet.
    pub hidden: bool,
}

/// A thread `@` offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub thread: ThreadRef,
    pub project: String,
}

/// The threads `@query` offers from `current`'s composer: those whose
/// title has `query` in it (any case), `current`'s project first, the most
/// recently active first within each, `limit` at most. Never `current`
/// itself, nor a hidden thread.
pub(crate) fn match_threads(
    threads: &[Listed<'_>],
    current: &ThreadId,
    query: &str,
    limit: usize,
) -> Vec<Found> {
    let query = query.to_lowercase();
    let here = threads.iter().find(|t| t.id == current).map(|t| t.project);
    let mut found: Vec<&Listed<'_>> = threads
        .iter()
        .filter(|t| t.id != current && !t.hidden)
        .filter(|t| t.title.to_lowercase().contains(&query))
        .collect();
    found.sort_by_key(|t| (Some(t.project) != here, std::cmp::Reverse(t.active_ms)));
    found
        .into_iter()
        .take(limit)
        .map(|t| Found {
            thread: ThreadRef::new(t.id.clone(), t.title),
            project: t.project_name.to_owned(),
        })
        .collect()
}

/// The chip's face: in the composer (which adds its ✕) and in a sent
/// message's bubble.
pub(crate) fn chip(r: &ThreadRef, p: &Palette) -> gpui_kit::Div {
    h_flex()
        .gap_1p5()
        .items_center()
        .max_w(px(320.))
        .px_2()
        .py_1()
        .rounded(px(9.))
        .border_1()
        .border_color(p.border)
        .bg(p.muted)
        .text_xs()
        .child(
            Icon::new(IconName::MessageSquare)
                .small()
                .text_color(p.accent),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(r.label()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ThreadId {
        ThreadId(s.to_owned())
    }

    #[test]
    fn the_sent_line_names_the_tool_and_its_argument() {
        let r = ThreadRef::new(id("t-42"), "Fix the  login\nflow");
        assert_eq!(
            r.to_text(),
            "[Agentloft thread \"Fix the login flow\" (thread_id t-42): read it with the \
             agentloft read_thread tool if it is relevant]"
        );
        assert_eq!(r.label(), "thread: Fix the login flow");
        assert_eq!(parse(&r.to_text()), Some(r));
    }

    #[test]
    fn parses_only_a_whole_reference() {
        // A title with quotes and brackets in it still reads back.
        let odd = ThreadRef::new(id("abc"), "Why \"quotes\" (thread_id x) [here]");
        assert_eq!(parse(&format!("  {}  ", odd.to_text())), Some(odd.clone()));
        assert_eq!(
            parse("[Agentloft thread \"x\" (thread_id ): read it]"),
            None
        );
        assert_eq!(parse(&format!("see {}", odd.to_text())), None);
        assert_eq!(parse("plain words"), None);
        // An id is one word.
        let spaced = odd.to_text().replace("thread_id abc", "thread_id a bc");
        assert_eq!(parse(&spaced), None);
    }

    #[test]
    fn a_sent_message_gives_its_references_back() {
        let (a, b) = (
            ThreadRef::new(id("a"), "First"),
            ThreadRef::new(id("b"), "Second"),
        );
        let sent = format!("compare these\n\n{}\n\n{}", a.to_text(), b.to_text());
        let (text, refs) = split(&sent);
        assert_eq!(text, "compare these");
        assert_eq!(refs, vec![a.clone(), b]);
        // Only a reference: no text left for a bubble.
        let only = a.to_text();
        let (text, refs) = split(&only);
        assert_eq!((text.as_ref(), refs.len()), ("", 1));
        // Text around one keeps its own paragraphs.
        let sent = format!("before\n\n{}\n\nafter", a.to_text());
        assert_eq!(split(&sent).0, "before\n\nafter");
        // Nothing to take out: the text itself, not a copy.
        assert!(matches!(split("no reference\nhere"), (Cow::Borrowed(_), r) if r.is_empty()));
        let inline = format!("inline {} stays", a.to_text());
        assert!(matches!(split(&inline), (Cow::Borrowed(_), r) if r.is_empty()));
    }

    #[test]
    fn matches_by_title_same_project_first_then_recency() {
        let (here, there) = (ProjectId("here".into()), ProjectId("there".into()));
        let ids: Vec<ThreadId> = ["me", "old", "new", "far", "arch", "blank", "other"]
            .iter()
            .map(|s| id(s))
            .collect();
        let listed = |ix: usize, title: &'static str, far: bool, active_ms: i64, hidden: bool| {
            let (project, project_name) = if far {
                (&there, "There")
            } else {
                (&here, "Here")
            };
            Listed {
                id: &ids[ix],
                title,
                project,
                project_name,
                active_ms,
                hidden,
            }
        };
        let threads = vec![
            listed(0, "Login page (this one)", false, 900, false),
            listed(1, "Login tests", false, 100, false),
            listed(2, "Fix LOGIN redirect", false, 500, false),
            listed(3, "Login for the other app", true, 800, false),
            listed(4, "Login, archived", false, 700, true),
            listed(5, "Login, never started", false, 950, true),
            listed(6, "Unrelated", false, 990, false),
        ];
        let names = |query: &str, limit: usize| -> Vec<String> {
            match_threads(&threads, &ids[0], query, limit)
                .into_iter()
                .map(|f| format!("{}/{}", f.project, f.thread.id.as_str()))
                .collect()
        };
        // This project's first (newest first), then others'; never the
        // current thread, an archived one or one with nothing in it.
        assert_eq!(names("login", 5), ["Here/new", "Here/old", "There/far"]);
        // Nothing typed yet: the most recent, same order.
        assert_eq!(
            names("", 5),
            ["Here/other", "Here/new", "Here/old", "There/far"]
        );
        assert_eq!(names("", 2), ["Here/other", "Here/new"]);
        assert!(names("nothing like it", 5).is_empty());
    }
}
