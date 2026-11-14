//! Agentloft on another computer, over SSH: "Connect to Another
//! Computer…" picks a host (one connected to before, one from
//! `~/.ssh/config`, or one typed in) and opens a window on its Agentloft.
//!
//! The window's store reaches that computer's daemon through `ssh <host>
//! agentloftd bridge` (`loft_client::ssh`), so SSH does the encrypting and
//! signing in, with the user's own keys and config.

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{IndexPath, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use std::path::Path;
use std::rc::Rc;

/// Hosts connected to before, most recent first.
const MOST_RECENT: usize = 12;

fn recent_file(cx: &App) -> Option<std::path::PathBuf> {
    cx.try_global::<crate::windows::AppEnv>()
        .map(|env| env.paths.home.join("ui-remotes.json"))
}

/// Hosts connected to before, most recent first.
pub fn recent(cx: &App) -> Vec<String> {
    recent_file(cx)
        .and_then(|file| crate::host::read(&file).ok())
        .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|h| loft_client::valid_destination(h))
        .collect()
}

/// Put `host` first among the recent ones.
pub fn remember(host: &str, cx: &App) {
    let mut hosts = recent(cx);
    hosts.retain(|h| h != host);
    hosts.insert(0, host.to_owned());
    hosts.truncate(MOST_RECENT);
    save(&hosts, cx);
}

pub fn forget(host: &str, cx: &App) {
    let mut hosts = recent(cx);
    hosts.retain(|h| h != host);
    save(&hosts, cx);
}

fn save(hosts: &[String], cx: &App) {
    if let (Some(file), Ok(json)) = (recent_file(cx), serde_json::to_vec_pretty(hosts)) {
        let _ = crate::host::write(&file, &json);
    }
}

/// The hosts named in `~/.ssh/config` (and the files it includes), without
/// patterns or git hosting services (`github.com`: not a computer).
pub fn ssh_config_hosts() -> Vec<String> {
    let ssh = crate::host::user_home().join(".ssh");
    let mut hosts = Vec::new();
    let mut git = Vec::new();
    read_hosts(&ssh.join("config"), &ssh, &mut hosts, &mut git, 0);
    hosts.retain(|h| !git.contains(h) && !is_git_service(h));
    hosts
}

/// A git hosting service rather than a computer of the user's.
fn is_git_service(host: &str) -> bool {
    const SERVICES: [&str; 5] = [
        "github.com",
        "gitlab.com",
        "bitbucket.org",
        "ssh.dev.azure.com",
        "codeberg.org",
    ];
    let host = host.to_ascii_lowercase();
    let host = host.rsplit('@').next().unwrap_or(&host);
    SERVICES
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")))
}

fn read_hosts(
    file: &Path,
    ssh_dir: &Path,
    hosts: &mut Vec<String>,
    git: &mut Vec<String>,
    depth: usize,
) {
    let Ok(text) = crate::host::read_to_string(file) else {
        return;
    };
    // The aliases of the `Host` block being read.
    let mut block: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let (key, rest) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((key, rest)) => (key, rest.trim().trim_start_matches('=').trim()),
            None => continue,
        };
        if key.eq_ignore_ascii_case("hostname") {
            // An alias for a git service (`Host gh` / `HostName github.com`).
            if is_git_service(rest.trim_matches('"')) {
                git.extend(block.iter().cloned());
            }
        } else if key.eq_ignore_ascii_case("host") {
            block.clear();
            for name in rest.split_whitespace() {
                block.push(name.trim_matches('"').to_owned());
                let name = name.trim_matches('"');
                if !name.contains(['*', '?', '!'])
                    && loft_client::valid_destination(name)
                    && !hosts.iter().any(|h| h == name)
                {
                    hosts.push(name.to_owned());
                }
            }
        } else if key.eq_ignore_ascii_case("include") && depth < 4 {
            for pattern in rest.split_whitespace() {
                let pattern = pattern.trim_matches('"');
                let path = match pattern.strip_prefix("~/") {
                    Some(rest) => crate::host::user_home().join(rest),
                    None if Path::new(pattern).is_absolute() => pattern.into(),
                    None => ssh_dir.join(pattern),
                };
                for found in expand_glob(&path) {
                    read_hosts(&found, ssh_dir, hosts, git, depth + 1);
                }
            }
        }
    }
}

/// `dir/*.conf`-style includes: a `*` in the file name only.
fn expand_glob(path: &Path) -> Vec<std::path::PathBuf> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    if !name.contains('*') {
        return vec![path.to_path_buf()];
    }
    let (before, after) = name.split_once('*').unwrap_or((name, ""));
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let mut found: Vec<_> = crate::host::list_files(dir)
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(before) && n.ends_with(after))
        })
        .collect();
    found.sort();
    found
}

#[derive(Clone)]
struct Row {
    host: String,
    detail: String,
    icon: IconName,
    recent: bool,
}

