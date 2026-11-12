//! Settings on the user's behalf (agents, through the Agentloft MCP
//! server): read them, change one key at a time (only known keys, with
//! their types checked), and add colour themes. The app watches
//! `config.toml` and the themes folder, so changes show at once.

use crate::{Core, CoreResult};
use loft_config::{Paths, SettingValue};
use loft_protocol::{ErrorBody, ErrorCode, Response};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Built-in colour themes (`ui.palette`); custom ones are `custom:<file>`.
pub(crate) const BUILTIN_THEMES: &[&str] = &[
    "loft",
    "studio",
    "lavender",
    "glacier",
    "dusk",
    "harvest",
    "evergreen",
    "neon",
    "slate",
    "nightshade",
];

const AGENTS: &[&str] = &["claude", "codex", "opencode", "gemini"];

/// What a setting holds.
#[derive(Clone, Copy)]
enum Kind {
    /// One of these strings.
    OneOf(&'static [&'static str]),
    /// Any text (fonts, folders, models…).
    Text,
    /// `#rrggbb`.
    Color,
    Number(f64, f64),
    /// A whole number in this range.
    Int(i64, i64),
    Bool,
    TextList,
    /// A table of strings (environment variables).
    TextMap,
}

struct Key {
    key: &'static str,
    kind: Kind,
    about: &'static str,
}

