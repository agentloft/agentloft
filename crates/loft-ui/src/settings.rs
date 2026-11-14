//! Settings window (⌘,): appearance, agents, worktrees, about. Every change
//! goes straight into `config.toml` (comments and formatting kept) and is
//! applied live: appearance in the app, harness/worktree settings in the
//! daemon (`Request::ReloadConfig`).

use crate::store::AppStore;
use crate::theme::{self, Palette};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, FontWeight, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, point, prelude::FluentBuilder as _, px, size,
};
use loft_config::{Config, SettingValue};
use loft_protocol::Request;

mod agents;
mod appearance;
mod browser;
mod computers;
mod projects;
mod shortcuts;
mod threads;
mod usage;
mod web;
mod worktrees;

struct SettingsWindow(AnyWindowHandle, gpui_kit::WeakEntity<SettingsView>);
impl Global for SettingsWindow {}

/// Settings drawn inside the main window: where there is only one window
/// (Agentloft in a browser tab).
struct SettingsOverlay(Entity<SettingsView>);
impl Global for SettingsOverlay {}

thread_local! {
    /// `AGENTLOFT_DEV_TAB=settings:<section>@end`: the page opens scrolled
    /// to its end (for screenshots of what is below the fold), for this
    /// many more frames while what it lists comes in.
    static DEV_SCROLL_END: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The settings page scrolls to its end as it opens (a dev hook).
pub fn dev_scroll_to_end() {
    DEV_SCROLL_END.set(300);
}

/// Open the settings window (optionally at a section), or bring the open
/// one to the front.
pub fn open(store: Entity<AppStore>, section: Option<Section>, cx: &mut App) {
    // Settings are this computer's, even from a window on another one.
    let store = if store.read(cx).remote().is_some() {
        crate::windows::local_store(cx).unwrap_or(store)
    } else {
        store
    };
    if crate::host::IS_WEB {
        open_in_window(store, section, cx);
        return;
    }
    if let Some(SettingsWindow(handle, view)) = cx
        .try_global::<SettingsWindow>()
        .map(|w| SettingsWindow(w.0, w.1.clone()))
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        if let Some(section) = section {
            let _ = view.update(cx, |v, cx| {
                v.section = section;
                cx.notify();
            });
        }
        return;
    }
    let bounds = Bounds::centered(None, size(px(860.), px(620.)), cx);
    let opened = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(640.), px(420.))),
            titlebar: Some(TitlebarOptions {
                title: Some("Settings".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.), px(18.))),
            }),
            ..Default::default()
        },
        cx,
        move |window, cx| {
            cx.new(|cx| {
                let mut view = SettingsView::new(store.clone(), window, cx);
                if let Some(section) = section {
                    view.section = section;
                }
                view
            })
        },
    );
    match opened {
        Ok((handle, view)) => cx.set_global(SettingsWindow(handle, view.downgrade())),
        Err(err) => tracing::error!("open settings: {err:#}"),
    }
}

/// Settings over the main window's content (see [`overlay`]).
fn open_in_window(store: Entity<AppStore>, section: Option<Section>, cx: &mut App) {
    if let Some(view) = cx.try_global::<SettingsOverlay>().map(|o| o.0.clone()) {
        if let Some(section) = section {
            view.update(cx, |v, cx| {
                v.section = section;
                cx.notify();
            });
        }
        return;
    }
    let Some(handle) = cx.active_window().or_else(|| cx.windows().first().copied()) else {
        return;
    };
    // Deferred: this is usually called from inside that window's own event
    // handling, where it can't be borrowed again.
    cx.defer(move |cx| {
        let _ = handle.update(cx, |_, window, cx| {
            let view = cx.new(|cx| {
                let mut view = SettingsView::new(store.clone(), window, cx);
                if let Some(section) = section {
                    view.section = section;
                }
                view
            });
            window.focus(&view.read(cx).focus.clone(), cx);
            cx.set_global(SettingsWindow(handle, view.downgrade()));
            cx.set_global(SettingsOverlay(view));
            window.refresh();
        });
    });
}

/// Close in-window settings.
pub fn close(cx: &mut App) {
    if cx.try_global::<SettingsOverlay>().is_some() {
        cx.remove_global::<SettingsOverlay>();
        cx.refresh_windows();
    }
}

