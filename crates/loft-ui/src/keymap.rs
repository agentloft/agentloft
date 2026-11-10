//! Every shortcut the app has, its default, and the user's own choice
//! (`[ui.keys]` in config.toml: `new_thread = "cmd-shift-t"`, or `""` to
//! turn one off). Applied at startup and again whenever the file changes.

use crate::workspace::{
    AddProject, ApproveOnce, ApproveSession, DenyRequest, FocusSearch, Interrupt, NewChat, NewTab,
    NewThread, NewWindow, OpenPalette, OpenSettings, Quit, ShowBrowser, ShowChanges, ShowFiles,
    SteerMessage, TogglePanel, ToggleSidebar, ToggleTerminal, ToggleTheme,
};
use gpui_kit::{Action, App, KeyBinding, KeyBindingContextPredicate};
use std::collections::BTreeMap;
use std::rc::Rc;

pub struct Shortcut {
    /// Its key in `[ui.keys]`.
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    /// `secondary` is ⌘ on macOS, Ctrl elsewhere.
    pub default: &'static str,
    context: Option<&'static str>,
    action: fn() -> Box<dyn Action>,
}

impl Shortcut {
    /// The keys this shortcut has until the user changes them. In a
    /// browser, the few the browser keeps for itself (new tab, new window,
    /// its own settings, quit) get keys of their own or none.
    pub fn default_keys(&self) -> &'static str {
        if !crate::host::IS_WEB {
            return self.default;
        }
        match self.id {
            "new_thread" => "ctrl-alt-n",
            "new_pane" => "ctrl-alt-t",
            "settings" => "ctrl-alt-,",
            // One window per tab, and the browser closes it.
            "new_window" | "quit" => "",
            _ => self.default,
        }
    }
}

const WORKSPACE: Option<&str> = Some("Workspace");
const COMPOSER: Option<&str> = Some("Composer > Input");
const BROWSER: Option<&str> = Some("BrowserView");

macro_rules! shortcut {
    ($id:literal, $label:literal, $group:literal, $keys:literal, $ctx:expr, $action:expr) => {
        Shortcut {
            id: $id,
            label: $label,
            group: $group,
            default: $keys,
            context: $ctx,
            action: || Box::new($action),
        }
    };
}

pub const SHORTCUTS: &[Shortcut] = &[
    shortcut!(
        "new_thread",
        "New thread",
        "General",
        "secondary-n",
        WORKSPACE,
        NewThread
    ),
    shortcut!(
        "new_chat",
        "New chat",
        "General",
        "alt-secondary-n",
        WORKSPACE,
        NewChat
    ),
    shortcut!(
        "new_pane",
        "New pane",
        "General",
        "secondary-t",
        None,
        NewTab
    ),
    shortcut!(
        "new_window",
        "New window",
        "General",
        "secondary-shift-n",
        None,
        NewWindow
    ),
    shortcut!(
        "add_project",
        "Add project",
        "General",
        "secondary-o",
        WORKSPACE,
        AddProject
    ),
    shortcut!(
        "command_center",
        "Command center",
        "General",
        "secondary-k",
        WORKSPACE,
        OpenPalette
    ),
    shortcut!(
        "search",
        "Search threads",
        "General",
        "secondary-shift-f",
        WORKSPACE,
        FocusSearch
    ),
    shortcut!(
        "settings",
        "Settings",
        "General",
        "secondary-,",
        WORKSPACE,
        OpenSettings
    ),
    shortcut!("quit", "Quit", "General", "secondary-q", None, Quit),
    shortcut!(
        "steer",
        "Send now (steer the running turn)",
        "Conversation",
        "secondary-enter",
        COMPOSER,
        SteerMessage
    ),
    shortcut!(
        "stop",
        "Stop the turn",
        "Conversation",
        "secondary-.",
        WORKSPACE,
        Interrupt
    ),
    shortcut!(
        "approve",
        "Approve",
        "Conversation",
        "secondary-enter",
        WORKSPACE,
        ApproveOnce
    ),
    shortcut!(
        "approve_session",
        "Approve for the session",
        "Conversation",
        "secondary-shift-enter",
        WORKSPACE,
        ApproveSession
    ),
    shortcut!(
        "deny",
        "Deny",
        "Conversation",
        "secondary-shift-backspace",
        WORKSPACE,
        DenyRequest
    ),
    shortcut!(
        "toggle_sidebar",
        "Toggle sidebar",
        "View",
        "secondary-b",
        WORKSPACE,
        ToggleSidebar
    ),
    shortcut!(
        "toggle_panel",
        "Toggle side panel",
        "View",
        "secondary-j",
        WORKSPACE,
        TogglePanel
    ),
    shortcut!(
        "toggle_terminal",
        "Toggle terminal",
        "View",
        "ctrl-`",
        WORKSPACE,
        ToggleTerminal
    ),
    shortcut!(
        "show_files",
        "Show files",
        "View",
        "secondary-shift-e",
        WORKSPACE,
        ShowFiles
    ),
    shortcut!(
        "show_changes",
        "Show changes",
        "View",
        "secondary-shift-g",
        WORKSPACE,
        ShowChanges
    ),
    shortcut!(
        "show_browser",
        "Show browser",
        "View",
        "secondary-shift-b",
        WORKSPACE,
        ShowBrowser
    ),
    shortcut!(
        "toggle_theme",
        "Light / dark",
        "View",
        "secondary-shift-l",
        WORKSPACE,
        ToggleTheme
    ),
    shortcut!(
        "browser_address",
        "Address bar",
        "Browser",
        "secondary-l",
        BROWSER,
        loft_browser::FocusAddressBar
    ),
    shortcut!(
        "browser_reload",
        "Reload",
        "Browser",
        "secondary-r",
        BROWSER,
        loft_browser::ReloadPage
    ),
    shortcut!(
        "browser_back",
        "Back",
        "Browser",
        "secondary-[",
        BROWSER,
        loft_browser::GoBack
    ),
    shortcut!(
        "browser_forward",
        "Forward",
        "Browser",
        "secondary-]",
        BROWSER,
        loft_browser::GoForward
    ),
    shortcut!(
        "browser_stop",
        "Stop loading",
        "Browser",
        "secondary-.",
        BROWSER,
        loft_browser::StopLoading
    ),
];

