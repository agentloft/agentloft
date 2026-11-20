//! Settings ▸ Voice: dictation into the composer (`[voice]`). Which speech
//! model turns speech into text (downloaded to the daemon's computer), the
//! language, how long a model stays in memory, and what tidies transcripts:
//! nothing, a small model on this computer, or an agent on your plan.

use super::{Field, SettingsView, row, text_input, web::heading};
use crate::theme::Palette;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Task, div,
    prelude::FluentBuilder as _, px,
};
use loft_config::{SettingValue, VoiceConfig};
use loft_protocol::{HarnessId, Request, Response, VoiceModel, VoiceModelState, VoiceStatus};
use std::time::Duration;

#[derive(Default)]
pub(super) struct VoicePage {
    status: Option<VoiceStatus>,
    /// Asks the daemon for the models' state while the page shows.
    polling: Option<Task<()>>,
    /// Where each model list is scrolled.
    speech_scroll: gpui_kit::ScrollHandle,
    cleanup_scroll: gpui_kit::ScrollHandle,
}

/// Languages offered for dictation, by ISO code (the models say which
/// they take).
const LANGUAGES: &[(&str, &str)] = &[
    ("auto", "Detect automatically"),
    ("en", "English"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("sv", "Swedish"),
    ("da", "Danish"),
    ("fi", "Finnish"),
    ("cs", "Czech"),
    ("el", "Greek"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ru", "Russian"),
    ("ar", "Arabic"),
    ("hi", "Hindi"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("zh", "Chinese"),
    ("vi", "Vietnamese"),
];

/// Agents that can tidy a transcript on the user's plan, each with the
/// small model offered first (`None`: the agent's default).
const AGENTS: &[(&str, &str, Option<&str>)] = &[
    (HarnessId::CLAUDE, "Claude Code", Some("haiku")),
    (HarnessId::CODEX, "Codex", None),
    (HarnessId::GEMINI, "Gemini CLI", None),
    (HarnessId::OPENCODE, "OpenCode", None),
];

/// An agent (id, name) with its small models (id, name).
type AgentModels = (&'static str, &'static str, Vec<(String, String)>);

/// Words in a model's id or name that mark a small, cheap one.
const CHEAP: &[&str] = &["haiku", "mini", "nano", "flash", "lite", "small"];

impl SettingsView {
    pub(super) fn voice_page(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        self.poll_voice(cx);
        let voice = self.config.voice.clone();
        let mut page = v_flex().child(heading(
            "Dictation",
            "Press the microphone in the composer (or its shortcut), speak, and press it again, or hold it while you speak with Hold to talk: the text goes into your message to edit or send. Speech is turned into text on this computer by the model below; recordings aren't kept.",
            p,
        ));
        page = page.child(row(
            "Show the microphone",
            "The composer's microphone button. The shortcut works either way.",
            Switch::new("voice-enabled")
                .cursor_pointer()
                .checked(voice.enabled)
                .on_click(cx.listener(|this, on: &bool, _, cx| {
                    let value = if *on {
                        SettingValue::Unset
                    } else {
                        SettingValue::Bool(false)
                    };
                    this.write(&["voice", "enabled"], value, false, cx);
                }))
                .into_any_element(),
            p,
        ));
        let hold = voice.hold_to_talk();
        let view = cx.entity();
        page = page.child(row(
            "Recording",
            "How the microphone and its shortcut record. Toggle: press to start, press again to finish. Hold to talk: records while you hold them, and finishes when you let go.",
            Button::new("voice-mode")
                .cursor_pointer()
                .outline()
                .small()
                .label(if hold { "Hold to talk" } else { "Toggle" })
                .dropdown_menu(move |menu, _, _| {
                    let (toggle, held) = (view.clone(), view.clone());
                    menu.item(PopupMenuItem::new("Toggle").checked(!hold).on_click(
                        move |_, _, cx| {
                            toggle.update(cx, |this, cx| {
                                this.write(&["voice", "mode"], SettingValue::Unset, false, cx)
                            });
                        },
                    ))
                    .item(PopupMenuItem::new("Hold to talk").checked(hold).on_click(
                        move |_, _, cx| {
                            held.update(cx, |this, cx| {
                                this.write(
                                    &["voice", "mode"],
                                    SettingValue::Str("hold".into()),
                                    false,
                                    cx,
                                )
                            });
                        },
                    ))
                })
                .into_any_element(),
            p,
        ));
        // The same keys as in Settings ▸ Shortcuts: click, then press.
        if let Some(shortcut) = crate::keymap::SHORTCUTS.iter().find(|s| s.id == "dictate") {
            page = page.child(self.editable_row(shortcut, p, cx));
        }
        let Some(status) = self.voice.status.clone() else {
            return page
                .child(
                    div()
                        .py_4()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child("Asking for the models…"),
                )
                .into_any_element();
        };
        if let Some(why) = &status.unsupported {
            return page
                .child(
                    div()
                        .py_4()
                        .text_sm()
                        .text_color(p.warning)
                        .child(why.clone()),
                )
                .into_any_element();
        }

        page = page.child(heading(
            "Speech model",
            "Downloaded once to this computer. Live preview models show the text while you speak; the others when you stop. Word errors are the share of English words the model got wrong in its benchmark; speed is the time it takes for 30 seconds of speech on this Mac's GPU.",
            p,
        ));
        let rows = status
            .models
            .iter()
            .map(|model| {
                let selected = model.id == voice.model;
                self.model_row("speech", model, selected, true, p, cx)
            })
            .collect();
        page = page.child(model_list(
            "speech-models",
            &self.voice.speech_scroll,
            rows,
            p,
        ));

        let language_label = LANGUAGES
            .iter()
            .find(|(code, _)| *code == voice.language)
            .map(|(_, name)| (*name).to_owned())
            .unwrap_or_else(|| voice.language.clone());
        let view = cx.entity();
        let current = voice.language.clone();
        page = page
            .child(row(
                "Language",
                "What you speak. Detect automatically works with models that know several languages; a model that knows one ignores this.",
                Button::new("voice-language")
                    .cursor_pointer()
                    .outline()
                    .small()
                    .label(language_label)
                    .dropdown_menu(move |mut menu, _, _| {
                        for (code, name) in LANGUAGES {
                            let view = view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(*name)
                                    .checked(*code == current)
                                    .on_click(move |_, _, cx| {
                                        let value = if *code == "auto" {
                                            SettingValue::Unset
                                        } else {
                                            SettingValue::Str((*code).to_owned())
                                        };
                                        view.update(cx, |this, cx| {
                                            this.write(&["voice", "language"], value, true, cx)
                                        });
                                    }),
                            );
                        }
                        menu
                    })
                    .into_any_element(),
                p,
            ))
            .child(row(
                "Words to spell right",
                "Names and terms the model should get right, separated by commas (models that take hints use them).",
                text_input(&self.input(Field::VoiceWords)),
                p,
            ));

        page = page
            .child(heading(
                "Memory",
                "A model is loaded when you dictate and unloaded once it has gone unused for a while, giving its memory back.",
                p,
            ))
            .child(row(
                "Unload the model",
                &loaded_hint(&status),
                h_flex()
                    .gap_2()
                    .when(!status.loaded.is_empty(), |d| {
                        d.child(
                            Button::new("voice-unload")
                                .cursor_pointer()
                                .ghost()
                                .small()
                                .label("Unload now")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.voice_request(Request::VoiceUnload, cx)
                                })),
                        )
                    })
                    .child(self.unload_picker(&voice, cx))
                    .into_any_element(),
                p,
            ));

        page = page
            .child(heading(
                "Cleanup",
                "Tidies the transcript before it goes in: punctuation, numbers, filler words, and corrections like \"make it 42, sorry, 24\". Only text is sent, never the recording. If it fails, the text goes in as heard; Skip does the same without waiting.",
                p,
            ))
            .child(row(
                "Tidy with",
                &cleanup_hint(&voice.cleanup),
                self.cleanup_picker(&voice, &status, cx),
                p,
            ));
        if !status.cleanup_models.is_empty() {
            let rows = status
                .cleanup_models
                .iter()
                .map(|model| {
                    let selected = voice.cleanup.trim() == format!("local:{}", model.id);
                    self.model_row("cleanup", model, selected, false, p, cx)
                })
                .collect();
            page = page.child(div().pt_3().child(model_list(
                "cleanup-models",
                &self.voice.cleanup_scroll,
                rows,
                p,
            )));
        }
        if voice.cleanup.trim() != "off" && !voice.cleanup.trim().is_empty() {
            page = page.child(row(
                "Cleanup instructions",
                "Added to the instructions, e.g. \"Use British spelling\" or \"Keep it informal\".",
                text_input(&self.input(Field::VoiceInstructions)),
                p,
            ));
        }
        page.into_any_element()
    }

    /// One model: what it is, and download / use / remove.
    fn model_row(
        &self,
        kind: &'static str,
        model: &VoiceModel,
        selected: bool,
        speech: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut facts = Vec::new();
        if model.size > 0 {
            facts.push(size_label(model.size));
        }
        if speech {
            facts.push(languages_label(&model.languages));
        }
        if let Some(err) = model.english_error {
            facts.push(format!("{err:.0}% word errors"));
        }
        if let Some(secs) = model.seconds_per_30s {
            facts.push(format!("{secs:.1} s per 30 s"));
        }
        let id = model.id.clone();
        let control: AnyElement = match &model.state {
            VoiceModelState::NotDownloaded => {
                let id = id.clone();
                Button::new(SharedString::from(format!("{kind}-get-{}", model.id)))
                    .cursor_pointer()
                    .outline()
                    .small()
                    .label("Download")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.voice_request(Request::VoiceDownload { model: id.clone() }, cx)
                    }))
                    .into_any_element()
            }
            VoiceModelState::Downloading { done, total } => {
                let id = id.clone();
                let pct = if *total > 0 {
                    (*done as f64 / *total as f64 * 100.0) as u32
                } else {
                    0
                };
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(90.))
                            .h(px(4.))
                            .rounded_full()
                            .bg(p.border)
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(p.accent)
                                    .w(px(0.9 * pct as f32)),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child(format!("{pct}%")),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{kind}-stop-{}", model.id)))
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.voice_request(
                                    Request::VoiceCancelDownload { model: id.clone() },
                                    cx,
                                )
                            })),
                    )
                    .into_any_element()
            }
            VoiceModelState::Verifying => div()
                .text_xs()
                .text_color(p.muted_foreground)
                .child("Checking…")
                .into_any_element(),
            VoiceModelState::Installed => {
                let remove_id = id.clone();
                let use_id = id.clone();
                let mut controls = h_flex().gap_2().items_center();
                if model.id != loft_voice_fake() {
                    controls = controls.child(
                        Button::new(SharedString::from(format!("{kind}-rm-{}", model.id)))
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .label("Remove")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_voice_model(kind, &remove_id, cx)
                            })),
                    );
                }
                if selected {
                    controls = controls.child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.accent)
                            .child("In use"),
                    );
                } else {
                    controls = controls.child(
                        Button::new(SharedString::from(format!("{kind}-use-{}", model.id)))
                            .cursor_pointer()
                            .outline()
                            .small()
                            .label("Use")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.use_voice_model(kind, &use_id, cx)
                            })),
                    );
                }
                controls.into_any_element()
            }
        };
        let loaded = self
            .voice
            .status
            .as_ref()
            .is_some_and(|s| s.loaded.contains(&model.id));
        h_flex()
            .gap_4()
            .items_center()
            .py_2p5()
            .border_b_1()
            .border_color(p.border.opacity(0.6))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(if selected {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .child(super::selectable(model.name.clone())),
                            )
                            .when(model.recommended, |d| d.child(badge("Recommended", p)))
                            .when(model.live, |d| d.child(badge("Live preview", p)))
                            .when(loaded, |d| d.child(badge("In memory", p))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child(super::selectable(model.description.clone())),
                    )
                    .when(!facts.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(p.faint_foreground)
                                .child(super::selectable(facts.join(" · "))),
                        )
                    })
                    .when_some(model.error.clone(), |d, err| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(p.danger)
                                .child(super::selectable(err)),
                        )
                    }),
            )
            .child(div().flex_none().child(control))
            .into_any_element()
    }

    fn use_voice_model(&mut self, kind: &str, id: &str, cx: &mut Context<Self>) {
        if kind == "speech" {
            self.write(
                &["voice", "model"],
                SettingValue::Str(id.to_owned()),
                true,
                cx,
            );
        } else {
            self.write(
                &["voice", "cleanup"],
                SettingValue::Str(format!("local:{id}")),
                true,
                cx,
            );
        }
    }

    fn remove_voice_model(&mut self, kind: &str, id: &str, cx: &mut Context<Self>) {
        // A model that is gone can't be the one in use.
        if kind == "speech" && self.config.voice.model == id {
            self.write(&["voice", "model"], SettingValue::Unset, true, cx);
        }
        if kind == "cleanup" && self.config.voice.cleanup.trim() == format!("local:{id}") {
            self.write(&["voice", "cleanup"], SettingValue::Unset, true, cx);
        }
        self.voice_request(
            Request::VoiceRemoveModel {
                model: id.to_owned(),
            },
            cx,
        );
    }

    fn unload_picker(&self, voice: &VoiceConfig, cx: &mut Context<Self>) -> AnyElement {
        let current = voice.unload_after.trim().to_owned();
        let label = VoiceConfig::UNLOAD_CHOICES
            .iter()
            .find(|(id, _)| *id == current)
            .map(|(_, label)| *label)
            .unwrap_or("After 5 minutes");
        let view = cx.entity();
        Button::new("voice-unload-after")
            .cursor_pointer()
            .outline()
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                for (id, label) in VoiceConfig::UNLOAD_CHOICES {
                    let view = view.clone();
                    let checked = id == current || (id == "5m" && current.is_empty());
                    menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                        move |_, _, cx| {
                            let value = if id == "5m" {
                                SettingValue::Unset
                            } else {
                                SettingValue::Str(id.to_owned())
                            };
                            view.update(cx, |this, cx| {
                                this.write(&["voice", "unload_after"], value, true, cx)
                            });
                        },
                    ));
                }
                menu
            })
            .into_any_element()
    }

    fn cleanup_picker(
        &self,
        voice: &VoiceConfig,
        status: &VoiceStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = voice.cleanup.trim().to_owned();
        let store = self.store.read(cx);
        // Agents installed here, each with its small models.
        let agents: Vec<AgentModels> = AGENTS
            .iter()
            .filter(|(id, _, _)| {
                store
                    .harness_status
                    .get(&HarnessId((*id).to_owned()))
                    .is_none_or(|s| s.installed)
                    && !store.disabled_agents.contains(*id)
            })
            .map(|(id, name, small)| {
                let mut models: Vec<(String, String)> = Vec::new();
                if let Some(small) = small {
                    models.push(((*small).to_owned(), capitalized(small)));
                }
                for m in store
                    .models
                    .get(&HarnessId((*id).to_owned()))
                    .into_iter()
                    .flatten()
                {
                    let words = format!("{} {}", m.id, m.display_name).to_lowercase();
                    if CHEAP.iter().any(|w| words.contains(w))
                        && !models.iter().any(|(id, _)| *id == m.id)
                    {
                        models.push((m.id.clone(), m.display_name.clone()));
                    }
                }
                (*id, *name, models)
            })
            .collect();
        let label = cleanup_label(&current, status, &agents);
        let locals: Vec<(String, String)> = status
            .cleanup_models
            .iter()
            .map(|m| (m.id.clone(), m.name.clone()))
            .collect();
        let view = cx.entity();
        Button::new("voice-cleanup")
            .cursor_pointer()
            .outline()
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                let set = |value: SettingValue| {
                    let view = view.clone();
                    move |_: &gpui_kit::ClickEvent,
                          _: &mut gpui_kit::Window,
                          cx: &mut gpui_kit::App| {
                        view.update(cx, |this, cx| {
                            this.write(&["voice", "cleanup"], value.clone(), true, cx)
                        });
                    }
                };
                let off = current.is_empty() || current == "off";
                menu = menu.item(
                    PopupMenuItem::new("Off: as heard")
                        .checked(off)
                        .on_click(set(SettingValue::Unset)),
                );
                if !locals.is_empty() {
                    menu = menu.separator().label("On this computer (free, private)");
                    for (id, name) in &locals {
                        let value = format!("local:{id}");
                        menu = menu.item(
                            PopupMenuItem::new(name.clone())
                                .checked(current == value)
                                .on_click(set(SettingValue::Str(value))),
                        );
                    }
                }
                if !agents.is_empty() {
                    menu = menu
                        .separator()
                        .label("With your plan (text goes to the agent)");
                    for (id, name, models) in &agents {
                        for (model, model_name) in models {
                            let value = format!("{id}:{model}");
                            menu = menu.item(
                                PopupMenuItem::new(format!("{name} · {model_name}"))
                                    .checked(current == value)
                                    .on_click(set(SettingValue::Str(value))),
                            );
                        }
                        let value = (*id).to_owned();
                        menu = menu.item(
                            PopupMenuItem::new(format!("{name} · its default model"))
                                .checked(current == value)
                                .on_click(set(SettingValue::Str(value))),
                        );
                    }
                }
                menu
            })
            .into_any_element()
    }

    fn voice_request(&mut self, request: Request, cx: &mut Context<Self>) {
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let pending = client.request(request);
        cx.spawn(async move |this, cx| {
            let result = pending.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Response::Voice { status }) => this.voice.status = Some(status),
                    Ok(_) => {}
                    Err(err) => this.error = Some(err.message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// While the page shows, keep the models' state current (quickly while
    /// one downloads).
    fn poll_voice(&mut self, cx: &mut Context<Self>) {
        if self.voice.polling.is_some() {
            return;
        }
        // The agents' small models, for the cleanup picker.
        self.store.update(cx, |s, cx| {
            for (id, _, _) in AGENTS {
                s.ensure_models(HarnessId((*id).to_owned()), cx);
            }
        });
        self.voice.polling = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(client)) = this.update(cx, |this, cx| {
                    (this.section == super::Section::Voice)
                        .then(|| this.store.read(cx).client())
                        .flatten()
                }) else {
                    break;
                };
                let result = client.request(Request::VoiceStatus).await;
                let busy = matches!(&result, Ok(Response::Voice { status })
                    if status.models.iter().chain(&status.cleanup_models).any(|m| m.state.is_busy()));
                let Ok(()) = this.update(cx, |this, cx| {
                    if let Ok(Response::Voice { status }) = result
                        && this.voice.status.as_ref() != Some(&status)
                    {
                        this.voice.status = Some(status);
                        cx.notify();
                    }
                }) else {
                    break;
                };
                let wait = if busy { 500 } else { 2000 };
                cx.background_executor()
                    .timer(Duration::from_millis(wait))
                    .await;
            }
            let _ = this.update(cx, |this, _| this.voice.polling = None);
        }));
    }
}