/// In-window settings, for the main window's root view to draw over
/// everything else (`None` when closed, and always on the desktop, where
/// settings have a window of their own).
pub fn overlay(cx: &App) -> Option<AnyElement> {
    let view = cx.try_global::<SettingsOverlay>()?.0.clone();
    Some(
        div()
            .absolute()
            .inset_0()
            .occlude()
            .child(view)
            .into_any_element(),
    )
}

/// Re-read `config.toml` into an open settings window (changed elsewhere).
pub fn refresh(cx: &mut App) {
    if let Some(view) = cx.try_global::<SettingsWindow>().map(|w| w.1.clone()) {
        let _ = view.update(cx, |v, cx| {
            let paths = v.store.read(cx).paths().clone();
            if let Ok(c) = crate::host::load_config(&paths) {
                v.config = c;
                v.studio.themes = crate::themes::all(&paths.home);
            }
            cx.notify();
        });
    }
}

/// Open the settings window on one project's page.
pub fn open_project(store: Entity<AppStore>, project: loft_protocol::ProjectId, cx: &mut App) {
    open(store, Some(Section::Projects), cx);
    if let Some(view) = cx.try_global::<SettingsWindow>().map(|w| w.1.clone()) {
        let _ = view.update(cx, |v, cx| {
            v.open_project(project);
            cx.notify();
        });
    }
}

/// Open the settings window on one agent's page.
pub fn open_agent(store: Entity<AppStore>, harness: &str, cx: &mut App) {
    open(store, Some(Section::Agents), cx);
    let Some(agent) = agents::AGENTS.iter().find(|(id, _)| *id == harness) else {
        return;
    };
    if let Some(view) = cx.try_global::<SettingsWindow>().map(|w| w.1.clone()) {
        let _ = view.update(cx, |v, cx| {
            v.agents.selected = agent.0;
            cx.notify();
        });
    }
}

/// The agents the settings window has a page for: (harness id, name).
pub fn agents() -> &'static [(&'static str, &'static str)] {
    &agents::AGENTS
}

/// A setting the command palette can find.
pub struct Findable {
    pub label: &'static str,
    /// Extra words it answers to.
    pub keywords: &'static str,
    pub section: Section,
}

