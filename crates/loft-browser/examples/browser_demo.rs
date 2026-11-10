//! The browser panel in an app-like window: an "agent" column on the left
//! that can drive the browser, and a side panel with Browser / Notes tabs.
//! Switching to Notes stops painting the browser, which must hide the
//! native web view.
//!
//! ```sh
//! cargo run -p loft-browser --example browser_demo -- [URL] [--self-test]
//! ```
//!
//! `--self-test` resizes the window and switches tabs on a timer, printing a
//! `STEP` line before each change, so screenshots can check that the web
//! view stays aligned and hides with its tab. It quits when done.

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, WindowBounds, WindowOptions, div,
    prelude::FluentBuilder as _, px, size,
};
use loft_browser::{BrowserCommand, BrowserEvent, BrowserView};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Browser,
    Notes,
}

/// One `--self-test` step: an optional resize and tab switch.
struct Step {
    label: &'static str,
    resize: Option<(f32, f32)>,
    tab: Option<Tab>,
}

impl Step {
    fn new(label: &'static str, resize: Option<(f32, f32)>, tab: Option<Tab>) -> Self {
        Self { label, resize, tab }
    }
}

struct Demo {
    browser: Entity<BrowserView>,
    tab: Tab,
    output: SharedString,
    status: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl Demo {
    fn new(url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let browser = cx.new(|cx| BrowserView::new(window, cx));
        browser.update(cx, |b, cx| b.navigate(&url, cx));
        let subscriptions = vec![cx.subscribe(&browser, |this, _, event, cx| {
            this.status = match event {
                BrowserEvent::UrlChanged(url) => format!("url: {url}"),
                BrowserEvent::TitleChanged(title) => format!("title: {title}"),
                BrowserEvent::LoadingChanged(true) => "loading…".into(),
                BrowserEvent::LoadingChanged(false) => "loaded".into(),
                BrowserEvent::Annotation(a) => format!("note on {}: {}", a.selector, a.note),
                BrowserEvent::Screenshot(png) => format!("screenshot: {} bytes", png.len()),
                BrowserEvent::AnnotatingStopped => "annotation mode off".into(),
            }
            .into();
            println!("event: {}", this.status);
            cx.notify();
        })];
        Self {
            browser,
            tab: Tab::Browser,
            output: "Use the buttons above to run commands against the page.".into(),
            status: SharedString::default(),
            _subscriptions: subscriptions,
        }
    }

    fn run(&mut self, cmd: BrowserCommand, window: &mut Window, cx: &mut Context<Self>) {
        let label = cmd.action();
        let task = self
            .browser
            .update(cx, |browser, cx| browser.execute(cmd, window, cx));
        self.output = format!("{label}…").into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let text = match task.await {
                Ok(value) => match value.get("snapshot").and_then(|s| s.as_str()) {
                    Some(snapshot) => snapshot.to_owned(),
                    None => {
                        let mut value = value;
                        if let Some(data) = value.get_mut("data") {
                            *data = format!("<{} base64 chars>", data.as_str().map_or(0, str::len))
                                .into();
                        }
                        serde_json::to_string_pretty(&value).unwrap_or_default()
                    }
                },
                Err(err) => format!("error: {err:#}"),
            };
            this.update(cx, |this, cx| {
                this.output = text.into();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    fn tab_button(&self, tab: Tab, label: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let active = self.tab == tab;
        let theme = cx.theme();
        div()
            .id(label)
            .px_3()
            .py_1()
            .rounded(px(6.))
            .text_sm()
            .cursor_pointer()
            .when(active, |d| {
                d.bg(theme.secondary).font_weight(FontWeight::MEDIUM)
            })
            .text_color(if active {
                theme.foreground
            } else {
                theme.muted_foreground
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
            .into_any_element()
    }
}

impl Render for Demo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, sidebar, border, muted, mono) = (
            theme.background,
            theme.sidebar,
            theme.border,
            theme.muted_foreground,
            theme.mono_font_family.clone(),
        );

        let agent = v_flex()
            .w(px(380.))
            .h_full()
            .flex_shrink_0()
            .bg(sidebar)
            .border_r_1()
            .border_color(border)
            .child(
                div()
                    .px_4()
                    .pt_4()
                    .pb_2()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Agent"),
            )
            .child(
                h_flex()
                    .px_3()
                    .pb_3()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Button::new("snapshot")
                            .small()
                            .outline()
                            .label("Snapshot")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.run(BrowserCommand::Snapshot, window, cx)
                            })),
                    )
                    .child(
                        Button::new("scroll")
                            .small()
                            .outline()
                            .label("Scroll ↓")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.run(BrowserCommand::Scroll { dy: 400. }, window, cx)
                            })),
                    )
                    .child(
                        Button::new("eval")
                            .small()
                            .outline()
                            .label("Eval links")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.run(
                                    BrowserCommand::Eval {
                                        script: "document.links.length".into(),
                                    },
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("screenshot")
                            .small()
                            .outline()
                            .label("Screenshot")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.run(BrowserCommand::Screenshot, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id("output")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .mx_3()
                    .mb_3()
                    .p_2()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .font_family(mono)
                    .text_xs()
                    .child(self.output.clone()),
            )
            .child(
                div()
                    .px_4()
                    .pb_3()
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(self.status.clone()),
            );

        let panel_body =
            match self.tab {
                Tab::Browser => self.browser.clone().into_any_element(),
                Tab::Notes => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_color(muted)
                    .child("Notes")
                    .child(div().text_xs().child(
                        "The browser is not painted here, so its native view must be hidden.",
                    ))
                    .into_any_element(),
            };

        h_flex()
            .size_full()
            .bg(bg)
            .text_color(theme.foreground)
            .child(agent)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        h_flex()
                            .h(px(40.))
                            .flex_shrink_0()
                            .px_2()
                            .gap_1()
                            .items_center()
                            .border_b_1()
                            .border_color(border)
                            .child(self.tab_button(Tab::Browser, "Browser", cx))
                            .child(self.tab_button(Tab::Notes, "Notes", cx)),
                    )
                    .child(div().flex_1().min_h_0().flex().flex_col().child(panel_body)),
            )
    }
}

