//! "New thread in…": pick where a new thread goes — the current project,
//! no project (a chat), or any other — by search, arrows or ⌘1…⌘9.

use crate::theme::Palette;
use crate::workspace::WorkspaceView;
use gpui_kit::assets::IconName;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{IndexPath, WindowExt as _};
use gpui_kit::{
    AnyElement, App, AppContext as _, Entity, Hsla, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Styled as _, Window, div, px,
};
use loft_protocol::ProjectId;
use std::rc::Rc;

/// Where a new thread can go.
#[derive(Clone)]
enum Target {
    Project(ProjectId),
    Chat,
}

#[derive(Clone)]
struct Row {
    target: Target,
    name: String,
    detail: Option<String>,
    shortcut: Option<String>,
}

/// Open the picker. Called from the workspace's own handlers, so it reads
/// the store, not the view.
pub fn open(
    view: Entity<WorkspaceView>,
    store: &Entity<crate::store::AppStore>,
    window: &mut Window,
    cx: &mut App,
) {
    let rows = Rc::new(rows(store, cx));
    let state = cx.new(|cx| CommandState::new(window, cx));
    let focus = state.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let (rows, view) = (rows.clone(), view.clone());
        let pick = {
            let (rows, view) = (rows.clone(), view.clone());
            Rc::new(move |ix: usize, window: &mut Window, cx: &mut App| {
                let Some(row) = rows.get(ix) else {
                    return;
                };
                window.close_dialog(cx);
                let target = row.target.clone();
                view.update(cx, |v, cx| match target {
                    Target::Project(id) => v.new_thread(Some(id), cx),
                    Target::Chat => v.store.update(cx, |s, cx| s.new_chat(cx)),
                });
            })
        };
        let command = Command::new(&state)
            .placeholder("New thread in…")
            .bordered(false)
            .max_h(px(460.))
            .footer(|_, _, cx| crate::palette::key_hints(cx))
            .group(
                CommandGroup::new()
                    .label("Projects")
                    .items(rows.iter().map(|r| {
                        let row = r.clone();
                        let mut keywords = vec![r.name.clone()];
                        keywords.extend(r.detail.clone());
                        CommandItem::new()
                            .label(r.name.clone())
                            .keywords(keywords)
                            .child(move |_, cx| render_row(&row, cx))
                    })),
            );
        let (keys_pick, keys_rows) = (pick.clone(), rows.clone());
        let esc_state = state.clone();
        dialog
            .w(px(640.))
            .p_0()
            .overflow_hidden()
            .margin_top(px(96.))
            .close_button(false)
            .child(
                div()
                    // ⌘1…⌘9 pick a project; ⌥⌘N, no project; Esc clears
                    // what's typed, then closes.
                    .capture_key_down(move |event: &gpui_kit::KeyDownEvent, window, cx| {
                        let ks = &event.keystroke;
                        let m = ks.modifiers;
                        if ks.key == "escape" {
                            cx.stop_propagation();
                            if esc_state.read(cx).query(cx).is_empty() {
                                window.close_dialog(cx);
                            } else {
                                esc_state.update(cx, |s, cx| s.set_query("", window, cx));
                            }
                            return;
                        }
                        if !m.platform {
                            return;
                        }
                        let found = if m.alt && ks.key == "n" {
                            keys_rows
                                .iter()
                                .position(|r| matches!(r.target, Target::Chat))
                        } else {
                            ks.key
                                .parse::<usize>()
                                .ok()
                                .filter(|n| (1..=9).contains(n))
                                .and_then(|n| {
                                    keys_rows.iter().position(|r| {
                                        r.shortcut.as_deref() == Some(&format!("⌘{n}"))
                                    })
                                })
                        };
                        if let Some(ix) = found {
                            cx.stop_propagation();
                            keys_pick(ix, window, cx);
                        }
                    })
                    .child(command.on_confirm(move |ix: IndexPath, window, cx| {
                        pick(ix.row, window, cx);
                    })),
            )
    });
    focus.update(cx, |s, cx| s.focus(window, cx));
}

