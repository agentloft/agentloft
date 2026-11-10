//! Settings → Shortcuts: every shortcut, its keys, and a way to change them.
//! Click the keys, press the new ones (Esc cancels); they're saved under
//! `[ui.keys]` in config.toml and work straight away.

use super::SettingsView;
use crate::keymap::{self, SHORTCUTS};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, div,
    prelude::FluentBuilder as _, px,
};
use loft_config::SettingValue;

#[derive(Default)]
pub(super) struct ShortcutsPage {
    /// The shortcut waiting for its new keys.
    recording: Option<&'static str>,
    /// Takes the next keystroke (before any shortcut can act on it).
    _listening: Option<Subscription>,
}

impl SettingsView {
    fn record(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        let listening = cx.intercept_keystrokes(move |event, _, cx: &mut App| {
            let k = &event.keystroke;
            let plain = !k.modifiers.modified();
            let keys = if plain && k.key == "escape" {
                None
            } else {
                match keymap::from_keystroke(k, cx) {
                    Some(keys) => Some(keys),
                    // A modifier on its own: wait for the rest.
                    None => return,
                }
            };
            cx.stop_propagation();
            let _ = view.update(cx, |this, cx| {
                let Some(id) = this.shortcuts.recording.take() else {
                    return;
                };
                // Dropped after this callback returns.
                let listener = this.shortcuts._listening.take();
                cx.defer(move |_| drop(listener));
                if let Some(keys) = keys {
                    this.set_keys(id, SettingValue::Str(keys), cx);
                }
                cx.notify();
            });
        });
        self.shortcuts.recording = Some(id);
        self.shortcuts._listening = Some(listening);
        cx.notify();
    }

    fn set_keys(&mut self, id: &str, value: SettingValue, cx: &mut Context<Self>) {
        // Back to the default: no entry at all.
        let default = SHORTCUTS.iter().find(|s| s.id == id).map(|s| s.default);
        let value = match value {
            SettingValue::Str(k)
                if Some(keymap::normalize(&k)) == default.map(keymap::normalize) =>
            {
                SettingValue::Unset
            }
            v => v,
        };
        self.write(&["ui", "keys", id], value, false, cx);
    }

    fn reset_all_keys(&mut self, cx: &mut Context<Self>) {
        self.write(&["ui", "keys"], SettingValue::Unset, false, cx);
    }