/// Every setting, by the name its row shows (for the command palette).
pub const FINDABLE: &[Findable] = &[
    Findable {
        label: "Light, dark or system mode",
        keywords: "appearance theme mode light dark system",
        section: Section::Appearance,
    },
    Findable {
        label: "Theme",
        keywords: "appearance themes colours colors palette create generate edit",
        section: Section::Appearance,
    },
    Findable {
        label: "Accent colour",
        keywords: "appearance accent color highlight buttons links",
        section: Section::Appearance,
    },
    Findable {
        label: "Contrast",
        keywords: "appearance borders secondary text",
        section: Section::Appearance,
    },
    Findable {
        label: "Density",
        keywords: "appearance spacing compact comfortable",
        section: Section::Appearance,
    },
    Findable {
        label: "Conversation width",
        keywords: "appearance column width",
        section: Section::Appearance,
    },
    Findable {
        label: "Text size",
        keywords: "appearance font size zoom",
        section: Section::Appearance,
    },
    Findable {
        label: "Code text size",
        keywords: "appearance font size code terminal diff",
        section: Section::Appearance,
    },
    Findable {
        label: "Interface font",
        keywords: "appearance font family typeface",
        section: Section::Appearance,
    },
    Findable {
        label: "Monospace font",
        keywords: "appearance font family code terminal",
        section: Section::Appearance,
    },
    Findable {
        label: "Agents",
        keywords: "claude codex opencode gemini installed binary model effort permissions environment",
        section: Section::Agents,
    },
    Findable {
        label: "New projects folder",
        keywords: "projects add clone new folder directory location base where",
        section: Section::Projects,
    },
    Findable {
        label: "Rename, pin or remove a project",
        keywords: "projects delete remove rename pin",
        section: Section::Projects,
    },
    Findable {
        label: "Generate thread titles",
        keywords: "threads titles names rename auto generate summary",
        section: Section::Threads,
    },
    Findable {
        label: "Title agent and model",
        keywords: "threads titles agent model haiku claude codex",
        section: Section::Threads,
    },
    Findable {
        label: "Title instructions",
        keywords: "threads titles instructions prompt language style",
        section: Section::Threads,
    },
    Findable {
        label: "New threads: worktree or local",
        keywords: "worktrees ask local",
        section: Section::Worktrees,
    },
    Findable {
        label: "Branch prefix",
        keywords: "worktrees git branch name",
        section: Section::Worktrees,
    },
    Findable {
        label: "Worktree location",
        keywords: "worktrees folder directory where",
        section: Section::Worktrees,
    },
    Findable {
        label: "Per-project setup",
        keywords: "worktrees setup script copy config.toml",
        section: Section::Worktrees,
    },
    Findable {
        label: "Search engine",
        keywords: "browser search engine google duckduckgo bing brave kagi ecosia startpage address bar default",
        section: Section::Browser,
    },
    Findable {
        label: "Home page",
        keywords: "browser home page new tab start address",
        section: Section::Browser,
    },
    Findable {
        label: "Browser app",
        keywords: "browser chrome chromium edge brave executable path web client",
        section: Section::Browser,
    },
    Findable {
        label: "Keyboard shortcuts",
        keywords: "shortcuts hotkeys keys keybindings keymap rebind steer",
        section: Section::Shortcuts,
    },
    Findable {
        label: "Settings file",
        keywords: "about config.toml edit",
        section: Section::About,
    },
    Findable {
        label: "Data folder",
        keywords: "about threads events logs storage",
        section: Section::About,
    },
    Findable {
        label: "Version",
        keywords: "about agentloft update",
        section: Section::About,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Appearance,
    Agents,
    /// How threads are named.
    Threads,
    Projects,
    Worktrees,
    /// The browser panel: search engine, home page, which browser.
    Browser,
    Shortcuts,
    /// Tokens and cost across threads, and the agents' plan limits.
    Usage,
    /// Agentloft in a browser (desktop only: a browser session may not
    /// manage its own access).
    Web,
    /// Agentloft on other computers, over SSH (desktop only).
    Computers,
    About,
}

impl Section {
    pub fn parse(s: &str) -> Option<Self> {
        Self::all()
            .into_iter()
            .find(|sec| sec.label().eq_ignore_ascii_case(s) || sec.slug() == s)
    }

    const ALL: [Section; 11] = [
        Section::Appearance,
        Section::Agents,
        Section::Threads,
        Section::Projects,
        Section::Worktrees,
        Section::Browser,
        Section::Shortcuts,
        Section::Usage,
        Section::Web,
        Section::Computers,
        Section::About,
    ];

    /// The sections this build has.
    fn all() -> Vec<Section> {
        Self::ALL
            .into_iter()
            .filter(|section| {
                !(crate::host::IS_WEB && matches!(section, Section::Web | Section::Computers))
            })
            .collect()
    }

    /// One word, for links and the dev hook (`settings:web`).
    fn slug(self) -> &'static str {
        match self {
            Section::Appearance => "appearance",
            Section::Agents => "agents",
            Section::Threads => "threads",
            Section::Projects => "projects",
            Section::Worktrees => "worktrees",
            Section::Browser => "browser",
            Section::Shortcuts => "shortcuts",
            Section::Usage => "usage",
            Section::Web => "web",
            Section::Computers => "computers",
            Section::About => "about",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Agents => "Agents",
            Section::Threads => "Threads",
            Section::Projects => "Projects",
            Section::Worktrees => "Worktrees",
            Section::Browser => "Browser",
            Section::Shortcuts => "Shortcuts",
            Section::Usage => "Usage",
            Section::Web => "Web access",
            Section::Computers => "Other computers",
            Section::About => "About",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            Section::Appearance => IconName::Sun,
            Section::Agents => IconName::Bot,
            Section::Threads => IconName::MessageSquare,
            Section::Projects => IconName::FolderOpen,
            Section::Worktrees => IconName::GitBranch,
            Section::Browser => IconName::Globe,
            Section::Shortcuts => IconName::Keyboard,
            Section::Usage => IconName::ChartPie,
            Section::Web => IconName::GlobeLock,
            Section::Computers => IconName::Server,
            Section::About => IconName::Sparkles,
        }
    }
}

/// Text fields, saved on Enter or blur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Field {
    UiFont,
    MonoFont,
    BranchPrefix,
    ProjectsDir,
    NewProjectsDir,
    WebBind,
    WebPort,
    WebCert,
    WebKey,
    WebHosts,
    /// A search address of the user's own (`browser.search`).
    BrowserSearch,
    BrowserHome,
    BrowserApp,
    TitleModel,
    TitleInstructions,
    /// Custom days before archived threads are deleted.
    ArchiveDays,
}