const KEYS: &[Key] = &[
    Key {
        key: "ui.theme",
        kind: Kind::OneOf(&["system", "light", "dark"]),
        about: "Light or dark mode, or follow the system",
    },
    Key {
        key: "ui.palette",
        kind: Kind::Text,
        about: "Colour theme: a built-in id or custom:<name> (see themes)",
    },
    Key {
        key: "ui.accent",
        kind: Kind::Color,
        about: "Accent colour #rrggbb; null = the theme's own",
    },
    Key {
        key: "ui.contrast",
        kind: Kind::OneOf(&["soft", "normal", "high"]),
        about: "How strongly borders and secondary text stand out",
    },
    Key {
        key: "ui.density",
        kind: Kind::OneOf(&["comfortable", "compact"]),
        about: "Spacing in the conversation",
    },
    Key {
        key: "ui.content_width",
        kind: Kind::Number(560., 1200.),
        about: "Max width of the conversation column (px)",
    },
    Key {
        key: "ui.font_size",
        kind: Kind::Number(11., 20.),
        about: "Interface and conversation text size (px)",
    },
    Key {
        key: "ui.mono_font_size",
        kind: Kind::Number(10., 18.),
        about: "Code, diffs and terminal text size (px)",
    },
    Key {
        key: "ui.font_family",
        kind: Kind::Text,
        about: "Interface font (an installed family); null = system",
    },
    Key {
        key: "ui.mono_font_family",
        kind: Kind::Text,
        about: "Monospace font; null = the theme's",
    },
    Key {
        key: "ui.sidebar",
        kind: Kind::OneOf(&["threads", "projects"]),
        about: "Sidebar: one recent list, or grouped by project",
    },
    Key {
        key: "worktrees.mode",
        kind: Kind::OneOf(&["ask", "local", "worktree"]),
        about: "New threads: ask, the checkout, or a worktree each",
    },
    Key {
        key: "worktrees.branch_prefix",
        kind: Kind::Text,
        about: "Worktree branches are <prefix>/<slug>",
    },
    Key {
        key: "worktrees.cleanup",
        kind: Kind::OneOf(&["off", "offer", "auto"]),
        about: "A worktree whose work is already in its base branch: keep it until archive, offer to clean it up, or clean it up automatically",
    },
    Key {
        key: "worktrees.delete_branch",
        kind: Kind::Bool,
        about: "Cleaning up a worktree also deletes its local branch (only once its work is in the base branch)",
    },
    Key {
        key: "worktrees.delete_remote_branch",
        kind: Kind::Bool,
        about: "Cleaning up also deletes the branch on the remote, once its pull request is merged",
    },
    Key {
        key: "worktrees.archive_after_cleanup",
        kind: Kind::Bool,
        about: "Archive a thread once its worktree is cleaned up; false = it stays and gets a new worktree with its next message",
    },
    Key {
        key: "browser.search",
        kind: Kind::Text,
        about: "What the browser's address bar searches with: duckduckgo, google, bing, brave, kagi, ecosia, startpage, or an http(s) address with %s where the words go; null = duckduckgo",
    },
    Key {
        key: "browser.home",
        kind: Kind::Text,
        about: "Address a new browser tab opens; null = a blank tab",
    },
    Key {
        key: "browser.executable",
        kind: Kind::Text,
        about: "Path of the Chromium-family browser run for the web client's browser tab; null = the first of Chrome, Chromium, Edge or Brave installed",
    },
    Key {
        key: "titles.generate",
        kind: Kind::Bool,
        about: "Name new threads with a short title an agent writes after the first message; false = the message's first line",
    },
    Key {
        key: "titles.agent",
        kind: Kind::OneOf(&["thread", "claude", "codex", "opencode", "gemini"]),
        about: "Which agent writes thread titles; \"thread\" = the thread's own",
    },
    Key {
        key: "titles.model",
        kind: Kind::Text,
        about: "Model for thread titles; \"auto\" = haiku for Claude, the agent's default otherwise",
    },
    Key {
        key: "titles.instructions",
        kind: Kind::Text,
        about: "Extra instructions for thread titles (style, language…); null = none",
    },
    Key {
        key: "threads.delete_archived_after_days",
        kind: Kind::Int(0, 3650),
        about: "Delete archived threads (conversation, worktree, checkpoints, attachments) this many days after archiving; 0 = never",
    },
    Key {
        key: "threads.compact_when_idle",
        kind: Kind::Bool,
        about: "Compact a thread's conversation before a message sent after it sat idle (its prompt cache gone); off = never",
    },
    Key {
        key: "threads.compact_idle_minutes",
        kind: Kind::Int(1, 10080),
        about: "How long a thread sits idle before a new message compacts it first (default 60)",
    },
    Key {
        key: "threads.compact_min_tokens",
        kind: Kind::Int(0, 10_000_000),
        about: "Compact when idle only when the context holds at least this many tokens (default 100000)",
    },
    Key {
        key: "threads.resume_at_reset",
        kind: Kind::Bool,
        about: "A thread stopped by its agent's usage limit carries on by itself when the limit resets (default off; each thread can be told to on its own)",
    },
    Key {
        key: "usage.price_catalog",
        kind: Kind::Text,
        about: "Where model prices for cost estimates come from each day (models.dev's format); empty = only the built-in prices and [pricing]",
    },
    Key {
        key: "projects.new_dir",
        kind: Kind::Text,
        about: "Where new projects go; null = ~/.agentloft/projects",
    },
    Key {
        key: "projects.base_dir",
        kind: Kind::Text,
        about: "Where Add Project browses and clones go; null = home",
    },
];

/// Per agent (`harness.<agent>.<field>`).
const AGENT_KEYS: &[Key] = &[
    Key {
        key: "enabled",
        kind: Kind::Bool,
        about: "Shown in pickers",
    },
    Key {
        key: "default_model",
        kind: Kind::Text,
        about: "Model for new threads; \"auto\" = the agent's default",
    },
    Key {
        key: "default_effort",
        kind: Kind::Text,
        about: "Reasoning effort for new threads; \"auto\" = the model's",
    },
    Key {
        key: "default_permission_mode",
        kind: Kind::OneOf(&[
            "harness-default",
            "read-only",
            "ask-all",
            "accept-edits",
            "auto",
            "full-access",
        ]),
        about: "Permissions for new threads",
    },
    Key {
        key: "path",
        kind: Kind::Text,
        about: "Binary; \"auto\" = from PATH",
    },
    Key {
        key: "extra_args",
        kind: Kind::TextList,
        about: "Extra launch arguments",
    },
    Key {
        key: "env",
        kind: Kind::TextMap,
        about: "Environment variables for the agent",
    },
    Key {
        key: "hidden_models",
        kind: Kind::TextList,
        about: "Models left out of the pickers",
    },
];