/// Models in a box of their own that scrolls once it holds more than
/// about four, so the settings below stay in reach. The wheel over it
/// scrolls the box, and the page only once the box can go no further that
/// way (as a browser chains scrolling).
fn model_list(
    id: &'static str,
    scroll: &gpui_kit::ScrollHandle,
    rows: Vec<AnyElement>,
    p: &Palette,
) -> AnyElement {
    let handle = scroll.clone();
    div()
        .id(SharedString::from(format!("{id}-box")))
        .mt_2()
        // The box has scrolled by now (its own listener comes first); the
        // page has a turn only at the box's ends.
        .on_scroll(move |event, window, cx| {
            let dy = event.delta.pixel_delta(window.line_height()).y;
            let (offset, max) = (handle.offset().y, handle.max_offset().y);
            let at_top = offset >= px(0.);
            let at_bottom = -offset >= max;
            let goes_up = dy > px(0.);
            if max > px(0.) && !(goes_up && at_top) && !(!goes_up && at_bottom) {
                cx.stop_propagation();
            }
        })
        .child(
            div()
                .id(id)
                .max_h(px(300.))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .px_3()
                .rounded(px(10.))
                .border_1()
                .border_color(p.border)
                .children(rows),
        )
        .into_any_element()
}

/// The id of the daemon's test model (`AGENTLOFT_FAKE_VOICE`).
fn loft_voice_fake() -> &'static str {
    "fake"
}