impl Field {
    fn key(self) -> &'static [&'static str] {
        match self {
            Field::UiFont => &["ui", "font_family"],
            Field::MonoFont => &["ui", "mono_font_family"],
            Field::BranchPrefix => &["worktrees", "branch_prefix"],
            Field::ProjectsDir => &["projects", "base_dir"],
            Field::NewProjectsDir => &["projects", "new_dir"],
            Field::WebBind => &["web", "bind"],
            Field::WebPort => &["web", "port"],
            Field::WebCert => &["web", "tls_cert"],
            Field::WebKey => &["web", "tls_key"],
            Field::WebHosts => &["web", "allowed_hosts"],
            Field::BrowserSearch => &["browser", "search"],
            Field::BrowserHome => &["browser", "home"],
            Field::BrowserApp => &["browser", "executable"],
            Field::TitleModel => &["titles", "model"],
            Field::TitleInstructions => &["titles", "instructions"],
            Field::ArchiveDays => &["threads", "delete_archived_after_days"],
        }
    }

    fn current(self, c: &Config) -> String {
        match self {
            Field::UiFont => c.ui.font_family.clone().unwrap_or_default(),
            Field::MonoFont => c.ui.mono_font_family.clone().unwrap_or_default(),
            Field::BranchPrefix => c.worktrees.branch_prefix.clone(),
            Field::ProjectsDir => c.projects.base_dir.clone().unwrap_or_default(),
            Field::NewProjectsDir => c.projects.new_dir.clone().unwrap_or_default(),
            Field::WebBind => c.web.bind.clone(),
            Field::WebPort => c.web.port.to_string(),
            Field::WebCert => c.web.tls_cert.clone().unwrap_or_default(),
            Field::WebKey => c.web.tls_key.clone().unwrap_or_default(),
            Field::WebHosts => c.web.allowed_hosts.join(", "),
            // Only an address of one's own is typed; an engine picked by
            // name leaves the field empty.
            Field::BrowserSearch => match c.browser.search_engine() {
                Some(_) => String::new(),
                None => c.browser.search.clone().unwrap_or_default(),
            },
            Field::BrowserHome => c.browser.home.clone().unwrap_or_default(),
            Field::BrowserApp => c.browser.executable.clone().unwrap_or_default(),
            Field::TitleModel => c.titles.model.clone(),
            Field::TitleInstructions => c.titles.instructions.clone(),
            Field::ArchiveDays => c.threads.delete_archived_after_days.to_string(),
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Field::UiFont => "System font",
            Field::MonoFont => "Theme monospace font",
            Field::BranchPrefix => "agentloft",
            Field::ProjectsDir => "~/",
            Field::NewProjectsDir => "~/.agentloft/projects",
            Field::WebBind => "127.0.0.1",
            Field::WebPort => "4780",
            Field::WebCert => "None",
            Field::WebKey => "None",
            Field::WebHosts => "None",
            Field::BrowserSearch => "https://example.com/search?q=%s",
            Field::BrowserHome => "A blank tab",
            Field::BrowserApp => "Chrome, Chromium, Edge or Brave",
            Field::TitleModel => "Automatic",
            Field::TitleInstructions => "None",
            Field::ArchiveDays => "Days",
        }
    }

    /// What the field holds when nothing was set (shown as its placeholder
    /// rather than typed in).
    fn default_text(self) -> &'static str {
        match self {
            Field::WebBind => "127.0.0.1",
            Field::WebPort => "4780",
            Field::TitleModel => "auto",
            Field::ArchiveDays => "0",
            _ => "",
        }
    }

    /// Empty input → back to the default.
    fn value(self, text: &str) -> SettingValue {
        let text = text.trim();
        match (self, text.is_empty()) {
            (
                Field::UiFont | Field::MonoFont | Field::ProjectsDir | Field::NewProjectsDir,
                true,
            ) => SettingValue::Unset,
            (Field::BranchPrefix, true) => SettingValue::Str("agentloft".into()),
            (Field::WebBind | Field::WebPort | Field::WebCert | Field::WebKey, true) => {
                SettingValue::Unset
            }
            (Field::BrowserSearch | Field::BrowserHome | Field::BrowserApp, true) => {
                SettingValue::Unset
            }
            (Field::TitleModel | Field::TitleInstructions, true) => SettingValue::Unset,
            // Empty or 0: never. Not a number: checked in `save_field`.
            (Field::ArchiveDays, _) => match text.parse::<u32>() {
                Ok(days) if days > 0 => SettingValue::Int(i64::from(days)),
                _ => SettingValue::Unset,
            },
            // Not a port: left as it was (the field shows the old value again).
            (Field::WebPort, false) => match text.parse::<u16>() {
                Ok(port) => SettingValue::Int(i64::from(port)),
                Err(_) => SettingValue::Unset,
            },
            (Field::WebHosts, _) => {
                let names: Vec<String> = text
                    .split([',', ' '])
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect();
                if names.is_empty() {
                    SettingValue::Unset
                } else {
                    SettingValue::List(names)
                }
            }
            _ => SettingValue::Str(text.to_owned()),
        }
    }

    /// Settings the daemon reads (it reloads on change).
    fn is_daemon(self) -> bool {
        !matches!(
            self,
            Field::UiFont
                | Field::MonoFont
                | Field::ProjectsDir
                | Field::NewProjectsDir
                | Field::BrowserSearch
                | Field::BrowserHome
        )
    }
}