fn bad(m: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, m)
}

/// The rule for `key`, if it's one agents may set.
/// `ui.keys.<shortcut>`: a keyboard shortcut.
const SHORTCUT_KEY: Key = Key {
    key: "ui.keys.<shortcut>",
    kind: Kind::Text,
    about: "Keys for one shortcut, e.g. \"cmd-shift-t\" (\"secondary\" is cmd on macOS, ctrl elsewhere); \"\" turns it off, null restores the default. Shortcuts: new_thread, new_chat, new_pane, new_window, add_project, command_center, search, settings, quit, steer, stop, approve, approve_session, deny, toggle_sidebar, toggle_panel, toggle_terminal, show_files, show_changes, show_browser, toggle_theme, browser_address, browser_reload, browser_back, browser_forward, browser_stop.",
};

fn rule(key: &str) -> Option<&'static Key> {
    if let Some(k) = KEYS.iter().find(|k| k.key == key) {
        return Some(k);
    }
    if let Some(id) = key.strip_prefix("ui.keys.")
        && !id.is_empty()
        && id.chars().all(|c| c.is_ascii_lowercase() || c == '_')
    {
        return Some(&SHORTCUT_KEY);
    }
    let mut parts = key.splitn(3, '.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("harness"), Some(agent), Some(field)) if AGENTS.contains(&agent) => {
            AGENT_KEYS.iter().find(|k| k.key == field)
        }
        _ => None,
    }
}

fn hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Check `value` against the key's kind; `null` resets to the default.
fn to_setting(key: &Key, value: &Value) -> CoreResult<SettingValue> {
    if value.is_null() {
        return Ok(SettingValue::Unset);
    }
    let name = key.key;
    Ok(match key.kind {
        Kind::OneOf(options) => match value.as_str() {
            Some(s) if options.contains(&s) => SettingValue::Str(s.into()),
            _ => return Err(bad(format!("{name} is one of: {}", options.join(", ")))),
        },
        Kind::Text => match value.as_str() {
            Some(s) => SettingValue::Str(s.into()),
            None => return Err(bad(format!("{name} is text"))),
        },
        Kind::Color => match value.as_str() {
            Some(s) if hex(s) => SettingValue::Str(s.to_ascii_lowercase()),
            _ => return Err(bad(format!("{name} is a #rrggbb colour"))),
        },
        Kind::Number(lo, hi) => match value.as_f64() {
            Some(n) if (lo..=hi).contains(&n) => SettingValue::Float(n),
            _ => return Err(bad(format!("{name} is a number from {lo} to {hi}"))),
        },
        Kind::Int(lo, hi) => match value.as_i64() {
            Some(n) if (lo..=hi).contains(&n) => SettingValue::Int(n),
            _ => return Err(bad(format!("{name} is a whole number from {lo} to {hi}"))),
        },
        Kind::Bool => match value.as_bool() {
            Some(b) => SettingValue::Bool(b),
            None => return Err(bad(format!("{name} is true or false"))),
        },
        Kind::TextList => match value.as_array() {
            Some(a) if a.iter().all(Value::is_string) => SettingValue::List(
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            ),
            _ => return Err(bad(format!("{name} is a list of strings"))),
        },
        Kind::TextMap => match value.as_object() {
            Some(m) if m.values().all(Value::is_string) => SettingValue::Map(
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                    .collect(),
            ),
            _ => return Err(bad(format!("{name} is a table of strings"))),
        },
    })
}

fn kind_text(kind: Kind) -> String {
    match kind {
        Kind::OneOf(o) => format!("one of {}", o.join(" | ")),
        Kind::Text => "text".into(),
        Kind::Color => "#rrggbb".into(),
        Kind::Number(lo, hi) => format!("number {lo}–{hi}"),
        Kind::Int(lo, hi) => format!("whole number {lo}–{hi}"),
        Kind::Bool => "true | false".into(),
        Kind::TextList => "list of text".into(),
        Kind::TextMap => "table of text".into(),
    }
}

