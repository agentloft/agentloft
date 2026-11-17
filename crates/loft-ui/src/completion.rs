//! Composer completions: `/` at the start lists the agent's slash commands,
//! `@` anywhere fuzzy-finds files in the thread's workspace and, after
//! them, other threads by title (picked, one becomes a chip: see
//! `thread_ref`). ↑/↓ move, Enter or Tab insert, Esc dismisses.

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{SlashCommandInfo, ThreadId};
use std::ops::Range;

pub const MAX_ITEMS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Slash,
    File,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub thread_id: ThreadId,
    pub kind: Kind,
    /// Byte range of the token being completed (`/co`, `@src/ma`).
    pub token: Range<usize>,
    pub items: Vec<String>,
    /// For slash commands: what each item does (same order as `items`).
    pub info: Vec<SlashCommandInfo>,
    /// For `@`: other threads whose title matches, listed after `items`
    /// (`selected` runs on through them).
    pub threads: Vec<crate::thread_ref::Found>,
    pub selected: usize,
    /// The list scrolls; the chosen row is kept in view.
    pub scroll: gpui_kit::ScrollHandle,
}

impl Completion {
    /// Text that replaces the token.
    pub fn insertion(&self) -> Option<String> {
        let item = self.items.get(self.selected)?;
        Some(match self.kind {
            Kind::Slash => match self
                .info
                .get(self.selected)
                .filter(|i| !i.insert.is_empty())
            {
                Some(info) => format!("{} ", info.insert),
                None => format!("/{item} "),
            },
            Kind::File => format!("@{item} "),
        })
    }

    /// The thread chosen, when the chosen row is one.
    pub fn picked_thread(&self) -> Option<&crate::thread_ref::ThreadRef> {
        let ix = self.selected.checked_sub(self.items.len())?;
        self.threads.get(ix).map(|found| &found.thread)
    }

    /// How many rows can be chosen.
    pub fn rows(&self) -> usize {
        self.items.len() + self.threads.len()
    }

    pub fn move_by(&mut self, delta: isize) {
        let n = self.rows() as isize;
        if n > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
            self.scroll_to_selected();
        }
    }

    /// Keep the chosen row in view (the threads' heading is one of the
    /// list's children too).
    pub fn scroll_to_selected(&self) {
        let heading = usize::from(self.selected >= self.items.len() && !self.threads.is_empty());
        self.scroll.scroll_to_item(self.selected + heading);
    }
}

/// What the cursor is in the middle of typing, if it's completable.
pub fn trigger(text: &str, cursor: usize) -> Option<(Kind, Range<usize>, String)> {
    let cursor = cursor.min(text.len());
    let before = text.get(..cursor)?;
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let token = &before[start..];
    if let Some(query) = token.strip_prefix('/')
        && start == 0
    {
        return Some((Kind::Slash, start..cursor, query.to_owned()));
    }
    if let Some(query) = token.strip_prefix('@') {
        return Some((Kind::File, start..cursor, query.to_owned()));
    }
    None
}

/// Slash commands matching `query`: name prefix matches first, then other
/// name matches, then description matches; the agent's order within each.
pub fn match_commands(commands: &[SlashCommandInfo], query: &str) -> Vec<SlashCommandInfo> {
    let q = query.to_lowercase();
    let mut ranked: Vec<(u8, usize, &SlashCommandInfo)> = Vec::new();
    for (ix, c) in commands.iter().enumerate() {
        let name = c.name.to_lowercase();
        let rank = if name.starts_with(&q) {
            0
        } else if name.contains(&q) {
            1
        } else if !q.is_empty() && c.description.to_lowercase().contains(&q) {
            2
        } else {
            continue;
        };
        ranked.push((rank, ix, c));
    }
    ranked.sort_by_key(|(rank, ix, _)| (*rank, *ix));
    ranked.into_iter().map(|(.., c)| c.clone()).collect()
}

/// Commands as names only (agents that don't describe them).
pub fn bare_commands(names: &[String]) -> Vec<SlashCommandInfo> {
    names
        .iter()
        .map(|name| SlashCommandInfo {
            name: name.clone(),
            ..Default::default()
        })
        .collect()
}

