//! Settings ▸ Threads: how a new thread gets its title (`[titles]`). After
//! the first message an agent writes a short one; until then, or with this
//! off, the message's first line is the title.

use super::{Field, SettingsView, agents::AGENTS, row, text_input};
use crate::theme::Palette;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Sizable as _, v_flex};
use gpui_kit::{AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _};
use loft_config::SettingValue;

/// `titles.agent` for "the thread's own agent".
const SAME_AGENT: &str = "thread";

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
        page.into_any_element()
    }
}