pub struct SettingsView {
    focus: gpui_kit::FocusHandle,
    store: Entity<AppStore>,
    section: Section,
    config: Config,
    inputs: Vec<(Field, Entity<InputState>)>,
    error: Option<String>,
    studio: appearance::Studio,
    agents: agents::AgentsPage,
    projects: projects::ProjectsPage,
    shortcuts: shortcuts::ShortcutsPage,
    web: web::WebPage,
    computers: computers::ComputersPage,
    browser: browser::BrowserPage,
    usage: usage::UsagePage,
    /// "Custom" is picked for deleting archived threads (its days field
    /// shows even while the value is a preset or never).
    archive_custom: bool,
    /// The worktree leftovers were asked for (once per opening).
    leftovers_asked: bool,
    /// The page's scroll position.
    scroll: gpui_kit::ScrollHandle,
    _subs: Vec<Subscription>,
}

const ACCENTS: [(&str, Option<&str>); 8] = [
    ("Default", None),
    ("Violet", Some("#7c5cff")),
    ("Blue", Some("#3b82f6")),
    ("Teal", Some("#14b8a6")),
    ("Green", Some("#22c55e")),
    ("Amber", Some("#f59e0b")),
    ("Red", Some("#ef4444")),
    ("Pink", Some("#ec4899")),
];

