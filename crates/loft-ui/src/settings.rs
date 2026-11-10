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
mod projects;
mod shortcuts;
mod web;

struct SettingsWindow(AnyWindowHandle, gpui_kit::WeakEntity<SettingsView>);
impl Global for SettingsWindow {}

/// Settings drawn inside the main window: where there is only one window
/// (Agentloft in a browser tab).
struct SettingsOverlay(Entity<SettingsView>);
impl Global for SettingsOverlay {}

/// Open the settings window (optionally at a section), or bring the open
/// one to the front.
pub fn open(store: Entity<AppStore>, section: Option<Section>, cx: &mut App) {
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
    Projects,
    Worktrees,
    Shortcuts,
    /// Agentloft in a browser (desktop only: a browser session may not
    /// manage its own access).
    Web,
    About,
}

impl Section {
    pub fn parse(s: &str) -> Option<Self> {
        Self::all()
            .into_iter()
            .find(|sec| sec.label().eq_ignore_ascii_case(s) || sec.slug() == s)
    }

    const ALL: [Section; 7] = [
        Section::Appearance,
        Section::Agents,
        Section::Projects,
        Section::Worktrees,
        Section::Shortcuts,
        Section::Web,
        Section::About,
    ];

    /// The sections this build has.
    fn all() -> Vec<Section> {
        Self::ALL
            .into_iter()
            .filter(|section| !(crate::host::IS_WEB && *section == Section::Web))
            .collect()
    }

    /// One word, for links and the dev hook (`settings:web`).
    fn slug(self) -> &'static str {
        match self {
            Section::Appearance => "appearance",
            Section::Agents => "agents",
            Section::Projects => "projects",
            Section::Worktrees => "worktrees",
            Section::Shortcuts => "shortcuts",
            Section::Web => "web",
            Section::About => "about",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Agents => "Agents",
            Section::Projects => "Projects",
            Section::Worktrees => "Worktrees",
            Section::Shortcuts => "Shortcuts",
            Section::Web => "Web access",
            Section::About => "About",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            Section::Appearance => IconName::Sun,
            Section::Agents => IconName::Bot,
            Section::Projects => IconName::FolderOpen,
            Section::Worktrees => IconName::GitBranch,
            Section::Shortcuts => IconName::Keyboard,
            Section::Web => IconName::GlobeLock,
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
        }
    }

    /// What the field holds when nothing was set (shown as its placeholder
    /// rather than typed in).
    fn default_text(self) -> &'static str {
        match self {
            Field::WebBind => "127.0.0.1",
            Field::WebPort => "4780",
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
            Field::UiFont | Field::MonoFont | Field::ProjectsDir | Field::NewProjectsDir
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
            Section::Projects => self.projects_page(&p, window, cx),
            Section::Worktrees => self.worktrees(&p, cx),
            Section::Shortcuts => self.shortcuts_page(&p, cx),
            Section::Web => self.web_page(&p, cx),
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
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        h_flex()
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
                            div()
                                .mx_8()
                                .mb_2()
                                .px_3()
                                .py_2()
                                .rounded(px(8.))
                                .text_sm()
                                .bg(p.danger_soft)
                                .text_color(p.danger)
                                .child(err),
                        )
                    })
                    .child(
                        div()
                            .id("settings-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_8()
                            .pb_8()
                            .child(body),
                    ),
            )
    }
}

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
