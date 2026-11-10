//! Terminal colors: default/background from the gpui-kit theme, plus a
//! calm ANSI palette tuned separately for dark and light backgrounds.

use crate::backend::{Colors, Rgb};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, Hsla, Rgba};

/// ANSI 0–15 for dark themes: muted, readable on near-black, with brights
/// that stay distinguishable from their normal twins.
const DARK_ANSI: [u32; 16] = [
    0x3a3d45, // black
    0xe5737a, // red
    0x98c379, // green
    0xe5c07b, // yellow
    0x6cadf0, // blue
    0xc792ea, // magenta
    0x56c2c6, // cyan
    0xc8ccd4, // white
    0x6b7180, // bright black
    0xff8f96, // bright red
    0xb3e092, // bright green
    0xf5d68f, // bright yellow
    0x8ec4ff, // bright blue
    0xdcabf6, // bright magenta
    0x7fdbe0, // bright cyan
    0xf3f4f6, // bright white
];

/// ANSI 0–15 for light themes: darker, saturated hues that keep contrast on
/// white; "white" is a mid gray so `ls`-style white-on-default stays visible.
const LIGHT_ANSI: [u32; 16] = [
    0x24292f, // black
    0xc4232f, // red
    0x18793a, // green
    0x946200, // yellow
    0x0a62c9, // blue
    0x8045c7, // magenta
    0x13757d, // cyan
    0x6e7781, // white
    0x57606a, // bright black
    0xe0414c, // bright red
    0x2a9d4e, // bright green
    0xb57f00, // bright yellow
    0x2f86f0, // bright blue
    0x9d64e8, // bright magenta
    0x2e97a1, // bright cyan
    0x8c959f, // bright white
];

/// The palette for the active gpui-kit theme.
pub(crate) fn colors(cx: &App) -> Colors {
    let theme = cx.theme();
    let dark = theme.is_dark();
    let foreground = to_rgb(theme.foreground);
    let background = to_rgb(theme.background);
    let ansi = if dark { DARK_ANSI } else { LIGHT_ANSI }.map(Rgb::hex);
    Colors {
        foreground,
        background,
        cursor: foreground,
        ansi,
    }
}

/// Selection highlight drawn under the text.
pub(crate) fn selection(cx: &App) -> Hsla {
    let theme = cx.theme();
    let mut color = theme.selection;
    color.a = if theme.is_dark() { 0.4 } else { 0.3 };
    color
}

pub(crate) fn to_hsla(c: Rgb) -> Hsla {
    Rgba {
        r: f32::from(c.r) / 255.,
        g: f32::from(c.g) / 255.,
        b: f32::from(c.b) / 255.,
        a: 1.,
    }
    .into()
}

pub(crate) fn to_rgb(c: Hsla) -> Rgb {
    let rgba = c.to_rgb();
    let channel = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    // The terminal needs opaque defaults; any theme alpha is dropped.
    Rgb::new(channel(rgba.r), channel(rgba.g), channel(rgba.b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_have_contrast_against_their_background() {
        fn luminance(c: Rgb) -> f32 {
            let lin = |v: u8| {
                let v = f32::from(v) / 255.;
                if v <= 0.039_28 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
        }
        fn contrast(a: Rgb, b: Rgb) -> f32 {
            let (la, lb) = (luminance(a), luminance(b));
            (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
        }
        let dark_bg = Rgb::hex(0x111113);
        let light_bg = Rgb::hex(0xffffff);
        // Colors 1-7 and 9-15 are used as text; require a readable ratio.
        for i in (1..8).chain(9..16) {
            let dark = contrast(Rgb::hex(DARK_ANSI[i]), dark_bg);
            let light = contrast(Rgb::hex(LIGHT_ANSI[i]), light_bg);
            assert!(dark >= 4.5, "dark ansi {i} contrast {dark}");
            assert!(light >= 3.0, "light ansi {i} contrast {light}");
        }
    }

    #[test]
    fn hsla_round_trip() {
        let c = Rgb::new(12, 200, 99);
        assert_eq!(to_rgb(to_hsla(c)), c);
    }
}