/// The popover above the composer.
pub fn popover(
    c: &Completion,
    p: &Palette,
    on_pick: impl Fn(usize, &mut Window, &mut App) + Clone + 'static,
) -> AnyElement {
    let mut list = v_flex()
        .id("completion-list")
        .max_h(px(340.))
        .overflow_y_scroll()
        .track_scroll(&c.scroll)
        .p_1()
        .gap_px();
    for (ix, item) in c.items.iter().enumerate() {
        let info = c.info.get(ix);
        let active = ix == c.selected;
        let on_pick = on_pick.clone();
        let (icon, dir, name) = match c.kind {
            Kind::Slash => (
                if info.is_none_or(|i| i.builtin) {
                    IconName::SquareTerminal
                } else {
                    IconName::WandSparkles
                },
                info.map(|i| i.description.clone()).unwrap_or_default(),
                match info.filter(|i| !i.insert.is_empty()) {
                    Some(i) => i.insert.clone(),
                    None => format!("/{item}"),
                },
            ),
            Kind::File => {
                let (dir, name) = match item.rsplit_once('/') {
                    Some((d, n)) => (format!("{d}/"), n.to_owned()),
                    None => (String::new(), item.clone()),
                };
                (IconName::File, dir, name)
            }
        };
        list = list.child(
            h_flex()
                .id(SharedString::from(format!("completion-{ix}")))
                .gap_2()
                .items_center()
                .px_2()
                .py(px(5.))
                .rounded(px(7.))
                .text_sm()
                .cursor_pointer()
                .when(active, |d| d.bg(p.selected))
                .when(!active, |d| d.hover(|d| d.bg(p.hover)))
                .child(Icon::new(icon).xsmall().text_color(p.muted_foreground))
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .child(name),
                )
                .children(info.filter(|i| !i.argument_hint.is_empty()).map(|i| {
                    div()
                        .flex_shrink(1.)
                        .max_w(px(180.))
                        .truncate()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(i.argument_hint.clone())
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(dir),
                )
                .on_click(move |_, window, cx| on_pick(ix, window, cx)),
        );
    }
    if !c.threads.is_empty() {
        list = list.child(
            div()
                .px_2()
                .pt(px(6.))
                .pb(px(2.))
                .text_xs()
                .text_color(p.faint_foreground)
                .child("Threads"),
        );
    }
    for (ix, found) in c.threads.iter().enumerate() {
        let ix = c.items.len() + ix;
        let active = ix == c.selected;
        let on_pick = on_pick.clone();
        list = list.child(
            h_flex()
                .id(SharedString::from(format!("completion-{ix}")))
                .gap_2()
                .items_center()
                .px_2()
                .py(px(5.))
                .rounded(px(7.))
                .text_sm()
                .cursor_pointer()
                .when(active, |d| d.bg(p.selected))
                .when(!active, |d| d.hover(|d| d.bg(p.hover)))
                .child(
                    Icon::new(IconName::MessageSquare)
                        .xsmall()
                        .text_color(p.muted_foreground),
                )
                .child(
                    div()
                        .flex_shrink(1.)
                        .min_w_0()
                        .truncate()
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .child(found.thread.title.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(40.))
                        .truncate()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(found.project.clone()),
                )
                .on_click(move |_, window, cx| on_pick(ix, window, cx)),
        );
    }
    v_flex()
        .mx_2()
        .rounded(px(12.))
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .shadow_lg()
        .child(list)
        .child(
            div()
                .px_3()
                .py_1()
                .border_t_1()
                .border_color(p.border)
                .text_xs()
                .text_color(p.faint_foreground)
                .child(match c.kind {
                    Kind::Slash => format!(
                        "{} command{} · ↑↓ to choose · ⏎ or ⇥ to insert · esc to dismiss",
                        c.items.len(),
                        if c.items.len() == 1 { "" } else { "s" }
                    ),
                    Kind::File if c.picked_thread().is_some() => {
                        "↑↓ to choose · ⏎ or ⇥ to add the thread as context · esc to dismiss".into()
                    }
                    Kind::File => "↑↓ to choose · ⏎ or ⇥ to insert · esc to dismiss".into(),
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers() {
        assert_eq!(trigger("/com", 4).map(|t| t.2), Some("com".into()));
        assert!(trigger("say /com", 8).is_none(), "slash only at the start");
        let (kind, range, q) = trigger("look at @src/ma please", 15).unwrap();
        assert_eq!((kind, range, q.as_str()), (Kind::File, 8..15, "src/ma"));
        assert!(trigger("plain words", 11).is_none());
        assert_eq!(
            trigger("日本 @x", "日本 @x".len()).map(|t| t.2),
            Some("x".into())
        );
    }

    #[test]
    fn threads_are_chosen_after_the_files() {
        use crate::thread_ref::{Found, ThreadRef};
        let thread = ThreadRef::new(ThreadId("t-1".into()), "Fix login");
        let mut c = Completion {
            thread_id: ThreadId("here".into()),
            kind: Kind::File,
            token: 0..4,
            items: vec!["src/login.rs".into()],
            info: Vec::new(),
            threads: vec![Found {
                thread: thread.clone(),
                project: "Shop".into(),
            }],
            selected: 0,
            scroll: Default::default(),
        };
        assert_eq!(c.rows(), 2);
        assert_eq!(c.insertion().as_deref(), Some("@src/login.rs "));
        assert_eq!(c.picked_thread(), None);
        // ↓ goes on from the last file to the first thread: a chip, not text.
        c.move_by(1);
        assert_eq!(c.picked_thread(), Some(&thread));
        assert_eq!(c.insertion(), None);
        // And round to the files again.
        c.move_by(1);
        assert_eq!(c.selected, 0);
    }

    #[test]
    fn commands_prefix_first() {
        let mut cmds =
            bare_commands(&["review", "compact", "context", "pr-comments"].map(String::from));
        cmds[0].description = "Look it over".into();
        let names = |q: &str| -> Vec<String> {
            match_commands(&cmds, q)
                .into_iter()
                .map(|c| c.name)
                .collect()
        };
        assert_eq!(names("co"), vec!["compact", "context", "pr-comments"]);
        // Every command shows for "/", in the agent's order.
        assert_eq!(
            names(""),
            vec!["review", "compact", "context", "pr-comments"]
        );
        // Descriptions match too, after names.
        assert_eq!(names("over"), vec!["review"]);
    }
}