/// A theme file as agents write it (checked before it's saved).
#[derive(serde::Deserialize)]
struct ThemeCheck {
    name: String,
    dark: Swatch,
    light: Swatch,
}

#[derive(serde::Deserialize)]
struct Swatch {
    background: String,
    foreground: String,
    accent: String,
    #[serde(flatten)]
    rest: std::collections::BTreeMap<String, toml::Value>,
}

fn theme_slug(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_owned();
    if s.is_empty() { "theme".into() } else { s }
}

impl Core {
    fn home(&self) -> CoreResult<PathBuf> {
        self.opts()
            .config_file
            .as_ref()
            .and_then(|f| f.parent())
            .map(Path::to_path_buf)
            .ok_or_else(|| bad("settings aren't available here"))
    }

    /// Current settings, what can be set, and the themes there are.
    pub(crate) fn get_settings(&self) -> CoreResult<Response> {
        let home = self.home()?;
        let paths = Paths::new(&home);
        let text = std::fs::read_to_string(paths.config_file()).unwrap_or_default();
        let current: Value = toml::from_str::<toml::Value>(&text)
            .ok()
            .and_then(|v| serde_json::to_value(v).ok())
            .unwrap_or(json!({}));
        let mut keys: Vec<Value> = KEYS
            .iter()
            .map(|k| json!({ "key": k.key, "type": kind_text(k.kind), "about": k.about }))
            .collect();
        keys.push(json!({
            "key": SHORTCUT_KEY.key,
            "type": kind_text(SHORTCUT_KEY.kind),
            "about": SHORTCUT_KEY.about,
        }));
        for k in AGENT_KEYS {
            keys.push(json!({
                "key": format!("harness.<{}>.{}", AGENTS.join("|"), k.key),
                "type": kind_text(k.kind),
                "about": k.about,
            }));
        }
        let custom: Vec<Value> = std::fs::read_dir(home.join("themes"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
            .filter_map(|e| {
                let stem = e.path().file_stem()?.to_string_lossy().into_owned();
                let name = std::fs::read_to_string(e.path())
                    .ok()
                    .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
                    .and_then(|v| v.get("name")?.as_str().map(str::to_owned))
                    .unwrap_or_else(|| stem.clone());
                Some(json!({ "id": format!("custom:{stem}"), "name": name }))
            })
            .collect();
        Ok(Response::Json {
            value: json!({
                "config_file": paths.config_file(),
                "current": current,
                "settable_keys": keys,
                "themes": { "built_in": BUILTIN_THEMES, "custom": custom },
                "note": "Unset keys use their defaults. change_setting with null resets one.",
            }),
        })
    }

    /// Set one key (`ui.theme`, `harness.codex.default_model`, …).
    pub(crate) fn change_setting(&self, key: &str, value: &Value) -> CoreResult<Response> {
        let rule = rule(key)
            .ok_or_else(|| bad(format!("`{key}` isn't a setting; get_settings lists them")))?;
        let setting = to_setting(rule, value)?;
        if key == "ui.palette"
            && let SettingValue::Str(id) = &setting
            && !BUILTIN_THEMES.contains(&id.as_str())
        {
            let stem = id.strip_prefix("custom:").unwrap_or(id);
            if !self
                .home()?
                .join("themes")
                .join(format!("{stem}.toml"))
                .exists()
            {
                return Err(bad(format!("no theme `{id}`; get_settings lists them")));
            }
        }
        let home = self.home()?;
        let parts: Vec<&str> = key.split('.').collect();
        loft_config::write_setting(&Paths::new(&home), &parts, setting)
            .map_err(|e| ErrorBody::new(ErrorCode::Internal, format!("{e:#}")))?;
        self.reload_config()?;
        Ok(Response::Text {
            text: format!("{key} = {value}"),
        })
    }

    /// Save a colour theme (TOML, as in ~/.agentloft/themes) and, with
    /// `select`, switch to it.
    pub(crate) fn save_theme(
        &self,
        text: &str,
        select: bool,
        id: Option<&str>,
    ) -> CoreResult<Response> {
        let check: ThemeCheck =
            toml::from_str(text).map_err(|e| bad(format!("not a theme file: {e}")))?;
        if check.name.trim().is_empty() {
            return Err(bad("the theme needs a name"));
        }
        for (variant, s) in [("dark", &check.dark), ("light", &check.light)] {
            for (field, v) in [
                ("background", &s.background),
                ("foreground", &s.foreground),
                ("accent", &s.accent),
            ] {
                if !hex(v) {
                    return Err(bad(format!("[{variant}] {field} = \"{v}\" isn't #rrggbb")));
                }
            }
            for (field, v) in &s.rest {
                if !v.as_str().is_some_and(hex) {
                    return Err(bad(format!("[{variant}] {field} isn't #rrggbb")));
                }
            }
        }
        let dir = self.home()?.join("themes");
        std::fs::create_dir_all(&dir).map_err(crate::internal)?;
        // The same name replaces that theme; a new one gets its own file.
        let stem = match id {
            Some(id) => {
                let stem = id.strip_prefix("custom:").unwrap_or(id);
                // A file name of our own making, or nothing.
                if stem.is_empty() || theme_slug(stem) != stem {
                    return Err(bad(format!("`{id}` isn't a theme id")));
                }
                stem.to_owned()
            }
            None => theme_slug(&check.name),
        };
        std::fs::write(dir.join(format!("{stem}.toml")), text).map_err(crate::internal)?;
        let id = format!("custom:{stem}");
        if select {
            self.change_setting("ui.palette", &json!(id))?;
        }
        Ok(Response::Text {
            text: if select {
                format!("Saved and switched to {id}")
            } else {
                format!("Saved as {id}")
            },
        })
    }
}

/// Largest piece of UI state a client may store.
const MAX_UI_STATE: usize = 256 * 1024;
/// Themes sent to a client at once.
const MAX_THEMES: usize = 200;

fn stamp_of(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => format!(
            "{}:{}",
            meta.len(),
            loft_protocol::file_mtime_ms(path).unwrap_or(0)
        ),
        Err(_) => "-".into(),
    }
}

/// `config.toml` as a remote session may see it: agents' environment
/// values (often API keys) replaced, web access left out. Text that doesn't
/// parse is withheld altogether.
pub(crate) fn redact_config(text: &str) -> String {
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return String::new();
    };
    doc.remove("web");
    if let Some(agents) = doc.get_mut("harness").and_then(|h| h.as_table_like_mut()) {
        for (_, agent) in agents.iter_mut() {
            let Some(env) = agent
                .as_table_like_mut()
                .and_then(|a| a.get_mut("env"))
                .and_then(|e| e.as_table_like_mut())
            else {
                continue;
            };
            for (_, value) in env.iter_mut() {
                *value = toml_edit::value("•••");
            }
        }
    }
    doc.to_string()
}

