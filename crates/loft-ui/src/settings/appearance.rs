//! Settings → Appearance: mode, a gallery of colour themes (built-in and
//! your own), a theme editor, "design one with Claude", and the finer
//! controls (accent, contrast, sizes, fonts).

use super::{ACCENTS, Field, SettingsView, row, segmented, text_input};
use crate::press::ClaimsPress as _;
use crate::spinner::Spinner;
use crate::theme::{self, Palette};
use crate::themes::{self, Swatch, Theme, ThemeFile};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use loft_config::SettingValue;
use loft_protocol::{Request, Response};

/// The colours the editor exposes (the rest are derived).
const TOKENS: [(&str, &str); 7] = [
    ("background", "Background"),
    ("foreground", "Text"),
    ("accent", "Accent"),
    ("sidebar", "Sidebar"),
    ("surface", "Menus & popovers"),
    ("border", "Borders"),
    ("muted_text", "Secondary text"),
];

pub(super) struct Studio {
    pub(super) themes: Vec<Theme>,
    editing: Option<Editing>,
    describe: Option<Entity<InputState>>,
    /// Which agent and model designs it.
    choice: Option<Entity<crate::agent_choice::AgentChoice>>,
    choice_open: bool,
    generating: bool,
    note: Option<(bool, String)>,
}

struct Editing {
    id: String,
    file: ThemeFile,
    dark: bool,
    name: Entity<InputState>,
    fields: Vec<(&'static str, Entity<InputState>)>,
}

impl Studio {
    pub(super) fn new(home: &std::path::Path) -> Self {
        Self {
            themes: themes::all(home),
            editing: None,
            describe: None,
            choice: None,
            choice_open: false,
            generating: false,
            note: None,
        }
    }
}

fn token_value(spec: &themes::SwatchSpec, sw: &Swatch, key: &str) -> String {
    let explicit = match key {
        "background" => Some(spec.background.clone()),
        "foreground" => Some(spec.foreground.clone()),
        "accent" => Some(spec.accent.clone()),
        "sidebar" => spec.sidebar.clone(),
        "surface" => spec.surface.clone(),
        "border" => spec.border.clone(),
        "muted_text" => spec.muted_text.clone(),
        _ => None,
    };
    explicit.unwrap_or_else(|| {
        themes::hex(match key {
            "sidebar" => sw.sidebar,
            "surface" => sw.surface,
            "border" => sw.border,
            "muted_text" => sw.muted_text,
            _ => sw.background,
        })
    })
}

fn set_token(spec: &mut themes::SwatchSpec, key: &str, value: String) {
    match key {
        "background" => spec.background = value,
        "foreground" => spec.foreground = value,
        "accent" => spec.accent = value,
        "sidebar" => spec.sidebar = Some(value),
        "surface" => spec.surface = Some(value),
        "border" => spec.border = Some(value),
        "muted_text" => spec.muted_text = Some(value),
        _ => {}
    }
}

impl SettingsView {
    fn home(&self, cx: &gpui_kit::App) -> std::path::PathBuf {
        self.store.read(cx).paths().home.clone()
    }

    fn reload_themes(&mut self, cx: &mut Context<Self>) {
        self.studio.themes = themes::all(&self.home(cx));
        // A custom theme's file changed under the current palette: repaint.
        theme::apply(cx);
        cx.notify();
    }

    fn select_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        self.write(
            &["ui", "palette"],
            SettingValue::Str(id.to_owned()),
            false,
            cx,
        );
    }

    /// Copy `source` into a new theme of yours, select it and open the editor.
    fn fork_theme(&mut self, source: &Theme, window: &mut Window, cx: &mut Context<Self>) {
        let mut file = source.file.clone();
        file.name = format!("{} (mine)", source.name);
        let home = self.home(cx);
        match themes::save(&home, &file, None) {
            Ok(id) => {
                self.reload_themes(cx);
                self.select_theme(&id, cx);
                self.start_editing(&id, window, cx);
            }
            Err(err) => self.studio.note = Some((true, format!("Couldn't save: {err:#}"))),
        }
    }