fn main() {
    let mut url = "https://example.com".to_owned();
    let mut self_test = false;
    for arg in std::env::args().skip(1) {
        if arg == "--self-test" {
            self_test = true;
        } else {
            url = arg;
        }
    }

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            loft_browser::bind_keys(cx);
            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            let (window, demo) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                move |window, cx| cx.new(|cx| Demo::new(url, window, cx)),
            )
            .expect("failed to open window");
            cx.activate(true);

            if self_test {
                cx.spawn(async move |cx| {
                    let executor = cx.background_executor().clone();
                    let pause = |secs| executor.timer(Duration::from_secs(secs));
                    let steps = [
                        Step::new("initial 1200x800", None, None),
                        Step::new("resized to 900x620", Some((900., 620.)), None),
                        Step::new("resized to 1500x950", Some((1500., 950.)), None),
                        Step::new("switched to the Notes tab", None, Some(Tab::Notes)),
                        Step::new("back on the Browser tab", None, Some(Tab::Browser)),
                    ];
                    pause(6).await;
                    for (ix, Step { label, resize, tab }) in steps.into_iter().enumerate() {
                        window
                            .update(cx, |_, window, cx| {
                                if let Some((w, h)) = resize {
                                    window.resize(size(px(w), px(h)));
                                }
                                if let Some(tab) = tab {
                                    demo.update(cx, |demo, cx| demo.set_tab(tab, cx));
                                }
                            })
                            .ok();
                        pause(2).await;
                        println!("STEP {} {label}", ix + 1);
                        pause(3).await;
                    }
                    println!("SELF-TEST DONE");
                    cx.update(|cx| cx.quit());
                })
                .detach();
            }
        });
}
