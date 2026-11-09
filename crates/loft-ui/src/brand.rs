//! The Agentloft app icon for in-app use (welcome screen, sidebar, About).

use gpui_kit::{Image, ImageFormat, IntoElement, Pixels, Styled as _, img};
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