/// Keys the composer handles itself (shown, not changeable).
pub const FIXED: &[(&str, &str, &str)] = &[
    (
        "Conversation",
        "Send (queued while the agent works)",
        "enter",
    ),
    ("Conversation", "New line", "shift-enter"),
];

/// The groups, in the order they're shown.
pub const GROUPS: &[&str] = &["General", "Conversation", "View", "Browser"];

fn overrides(cx: &App) -> BTreeMap<String, String> {
    crate::theme::key_overrides(cx)
}

/// The keys bound to a shortcut now (`None`: turned off).
pub fn keys_for(id: &str, cx: &App) -> Option<String> {
    let shortcut = SHORTCUTS.iter().find(|s| s.id == id)?;
    let keys = overrides(cx)
        .get(id)
        .cloned()
        .unwrap_or_else(|| shortcut.default_keys().to_owned());
    let keys = normalize(&keys);
    (!keys.is_empty()).then_some(keys)
}

/// Whether the user changed this one.
pub fn is_custom(id: &str, cx: &App) -> bool {
    overrides(cx).contains_key(id)
}

/// A shortcut as the hint text next to its button (`⌘N`), or "".
pub fn hint(id: &str, cx: &App) -> String {
    keys_for(id, cx).map(|k| display(&k)).unwrap_or_default()
}

