//! A thread's browser: its own set of tabs (agents drive the active one).

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loft_browser::{BrowserEvent, BrowserView};

/// What the tabs pass up: the active page's notes and screenshots.
pub enum BrowserTabsEvent {
    Annotation(loft_browser::Annotation),
    Screenshot(Vec<u8>),
}

pub struct BrowserTabs {
    tabs: Vec<(Entity<BrowserView>, [Subscription; 2])>,
    active: usize,
}

impl EventEmitter<BrowserTabsEvent> for BrowserTabs {}

impl BrowserTabs {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
        };
        this.push(window, cx);
        this
    }

    fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<BrowserView> {
        let view = cx.new(|cx| BrowserView::new(window, cx));
        let subs = [
            cx.observe(&view, |_, _, cx| cx.notify()),
            cx.subscribe(&view, |_, _, event, cx| match event {
                BrowserEvent::Annotation(note) => {
                    cx.emit(BrowserTabsEvent::Annotation(note.clone()))
                }
                BrowserEvent::Screenshot(png) => cx.emit(BrowserTabsEvent::Screenshot(png.clone())),
                _ => cx.notify(),
            }),
        ];
        self.tabs.push((view.clone(), subs));
        self.active = self.tabs.len() - 1;
        view
    }

    /// The tab agents and the toolbar act on.
    pub fn active(&self) -> Entity<BrowserView> {
        self.tabs[self.active.min(self.tabs.len() - 1)].0.clone()
    }

    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.push(window, cx);
        view.update(cx, |b, cx| b.focus_address_bar(window, cx));
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.tabs.len() && ix != self.active {
            // Notes are per page: stop annotating the tab we leave.
            self.active()
                .update(cx, |b, cx| b.set_annotating(false, cx));
            self.active = ix;
            cx.notify();
        }
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            // The last tab: start over with a blank one.
            self.tabs.clear();
            self.new_tab(window, cx);
            return;
        }
        drop(self.tabs.remove(ix));
        if self.active >= self.tabs.len() || self.active > ix {
            self.active = self.active.saturating_sub(1);
        }
        cx.notify();
    }
}

/// "github.com" for a URL, for untitled pages.
fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| host.trim_start_matches("www.").to_owned())
}

impl Render for BrowserTabs {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let mut strip = h_flex()
            .id("browser-tabs")
            .h(px(34.))
            .flex_shrink_0()
            .px_1p5()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .bg(p.sidebar);
        for (ix, (view, _)) in self.tabs.iter().enumerate() {
            let active = ix == self.active;
            let (title, loading) = {
                let b = view.read(cx);
                let title = b
                    .title()
                    .filter(|t| !t.trim().is_empty())
                    .map(str::to_owned)
                    .or_else(|| b.url().and_then(host))
                    .unwrap_or_else(|| "New Tab".into());
                (title, b.is_loading())
            };
            let icon = if loading {
                Spinner::new()
                    .xsmall()
                    .color(p.muted_foreground)
                    .into_any_element()
            } else {
                Icon::new(IconName::Globe).xsmall().into_any_element()
            };
            strip = strip.child(
                h_flex()
                    .id(SharedString::from(format!("browser-tab-{ix}")))
                    .group("browser-tab")
                    .flex_1()
                    .min_w(px(64.))
                    .max_w(px(200.))
                    .h(px(24.))
                    .pl_2()
                    .pr_1()
                    .gap_1p5()
                    .items_center()
                    .rounded(px(7.))
                    .text_xs()
                    .cursor_pointer()
                    .when(active, |d| {
                        d.bg(p.background)
                            .text_color(p.foreground)
                            .border_1()
                            .border_color(p.border)
                    })
                    .when(!active, |d| {
                        d.text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    })
                    .child(div().flex_none().child(icon))
                    .child(div().flex_1().min_w_0().truncate().child(title))
                    .child(
                        div()
                            .id(SharedString::from(format!("browser-tab-x-{ix}")))
                            .flex_none()
                            .cursor_pointer()
                            .p(px(2.))
                            .rounded(px(4.))
                            .when(!active, |d| {
                                d.invisible().group_hover("browser-tab", |d| d.visible())
                            })
                            .hover(|d| d.bg(p.border))
                            .child(Icon::new(IconName::X).xsmall())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, window, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx))),
            );
        }
        strip = strip.child(
            div()
                .id("browser-tab-new")
                .flex_none()
                .p(px(4.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(p.muted_foreground)
                .hover(|d| d.bg(p.hover).text_color(p.foreground))
                .child(Icon::new(IconName::Plus).xsmall())
                .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
        );
        v_flex()
            .size_full()
            .child(strip)
            .child(div().flex_1().min_h_0().child(self.active()))
    }
}

#[cfg(test)]
mod tests {
    use super::host;

    #[test]
    fn hosts_for_untitled_tabs() {
        assert_eq!(
            host("https://www.youtube.com/watch?v=1").as_deref(),
            Some("youtube.com")
        );
        assert_eq!(
            host("http://localhost:3000/").as_deref(),
            Some("localhost:3000")
        );
        assert_eq!(host("about:blank"), None);
    }
}
