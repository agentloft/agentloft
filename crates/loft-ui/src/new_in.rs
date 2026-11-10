//! "New thread in…": pick where a new thread goes — the current project,
//! no project (a chat), a pinned or any other project — or add a project
//! (new, local folder, git URL, GitHub) and start there.

use crate::theme::Palette;
use crate::workspace::WorkspaceView;
use gpui_kit::assets::IconName;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{IndexPath, WindowExt as _};
use gpui_kit::{
    AnyElement, App, AppContext as _, Entity, Hsla, InteractiveElement as _, IntoElement as _,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use loft_protocol::{ProjectId, Request};
use std::rc::Rc;

/// Pins or unpins a project.
type PinFn = Rc<dyn Fn(ProjectId, &mut Window, &mut App)>;

/// Where a row leads.
#[derive(Clone)]
enum Target {
    Project(ProjectId),
    Chat,
    /// "Add project…": the sources step.
    Sources,
    Source(Source),
    /// Run the step's action with what's typed.
    Submit(Source, String),
    /// New project: also make a private GitHub repository (on/off).
    TogglePublish,
}

/// Ways to add a project.
#[derive(Clone, Copy, PartialEq)]
enum Source {
    New,
    Folder,
    Url,
    GitHub,
}

/// Which list the picker shows.
#[derive(Clone, Copy, PartialEq)]
enum Step {
    Projects,
    Sources,
    Enter(Source),
}

#[derive(Clone)]
struct Row {
    target: Target,
    name: String,
    detail: Option<String>,
    shortcut: Option<String>,
    icon: Option<IconName>,
    pinned: Option<bool>,
    /// A checkbox at the end (an option row).
    checked: Option<bool>,
}

impl Row {
    fn plain(target: Target, name: &str, detail: &str, icon: IconName) -> Self {
        Row {
            target,
            name: name.into(),
            detail: (!detail.is_empty()).then(|| detail.into()),
            shortcut: None,
            icon: Some(icon),
            pinned: None,
            checked: None,
        }
    }
}

/// Open the picker. Called from the workspace's own handlers, so it reads
/// the store, not the view.
pub fn open(
    view: Entity<WorkspaceView>,
    store: &Entity<crate::store::AppStore>,
    window: &mut Window,
    cx: &mut App,
) {
    open_at(Step::Projects, view, store, window, cx)
}

/// Open straight at "Add a project from…" (⌘O, the sidebar's folder button).
pub fn open_sources(
    view: Entity<WorkspaceView>,
    store: &Entity<crate::store::AppStore>,
    window: &mut Window,
    cx: &mut App,
) {
    open_at(Step::Sources, view, store, window, cx)
}

fn open_at(
    start: Step,
    view: Entity<WorkspaceView>,
    store: &Entity<crate::store::AppStore>,
    window: &mut Window,
    cx: &mut App,
) {
    // Each step gets a search field of its own (fresh text, highlight and
    // focus): (step, its field).
    let first = cx.new(|cx| CommandState::new(window, cx));
    let focus = first.clone();
    let at = cx.new(|_| (start, first));
    // New project: also publish to GitHub?
    let publish = cx.new(|_| false);
    store.update(cx, |s, cx| s.ensure_github_login(cx));
    let store = store.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let (current, state) = at.read(cx).clone();
        let query = state.read(cx).query(cx).to_string();
        let publishing = *publish.read(cx);
        let rows = Rc::new(rows(&store, current, &query, publishing, cx));
        // Act on a row: open a thread, go to another step, or add a project.
        let pick = {
            let (rows, view, at, store, publish) = (
                rows.clone(),
                view.clone(),
                at.clone(),
                store.clone(),
                publish.clone(),
            );
            Rc::new(move |ix: usize, window: &mut Window, cx: &mut App| {
                let Some(row) = rows.get(ix) else {
                    return;
                };
                let go = |to: Step, window: &mut Window, cx: &mut App| goto(&at, to, window, cx);
                match row.target.clone() {
                    Target::Project(id) => {
                        window.close_dialog(cx);
                        view.update(cx, |v, cx| v.new_thread(Some(id), cx));
                    }
                    Target::Chat => {
                        window.close_dialog(cx);
                        store.update(cx, |s, cx| s.new_chat(cx));
                    }
                    Target::Sources => go(Step::Sources, window, cx),
                    Target::Source(Source::Folder) => {
                        window.close_dialog(cx);
                        let view = view.clone();
                        window.defer(cx, move |window, cx| {
                            view.update(cx, |v, cx| v.add_project(window, cx));
                        });
                    }
                    Target::Source(src) => go(Step::Enter(src), window, cx),
                    Target::TogglePublish => {
                        publish.update(cx, |p, _| *p = !*p);
                        window.refresh();
                    }
                    Target::Submit(src, text) => {
                        window.close_dialog(cx);
                        let parent = store.read(cx).projects_parent();
                        let request = match src {
                            Source::New => Request::CreateProject {
                                parent: Some(
                                    store
                                        .read(cx)
                                        .new_projects_dir()
                                        .to_string_lossy()
                                        .into_owned(),
                                ),
                                name: text,
                                github: *publish.read(cx),
                            },
                            Source::GitHub => Request::CloneProject {
                                parent,
                                source: text,
                                github: true,
                            },
                            _ => Request::CloneProject {
                                parent,
                                source: text,
                                github: false,
                            },
                        };
                        store.update(cx, |s, cx| s.make_project(request, cx));
                    }
                }
            })
        };
        let toggle_pin = {
            let store = store.clone();
            Rc::new(move |id: ProjectId, window: &mut Window, cx: &mut App| {
                store.update(cx, |s, cx| s.toggle_pinned(id, cx));
                window.refresh();
            })
        };
        let entering = matches!(current, Step::Enter(_));
        let command = Command::new(&state)
            .placeholder(match current {
                Step::Projects => "New thread in…",
                Step::Sources => "Add a project from…",
                Step::Enter(Source::New) => "Name of the new project",
                Step::Enter(Source::GitHub) => "owner/repo",
                Step::Enter(_) => "Git URL (https or ssh)",
            })
            .bordered(false)
            // Typing a name or URL isn't a search.
            .filterable(!entering)
            .max_h(px(460.))
            .footer(move |_, _, cx| {
                crate::palette::key_hints_with_back(cx, current != Step::Projects)
            })
            .group(
                CommandGroup::new()
                    .label(match current {
                        Step::Projects => "Projects",
                        Step::Sources => "Sources",
                        Step::Enter(_) => "Add project",
                    })
                    .items(rows.iter().map(|r| {
                        let row = r.clone();
                        let pin = toggle_pin.clone();
                        let mut keywords = vec![r.name.clone()];
                        keywords.extend(r.detail.clone());
                        CommandItem::new()
                            .label(r.name.clone())
                            .keywords(keywords)
                            .child(move |_, cx| render_row(&row, pin.clone(), cx))
                    })),
            );
        let (keys_pick, keys_rows, keys_state, keys_at, keys_pin) = (
            pick.clone(),
            rows.clone(),
            state.clone(),
            at.clone(),
            toggle_pin.clone(),
        );
        dialog
            .w(px(640.))
            .p_0()
            .overflow_hidden()
            .margin_top(px(96.))
            .close_button(false)
            .child(
                div()
                    .capture_key_down(move |event: &gpui_kit::KeyDownEvent, window, cx| {
                        let ks = &event.keystroke;
                        let m = ks.modifiers;
                        let empty = keys_state.read(cx).query(cx).is_empty();
                        let at = current;
                        // Esc clears what's typed, then closes.
                        if ks.key == "escape" {
                            cx.stop_propagation();
                            if empty {
                                window.close_dialog(cx);
                            } else {
                                keys_state.update(cx, |s, cx| s.set_query("", window, cx));
                            }
                            return;
                        }
                        // Backspace on nothing typed: back a step.
                        if ks.key == "backspace" && empty && !m.platform && at != Step::Projects {
                            cx.stop_propagation();
                            let back = match at {
                                Step::Enter(_) => Step::Sources,
                                _ => Step::Projects,
                            };
                            goto(&keys_at, back, window, cx);
                            return;
                        }
                        // ⌘↩ creates the new project.
                        if m.platform && ks.key == "enter" && at == Step::Enter(Source::New) {
                            if let Some(ix) = keys_rows
                                .iter()
                                .position(|r| matches!(r.target, Target::Submit(..)))
                            {
                                cx.stop_propagation();
                                keys_pick(ix, window, cx);
                            }
                            return;
                        }
                        if !m.platform || at != Step::Projects {
                            return;
                        }
                        // ⇧⌘P pins the highlighted project.
                        if m.shift && ks.key == "p" {
                            cx.stop_propagation();
                            let selected = keys_state.read(cx).selected_index().map(|i| i.row);
                            if let Some(Target::Project(id)) = selected
                                .and_then(|i| keys_rows.get(i))
                                .map(|r| r.target.clone())
                            {
                                keys_pin(id, window, cx);
                            }
                            return;
                        }
                        // ⌘1…⌘9 a project; ⌥⌘N no project.
                        let wanted = if m.alt && ks.key == "n" {
                            Some("⌥⌘N".to_owned())
                        } else {
                            ks.key
                                .parse::<usize>()
                                .ok()
                                .filter(|n| (1..=9).contains(n))
                                .map(|n| format!("⌘{n}"))
                        };
                        if let Some(ix) = wanted.and_then(|w| {
                            keys_rows
                                .iter()
                                .position(|r| r.shortcut.as_deref() == Some(&w))
                        }) {
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

/// Show another step, with a fresh search field that has the keys.
fn goto(at: &Entity<(Step, Entity<CommandState>)>, to: Step, window: &mut Window, cx: &mut App) {
    let fresh = cx.new(|cx| CommandState::new(window, cx));
    at.update(cx, |a, _| *a = (to, fresh.clone()));
    fresh.update(cx, |s, cx| s.focus(window, cx));
    window.refresh();
}

/// The rows for a step.
fn rows(
    store: &Entity<crate::store::AppStore>,
    step: Step,
    query: &str,
    publishing: bool,
    cx: &App,
) -> Vec<Row> {
    match step {
        Step::Projects => project_rows(store, cx),
        Step::Sources => vec![
            Row::plain(
                Target::Source(Source::New),
                "New project",
                "Start a new git repository from a name",
                IconName::FolderGit2,
            ),
            Row::plain(
                Target::Source(Source::Folder),
                "Local folder",
                "Browse a folder on disk",
                IconName::FolderPlus,
            ),
            Row::plain(
                Target::Source(Source::Url),
                "Git URL",
                "Clone from a remote URL",
                IconName::Link,
            ),
            Row::plain(
                Target::Source(Source::GitHub),
                "GitHub repository",
                "Clone GitHub owner/repo",
                IconName::Github,
            ),
        ],
        Step::Enter(Source::New) => {
            let dir = crate::workspace::tilde_path_str(
                &store.read(cx).new_projects_dir().to_string_lossy(),
            );
            let text = query.trim();
            let mut rows = vec![if text.is_empty() {
                Row::plain(
                    Target::Source(Source::New),
                    "Type a name for the project",
                    &format!("Goes in {dir}"),
                    IconName::Info,
                )
            } else {
                Row {
                    shortcut: Some("⌘↩".into()),
                    ..Row::plain(
                        Target::Submit(Source::New, text.to_owned()),
                        &format!("Create “{text}”"),
                        &format!("Goes in {dir}/{}", slug(text)),
                        IconName::FolderGit2,
                    )
                }
            }];
            // Publishing needs gh signed in.
            if let Some(Some(login)) = &store.read(cx).github_login {
                rows.push(Row {
                    checked: Some(publishing),
                    ..Row::plain(
                        Target::TogglePublish,
                        "Create private repository on GitHub",
                        login,
                        IconName::Github,
                    )
                });
            }
            rows
        }
        Step::Enter(src) => {
            let parent = crate::workspace::tilde_path_str(&store.read(cx).projects_parent());
            let text = query.trim();
            if text.is_empty() {
                return vec![Row::plain(
                    Target::Source(src),
                    match src {
                        Source::New => "Type a name for the project",
                        Source::GitHub => "Type owner/repo",
                        _ => "Paste a git URL",
                    },
                    &format!("It goes in {parent}"),
                    IconName::Info,
                )];
            }
            let (name, icon) = match src {
                Source::New => (format!("Create “{text}”"), IconName::FolderGit2),
                Source::GitHub => (format!("Clone {text}"), IconName::Github),
                _ => (format!("Clone {text}"), IconName::Link),
            };
            vec![Row::plain(
                Target::Submit(src, text.to_owned()),
                &name,
                &format!("Into {parent}, then a new thread there"),
                icon,
            )]
        }
    }
}

/// The folder name a project gets ("Pinball Stats" → pinball-stats).
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() {
        "project".into()
    } else {
        out
    }
}

/// The current project, then no project, then pinned projects (in pin
/// order), then the rest by name; "Add project…" last.
fn project_rows(store: &Entity<crate::store::AppStore>, cx: &App) -> Vec<Row> {
    let store = store.read(cx);
    let current = store
        .selected
        .as_ref()
        .and_then(|id| store.workspace.thread(id))
        .map(|t| t.project_id.clone());
    let pinned = &store.pinned_projects;
    let mut projects: Vec<&loft_protocol::Project> = store
        .workspace
        .projects
        .iter()
        .filter(|p| !store.is_chats(p))
        .collect();
    projects.sort_by_key(|p| {
        (
            Some(&p.id) != current.as_ref(),
            pinned.iter().position(|x| *x == p.id).unwrap_or(usize::MAX),
            p.name.to_lowercase(),
        )
    });
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
            icon: None,
            pinned: Some(pinned.contains(&p.id)),
            checked: None,
        });
        // No project sits right under the current one (or first).
        if rows.len() == 1 && is_current {
            rows.push(chat_row());
        }
    }
    if !rows.iter().any(|r| matches!(r.target, Target::Chat)) {
        rows.insert(0, chat_row());
    }
    rows.push(Row::plain(
        Target::Sources,
        "Add project…",
        "New, local folder, git URL or GitHub",
        IconName::Plus,
    ));
    rows
}

fn chat_row() -> Row {
    Row {
        target: Target::Chat,
        name: "No project".into(),
        detail: None,
        shortcut: Some("⌥⌘N".into()),
        icon: Some(IconName::MessageSquareDashed),
        pinned: None,
        checked: None,
    }
}

fn render_row(row: &Row, toggle_pin: PinFn, cx: &App) -> AnyElement {
    let p = Palette::of(cx);
    let badge: AnyElement = match (&row.target, row.icon) {
        (Target::Project(_), _) => {
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
        (_, Some(icon)) => gpui_kit::component::Icon::new(icon)
            .text_color(p.muted_foreground)
            .into_any_element(),
        _ => div().into_any_element(),
    };
    // Projects: a pin, shown on hover (always once pinned).
    let pin = match (&row.target, row.pinned) {
        (Target::Project(id), Some(pinned)) => {
            let (id, name) = (id.clone(), row.name.clone());
            let tip = format!("{} {name} (⇧⌘P)", if pinned { "Unpin" } else { "Pin" });
            Some(
                div()
                    .id(gpui_kit::SharedString::from(format!("pin-{id}")))
                    .flex_none()
                    .size(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(if pinned {
                        p.foreground
                    } else {
                        p.muted_foreground
                    })
                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    .when(!pinned, |d| {
                        d.invisible().group_hover("new-in-row", |d| d.visible())
                    })
                    .child(
                        gpui_kit::component::Icon::new(if pinned {
                            IconName::PinOff
                        } else {
                            IconName::Pin
                        })
                        .size(px(14.)),
                    )
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    })
                    // Not a pick: just the pin.
                    .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation()
                    })
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        toggle_pin(id.clone(), window, cx);
                    }),
            )
        }
        _ => None,
    };
    gpui_kit::component::h_flex()
        .id(gpui_kit::SharedString::from(format!("new-in-{}", row.name)))
        .group("new-in-row")
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
        .children(pin)
        .children(row.checked.map(|on| {
            div()
                .flex_none()
                .size(px(18.))
                .rounded(px(5.))
                .border_1()
                .border_color(if on { p.accent } else { p.border })
                .when(on, |d| d.bg(p.accent))
                .flex()
                .items_center()
                .justify_center()
                .when(on, |d| {
                    d.child(
                        gpui_kit::component::Icon::new(IconName::Check)
                            .size(px(12.))
                            .text_color(p.accent_foreground),
                    )
                })
        }))
        .children(row.shortcut.clone().map(|k| {
            div()
                .flex_none()
                .min_w(px(30.))
                .text_right()
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
pub(crate) fn initials(name: &str) -> String {
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
pub(crate) fn badge_color(name: &str) -> Hsla {
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