fn badge(text: &'static str, p: &Palette) -> impl gpui_kit::IntoElement {
    div()
        .px_1p5()
        .rounded(px(4.))
        .bg(p.accent.opacity(0.12))
        .text_color(p.accent)
        .text_xs()
        .child(text)
}

fn size_label(bytes: u64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 1000.0 {
        format!("{:.1} GB", mb / 1000.0)
    } else {
        format!("{mb:.0} MB")
    }
}

fn languages_label(languages: &[String]) -> String {
    match languages {
        [] => "Any language".to_owned(),
        [one] => LANGUAGES
            .iter()
            .find(|(code, _)| code == one)
            .map(|(_, name)| format!("{name} only"))
            .unwrap_or_else(|| format!("{one} only")),
        many if many.len() <= 4 => many
            .iter()
            .map(|code| {
                LANGUAGES
                    .iter()
                    .find(|(c, _)| c == code)
                    .map(|(_, name)| (*name).to_owned())
                    .unwrap_or_else(|| code.clone())
            })
            .collect::<Vec<_>>()
            .join(", "),
        many => format!("{} languages", many.len()),
    }
}

fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn loaded_hint(status: &VoiceStatus) -> String {
    let names: Vec<&str> = status
        .loaded
        .iter()
        .filter_map(|id| {
            status
                .models
                .iter()
                .chain(&status.cleanup_models)
                .find(|m| &m.id == id)
                .map(|m| m.name.as_str())
        })
        .collect();
    if names.is_empty() {
        "Nothing is loaded now.".to_owned()
    } else {
        format!("Loaded now: {}.", names.join(", "))
    }
}

