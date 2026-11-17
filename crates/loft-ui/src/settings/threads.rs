//! Settings ▸ Threads: how a new thread gets its title (`[titles]`). After
//! the first message an agent writes a short one; until then, or with this
//! off, the message's first line is the title. And when archived threads
//! are deleted for good (`[threads] delete_archived_after_days`), and
//! whether threads a usage limit stopped carry on at its reset
//! (`[threads] resume_at_reset`). And whether the empty composer offers
//! the agent's guess at the next message (`[threads] suggest_next`).

use super::{Field, SettingsView, agents::AGENTS, row, text_input, web::heading};
use crate::theme::Palette;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Sizable as _, v_flex};
use gpui_kit::{AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _};
use loft_config::SettingValue;

/// `titles.agent` for "the thread's own agent".
const SAME_AGENT: &str = "thread";

/// Picks for deleting archived threads: days (0 = never) and their label.
const ARCHIVE_PRESETS: [(u32, &str); 4] = [
    (0, "Never"),
    (1, "After a day"),
    (7, "After a week"),
    (30, "After a month"),
];

fn archive_label(days: u32) -> String {
    match ARCHIVE_PRESETS.iter().find(|(d, _)| *d == days) {
        Some((_, label)) => (*label).to_owned(),
        None => format!("After {days} days"),
    }
}

impl SettingsView {
    pub(super) fn threads_page(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let titles = &self.config.titles;
        let generate = titles.generate;
        let current = titles.agent.trim().to_owned();
        let label = AGENTS
            .iter()
            .find(|(id, _)| *id == current)
            .map(|(_, name)| *name)
            .unwrap_or("Same as the thread");
        let view = cx.entity();
        let picker = Button::new("titles-agent")
            .cursor_pointer()
            .outline()
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                let same = !AGENTS.iter().any(|(id, _)| *id == current);
                let options = std::iter::once((SAME_AGENT, "Same as the thread", same)).chain(
                    AGENTS
                        .iter()
                        .map(|(id, name)| (*id, *name, *id == current.as_str())),
                );
                for (i, (id, name, checked)) in options.enumerate() {
                    let view = view.clone();
                    if i == 1 {
                        menu = menu.separator();
                    }
                    menu = menu.item(PopupMenuItem::new(name).checked(checked).on_click(
                        move |_, _, cx| {
                            let value = if id == SAME_AGENT {
                                SettingValue::Unset
                            } else {
                                SettingValue::Str(id.to_owned())
                            };
                            view.update(cx, |this, cx| {
                                this.write(&["titles", "agent"], value, true, cx)
                            });
                        },
                    ));
                }
                menu
            })
            .into_any_element();

        let mut page = v_flex().child(row(
            "Generate titles",
            "After your first message, an agent names the thread in a few words. Off: the first line of the message is the title.",
            Switch::new("titles-generate")
                .checked(generate)
                .on_click(cx.listener(|this, on: &bool, _, cx| {
                    // On is the default.
                    let value = if *on {
                        SettingValue::Unset
                    } else {
                        SettingValue::Bool(false)
                    };
                    this.write(&["titles", "generate"], value, true, cx);
                }))
                .into_any_element(),
            p,
        ));
        if generate {
            page = page
                .child(row(
                    "Title agent",
                    "Which agent writes titles. Same as the thread: the agent the thread runs on.",
                    picker,
                    p,
                ))
                .child(row(
                    "Title model",
                    "Leave empty for a small, fast model: Haiku for Claude Code, the agent's default for the others.",
                    text_input(&self.input(Field::TitleModel)),
                    p,
                ))
                .child(row(
                    "Title instructions",
                    "Added to the request, e.g. \"Write titles in English\" or \"Use lowercase\".",
                    text_input(&self.input(Field::TitleInstructions)),
                    p,
                ));
        }
        page = page
            .child(heading(
                "Next message",
                "After each turn, the empty composer shows what you will likely send next. Tab puts it in, to send or edit.",
                p,
            ))
            .child(row(
                "Suggest the next message",
                "The thread's agent is asked once after each turn, a small request: Claude on Haiku, others on the thread's model.",
                Switch::new("threads-suggest-next")
                    .cursor_pointer()
                    .checked(self.config.threads.suggest_next)
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        // On is the default.
                        let value = if *on {
                            SettingValue::Unset
                        } else {
                            SettingValue::Bool(false)
                        };
                        this.write(&["threads", "suggest_next"], value, true, cx);
                    }))
                    .into_any_element(),
                p,
            ))
            .child(heading(
                "Usage limits",
                "A thread whose agent runs out of its plan's usage stops and says when the limit resets.",
                p,
            ))
            .child(row(
                "Carry on when the limit resets",
                "Limited threads go on by themselves a minute after the reset, then send what was queued. Off: each thread offers it on its own.",
                Switch::new("threads-resume-at-reset")
                    .cursor_pointer()
                    .checked(self.config.threads.resume_at_reset)
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        // Off is the default.
                        let value = if *on {
                            SettingValue::Bool(true)
                        } else {
                            SettingValue::Unset
                        };
                        this.write(&["threads", "resume_at_reset"], value, true, cx);
                    }))
                    .into_any_element(),
                p,
            ));
        page = page.child(heading(
            "Archive",
            "Archived threads leave the sidebar and wait under Archived, where you can bring them back.",
            p,
        ));
        page = page.child(row(
            "Delete archived threads",
            "Deletes a thread for good this long after it was archived: its conversation, worktree, checkpoints and attachments. Its branch is kept.",
            self.archive_picker(cx),
            p,
        ));
        let days = self.config.threads.delete_archived_after_days;
        let preset = ARCHIVE_PRESETS.iter().any(|(d, _)| *d == days);
        if self.archive_custom || !preset {
            page = page.child(row(
                "Days",
                "How many days after archiving, up to 3650. Press Enter to save; empty or 0 is never.",
                text_input(&self.input(Field::ArchiveDays)),
                p,
            ));
        }
        page.into_any_element()
    }

    fn archive_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let days = self.config.threads.delete_archived_after_days;
        let custom = self.archive_custom || !ARCHIVE_PRESETS.iter().any(|(d, _)| *d == days);
        let label = if custom && days == 0 {
            "Custom".to_owned()
        } else {
            archive_label(days)
        };
        let view = cx.entity();
        Button::new("archive-delete-after")
            .cursor_pointer()
            .outline()
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                for (preset, name) in ARCHIVE_PRESETS {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(name)
                            .checked(!custom && preset == days)
                            .on_click(move |_, _, cx| {
                                let value = if preset == 0 {
                                    SettingValue::Unset
                                } else {
                                    SettingValue::Int(i64::from(preset))
                                };
                                view.update(cx, |this, cx| {
                                    this.archive_custom = false;
                                    this.write(
                                        &["threads", "delete_archived_after_days"],
                                        value,
                                        true,
                                        cx,
                                    );
                                });
                            }),
                    );
                }
                let view = view.clone();
                menu.separator()
                    .item(PopupMenuItem::new("Custom…").checked(custom).on_click(
                        move |_, window, cx| {
                            view.update(cx, |this, cx| {
                                this.archive_custom = true;
                                let input = this.input(Field::ArchiveDays);
                                input.update(cx, |s, cx| {
                                    // Coming from a preset: start empty, so
                                    // typing doesn't add to its number.
                                    let text = if custom && days > 0 {
                                        days.to_string()
                                    } else {
                                        String::new()
                                    };
                                    s.set_value(text, window, cx);
                                    s.focus(window, cx);
                                });
                                cx.notify();
                            });
                        },
                    ))
            })
            .into_any_element()
    }
}