    pub(super) fn shortcuts_page(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let any_custom = SHORTCUTS.iter().any(|s| keymap::is_custom(s.id, cx));
        let mut page = v_flex().child(
            h_flex()
                .gap_4()
                .items_center()
                .py_3()
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child("Click a shortcut and press the keys you want. Esc cancels."),
                )
                .when(any_custom, |d| {
                    d.child(
                        h_flex()
                            .id("keys-reset-all")
                            .gap_1p5()
                            .items_center()
                            .px_2p5()
                            .py_1()
                            .rounded(px(7.))
                            .border_1()
                            .border_color(p.border)
                            .text_sm()
                            .cursor_pointer()
                            .hover(|d| d.bg(p.hover))
                            .child(Icon::new(IconName::RotateCcw).xsmall())
                            .child("Reset all")
                            .on_click(cx.listener(|this, _, _, cx| this.reset_all_keys(cx))),
                    )
                }),
        );
        for group in keymap::GROUPS {
            page = page.child(
                div()
                    .pt_5()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.muted_foreground)
                    .child(group.to_uppercase()),
            );
            for (_, label, keys) in keymap::FIXED.iter().filter(|(g, ..)| g == group) {
                page = page.child(shortcut_row(label, None, fixed_keys(keys, p), p));
            }
            for s in SHORTCUTS.iter().filter(|s| s.group == *group) {
                page = page.child(self.editable_row(s, p, cx));
            }
        }
        page.into_any_element()
    }

    fn editable_row(
        &self,
        s: &'static keymap::Shortcut,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = s.id;
        let recording = self.shortcuts.recording == Some(id);
        let keys = keymap::keys_for(id, cx);
        let custom = keymap::is_custom(id, cx);
        let clashes = keys
            .as_deref()
            .map(|k| keymap::clashes(id, k, cx))
            .unwrap_or_default();
        let note = (!clashes.is_empty()).then(|| format!("Also used by {}", clashes.join(", ")));

        let cap = h_flex()
            .id(SharedString::from(format!("keys-{id}")))
            .min_w(px(96.))
            .h(px(28.))
            .px_2()
            .justify_center()
            .items_center()
            .rounded(px(7.))
            .border_1()
            .text_sm()
            .cursor_pointer()
            .map(|d| {
                if recording {
                    d.border_color(p.accent)
                        .bg(p.accent.opacity(0.08))
                        .text_color(p.accent)
                        .child("Press keys…")
                } else {
                    match &keys {
                        Some(k) => d
                            .border_color(p.border)
                            .bg(p.elevated)
                            .hover(|d| d.border_color(p.accent.opacity(0.6)))
                            .font_weight(FontWeight::MEDIUM)
                            .child(keymap::display(k)),
                        None => d
                            .border_color(p.border)
                            .border_dashed()
                            .text_color(p.faint_foreground)
                            .hover(|d| d.text_color(p.foreground))
                            .child("Not set"),
                    }
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.shortcuts.recording == Some(id) {
                    this.shortcuts.recording = None;
                    this.shortcuts._listening = None;
                    cx.notify();
                } else {
                    this.record(id, cx);
                }
            }));

        // `on_hover`: shown only while the row is hovered.
        let icon_button =
            |name: &'static str, icon: IconName, tip: &'static str, on_hover: bool| {
                div()
                    .id(SharedString::from(format!("keys-{name}-{id}")))
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(on_hover, |d| {
                        d.invisible().group_hover("shortcut-row", |d| d.visible())
                    })
                    .text_color(p.muted_foreground)
                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    .child(Icon::new(icon).xsmall())
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
                    })
            };
        let controls = h_flex()
            .gap_1()
            .items_center()
            // Fixed slots, so the keys line up down the page.
            .child(div().w(px(26.)).when(keys.is_some() && !recording, |d| {
                d.child(
                    icon_button("clear", IconName::X, "Turn off", true).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.set_keys(id, SettingValue::Str(String::new()), cx)
                        },
                    )),
                )
            }))
            .child(div().w(px(26.)).when(custom && !recording, |d| {
                d.child(
                    icon_button("reset", IconName::RotateCcw, "Back to the default", false)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_keys(id, SettingValue::Unset, cx)
                        })),
                )
            }))
            .child(cap);
        shortcut_row(s.label, note, controls.into_any_element(), p)
    }
}

fn fixed_keys(keys: &str, p: &Palette) -> AnyElement {
    h_flex()
        .gap_1()
        .items_center()
        .child(
            div()
                .text_xs()
                .text_color(p.faint_foreground)
                .mr_1()
                .child("Built in"),
        )
        .child(
            h_flex()
                .min_w(px(96.))
                .h(px(28.))
                .px_2()
                .justify_center()
                .items_center()
                .rounded(px(7.))
                .bg(p.muted)
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.muted_foreground)
                .child(keymap::display(keys)),
        )
        .into_any_element()
}

fn shortcut_row(label: &str, note: Option<String>, control: AnyElement, p: &Palette) -> AnyElement {
    h_flex()
        .group("shortcut-row")
        .gap_6()
        .items_center()
        .py(px(7.))
        .border_b_1()
        .border_color(p.border.opacity(0.6))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().child(label.to_owned()))
                .when_some(note, |d, note| {
                    d.child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .text_color(p.warning)
                            .child(Icon::new(IconName::TriangleAlert).xsmall())
                            .child(note),
                    )
                }),
        )
        .child(div().flex_shrink_0().child(control))
        .into_any_element()
}