impl Core {
    /// The settings files behind the UI (`Request::UiFiles`); `None` when
    /// they still match `known`.
    pub(crate) fn ui_files(
        &self,
        known: Option<&str>,
        remote: bool,
    ) -> CoreResult<Option<loft_protocol::UiFiles>> {
        let home = self.home()?;
        let paths = Paths::new(&home);
        let mut themes: Vec<PathBuf> = std::fs::read_dir(paths.themes_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .take(MAX_THEMES)
            .collect();
        themes.sort();
        let sidebar = home.join("ui-sidebar.json");
        let mut stamp = format!("{}|{}", stamp_of(&paths.config_file()), stamp_of(&sidebar));
        for theme in &themes {
            stamp.push_str(&format!(
                "|{}={}",
                theme.file_name().unwrap_or_default().to_string_lossy(),
                stamp_of(theme)
            ));
        }
        if known == Some(stamp.as_str()) {
            return Ok(None);
        }
        let config = std::fs::read_to_string(paths.config_file()).unwrap_or_default();
        Ok(Some(loft_protocol::UiFiles {
            stamp,
            config_toml: if remote {
                redact_config(&config)
            } else {
                config
            },
            themes: themes
                .iter()
                .filter_map(|p| {
                    Some(loft_protocol::UiFile {
                        name: p.file_stem()?.to_string_lossy().into_owned(),
                        text: std::fs::read_to_string(p).ok()?,
                    })
                })
                .collect(),
            sidebar: std::fs::read_to_string(&sidebar).ok(),
            user_home: std::env::home_dir()
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_default(),
            home: home.to_string_lossy().into_owned(),
        }))
    }

    /// Save shared UI state (`Request::PutUiState`).
    pub(crate) fn put_ui_state(&self, name: &str, json: &str) -> CoreResult<Response> {
        let file = match name {
            "sidebar" => self.home()?.join("ui-sidebar.json"),
            _ => return Err(bad(format!("no UI state named `{name}`"))),
        };
        if json.len() > MAX_UI_STATE {
            return Err(bad("that's too much UI state"));
        }
        serde_json::from_str::<serde_json::Value>(json)
            .map_err(|e| bad(format!("not JSON: {e}")))?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .and_then(|()| std::fs::rename(&tmp, &file))
            .map_err(crate::internal)?;
        Ok(Response::Ok)
    }

    /// Delete a colour theme the user added (`Request::DeleteTheme`).
    pub(crate) fn delete_theme(&self, id: &str) -> CoreResult<Response> {
        let stem = id.strip_prefix("custom:").unwrap_or(id);
        // A file name of our own making: nothing that could leave the folder.
        if stem.is_empty() || theme_slug(stem) != stem {
            return Err(bad(format!("no theme `{id}`")));
        }
        let file = self.home()?.join("themes").join(format!("{stem}.toml"));
        match std::fs::remove_file(&file) {
            Ok(()) => Ok(Response::Ok),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("no theme `{id}`"),
            )),
            Err(e) => Err(crate::internal(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_redacted_for_remote_sessions() {
        let text = "# mine\n[ui]\ntheme = \"dark\"\n\n[harness.claude]\npath = \"/opt/claude\"\nenv = { ANTHROPIC_API_KEY = \"sk-secret\", DEBUG = \"1\" }\n\n[harness.codex.env]\nOPENAI_API_KEY = \"sk-other\"\n\n[web]\nenabled = true\ntls_key = \"/etc/key.pem\"\n";
        let out = redact_config(text);
        assert!(
            !out.contains("sk-secret") && !out.contains("sk-other"),
            "{out}"
        );
        assert!(out.contains("ANTHROPIC_API_KEY") && out.contains("OPENAI_API_KEY"));
        assert!(!out.contains("[web]") && !out.contains("key.pem"));
        // The rest is as the user wrote it, and still parses.
        assert!(out.contains("# mine") && out.contains("theme = \"dark\""));
        let config: loft_config::Config = toml::from_str(&out).unwrap();
        assert_eq!(config.harness.claude.env["DEBUG"], "•••");
        assert!(!config.web.enabled);
        // Unparseable text is withheld, not passed along.
        assert_eq!(
            redact_config("[harness.claude\nenv = { K = \"sk-secret\" }"),
            ""
        );
    }

    #[test]
    fn only_known_keys_with_the_right_values() {
        assert!(rule("ui.theme").is_some());
        assert!(rule("harness.codex.default_model").is_some());
        assert!(rule("harness.nope.default_model").is_none());
        assert!(rule("ui.secret").is_none());
        assert!(rule("ui.keys.new_thread").is_some());
        assert!(rule("ui.keys.").is_none());
        assert!(rule("ui.keys.a.b").is_none());
        let theme = rule("ui.theme").unwrap();
        assert!(to_setting(theme, &json!("dark")).is_ok());
        assert!(to_setting(theme, &json!("purple")).is_err());
        assert!(matches!(
            to_setting(theme, &Value::Null),
            Ok(SettingValue::Unset)
        ));
        let accent = rule("ui.accent").unwrap();
        assert!(to_setting(accent, &json!("#7C5CFF")).is_ok());
        assert!(to_setting(accent, &json!("violet")).is_err());
        let size = rule("ui.font_size").unwrap();
        assert!(to_setting(size, &json!(15)).is_ok());
        assert!(to_setting(size, &json!(90)).is_err());
    }
}