impl SettingsView {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let paths = store.read(cx).paths().clone();
        let (config, error) = match crate::host::load_config(&paths) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(format!("{e:#}"))),
        };
        let mut inputs = Vec::new();
        let mut subs = Vec::new();
        for field in [
            Field::UiFont,
            Field::MonoFont,
            Field::BranchPrefix,
            Field::ProjectsDir,
            Field::NewProjectsDir,
            Field::WebBind,
            Field::WebPort,
            Field::WebCert,
            Field::WebKey,
            Field::WebHosts,
            Field::BrowserSearch,
            Field::BrowserHome,
            Field::BrowserApp,
            Field::TitleModel,
            Field::TitleInstructions,
            Field::ArchiveDays,
        ] {
            let current = field.current(&config);
            let input = cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(field.placeholder());
                // Defaults read better as the placeholder.
                if current != "auto" && current != field.default_text() {
                    s.set_value(current, window, cx);
                }
                s
            });
            subs.push(
                cx.subscribe_in(&input, window, move |this, state, event, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        let text = state.read(cx).value().to_string();
                        this.save_field(field, &text, cx);
                    }
                }),
            );
            inputs.push((field, input));
        }
        let studio = appearance::Studio::new(&paths.home);
        Self {
            focus: cx.focus_handle(),
            store,
            section: Section::Appearance,
            config,
            inputs,
            error,
            studio,
            agents: agents::AgentsPage::new(),
            projects: projects::ProjectsPage::default(),
            shortcuts: shortcuts::ShortcutsPage::default(),
            web: web::WebPage::default(),
            computers: Default::default(),
            browser: browser::BrowserPage::default(),
            usage: usage::UsagePage::default(),
            archive_custom: false,
            leftovers_asked: false,
            scroll: gpui_kit::ScrollHandle::new(),
            _subs: subs,
        }
    }

    pub(super) fn input(&self, field: Field) -> Entity<InputState> {
        self.inputs
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, i)| i.clone())
            .expect("all fields have inputs")
    }

    fn save_field(&mut self, field: Field, text: &str, cx: &mut Context<Self>) {
        // A search address has to have a place for the words; one that
        // doesn't is said so, not saved.
        if field == Field::BrowserSearch
            && !text.trim().is_empty()
            && loft_config::custom_search_template(text).is_none()
        {
            self.error = Some(
                "A search address starts with https:// and has %s where the words go, like https://example.com/search?q=%s"
                    .into(),
            );
            cx.notify();
            return;
        }
        if field == Field::ArchiveDays
            && !text.trim().is_empty()
            && !text.trim().parse::<u32>().is_ok_and(|days| days <= 3650)
        {
            self.error = Some("Days is a whole number from 0 to 3650, like 14.".into());
            cx.notify();
            return;
        }
        let value = field.value(text);
        let unchanged = match &value {
            SettingValue::Str(s) => *s == field.current(&self.config),
            SettingValue::Int(n) => n.to_string() == field.current(&self.config),
            SettingValue::Unset => {
                let current = field.current(&self.config);
                current.is_empty() || current == field.default_text()
            }
            _ => false,
        };
        if !unchanged {
            self.write(field.key(), value, field.is_daemon(), cx);
        }
    }

    /// Write one setting, reload, and apply it.
    fn write(&mut self, key: &[&str], value: SettingValue, daemon: bool, cx: &mut Context<Self>) {
        let paths = self.store.read(cx).paths().clone();
        if let Err(err) = crate::host::write_setting(&paths, key, value) {
            self.error = Some(format!("Couldn't save: {err:#}"));
            cx.notify();
            return;
        }
        match crate::host::load_config(&paths) {
            Ok(c) => {
                self.config = c;
                self.error = None;
                self.store.update(cx, |s, cx| s.reload_agent_prefs(cx));
            }
            Err(err) => self.error = Some(format!("{err:#}")),
        }
        theme::reload_if_changed(cx);
        if daemon && let Some(client) = self.store.read(cx).client() {
            let fut = client.request(Request::ReloadConfig);
            let store = self.store.clone();
            cx.spawn(async move |this, cx| {
                let result = fut.await;
                let _ = this.update(cx, |this, cx| {
                    match result {
                        Err(err) => this.error = Some(err.message),
                        // A new binary path: re-check what's installed.
                        Ok(_) => store.update(cx, |s, cx| s.probe_harnesses(cx)),
                    }
                    // The daemon has applied `[web]` by now.
                    if this.section == Section::Web {
                        this.refresh_web(cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn set_str(&mut self, key: &[&str], value: &str, daemon: bool, cx: &mut Context<Self>) {
        self.write(key, SettingValue::Str(value.to_owned()), daemon, cx);
    }

    // -- sections -----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn stepper(
        &self,
        key: &'static str,
        value: f32,
        min: f32,
        max: f32,
        step: f32,
        unit: &str,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let button = |id: &'static str, icon: IconName, delta: f32, enabled: bool| {
            div()
                .id(SharedString::from(format!("{key}-{id}")))
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .when(enabled, |d| {
                    d.cursor_pointer()
                        .hover(|d| d.bg(p.hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let next = ((value + delta) / step).round() * step;
                            let next = next.clamp(min, max) as f64;
                            this.write(&["ui", key], SettingValue::Float(next), false, cx);
                        }))
                })
                .when(!enabled, |d| d.opacity(0.35))
                .child(Icon::new(icon).xsmall())
        };
        let shown = if value.fract() == 0. {
            format!("{value:.0} {unit}")
        } else {
            format!("{value:.1} {unit}")
        };
        h_flex()
            .gap_1()
            .items_center()
            .p(px(2.))
            .rounded(px(8.))
            .border_1()
            .border_color(p.border)
            .child(button("dec", IconName::Minus, -step, value > min))
            .child(div().min_w(px(64.)).text_center().text_sm().child(shown))
            .child(button("inc", IconName::Plus, step, value < max))
            .into_any_element()
    }

    fn worktrees(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mode = segmented(
            "wt-mode",
            &[("ask", "Ask"), ("local", "Local"), ("worktree", "Worktree")],
            &self.config.worktrees.mode,
            p,
            cx.listener(|this, value: &&str, _, cx| {
                this.set_str(&["worktrees", "mode"], value, true, cx)
            }),
        );
        let dir = self.store.read(cx).paths().worktrees_dir();
        v_flex()
            .child(row(
                "New threads",
                "Ask: pick per thread (local preselected). Worktree: every new thread gets its own git worktree.",
                mode,
                p,
            ))
            .child(row(
                "Branch prefix",
                "Worktree branches are named <prefix>/<slug>.",
                text_input(&self.input(Field::BranchPrefix)),
                p,
            ))
            .child(row(
                "Location",
                "Where worktrees are created.",
                path_link("wt-dir", dir, p, cx),
                p,
            ))
            .child(row(
                "Per-project setup",
                "Commit .agentloft/config.toml to a repo: [worktree] setup = \"npm ci\" runs before the agent starts; copy = [\".env*\"] lists untracked files to bring along.",
                div()
                    .text_xs()
                    .font_family(p.mono.clone())
                    .text_color(p.muted_foreground)
                    .child(".agentloft/config.toml")
                    .into_any_element(),
                p,
            ))
            .child(self.worktree_cleanup(p, cx))
            .child(self.worktree_leftovers(p, cx))
            .into_any_element()
    }

    fn about(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let paths = self.store.read(cx).paths().clone();
        let config_file = paths.config_file();
        v_flex()
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .py_4()
                    .child(crate::brand::app_icon(px(64.)))
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Agentloft"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(p.muted_foreground)
                                    .child(format!("Version {}", crate::brand::VERSION)),
                            ),
                    ),
            )
            .child(row(
                "Settings file",
                "Everything here is stored in config.toml; edit it directly if you prefer.",
                path_link("cfg", config_file, p, cx),
                p,
            ))
            .child(row(
                "Data",
                "Threads, events and logs.",
                path_link("home", paths.home.clone(), p, cx),
                p,
            ))
            .into_any_element()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        // Laid out once first, so the end is known; then again until the
        // page has settled (lists fill in as answers come).
        let frames = DEV_SCROLL_END.get();
        if frames > 0 {
            DEV_SCROLL_END.set(frames - 1);
            if self.scroll.max_offset().y > px(0.) {
                self.scroll.scroll_to_bottom();
            }
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
        let mut nav = v_flex().gap_0p5().px_2();
        for section in Section::all() {
            let active = section == self.section;
            nav = nav.child(
                h_flex()
                    .id(section.label())
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py(px(6.))
                    .rounded(px(7.))
                    .text_sm()
                    .cursor_pointer()
                    .when(active, |d| d.bg(p.selected).text_color(p.foreground))
                    .when(!active, |d| {
                        d.text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    })
                    .child(Icon::new(section.icon()).small())
                    .child(section.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        cx.notify();
                    })),
            );
        }
        let body = match self.section {
            Section::Appearance => self.appearance(&p, window, cx),
            Section::Agents => self.agents_page(&p, window, cx),
            Section::Threads => self.threads_page(&p, cx),
            Section::Projects => self.projects_page(&p, window, cx),
            Section::Worktrees => self.worktrees(&p, cx),
            Section::Browser => self.browser_page(&p, cx),
            Section::Shortcuts => self.shortcuts_page(&p, cx),
            Section::Usage => self.usage_page(&p, window, cx),
            Section::Web => self.web_page(&p, cx),
            Section::Computers => self.computers_page(&p, window, cx),
            Section::About => self.about(&p, cx),
        };
        // In a window of its own the title bar closes it; drawn inside the
        // main window (the browser) it brings its own way out.
        let in_window = crate::host::IS_WEB;
        h_flex()
            .size_full()
            .bg(p.background)
            .text_color(p.foreground)
            .track_focus(&self.focus)
            .when(in_window, |d| {
                d.on_key_down(|event: &gpui_kit::KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        close(cx);
                    }
                })
            })
            .child(
                v_flex()
                    .w(px(200.))
                    .h_full()
                    .flex_shrink_0()
                    .bg(p.sidebar)
                    .border_r_1()
                    .border_color(p.border)
                    .when(!in_window, |d| d.pt(px(52.)))
                    .when(in_window, |d| {
                        d.child(
                            h_flex()
                                .h(px(52.))
                                .flex_shrink_0()
                                .px_2()
                                .items_center()
                                .child(
                                    h_flex()
                                        .id("settings-close")
                                        .gap_1p5()
                                        .items_center()
                                        .px_2()
                                        .py(px(5.))
                                        .rounded(px(7.))
                                        .text_sm()
                                        .text_color(p.muted_foreground)
                                        .cursor_pointer()
                                        .hover(|d| d.bg(p.hover).text_color(p.foreground))
                                        .child(Icon::new(IconName::ArrowLeft).small())
                                        .child("Back")
                                        .on_click(|_, _, cx| close(cx)),
                                ),
                        )
                    })
                    .child(nav)
                    .when(in_window, |d| {
                        d.child(div().flex_1()).child(
                            div().p_2().child(
                                h_flex()
                                    .id("settings-sign-out")
                                    .gap_2()
                                    .items_center()
                                    .px_2()
                                    .py(px(6.))
                                    .rounded(px(7.))
                                    .text_sm()
                                    .text_color(p.muted_foreground)
                                    .cursor_pointer()
                                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                                    .child(Icon::new(IconName::LogOut).small())
                                    .child("Sign out of this browser")
                                    .on_click(|_, _, _| crate::host::sign_out()),
                            ),
                        )
                    }),
            )
            .child(
                // A page is a column of a width that reads well, in the
                // middle of whatever room there is (a browser window can be
                // very wide).
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .items_center()
                    .child(
                        h_flex()
                            .w_full()
                            .max_w(px(PAGE_MAX))
                            .h(px(52.))
                            .flex_shrink_0()
                            .px_8()
                            .items_center()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.section.label()),
                    )
                    .when_some(self.error.clone(), |d, err| {
                        d.child(
                            div().w_full().max_w(px(PAGE_MAX)).px_8().pb_2().child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .rounded(px(8.))
                                    .text_sm()
                                    .bg(p.danger_soft)
                                    .text_color(p.danger)
                                    .child(err),
                            ),
                        )
                    })
                    .child(
                        v_flex()
                            .id("settings-scroll")
                            .track_scroll(&self.scroll)
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .items_center()
                            .overflow_y_scroll()
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(PAGE_MAX))
                                    .flex_none()
                                    .px_8()
                                    .pb_8()
                                    .child(body),
                            ),
                    ),
            )
    }
}

