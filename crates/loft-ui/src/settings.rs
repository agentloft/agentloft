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
use loft_protocol::{HarnessId, PermissionMode, Request};

struct SettingsWindow(AnyWindowHandle, gpui_kit::WeakEntity<SettingsView>);
impl Global for SettingsWindow {}

/// Open the settings window (optionally at a section), or bring the open
/// one to the front.
pub fn open(store: Entity<AppStore>, section: Option<Section>, cx: &mut App) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Appearance,
    Agents,
    Worktrees,
    About,
}

impl Section {
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|sec| sec.label().eq_ignore_ascii_case(s))
    }

    const ALL: [Section; 4] = [
        Section::Appearance,
        Section::Agents,
        Section::Worktrees,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Agents => "Agents",
            Section::Worktrees => "Worktrees",
            Section::About => "About",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Section::Appearance => IconName::Sun,
            Section::Agents => IconName::Bot,
            Section::Worktrees => IconName::GitBranch,
            Section::About => IconName::Sparkles,
        }
    }
}

/// Text fields, saved on Enter or blur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    UiFont,
    MonoFont,
    ClaudePath,
    ClaudeModel,
    CodexPath,
    CodexModel,
    BranchPrefix,
}

impl Field {
    fn key(self) -> &'static [&'static str] {
        match self {
            Field::UiFont => &["ui", "font_family"],
            Field::MonoFont => &["ui", "mono_font_family"],
            Field::ClaudePath => &["harness", "claude", "path"],
            Field::ClaudeModel => &["harness", "claude", "default_model"],
            Field::CodexPath => &["harness", "codex", "path"],
            Field::CodexModel => &["harness", "codex", "default_model"],
            Field::BranchPrefix => &["worktrees", "branch_prefix"],
        }
    }

    fn current(self, c: &Config) -> String {
        match self {
            Field::UiFont => c.ui.font_family.clone().unwrap_or_default(),
            Field::MonoFont => c.ui.mono_font_family.clone().unwrap_or_default(),
            Field::ClaudePath => c.harness.claude.path.clone(),
            Field::ClaudeModel => c.harness.claude.default_model.clone(),
            Field::CodexPath => c.harness.codex.path.clone(),
            Field::CodexModel => c.harness.codex.default_model.clone(),
            Field::BranchPrefix => c.worktrees.branch_prefix.clone(),
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Field::UiFont => "System font",
            Field::MonoFont => "Theme monospace font",
            Field::ClaudePath | Field::CodexPath => "auto (login-shell PATH)",
            Field::ClaudeModel | Field::CodexModel => "auto (the agent's default)",
            Field::BranchPrefix => "agentloft",
        }
    }

    /// Empty input → back to the default.
    fn value(self, text: &str) -> SettingValue {
        let text = text.trim();
        match (self, text.is_empty()) {
            (Field::UiFont | Field::MonoFont, true) => SettingValue::Unset,
            (Field::ClaudePath | Field::CodexPath, true) => SettingValue::Str("auto".into()),
            (Field::ClaudeModel | Field::CodexModel, true) => SettingValue::Str("auto".into()),
            (Field::BranchPrefix, true) => SettingValue::Str("agentloft".into()),
            _ => SettingValue::Str(text.to_owned()),
        }
    }

    /// Settings the daemon reads (it reloads on change).
    fn is_daemon(self) -> bool {
        !matches!(self, Field::UiFont | Field::MonoFont)
    }
}