fn rows(query: &str, recent: &[String], configured: &[String]) -> Vec<Row> {
    let typed = query.trim();
    let needle = typed.to_lowercase();
    let matches = |h: &str| needle.is_empty() || h.to_lowercase().contains(&needle);
    let mut rows = Vec::new();
    if !typed.is_empty()
        && loft_client::valid_destination(typed)
        && !recent.iter().chain(configured).any(|h| h == typed)
    {
        rows.push(Row {
            host: typed.to_owned(),
            detail: "Connect over SSH".into(),
            icon: IconName::ArrowRight,
            recent: false,
        });
    }
    for host in recent.iter().filter(|h| matches(h)) {
        rows.push(Row {
            host: host.clone(),
            detail: "Connected before".into(),
            icon: IconName::Clock,
            recent: true,
        });
    }
    for host in configured
        .iter()
        .filter(|h| matches(h) && !recent.contains(h))
    {
        rows.push(Row {
            host: host.clone(),
            detail: "From ~/.ssh/config".into(),
            icon: IconName::Server,
            recent: false,
        });
    }
    rows
}

/// Open the picker.
pub fn open(window: &mut Window, cx: &mut App) {
    let state = cx.new(|cx| CommandState::new(window, cx));
    let configured = Rc::new(ssh_config_hosts());
    let focus = state.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let query = state.read(cx).query(cx).to_string();
        let rows = Rc::new(rows(&query, &recent(cx), &configured));
        let p = Palette::of(cx);
        let pick = {
            let rows = rows.clone();
            move |ix: IndexPath, window: &mut Window, cx: &mut App| {
                let Some(row) = rows.get(ix.row) else {
                    return;
                };
                window.close_dialog(cx);
                let host = row.host.clone();
                window.defer(cx, move |_, cx| {
                    crate::windows::open_remote(host, cx);
                });
            }
        };
        let hint = if rows.is_empty() {
            "Type a host: an alias from ~/.ssh/config, host or user@host"
        } else {
            "Agentloft must be installed there. SSH signs in with your keys."
        };
        let command = Command::new(&state)
            .placeholder("Connect to another computer over SSH…")
            .bordered(false)
            .filterable(false)
            .max_h(px(420.))
            .footer(move |_, _, cx| crate::palette::key_hints_with_back(cx, false))
            .group(
                CommandGroup::new()
                    .label("Computers")
                    .items(rows.iter().map(|r| {
                        let row = r.clone();
                        let p = p.clone();
                        CommandItem::new()
                            .label(r.host.clone())
                            .keywords(vec![r.host.clone()])
                            .child(move |_, _| render_row(&row, &p))
                    })),
            );
        let forget_rows = rows.clone();
        let forget_state = state.clone();
        dialog
            .w(px(560.))
            .p_0()
            .overflow_hidden()
            .margin_top(px(96.))
            .close_button(false)
            .child(
                div()
                    .capture_key_down(move |event: &gpui_kit::KeyDownEvent, window, cx| {
                        let ks = &event.keystroke;
                        if ks.key == "escape" {
                            cx.stop_propagation();
                            window.close_dialog(cx);
                        }
                        // ⌘⌫ forgets a host connected to before.
                        if ks.key == "backspace" && ks.modifiers.platform {
                            let selected = forget_state.read(cx).selected_index().map(|i| i.row);
                            if let Some(row) = selected
                                .and_then(|i| forget_rows.get(i))
                                .filter(|r| r.recent)
                            {
                                cx.stop_propagation();
                                forget(&row.host, cx);
                                window.refresh();
                            }
                        }
                    })
                    .child(command.on_confirm(pick))
                    .child(
                        div()
                            .px_4()
                            .pb_3()
                            .text_xs()
                            .text_color(p.faint_foreground)
                            .child(hint),
                    ),
            )
    });
    focus.update(cx, |s, cx| s.focus(window, cx));
}

fn render_row(row: &Row, p: &Palette) -> gpui_kit::AnyElement {
    gpui_kit::component::h_flex()
        .w_full()
        .gap_3()
        .items_center()
        .py(px(2.))
        .child(
            div()
                .w(px(22.))
                .flex()
                .justify_center()
                .child(gpui_kit::component::Icon::new(row.icon).text_color(p.muted_foreground)),
        )
        .child(
            gpui_kit::component::v_flex()
                .flex_1()
                .min_w_0()
                .child(div().truncate().child(row.host.clone()))
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(row.detail.clone()),
                ),
        )
        .when(row.recent, |d| {
            d.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child("⌘⌫ forget"),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_services_are_not_computers() {
        assert!(is_git_service("github.com"));
        assert!(is_git_service("git@GitHub.com"));
        assert!(is_git_service("ssh.github.com"));
        assert!(!is_git_service("mini.local"));
        assert!(!is_git_service("notgithub.com"));
    }

    #[test]
    fn typed_hosts_come_first_then_recent_then_configured() {
        let recent = vec!["mini".to_owned()];
        let configured = vec!["mini".to_owned(), "box".to_owned()];
        let all: Vec<_> = rows("", &recent, &configured)
            .into_iter()
            .map(|r| r.host)
            .collect();
        assert_eq!(all, ["mini", "box"]);
        let typed: Vec<_> = rows("me@new", &recent, &configured)
            .into_iter()
            .map(|r| r.host)
            .collect();
        assert_eq!(typed, ["me@new"]);
        // Something that isn't a host is never offered.
        assert!(rows("-oProxyCommand=x", &recent, &configured).is_empty());
        let filtered: Vec<_> = rows("bo", &recent, &configured)
            .into_iter()
            .map(|r| r.host)
            .collect();
        assert_eq!(filtered, ["bo", "box"]);
    }
}