/// `secondary` spelled out for this OS.
pub fn normalize(keys: &str) -> String {
    let secondary = if crate::host::is_mac() { "cmd" } else { "ctrl" };
    keys.split_whitespace()
        .map(|stroke| {
            stroke
                .split('-')
                .map(|part| if part == "secondary" { secondary } else { part })
                .collect::<Vec<_>>()
                .join("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `cmd-shift-n` → `⇧⌘N` on macOS, `Ctrl+Shift+N` elsewhere.
pub fn display(keys: &str) -> String {
    let mac = crate::host::is_mac();
    normalize(keys)
        .split_whitespace()
        .map(|stroke| {
            // The key is last; `-` itself can be the key (`cmd--`).
            let (mods, key) = match stroke.strip_suffix("--") {
                Some(rest) => (rest, "-"),
                None => match stroke.rsplit_once('-') {
                    Some((mods, key)) => (mods, key),
                    None => ("", stroke),
                },
            };
            let mods: Vec<&str> = mods.split('-').filter(|m| !m.is_empty()).collect();
            let has = |m: &str| mods.contains(&m);
            let key = key_name(key, mac);
            if mac {
                let mut s = String::new();
                for (m, sym) in [
                    ("ctrl", "⌃"),
                    ("alt", "⌥"),
                    ("shift", "⇧"),
                    ("cmd", "⌘"),
                    ("fn", "fn "),
                ] {
                    if has(m) {
                        s.push_str(sym);
                    }
                }
                s + &key
            } else {
                let mut parts: Vec<String> = Vec::new();
                for (m, name) in [
                    ("ctrl", "Ctrl"),
                    ("alt", "Alt"),
                    ("shift", "Shift"),
                    ("cmd", "Win"),
                    ("win", "Win"),
                    ("super", "Super"),
                ] {
                    if has(m) {
                        parts.push(name.into());
                    }
                }
                parts.push(key);
                parts.join("+")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn key_name(key: &str, mac: bool) -> String {
    let named = match key {
        "enter" => Some(if mac { "↩" } else { "Enter" }),
        "backspace" => Some(if mac { "⌫" } else { "Backspace" }),
        "delete" => Some(if mac { "⌦" } else { "Delete" }),
        "escape" => Some("Esc"),
        "tab" => Some(if mac { "⇥" } else { "Tab" }),
        "space" => Some("Space"),
        "up" => Some("↑"),
        "down" => Some("↓"),
        "left" => Some("←"),
        "right" => Some("→"),
        "pageup" => Some("PgUp"),
        "pagedown" => Some("PgDn"),
        "home" => Some("Home"),
        "end" => Some("End"),
        _ => None,
    };
    match named {
        Some(n) => n.to_owned(),
        None => key.to_uppercase(),
    }
}

/// Shortcuts that would share keys with this one where both apply.
pub fn clashes(id: &str, keys: &str, cx: &App) -> Vec<&'static str> {
    let Some(me) = SHORTCUTS.iter().find(|s| s.id == id) else {
        return vec![];
    };
    let keys = normalize(keys);
    SHORTCUTS
        .iter()
        .filter(|s| s.id != id && keys_for(s.id, cx).as_deref() == Some(keys.as_str()))
        // ⌘↩ steers while typing and approves elsewhere: that's on purpose.
        .filter(|s| !matches!((me.id, s.id), ("steer", "approve") | ("approve", "steer")))
        // The browser has its own keys while it has focus.
        .filter(|s| (me.context == BROWSER) == (s.context == BROWSER))
        .map(|s| s.label)
        .collect()
}

/// Bind every shortcut to its current keys, replacing what was bound
/// before (other bindings — text fields, the terminal — stay as they are).
pub fn apply(cx: &mut App) {
    let names: Vec<&'static str> = SHORTCUTS.iter().map(|s| (s.action)().name()).collect();
    let kept: Vec<KeyBinding> = cx
        .key_bindings()
        .borrow()
        .bindings()
        .filter(|b| !names.contains(&b.action().name()))
        .cloned()
        .collect();
    let overrides = overrides(cx);
    let mut ours = Vec::new();
    for s in SHORTCUTS {
        let keys = overrides.get(s.id).map_or(s.default_keys(), String::as_str);
        let keys = normalize(keys);
        if keys.is_empty() {
            continue;
        }
        let context = s
            .context
            .and_then(|c| KeyBindingContextPredicate::parse(c).ok())
            .map(Rc::new);
        match KeyBinding::load(&keys, (s.action)(), context, false, None, cx) {
            Ok(binding) => ours.push(binding),
            Err(err) => tracing::warn!(shortcut = s.id, keys, "invalid shortcut: {err}"),
        }
    }
    cx.clear_key_bindings();
    cx.bind_keys(kept);
    cx.bind_keys(ours);
    // The menu bar shows the new keys too.
    cx.set_menus(crate::workspace::app_menus());
}

/// A pressed key as config text (`cmd-shift-k`), or `None` for a lone
/// modifier.
pub fn from_keystroke(k: &gpui_kit::Keystroke, cx: &App) -> Option<String> {
    if matches!(
        k.key.as_str(),
        "shift" | "control" | "alt" | "platform" | "function" | "cmd" | "ctrl" | "fn" | ""
    ) {
        return None;
    }
    Some(k.unparse(cx.platform_info().os))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_keys_the_platform_way() {
        if cfg!(target_os = "macos") {
            assert_eq!(display("secondary-shift-n"), "⇧⌘N");
            assert_eq!(display("cmd-enter"), "⌘↩");
            assert_eq!(display("ctrl-`"), "⌃`");
            assert_eq!(display("cmd--"), "⌘-");
            assert_eq!(display("cmd-k cmd-s"), "⌘K ⌘S");
        } else {
            assert_eq!(display("secondary-shift-n"), "Ctrl+Shift+N");
            assert_eq!(display("secondary-enter"), "Ctrl+Enter");
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<_> = SHORTCUTS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SHORTCUTS.len());
        assert!(SHORTCUTS.iter().all(|s| GROUPS.contains(&s.group)));
    }
}