/// The current project, then no project, then the rest by name.
fn rows(store: &Entity<crate::store::AppStore>, cx: &App) -> Vec<Row> {
    let store = store.read(cx);
    let current = store
        .selected
        .as_ref()
        .and_then(|id| store.workspace.thread(id))
        .map(|t| t.project_id.clone());
    let mut projects: Vec<&loft_protocol::Project> = store
        .workspace
        .projects
        .iter()
        .filter(|p| !store.is_chats(p))
        .collect();
    projects.sort_by_key(|p| p.name.to_lowercase());
    if let Some(at) = projects
        .iter()
        .position(|p| Some(&p.id) == current.as_ref())
    {
        let here = projects.remove(at);
        projects.insert(0, here);
    }
    let mut rows = Vec::new();
    let mut n = 0;
    for p in projects {
        let is_current = Some(&p.id) == current.as_ref();
        let path = crate::workspace::tilde_path_str(&p.root_path);
        n += 1;
        rows.push(Row {
            target: Target::Project(p.id.clone()),
            name: p.name.clone(),
            detail: Some(if is_current {
                format!("Current · {path}")
            } else {
                path
            }),
            shortcut: (n <= 9).then(|| format!("⌘{n}")),
        });
        // No project sits right under the current one (or first).
        if rows.len() == 1 && is_current {
            rows.push(chat_row());
        }
    }
    if !rows.iter().any(|r| matches!(r.target, Target::Chat)) {
        rows.insert(0, chat_row());
    }
    rows
}

fn chat_row() -> Row {
    Row {
        target: Target::Chat,
        name: "No project".into(),
        detail: None,
        shortcut: Some("⌥⌘N".into()),
    }
}

fn render_row(row: &Row, cx: &App) -> AnyElement {
    let p = Palette::of(cx);
    let badge: AnyElement = match row.target {
        Target::Chat => gpui_kit::component::Icon::new(IconName::MessageSquareDashed)
            .text_color(p.muted_foreground)
            .into_any_element(),
        Target::Project(_) => {
            let color = badge_color(&row.name);
            div()
                .size(px(22.))
                .flex_none()
                .rounded(px(5.))
                .bg(color.opacity(0.18))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .text_color(color)
                .child(initials(&row.name))
                .into_any_element()
        }
    };
    gpui_kit::component::h_flex()
        .w_full()
        .gap_3()
        .items_center()
        .py(px(2.))
        .child(div().w(px(22.)).flex().justify_center().child(badge))
        .child(
            gpui_kit::component::v_flex()
                .flex_1()
                .min_w_0()
                .child(div().truncate().child(row.name.clone()))
                .when_some_detail(row.detail.clone(), &p),
        )
        .children(row.shortcut.clone().map(|k| {
            div()
                .flex_none()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(k)
        }))
        .into_any_element()
}

trait DetailExt {
    fn when_some_detail(self, detail: Option<String>, p: &Palette) -> Self;
}

impl DetailExt for gpui_kit::Div {
    fn when_some_detail(self, detail: Option<String>, p: &Palette) -> Self {
        match detail {
            Some(d) => self.child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(d),
            ),
            None => self,
        }
    }
}

/// "Matter & Why" → "MW"; "agentloft" → "AT" (first and last letter).
fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let first = |w: &str| w.chars().next().map(|c| c.to_ascii_uppercase());
    match words.as_slice() {
        [] => "?".into(),
        [one] => {
            let chars: Vec<char> = one.chars().collect();
            let mut s = String::new();
            s.extend(chars.first().map(|c| c.to_ascii_uppercase()));
            if chars.len() > 1 {
                s.extend(chars.last().map(|c| c.to_ascii_uppercase()));
            }
            s
        }
        [a, b, ..] => first(a).into_iter().chain(first(b)).collect(),
    }
}

/// A steady colour per project name.
fn badge_color(name: &str) -> Hsla {
    const HUES: [f32; 8] = [205., 150., 0., 85., 38., 270., 175., 325.];
    let hash = name
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    gpui_kit::hsla(HUES[hash as usize % HUES.len()] / 360., 0.75, 0.62, 1.)
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn initials_read_like_names() {
        assert_eq!(initials("Matter & Why"), "MW");
        assert_eq!(initials("agentloft"), "AT");
        assert_eq!(initials("dev"), "DV");
        assert_eq!(initials("x"), "X");
        assert_eq!(initials("stickfigure-studio"), "SS");
    }
}