fn cleanup_hint(setting: &str) -> String {
    match setting.split_once(':').map(|(k, _)| k).unwrap_or(setting) {
        "" | "off" => "Off: the text goes in as the speech model heard it.".to_owned(),
        "local" => "A small model on this computer: free, and nothing leaves it. Unloaded like the speech model.".to_owned(),
        _ => "The agent's CLI on your plan, without tools: a few seconds per message, and the text goes to its provider.".to_owned(),
    }
}

fn cleanup_label(current: &str, status: &VoiceStatus, agents: &[AgentModels]) -> String {
    if current.is_empty() || current == "off" {
        return "Off".to_owned();
    }
    if let Some(id) = current.strip_prefix("local:") {
        return status
            .cleanup_models
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| id.to_owned());
    }
    let (agent, model) = current.split_once(':').unwrap_or((current, ""));
    let name = AGENTS
        .iter()
        .find(|(id, _, _)| *id == agent)
        .map(|(_, name, _)| *name)
        .unwrap_or(agent);
    if model.is_empty() {
        return name.to_owned();
    }
    let model_name = agents
        .iter()
        .find(|(id, _, _)| *id == agent)
        .and_then(|(_, _, models)| models.iter().find(|(id, _)| id == model))
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| model.to_owned());
    format!("{name} · {model_name}")
}
