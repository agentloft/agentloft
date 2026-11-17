//! The card on a tab its agent handed to the person (ADR-0017 §9): the
//! site, the agent's reason, and **Continue** (or, while the person shows
//! the agent something, **Done**). It is drawn by the app, not the page, so
//! a page can't press it; the page stays usable under it.

use super::PageView;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{AnyElement, Context, IntoElement, ParentElement as _, Styled as _, div};
use loft_protocol::{HandoffKind, Request};

impl PageView {
    /// The card, while the tab waits for the person.
    pub(super) fn handoff_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let info = self.info.as_ref().or(self.known.as_ref())?;
        let handoff = info.agent.handoff.clone()?;
        let id = info.id.clone();
        let agent = self
            .store
            .read(cx)
            .workspace
            .threads
            .iter()
            .find(|t| t.id == self.thread)
            .map(|t| crate::workspace::harness_name(&t.harness))
            .unwrap_or("The agent");
        let (icon, tint, headline, button) = match handoff.kind {
            HandoffKind::Handoff => (
                IconName::Hand,
                p.warning,
                format!("{agent} needs you here"),
                "Continue",
            ),
            HandoffKind::Demonstrate => (
                IconName::CircleDot,
                p.danger,
                "Recording your steps…".to_owned(),
                "Done",
            ),
        };
        let site = if handoff.origin.is_empty() {
            None
        } else {
            Some(handoff.origin.clone())
        };
        Some(
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(p.border)
                .bg(tint.opacity(0.12))
                .child(Icon::new(icon).small().text_color(tint))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_2()
                                .text_sm()
                                .child(
                                    div()
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .child(headline),
                                )
                                .children(site.map(|site| {
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(p.muted_foreground)
                                        .child(site)
                                })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.foreground)
                                .child(handoff.reason.clone()),
                        ),
                )
                .child(
                    Button::new("page-handoff-done")
                        .cursor_pointer()
                        .primary()
                        .small()
                        .label(button)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send(Request::PageHandoffDone { id: id.clone() }, cx);
                        })),
                )
                .into_any_element(),
        )
    }
}