    fn start_editing(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(theme) = self.studio.themes.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let name = cx.new(|cx| {
            let mut s = InputState::new(window, cx);
            s.set_value(theme.file.name.clone(), window, cx);
            s
        });
        self._subs
            .push(cx.subscribe_in(&name, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let text = state.read(cx).value().trim().to_owned();
                    if let Some(ed) = &mut this.studio.editing
                        && !text.is_empty()
                        && text != ed.file.name
                    {
                        ed.file.name = text;
                        this.save_editing(cx);
                    }
                }
            }));
        let dark = Palette::of(cx).dark;
        self.studio.editing = Some(Editing {
            id: id.to_owned(),
            file: theme.file.clone(),
            dark,
            name,
            fields: Vec::new(),
        });
        self.rebuild_fields(window, cx);
    }

    /// Hex inputs for the variant being edited.
    fn rebuild_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = &self.studio.editing else {
            return;
        };
        let theme = Theme {
            id: ed.id.clone(),
            name: ed.file.name.clone(),
            custom: true,
            file: ed.file.clone(),
        };
        let sw = theme.swatch(ed.dark, 1.0);
        let spec = if ed.dark {
            ed.file.dark.clone()
        } else {
            ed.file.light.clone()
        };
        let mut fields = Vec::new();
        for (key, _) in TOKENS {
            let value = token_value(&spec, &sw, key);
            let input = cx.new(|cx| {
                let mut s = InputState::new(window, cx);
                s.set_value(value, window, cx);
                s
            });
            self._subs.push(
                cx.subscribe_in(&input, window, move |this, state, event, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        let text = state.read(cx).value().trim().to_owned();
                        this.edit_token(key, text, cx);
                    }
                }),
            );
            fields.push((key, input));
        }
        if let Some(ed) = &mut self.studio.editing {
            ed.fields = fields;
        }
        cx.notify();
    }

    fn edit_token(&mut self, key: &str, text: String, cx: &mut Context<Self>) {
        let Some(c) = themes::parse_color(&text) else {
            self.studio.note = Some((true, format!("“{text}” isn't a colour like #1e2a38.")));
            cx.notify();
            return;
        };
        let Some(ed) = &mut self.studio.editing else {
            return;
        };
        let spec = if ed.dark {
            &mut ed.file.dark
        } else {
            &mut ed.file.light
        };
        set_token(spec, key, themes::hex(c));
        self.studio.note = None;
        self.save_editing(cx);
    }

    fn save_editing(&mut self, cx: &mut Context<Self>) {
        let Some(ed) = &self.studio.editing else {
            return;
        };
        let home = self.home(cx);
        if let Err(err) = themes::save(&home, &ed.file, Some(&ed.id)) {
            self.studio.note = Some((true, format!("Couldn't save: {err:#}")));
        }
        self.reload_themes(cx);
    }

    fn delete_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        let home = self.home(cx);
        if let Err(err) = themes::delete(&home, id) {
            self.studio.note = Some((true, format!("Couldn't delete: {err:#}")));
        }
        if self.studio.editing.as_ref().is_some_and(|e| e.id == id) {
            self.studio.editing = None;
        }
        if self.config.ui.palette == id {
            self.select_theme("loft", cx);
        }
        self.reload_themes(cx);
    }

    /// Ask Claude for a theme from a description, then save and use it.
    fn generate_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.studio.describe else {
            return;
        };
        let description = input.read(cx).value().trim().to_owned();
        if description.is_empty() || self.studio.generating {
            return;
        }
        let Some(client) = self.store.read(cx).client() else {
            self.studio.note = Some((true, "Not connected to agentloftd.".into()));
            return;
        };
        let (harness, model) = match &self.studio.choice {
            Some(c) => {
                let c = c.read(cx);
                (Some(c.harness.clone()), c.model.clone())
            }
            None => (None, None),
        };
        self.studio.generating = true;
        self.studio.note = None;
        let fut = client.request(Request::AskOnce {
            prompt: themes::generation_prompt(&description),
            harness,
            model,
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.studio.generating = false;
                let text = match result {
                    Ok(Response::Text { text }) => text,
                    Ok(_) => String::new(),
                    Err(err) => {
                        this.studio.note = Some((true, err.message));
                        cx.notify();
                        return;
                    }
                };
                match themes::parse(&text) {
                    Ok(file) => {
                        let home = this.home(cx);
                        match themes::save(&home, &file, None) {
                            Ok(id) => {
                                this.studio.note =
                                    Some((false, format!("Made “{}” for you.", file.name)));
                                this.studio.describe = None;
                                this.reload_themes(cx);
                                this.select_theme(&id, cx);
                                this.start_editing(&id, window, cx);
                            }
                            Err(err) => {
                                this.studio.note = Some((true, format!("Couldn't save: {err:#}")))
                            }
                        }
                    }
                    Err(err) => {
                        this.studio.note =
                            Some((true, format!("Claude's answer wasn't a theme: {err:#}")))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn appearance(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ui = self.config.ui.clone();
        let current = self
            .studio
            .themes
            .iter()
            .find(|t| t.id == ui.palette)
            .or(self.studio.themes.first())
            .cloned();
        let contrast = themes::contrast_factor(&ui.contrast);

        // Mode: three little windows painted in the current theme.
        let mut modes = h_flex().gap_3();
        if let Some(theme) = &current {
            let (light, dark) = (theme.swatch(false, contrast), theme.swatch(true, contrast));
            for (value, label) in [("system", "System"), ("light", "Light"), ("dark", "Dark")] {
                let active = ui.theme == value;
                let preview = match value {
                    "light" => mini_window(&light, None),
                    "dark" => mini_window(&dark, None),
                    _ => mini_window(&light, Some(&dark)),
                };
                modes = modes.child(
                    v_flex()
                        .id(SharedString::from(format!("mode-{value}")))
                        .flex_1()
                        .gap_1p5()
                        .p_1p5()
                        .rounded(px(12.))
                        .border_2()
                        .border_color(if active {
                            p.accent
                        } else {
                            gpui_kit::transparent_black()
                        })
                        .cursor_pointer()
                        .hover(|d| d.bg(p.hover))
                        .child(preview)
                        .child(
                            div()
                                .text_center()
                                .text_sm()
                                .when(active, |d| d.font_weight(FontWeight::MEDIUM))
                                .child(label),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_str(&["ui", "theme"], value, false, cx)
                        })),
                );
            }
        }

        // Theme gallery.
        let mut grid = div().grid().grid_cols(3).gap_3();
        for theme in &self.studio.themes {
            grid = grid.child(self.theme_card(theme, &ui.palette, contrast, p, cx));
        }
        let header = h_flex()
            .justify_between()
            .items_center()
            .pt_6()
            .pb_3()
            .child(
                v_flex()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Themes"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child("Each has a light and a dark side; the mode above picks one."),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        pill_button("design-theme", IconName::Sparkles, "Design with AI", p)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.studio.describe.is_none() {
                                    let input = cx.new(|cx| {
                                        InputState::new(window, cx).placeholder(
                                        "e.g. “amber terminal on warm charcoal”, “Nordic winter”",
                                    )
                                    });
                                    this._subs.push(cx.subscribe_in(
                                        &input,
                                        window,
                                        |this, _, event, window, cx| {
                                            if let InputEvent::PressEnter { .. } = event {
                                                this.generate_theme(window, cx);
                                            }
                                        },
                                    ));
                                    input.update(cx, |s, cx| s.focus(window, cx));
                                    this.studio.describe = Some(input);
                                    if this.studio.choice.is_none() {
                                        let store = this.store.clone();
                                        let choice = cx.new(|cx| {
                                            crate::agent_choice::AgentChoice::new(store, cx)
                                        });
                                        this._subs.push(cx.subscribe(&choice, |this, _, _, cx| {
                                            this.studio.choice_open = false;
                                            cx.notify();
                                        }));
                                        this.studio.choice = Some(choice);
                                    }
                                } else {
                                    this.studio.describe = None;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        pill_button("new-theme", IconName::Plus, "New theme", p).on_click(
                            cx.listener(|this, _, window, cx| {
                                let base = this
                                    .studio
                                    .themes
                                    .iter()
                                    .find(|t| t.id == this.config.ui.palette)
                                    .or(this.studio.themes.first())
                                    .cloned();
                                if let Some(base) = base {
                                    this.fork_theme(&base, window, cx);
                                }
                            }),
                        ),
                    ),
            );

        let chooser = self.studio.choice.clone().map(|choice| {
            use gpui_kit::component::button::{Button, ButtonVariants as _};
            use gpui_kit::component::popover::Popover;
            let chip = crate::agent_choice::chip(choice.read(cx), p, cx);
            Popover::new("theme-agent")
                .anchor(gpui_kit::Anchor::TopRight)
                .offset(px(6.))
                .p_0()
                .open(self.studio.choice_open)
                .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                    this.studio.choice_open = *open;
                    cx.notify();
                }))
                .trigger(
                    Button::new("theme-agent-chip")
                        .cursor_pointer()
                        .ghost()
                        .small()
                        .child(chip),
                )
                .content(move |_, _, _| choice.clone())
        });
        let describe = self.studio.describe.clone().map(|input| {
            let generating = self.studio.generating;
            h_flex()
                .gap_2()
                .items_center()
                .mb_3()
                .p_3()
                .rounded(px(12.))
                .border_1()
                .border_color(p.accent.opacity(0.5))
                .bg(p.accent_soft)
                .child(Icon::new(IconName::Sparkles).small().text_color(p.accent))
                .child(div().flex_1().child(Input::new(&input).small()))
                .children(chooser)
                .child(if generating {
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(Spinner::new().xsmall())
                        .child("Mixing colours…")
                        .into_any_element()
                } else {
                    pill_button("generate-theme", IconName::ArrowRight, "Design", p)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.generate_theme(window, cx)),
                        )
                        .into_any_element()
                })
        });
        let note = self.studio.note.clone().map(|(error, text)| {
            div()
                .mb_3()
                .px_3()
                .py_2()
                .rounded(px(8.))
                .text_sm()
                .when(error, |d| d.bg(p.danger_soft).text_color(p.danger))
                .when(!error, |d| d.bg(p.accent_soft).text_color(p.foreground))
                .child(text)
        });
        let editor = self
            .studio
            .editing
            .as_ref()
            .map(|ed| self.editor(ed, p, cx));

        // Accent override.
        let mut swatches = h_flex().gap_2().flex_wrap();
        for (ix, (name, hex)) in ACCENTS.iter().enumerate() {
            let selected = ui.accent.as_deref().map(str::to_lowercase) == hex.map(str::to_owned);
            let color = hex.and_then(theme::parse_hex).unwrap_or_else(|| {
                current
                    .as_ref()
                    .map_or(p.accent, |t| rgb(t.swatch(p.dark, 1.0).accent).into())
            });
            let value = hex.map(str::to_owned);
            let label = if hex.is_none() { "Theme's own" } else { *name };
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
                    .child(div().size_full().rounded_full().bg(color))
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(label).build(window, cx)
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
        let contrast_ctl = segmented(
            "contrast",
            &[("soft", "Soft"), ("normal", "Normal"), ("high", "High")],
            &ui.contrast,
            p,
            cx.listener(|this, value: &&str, _, cx| {
                this.set_str(&["ui", "contrast"], value, false, cx)
            }),
        );
        let density = segmented(
            "density",
            &[("comfortable", "Comfortable"), ("compact", "Compact")],
            &ui.density,
            p,
            cx.listener(|this, value: &&str, _, cx| {
                this.set_str(&["ui", "density"], value, false, cx)
            }),
        );
        let sidebar = segmented(
            "sidebar-layout",
            &[("threads", "Threads"), ("projects", "Projects")],
            &ui.sidebar,
            p,
            cx.listener(|this, value: &&str, _, cx| {
                this.set_str(&["ui", "sidebar"], value, false, cx)
            }),
        );
        let _ = window;

        v_flex()
            .child(
                div()
                    .pt_2()
                    .pb_3()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Mode"),
            )
            .child(modes)
            .child(header)
            .children(describe)
            .children(note)
            .children(editor)
            .child(grid)
            .child(
                div()
                    .pt_8()
                    .pb_1()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Interface"),
            )
            .child(row(
                "Accent",
                "Buttons, highlights and links. Keep the theme's own or pick one.",
                swatches.into_any_element(),
                p,
            ))
            .child(row(
                "Contrast",
                "How strongly borders and secondary text stand out.",
                contrast_ctl,
                p,
            ))
            .child(row(
                "Sidebar",
                "Threads: one list, most recent first, each with its project. Projects: threads grouped under their project.",
                sidebar,
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
            .child(
                div()
                    .pt_8()
                    .pb_1()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Typography"),
            )
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

    fn theme_card(
        &self,
        theme: &Theme,
        selected_id: &str,
        contrast: f32,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = theme.id == selected_id;
        let (light, dark) = (theme.swatch(false, contrast), theme.swatch(true, contrast));
        let id = theme.id.clone();
        let editable = theme.custom;
        let action = {
            let theme = theme.clone();
            let id = theme.id.clone();
            if editable {
                h_flex()
                    .gap_0p5()
                    .child(
                        icon_action(&format!("edit-{id}"), IconName::SquarePen, "Edit", p)
                            .claims_press()
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.select_theme(&id, cx);
                                    this.start_editing(&id, window, cx);
                                }
                            })),
                    )
                    .child(
                        icon_action(&format!("delete-{id}"), IconName::Trash, "Delete", p)
                            .claims_press()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.delete_theme(&id, cx);
                            })),
                    )
                    .into_any_element()
            } else {
                icon_action(
                    &format!("copy-{id}"),
                    IconName::Copy,
                    "Make a copy to edit",
                    p,
                )
                .claims_press()
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.fork_theme(&theme, window, cx);
                }))
                .into_any_element()
            }
        };
        v_flex()
            .id(SharedString::from(format!("theme-{}", theme.id)))
            .gap_2()
            .p_2()
            .rounded(px(12.))
            .border_2()
            .border_color(if selected { p.accent } else { p.border })
            .cursor_pointer()
            .hover(|d| d.bg(p.hover))
            .child(theme_tile(&light, &dark))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(theme.name.clone()),
                                    )
                                    .when(theme.custom, |d| {
                                        d.child(
                                            div()
                                                .px_1()
                                                .rounded(px(4.))
                                                .bg(p.accent_soft)
                                                .text_color(p.accent)
                                                .text_xs()
                                                .child("yours"),
                                        )
                                    }),
                            )
                            .children(theme.file.description.clone().map(|d| {
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(p.muted_foreground)
                                    .child(d)
                            })),
                    )
                    .child(action),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.select_theme(&id, cx)))
            .into_any_element()
    }

    fn editor(&self, ed: &Editing, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let variant = segmented(
            "edit-variant",
            &[("light", "Light side"), ("dark", "Dark side")],
            if ed.dark { "dark" } else { "light" },
            p,
            cx.listener(|this, value: &&str, window, cx| {
                if let Some(ed) = &mut this.studio.editing {
                    ed.dark = *value == "dark";
                }
                this.rebuild_fields(window, cx);
            }),
        );
        let mut fields = div().grid().grid_cols(2).gap_x_4().gap_y_2();
        for ((key, label), (_, input)) in TOKENS.iter().zip(&ed.fields) {
            let color = themes::parse_color(&input.read(cx).value())
                .map(|c| rgb(c).into())
                .unwrap_or(p.border);
            fields = fields.child(
                h_flex()
                    .id(SharedString::from(format!("token-{key}")))
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .size(px(22.))
                            .flex_none()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(p.border)
                            .bg(color),
                    )
                    .child(div().flex_1().min_w_0().text_sm().truncate().child(*label))
                    .child(div().w(px(110.)).child(Input::new(input).small())),
            );
        }
        v_flex()
            .mb_3()
            .p_4()
            .gap_3()
            .rounded(px(12.))
            .border_1()
            .border_color(p.border)
            .bg(p.muted)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(Icon::new(IconName::SquarePen).small().text_color(p.accent))
                    .child(div().w(px(220.)).child(Input::new(&ed.name).small()))
                    .child(div().flex_1())
                    .child(variant)
                    .child(
                        pill_button("done-editing", IconName::Check, "Done", p).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.studio.editing = None;
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(fields)
            .child(div().text_xs().text_color(p.muted_foreground).child(
                "Type a colour and press ⏎ to see it. Colours you don't set are mixed \
                         from the background and text. Saved in ~/.agentloft/themes/.",
            ))
            .into_any_element()
    }
}

