//! The Agentloft app icon for in-app use (welcome screen, sidebar, About).

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::{AnyElement, Image, ImageFormat, IntoElement, Pixels, Styled as _, img, rgb, svg};
use loft_protocol::HarnessId;
use std::sync::{Arc, OnceLock};

/// 256 px master is crisp up to 128 pt on Retina displays.
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon/png/agentloft-256.png");

fn icon_image() -> Arc<Image> {
    static ICON: OnceLock<Arc<Image>> = OnceLock::new();
    ICON.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Png, ICON_PNG.to_vec())))
        .clone()
}

/// The app icon at `size` (square).
pub fn app_icon(size: Pixels) -> impl IntoElement {
    img(icon_image()).size(size).flex_shrink_0()
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// An agent's logo in its brand colour (monochrome marks follow the text
/// colour). Unknown agents get a generic bot.
pub fn agent_logo(harness: &str, size: Pixels, p: &Palette) -> AnyElement {
    let (path, color) = match harness {
        HarnessId::CLAUDE => ("brand/claude.svg", rgb(0xD97757).into()),
        HarnessId::CODEX => ("brand/openai.svg", p.foreground),
        HarnessId::GEMINI => ("brand/gemini.svg", rgb(0x8E75B2).into()),
        HarnessId::OPENCODE => ("brand/opencode.svg", p.foreground),
        _ => {
            return Icon::new(IconName::Bot)
                .with_size(size)
                .text_color(p.muted_foreground)
                .into_any_element();
        }
    };
    svg()
        .path(path)
        .size(size)
        .flex_shrink_0()
        .text_color(color)
        .into_any_element()
}
