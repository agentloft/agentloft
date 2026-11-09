//! Composer completions: `/` at the start lists the agent's slash commands,
//! `@` anywhere fuzzy-finds files in the thread's workspace. ↑/↓ move,
//! Enter or Tab insert, Esc dismisses.

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::ThreadId;
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
    pub selected: usize,
}

impl Completion {
    /// Text that replaces the token.
    pub fn insertion(&self) -> Option<String> {
        let item = self.items.get(self.selected)?;
        Some(match self.kind {
            Kind::Slash => format!("/{item} "),
            Kind::File => format!("@{item} "),
        })
    }

    pub fn move_by(&mut self, delta: isize) {
        let n = self.items.len() as isize;
        if n > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
        }
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

/// Slash commands matching `query`: prefix matches first.
pub fn match_commands(commands: &[String], query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    let mut prefix: Vec<String> = Vec::new();
    let mut inner: Vec<String> = Vec::new();
    for c in commands {
        let lc = c.to_lowercase();
        if lc.starts_with(&q) {
            prefix.push(c.clone());
        } else if lc.contains(&q) {
            inner.push(c.clone());
        }
    }
    prefix.extend(inner);
    prefix.truncate(MAX_ITEMS);
    prefix
}

/// The popover above the composer.
pub fn popover(
    c: &Completion,
    p: &Palette,
    on_pick: impl Fn(usize, &mut Window, &mut App) + Clone + 'static,
) -> AnyElement {
    let mut list = v_flex().p_1().gap_px();
    for (ix, item) in c.items.iter().enumerate() {
        let active = ix == c.selected;
        let on_pick = on_pick.clone();
        let (icon, dir, name) = match c.kind {
            Kind::Slash => (IconName::SquareTerminal, String::new(), format!("/{item}")),
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
                .child(div().font_weight(gpui_kit::FontWeight::MEDIUM).child(name))
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
                .child("↑↓ to choose · ⏎ or ⇥ to insert · esc to dismiss"),
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
    fn commands_prefix_first() {
        let cmds: Vec<String> = ["review", "compact", "context", "pr-comments"]
            .map(String::from)
            .into();
        assert_eq!(
            match_commands(&cmds, "co"),
            vec!["compact", "context", "pr-comments"]
        );
    }
}