/// How wide a settings page gets, its side padding included.
const PAGE_MAX: f32 = 800.;

// -- building blocks --------------------------------------------------------

pub(super) fn row(label: &str, hint: &str, control: AnyElement, p: &Palette) -> impl IntoElement {
    h_flex()
        .gap_6()
        .items_center()
        .py_3()
        .border_b_1()
        .border_color(p.border.opacity(0.6))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(label.to_owned()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(hint.to_owned()),
                ),
        )
        .child(div().flex_shrink_0().child(control))
}

pub(super) fn text_input(state: &Entity<InputState>) -> AnyElement {
    div()
        .w(px(260.))
        .child(Input::new(state).small())
        .into_any_element()
}

pub(super) fn path_link(
    id: &'static str,
    path: std::path::PathBuf,
    p: &Palette,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let shown = crate::workspace::tilde_path(&path);
    h_flex()
        .id(id)
        .gap_1p5()
        .items_center()
        .max_w(px(320.))
        .px_2()
        .py_1()
        .rounded(px(6.))
        .text_sm()
        .text_color(p.muted_foreground)
        .cursor_pointer()
        .hover(|d| d.bg(p.hover).text_color(p.foreground))
        .child(div().truncate().child(shown))
        .child(Icon::new(IconName::FolderOpen).xsmall())
        .on_click(cx.listener(move |_, _, _, cx| {
            if path.is_file() {
                cx.open_with_system(&path);
            } else {
                cx.reveal_path(&path);
            }
        }))
        .into_any_element()
}

fn segmented(
    id: &'static str,
    options: &[(&'static str, &'static str)],
    current: &str,
    p: &Palette,
    on_select: impl Fn(&&'static str, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let on_select = std::rc::Rc::new(on_select);
    let mut row = h_flex().p(px(2.)).gap(px(2.)).rounded(px(8.)).bg(p.muted);
    for (value, label) in options {
        let active = current == *value;
        let on_select = on_select.clone();
        let value = *value;
        row = row.child(
            div()
                .id(SharedString::from(format!("{id}-{value}")))
                .px_3()
                .py_1()
                .rounded(px(6.))
                .text_sm()
                .cursor_pointer()
                .when(active, |d| {
                    d.bg(p.background)
                        .text_color(p.foreground)
                        .font_weight(FontWeight::MEDIUM)
                        .shadow_sm()
                })
                .when(!active, |d| {
                    d.text_color(p.muted_foreground)
                        .hover(|d| d.text_color(p.foreground))
                })
                .child(*label)
                .on_click(move |_, window, cx| {
                    if !active {
                        on_select(&value, window, cx);
                    }
                }),
        );
    }
    row.into_any_element()
}