/// A theme's light and dark sides, side by side, as tiny windows.
fn theme_tile(light: &Swatch, dark: &Swatch) -> AnyElement {
    h_flex()
        .h(px(70.))
        .rounded(px(8.))
        .overflow_hidden()
        .child(div().flex_1().h_full().child(mini_body(light)))
        .child(div().flex_1().h_full().child(mini_body(dark)))
        .into_any_element()
}

/// Sidebar, a reply, a bubble and a send button.
fn mini_body(s: &Swatch) -> AnyElement {
    let c = |v: u32| -> gpui_kit::Hsla { rgb(v).into() };
    h_flex()
        .size_full()
        .bg(c(s.background))
        .child(
            v_flex()
                .w(px(16.))
                .h_full()
                .gap(px(3.))
                .pt(px(8.))
                .px(px(3.))
                .bg(c(s.sidebar))
                .border_r_1()
                .border_color(c(s.border))
                .child(
                    div()
                        .h(px(3.))
                        .rounded_full()
                        .bg(c(s.muted_text))
                        .opacity(0.6),
                )
                .child(div().h(px(3.)).rounded_full().bg(c(s.accent)))
                .child(
                    div()
                        .h(px(3.))
                        .rounded_full()
                        .bg(c(s.muted_text))
                        .opacity(0.6),
                ),
        )
        .child(
            v_flex()
                .flex_1()
                .h_full()
                .p(px(6.))
                .gap(px(4.))
                .child(
                    h_flex()
                        .justify_end()
                        .child(div().w(px(26.)).h(px(7.)).rounded_full().bg(c(s.bubble))),
                )
                .child(
                    div()
                        .w(px(40.))
                        .h(px(3.))
                        .rounded_full()
                        .bg(c(s.foreground))
                        .opacity(0.8),
                )
                .child(
                    div()
                        .w(px(30.))
                        .h(px(3.))
                        .rounded_full()
                        .bg(c(s.muted_text)),
                )
                .child(div().flex_1())
                .child(
                    h_flex()
                        .h(px(10.))
                        .px(px(3.))
                        .items_center()
                        .justify_end()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(c(s.border))
                        .bg(c(s.surface))
                        .child(div().size(px(5.)).rounded_full().bg(c(s.accent))),
                ),
        )
        .into_any_element()
}

/// A small window; with `split`, its right half shows the dark side.
fn mini_window(s: &Swatch, split: Option<&Swatch>) -> AnyElement {
    let body = match split {
        Some(dark) => h_flex()
            .size_full()
            .child(div().flex_1().h_full().child(mini_body(s)))
            .child(div().flex_1().h_full().child(mini_body(dark)))
            .into_any_element(),
        None => mini_body(s),
    };
    div()
        .h(px(84.))
        .rounded(px(9.))
        .overflow_hidden()
        .border_1()
        .border_color(rgb(s.border))
        .child(body)
        .into_any_element()
}

fn pill_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(id)
        .gap_1p5()
        .items_center()
        .px_2p5()
        .py_1()
        .rounded(px(8.))
        .border_1()
        .border_color(p.border)
        .text_sm()
        .cursor_pointer()
        .hover(|d| d.bg(p.hover))
        .child(Icon::new(icon).xsmall())
        .child(label)
}

fn icon_action(
    id: &str,
    icon: IconName,
    tooltip: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(SharedString::from(id.to_owned()))
        .p(px(4.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_color(p.muted_foreground)
        .hover(|d| d.bg(p.selected).text_color(p.foreground))
        .child(Icon::new(icon).xsmall())
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx)
        })
}
