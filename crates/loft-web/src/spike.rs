//! Step 0 spike: a trivial gpui-kit view in the browser.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, Window, WindowOptions, div, px};
use std::borrow::Cow;
use wasm_bindgen::prelude::*;

struct Hello {
    input: Entity<InputState>,
    clicks: usize,
}

impl Render for Hello {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(div().text_2xl().child("Agentloft on the web"))
            .child(format!("Clicked {} times · 日本語 · emoji 🎉", self.clicks))
            .child(div().w(px(360.)).child(Input::new(&self.input)))
            .child(
                Button::new("click")
                    .primary()
                    .label("Click me")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clicks += 1;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .font_family("JetBrains Mono")
                    .child("fn main() { println!(\"mono\"); }"),
            )
    }
}

#[wasm_bindgen(start)]
pub fn start() {
    gpui_kit::platform::web_init();
    // Single-threaded (no SharedArrayBuffer needed); CJK falls back to the
    // browser's own fonts, since only Latin fonts are bundled.
    let platform = std::rc::Rc::new(
        gpui_kit::web::WebPlatform::new_with_backend_and_font_fallback(
            false,
            gpui_kit::web::WebBackendPreference::Auto,
            gpui_kit::web::CanvasFontFallback::EmojiAndCjk,
        ),
    );
    gpui_kit::Application::with_platform(platform).run(|cx| {
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(include_bytes!("../fonts/IBMPlexSans-Regular.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../fonts/Inter-Regular.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../fonts/Inter-Medium.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../fonts/Inter-SemiBold.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../fonts/Inter-Bold.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../fonts/Inter-Italic.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../fonts/JetBrainsMono-Regular.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../fonts/JetBrainsMono-Bold.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../fonts/JetBrainsMono-Italic.ttf").as_slice()),
            ])
            .expect("fonts load");
        gpui_kit::init(cx);
        {
            let theme = gpui_kit::component::Theme::global_mut(cx);
            theme.font_family = "Inter".into();
            theme.mono_font_family = "JetBrains Mono".into();
        }
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Hello {
                input: cx.new(|cx| InputState::new(window, cx).placeholder("Type here…")),
                clicks: 0,
            })
        })
        .expect("open window");
    });
}