pub struct SettingsView {
    store: Entity<AppStore>,
    section: Section,
    config: Config,
    inputs: Vec<(Field, Entity<InputState>)>,
    error: Option<String>,
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
        let (config, error) = match Config::load(&paths) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(format!("{e:#}"))),
        };
        let mut inputs = Vec::new();
        let mut subs = Vec::new();
        for field in [
            Field::UiFont,
            Field::MonoFont,
            Field::ClaudePath,
            Field::ClaudeModel,
            Field::CodexPath,
            Field::CodexModel,
            Field::BranchPrefix,
        ] {
            let current = field.current(&config);
            let input = cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(field.placeholder());
                // `auto` reads better as the placeholder.
                if current != "auto" {
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
        Self {
            store,
            section: Section::Appearance,
            config,
            inputs,
            error,
            _subs: subs,
        }
    }

    fn input(&self, field: Field) -> Entity<InputState> {
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
            SettingValue::Unset => field.current(&self.config).is_empty(),
            SettingValue::Float(_) => false,
        };
        if !unchanged {
            self.write(field.key(), value, field.is_daemon(), cx);
        }
    }

    /// Write one setting, reload, and apply it.
    fn write(&mut self, key: &[&str], value: SettingValue, daemon: bool, cx: &mut Context<Self>) {
        let paths = self.store.read(cx).paths().clone();
        if let Err(err) = loft_config::write_setting(&paths, key, value) {
            self.error = Some(format!("Couldn't save: {err:#}"));
            cx.notify();
            return;
        }
        match Config::load(&paths) {
            Ok(c) => {
                self.config = c;
                self.error = None;
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

    fn appearance(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let ui = self.config.ui.clone();
        let theme = segmented(
            "theme",
            &[("system", "System"), ("light", "Light"), ("dark", "Dark")],
            &ui.theme,
            p,
            cx.listener(|this, value: &&str, _, cx| {
                this.set_str(&["ui", "theme"], value, false, cx)
            }),
        );
        let mut swatches = h_flex().gap_2().flex_wrap();
        for (ix, (name, hex)) in ACCENTS.iter().enumerate() {
            let selected = ui.accent.as_deref().map(str::to_lowercase) == hex.map(str::to_owned);
            let color = hex.and_then(theme::parse_hex).unwrap_or(p.accent);
            let value = hex.map(str::to_owned);
            swatches = swatches.child(
                div()
                    .id(("accent", ix))
                    .size(px(24.))
                    .rounded_full()
                    .p(px(3.))
                    .border_2()
                    .border_color(if selected {
                        p.foreground
                    } else {
                        gpui_kit::transparent_black()
                    })
                    .cursor_pointer()
                    .child(
                        div()
                            .size_full()
                            .rounded_full()
                            .bg(color)
                            // "Default" shows the theme accent, dimmed ring.
                            .when(hex.is_none(), |d| d.opacity(0.6)),
                    )
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(*name).build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let v = match &value {
                            Some(h) => SettingValue::Str(h.clone()),
                            None => SettingValue::Unset,
                        };
                        this.write(&["ui", "accent"], v, false, cx);
                    })),
            );
        }
        let density = segmented(
            "density",
            &[("comfortable", "Comfortable"), ("compact", "Compact")],
            &ui.density,
            p,
            cx.listener(|this, value: &&str, _, cx| {
                this.set_str(&["ui", "density"], value, false, cx)
            }),
        );
        v_flex()
            .child(row("Theme", "Follow the system, or pick one.", theme, p))
            .child(row(
                "Accent color",
                "Used for highlights, the send button and links.",
                swatches.into_any_element(),
                p,
            ))
            .child(row(
                "Text size",
                "Base size of the interface and conversation.",
                self.stepper("font_size", ui.font_size, 11., 20., 1., "px", p, cx),
                p,
            ))
            .child(row(
                "Code text size",
                "Code blocks, tool output, diffs and the terminal.",
                self.stepper(
                    "mono_font_size",
                    ui.mono_font_size,
                    10.,
                    18.,
                    0.5,
                    "px",
                    p,
                    cx,
                ),
                p,
            ))
            .child(row("Density", "Spacing in the conversation.", density, p))
            .child(row(
                "Conversation width",
                "Maximum width of the conversation column.",
                self.stepper(
                    "content_width",
                    ui.content_width,
                    560.,
                    1200.,
                    40.,
                    "px",
                    p,
                    cx,
                ),
                p,
            ))
            .child(row(
                "Interface font",
                "Any installed font family.",
                text_input(&self.input(Field::UiFont)),
                p,
            ))
            .child(row(
                "Monospace font",
                "For code, diffs and the terminal.",
                text_input(&self.input(Field::MonoFont)),
                p,
            ))
            .into_any_element()
    }

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

    fn agents(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut col = v_flex();
        for (id, name, path, model) in [
            (
                HarnessId::CLAUDE,
                "Claude Code",
                Field::ClaudePath,
                Field::ClaudeModel,
            ),
            (
                HarnessId::CODEX,
                "Codex",
                Field::CodexPath,
                Field::CodexModel,
            ),
        ] {
            let h = if id == HarnessId::CODEX {
                self.config.harness.codex.clone()
            } else {
                self.config.harness.claude.clone()
            };
            let status = self
                .store
                .read(cx)
                .harness_status
                .get(&HarnessId(id.into()))
                .cloned();
            let (status_text, status_hint, ok) = match &status {
                None => ("Checking…".to_owned(), String::new(), true),
                Some(s) if s.installed => (
                    s.version.clone().unwrap_or_else(|| "Installed".into()),
                    s.path.clone().unwrap_or_default(),
                    s.error.is_none(),
                ),
                Some(s) => (
                    "Not found".to_owned(),
                    s.error.clone().unwrap_or_default(),
                    false,
                ),
            };
            col = col
                .child(group_title(name, p))
                .child(row(
                    "Status",
                    &status_hint,
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_sm()
                        .text_color(if ok { p.success } else { p.warning })
                        .child(
                            Icon::new(if ok {
                                IconName::CircleCheck
                            } else {
                                IconName::CircleAlert
                            })
                            .small(),
                        )
                        .child(status_text)
                        .into_any_element(),
                    p,
                ))
                .child(row(
                    "Binary",
                    "Path to the CLI. Leave empty to find it on your login shell's PATH.",
                    text_input(&self.input(path)),
                    p,
                ))
                .child(row(
                    "Default model",
                    "Used for new threads; empty means the agent's own default.",
                    text_input(&self.input(model)),
                    p,
                ))
                .child(row(
                    "Default permissions",
                    "How much the agent may do without asking, for new threads.",
                    permission_menu(id, &h.default_permission_mode, cx),
                    p,
                ));
        }
        col.into_any_element()
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let mut nav = v_flex().gap_0p5().px_2();
        for section in Section::ALL {
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
            Section::Appearance => self.appearance(&p, cx),
            Section::Agents => self.agents(&p, cx),
            Section::Worktrees => self.worktrees(&p, cx),
            Section::About => self.about(&p, cx),
        };
        h_flex()
            .size_full()
            .bg(p.background)
            .text_color(p.foreground)
            .child(
                v_flex()
                    .w(px(200.))
                    .h_full()
                    .flex_shrink_0()
                    .bg(p.sidebar)
                    .border_r_1()
                    .border_color(p.border)
                    .pt(px(52.))
                    .child(nav),
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

fn row(label: &str, hint: &str, control: AnyElement, p: &Palette) -> impl IntoElement {
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

fn group_title(title: &str, p: &Palette) -> impl IntoElement {
    div()
        .pt_5()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.muted_foreground)
        .child(title.to_uppercase())
}

fn text_input(state: &Entity<InputState>) -> AnyElement {
    div()
        .w(px(260.))
        .child(Input::new(state).small())
        .into_any_element()
}

fn path_link(
    id: &'static str,
    path: std::path::PathBuf,
    p: &Palette,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let shown =
        match std::env::home_dir().and_then(|h| path.strip_prefix(h).ok().map(|r| r.to_owned())) {
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        };
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

fn permission_menu(
    harness: &'static str,
    current: &str,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    use PermissionMode::*;
    use gpui_kit::component::button::Button;
    use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
    let current_mode: Option<PermissionMode> =
        serde_json::from_value(serde_json::json!(current)).ok();
    let label = crate::workspace::mode_label(current_mode);
    let view = cx.entity().downgrade();
    Button::new(SharedString::from(format!("perm-{harness}")))
        .label(label)
        .icon(IconName::ShieldCheck)
        .outline()
        .small()
        .dropdown_menu(move |mut menu, _, _| {
            for mode in [
                None,
                Some(ReadOnly),
                Some(AskAll),
                Some(AcceptEdits),
                Some(Auto),
                Some(FullAccess),
            ] {
                let view = view.clone();
                let value = match mode {
                    None => "harness-default".to_owned(),
                    Some(m) => serde_json::to_value(m)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                };
                menu = menu.item(
                    PopupMenuItem::new(crate::workspace::mode_label(mode))
                        .checked(mode == current_mode)
                        .on_click(move |_, _, cx| {
                            let value = value.clone();
                            let _ = view.update(cx, |this, cx| {
                                this.set_str(
                                    &["harness", harness, "default_permission_mode"],
                                    &value,
                                    true,
                                    cx,
                                )
                            });
                        }),
                );
                if mode.is_none() {
                    menu = menu.separator();
                }
            }
            menu
        })
        .into_any_element()
}
