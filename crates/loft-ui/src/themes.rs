//! Colour themes: built-in palettes plus your own, each with a light and a
//! dark variant. Your themes are small TOML files in `~/.agentloft/themes/`
//! (written by the editor, an agent, or by hand):
//!
//! ```toml
//! name = "Harbor"
//! [dark]
//! background = "#0e1217"
//! foreground = "#e6edf3"
//! accent = "#3b82f6"
//! # optional: sidebar, surface, border, muted, muted_text, hover,
//! # selected, bubble, input
//! [light]
//! background = "#f7fafc"
//! foreground = "#14202b"
//! accent = "#2563eb"
//! ```
//!
//! Only background, foreground and accent are required; everything else is
//! derived from them so any combination looks coherent.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One variant's colours, fully resolved (0xRRGGBB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swatch {
    pub background: u32,
    pub foreground: u32,
    pub accent: u32,
    pub sidebar: u32,
    pub surface: u32,
    pub border: u32,
    pub muted: u32,
    pub muted_text: u32,
    pub hover: u32,
    pub selected: u32,
    pub bubble: u32,
    pub input: u32,
}

/// As written in a theme file: anything but the first three may be left out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SwatchSpec {
    pub background: String,
    pub foreground: String,
    pub accent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hover: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bubble: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeFile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub dark: SwatchSpec,
    pub light: SwatchSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// `loft`, or a custom theme's file stem.
    pub id: String,
    pub name: String,
    pub custom: bool,
    pub file: ThemeFile,
}

impl Theme {
    pub fn swatch(&self, dark: bool, contrast: f32) -> Swatch {
        let spec = if dark {
            &self.file.dark
        } else {
            &self.file.light
        };
        resolve(spec, dark, contrast).unwrap_or(if dark { FALLBACK_DARK } else { FALLBACK_LIGHT })
    }
}

/// How strongly borders and secondary text stand out.
pub fn contrast_factor(setting: &str) -> f32 {
    match setting {
        "soft" => 0.75,
        "high" => 1.45,
        _ => 1.0,
    }
}

pub fn parse_color(s: &str) -> Option<u32> {
    let hex = s.trim().trim_start_matches('#');
    match hex.len() {
        6 => u32::from_str_radix(hex, 16).ok(),
        3 => {
            let v = u32::from_str_radix(hex, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
            Some((r * 17) << 16 | (g * 17) << 8 | (b * 17))
        }
        _ => None,
    }
}

pub fn hex(c: u32) -> String {
    format!("#{c:06x}")
}

/// `a` → `b` by `t` (0..1), per channel.
pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |s: u32| -> (f32, f32) { (((a >> s) & 0xff) as f32, ((b >> s) & 0xff) as f32) };
    let mut out = 0;
    for s in [16, 8, 0] {
        let (x, y) = ch(s);
        out |= ((x + (y - x) * t).round() as u32) << s;
    }
    out
}

/// Relative luminance (0..1), for picking readable text on a colour.
pub fn luminance(c: u32) -> f32 {
    let lin = |v: u32| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin((c >> 16) & 0xff) + 0.7152 * lin((c >> 8) & 0xff) + 0.0722 * lin(c & 0xff)
}

/// Black or white, whichever reads better on `c`.
pub fn text_on(c: u32) -> u32 {
    if luminance(c) > 0.45 {
        0x111111
    } else {
        0xffffff
    }
}

fn resolve(spec: &SwatchSpec, dark: bool, contrast: f32) -> Option<Swatch> {
    let bg = parse_color(&spec.background)?;
    let fg = parse_color(&spec.foreground)?;
    let accent = parse_color(&spec.accent)?;
    let get =
        |v: &Option<String>, default: u32| v.as_deref().and_then(parse_color).unwrap_or(default);
    let k = contrast;
    // Derived from the background/text pair so any theme hangs together.
    let sidebar_default = if dark {
        mix(bg, 0x000000, 0.28)
    } else {
        mix(bg, fg, 0.03)
    };
    let surface_default = if dark {
        mix(bg, fg, 0.05)
    } else {
        mix(bg, 0xffffff, 0.6)
    };
    Some(Swatch {
        background: bg,
        foreground: fg,
        accent,
        sidebar: get(&spec.sidebar, sidebar_default),
        surface: get(&spec.surface, surface_default),
        border: get(&spec.border, mix(bg, fg, 0.11 * k)),
        muted: get(&spec.muted, mix(bg, fg, 0.05)),
        muted_text: get(&spec.muted_text, mix(bg, fg, (0.55 * k).min(0.85))),
        hover: get(&spec.hover, mix(bg, fg, 0.04)),
        selected: get(&spec.selected, mix(bg, fg, 0.085)),
        bubble: get(&spec.bubble, mix(bg, fg, 0.09)),
        input: get(&spec.input, mix(bg, fg, 0.14 * k)),
    })
}

const FALLBACK_DARK: Swatch = Swatch {
    background: 0x111113,
    foreground: 0xededef,
    accent: 0x6e5bff,
    sidebar: 0x0b0b0c,
    surface: 0x19191c,
    border: 0x26262a,
    muted: 0x1c1c20,
    muted_text: 0x8d8d96,
    hover: 0x18181b,
    selected: 0x222227,
    bubble: 0x232328,
    input: 0x2c2c31,
};

const FALLBACK_LIGHT: Swatch = Swatch {
    background: 0xffffff,
    foreground: 0x1a1a1e,
    accent: 0x6e5bff,
    sidebar: 0xf7f7f8,
    surface: 0xffffff,
    border: 0xe7e7ea,
    muted: 0xf3f3f5,
    muted_text: 0x6f6f7a,
    hover: 0xefeff2,
    selected: 0xe8e8ec,
    bubble: 0xf1f1f4,
    input: 0xe2e2e6,
};

/// A fully specified side: background, text, accent, then sidebar,
/// surface (menus), border, subtle fill, secondary text, hover, selected,
/// message bubble and input.
#[allow(clippy::too_many_arguments)]
fn full(
    background: &str,
    foreground: &str,
    accent: &str,
    sidebar: &str,
    surface: &str,
    border: &str,
    muted: &str,
    muted_text: &str,
    hover: &str,
    selected: &str,
    bubble: &str,
    input: &str,
) -> SwatchSpec {
    let o = |v: &str| Some(v.to_owned());
    SwatchSpec {
        background: background.into(),
        foreground: foreground.into(),
        accent: accent.into(),
        sidebar: o(sidebar),
        surface: o(surface),
        border: o(border),
        muted: o(muted),
        muted_text: o(muted_text),
        hover: o(hover),
        selected: o(selected),
        bubble: o(bubble),
        input: o(input),
    }
}

fn spec_from(s: &Swatch) -> SwatchSpec {
    SwatchSpec {
        background: hex(s.background),
        foreground: hex(s.foreground),
        accent: hex(s.accent),
        sidebar: Some(hex(s.sidebar)),
        surface: Some(hex(s.surface)),
        border: Some(hex(s.border)),
        muted: Some(hex(s.muted)),
        muted_text: Some(hex(s.muted_text)),
        hover: Some(hex(s.hover)),
        selected: Some(hex(s.selected)),
        bubble: Some(hex(s.bubble)),
        input: Some(hex(s.input)),
    }
}

/// The themes that ship with the app. Every colour is set by hand; the
/// palettes are modelled on well-loved editor themes (see
/// THIRD_PARTY_NOTICES.md).
pub fn builtins() -> Vec<Theme> {
    let t = |id: &str, name: &str, description: &str, dark: SwatchSpec, light: SwatchSpec| Theme {
        id: id.into(),
        name: name.into(),
        custom: false,
        file: ThemeFile {
            name: name.into(),
            description: Some(description.into()),
            dark,
            light,
        },
    };
    vec![
        t(
            "loft",
            "Loft",
            "Quiet neutrals with a violet accent",
            spec_from(&FALLBACK_DARK),
            spec_from(&FALLBACK_LIGHT),
        ),
        t(
            "studio",
            "Studio",
            "Crisp and familiar, with a clear blue",
            full(
                "#0d1117", "#e6edf3", "#2f81f7", "#010409", "#161b22", "#30363d", "#161b22",
                "#8b949e", "#151b23", "#1f2630", "#21262d", "#30363d",
            ),
            full(
                "#ffffff", "#1f2328", "#0969da", "#f6f8fa", "#ffffff", "#d1d9e0", "#f6f8fa",
                "#59636e", "#f3f5f7", "#e9edf1", "#eef1f4", "#d1d9e0",
            ),
        ),
        t(
            "lavender",
            "Lavender",
            "Soft pastels on a plum-grey base",
            full(
                "#1e1e2e", "#cdd6f4", "#cba6f7", "#181825", "#24243a", "#313244", "#232334",
                "#9399b2", "#25253a", "#313244", "#2b2b40", "#45475a",
            ),
            full(
                "#eff1f5", "#4c4f69", "#8839ef", "#e6e9ef", "#f5f6f9", "#ccd0da", "#e6e9ef",
                "#6c6f85", "#e3e6ee", "#dce0e8", "#e3e6ed", "#ccd0da",
            ),
        ),
        t(
            "glacier",
            "Glacier",
            "Arctic blues and snow",
            full(
                "#2e3440", "#eceff4", "#88c0d0", "#272c36", "#353c4a", "#434c5e", "#323946",
                "#a3adbf", "#353c4a", "#3b4252", "#3b4252", "#4c566a",
            ),
            full(
                "#f3f5f8", "#2e3440", "#5e81ac", "#e5e9f0", "#ffffff", "#d3d9e3", "#e9edf2",
                "#5b6576", "#e7ebf1", "#dde3ec", "#e5e9f0", "#d3d9e3",
            ),
        ),
        t(
            "dusk",
            "Dusk",
            "Muted rose and iris at nightfall",
            full(
                "#191724", "#e0def4", "#c4a7e7", "#141220", "#1f1d2e", "#2a273f", "#1f1d2e",
                "#908caa", "#21202f", "#26233a", "#26233a", "#393552",
            ),
            full(
                "#faf4ed", "#464261", "#907aa9", "#f2e9e1", "#fffaf3", "#e4dad0", "#f4ede4",
                "#6e6a86", "#f2ebe2", "#ebe2d8", "#f2e9e1", "#dfd6cc",
            ),
        ),
        t(
            "harvest",
            "Harvest",
            "Retro warm browns and gold",
            full(
                "#282828", "#ebdbb2", "#d79921", "#1d2021", "#32302f", "#3c3836", "#2f2d2c",
                "#a89984", "#32302f", "#3c3836", "#3c3836", "#504945",
            ),
            full(
                "#fbf1c7", "#3c3836", "#af3a03", "#f2e5bc", "#fdf6d8", "#d5c4a1", "#f2e5bc",
                "#665c54", "#f4e8be", "#ebdbb2", "#efe1b4", "#d5c4a1",
            ),
        ),
        t(
            "evergreen",
            "Evergreen",
            "Calm forest greens, easy on the eyes",
            full(
                "#2d353b", "#d3c6aa", "#a7c080", "#272e33", "#343f44", "#3d484d", "#323c41",
                "#9da9a0", "#343f44", "#3d484d", "#3d484d", "#475258",
            ),
            full(
                "#fdf6e3", "#3f4b51", "#8da101", "#f4f0d9", "#fffbef", "#e0dcc7", "#f4f0d9",
                "#708089", "#f2eed6", "#e9e5cf", "#efebd4", "#e0dcc7",
            ),
        ),
        t(
            "neon",
            "Neon",
            "Night-time city blues and violets",
            full(
                "#1a1b26", "#c0caf5", "#7aa2f7", "#16161e", "#1f2335", "#292e42", "#1e2030",
                "#7982a9", "#1f2335", "#292e42", "#24283b", "#3b4261",
            ),
            full(
                "#e9ebf2", "#343b58", "#2e7de9", "#dfe2ec", "#f5f6fa", "#c8cde0", "#e2e5ee",
                "#5c6488", "#e0e3ec", "#d5d9e7", "#dde1ec", "#c8cde0",
            ),
        ),
        t(
            "slate",
            "Slate",
            "Balanced greys with a sky-blue accent",
            full(
                "#282c34", "#dcdfe4", "#61afef", "#21252b", "#2c313a", "#3e4451", "#2c313a",
                "#8b919c", "#2c313a", "#333842", "#333842", "#454b57",
            ),
            full(
                "#fafafa", "#383a42", "#4078f2", "#f0f0f1", "#ffffff", "#dbdbdc", "#f0f0f1",
                "#6b6d75", "#efeff0", "#e5e5e6", "#ebebec", "#dbdbdc",
            ),
        ),
        t(
            "nightshade",
            "Nightshade",
            "Bold purple on deep violet-grey",
            full(
                "#282a36", "#f8f8f2", "#bd93f9", "#21222c", "#2f3141", "#44475a", "#2d2f3d",
                "#a0a8cd", "#30323f", "#3a3c4e", "#383a4a", "#4d5066",
            ),
            full(
                "#fbf8f0", "#1f1f1f", "#644ac9", "#f2eee3", "#fffdf8", "#dcd6c8", "#f2eee3",
                "#5f5b54", "#f0ece1", "#e8e3d6", "#ece8dc", "#dcd6c8",
            ),
        ),
    ]
}

pub fn themes_dir(home: &Path) -> PathBuf {
    home.join("themes")
}

/// Your themes, from `dir` (sorted by name). Broken files are skipped.
pub fn load_custom(dir: &Path) -> Vec<Theme> {
    let mut out: Vec<Theme> = crate::host::list_files(dir)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|x| x == "toml"))
        .filter_map(|path| {
            let text = crate::host::read_to_string(&path).ok()?;
            let file = parse(&text).ok()?;
            let id = path.file_stem()?.to_string_lossy().into_owned();
            Some(Theme {
                id: format!("custom:{id}"),
                name: file.name.clone(),
                custom: true,
                file,
            })
        })
        .collect();
    out.sort_by_key(|t| t.name.to_lowercase());
    out
}

pub fn all(home: &Path) -> Vec<Theme> {
    let mut v = builtins();
    v.extend(load_custom(&themes_dir(home)));
    v
}

/// The theme `id` (falls back to Loft).
pub fn find(home: &Path, id: &str) -> Theme {
    let mut all = all(home);
    let ix = all.iter().position(|t| t.id == id).unwrap_or(0);
    all.swap_remove(ix)
}

/// A theme file's text, tolerating a Markdown code fence around it (what
/// an agent might answer with).
pub fn parse(text: &str) -> anyhow::Result<ThemeFile> {
    let body = text.trim();
    let body = match body.find("```") {
        Some(start) => {
            let after = &body[start + 3..];
            let after = after
                .split_once('\n')
                .map(|(_, rest)| rest)
                .unwrap_or(after);
            after.split("```").next().unwrap_or(after)
        }
        None => body,
    };
    let file: ThemeFile = toml::from_str(body)?;
    for (variant, spec) in [("dark", &file.dark), ("light", &file.light)] {
        for (key, value) in [
            ("background", &spec.background),
            ("foreground", &spec.foreground),
            ("accent", &spec.accent),
        ] {
            anyhow::ensure!(
                parse_color(value).is_some(),
                "[{variant}] {key} = \"{value}\" isn't a #rrggbb colour"
            );
        }
    }
    anyhow::ensure!(!file.name.trim().is_empty(), "the theme needs a name");
    Ok(file)
}

pub fn to_toml(file: &ThemeFile) -> String {
    toml::to_string_pretty(file).unwrap_or_default()
}

/// Save as a custom theme; returns its id (`custom:<slug>`), keeping
/// `existing` when overwriting one.
pub fn save(home: &Path, file: &ThemeFile, existing: Option<&str>) -> anyhow::Result<String> {
    let dir = themes_dir(home);
    let stem = match existing.and_then(|id| id.strip_prefix("custom:")) {
        Some(stem) => stem.to_owned(),
        None => {
            let base = slug(&file.name);
            let mut stem = base.clone();
            let mut n = 2;
            while crate::host::exists(&dir.join(format!("{stem}.toml"))) {
                stem = format!("{base}-{n}");
                n += 1;
            }
            stem
        }
    };
    crate::host::write(&dir.join(format!("{stem}.toml")), to_toml(file).as_bytes())?;
    Ok(format!("custom:{stem}"))
}

pub fn delete(home: &Path, id: &str) -> anyhow::Result<()> {
    if let Some(stem) = id.strip_prefix("custom:") {
        crate::host::remove_file(&themes_dir(home).join(format!("{stem}.toml")))?;
    }
    Ok(())
}

fn slug(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_owned();
    if s.is_empty() { "theme".into() } else { s }
}

/// What an agent is told when asked to design a theme.
pub fn generation_prompt(description: &str) -> String {
    format!(
        "Design a colour theme for a dark/light desktop coding app, inspired by: \"{description}\".\n\
         Answer with ONLY a TOML document, no prose, in exactly this shape:\n\n\
         name = \"<2-3 word evocative name>\"\n\
         description = \"<one short line>\"\n\
         [dark]\nbackground = \"#rrggbb\"\nforeground = \"#rrggbb\"\naccent = \"#rrggbb\"\n\
         sidebar = \"#rrggbb\"\nsurface = \"#rrggbb\"\nborder = \"#rrggbb\"\nmuted_text = \"#rrggbb\"\n\
         [light]\nbackground = \"#rrggbb\"\nforeground = \"#rrggbb\"\naccent = \"#rrggbb\"\n\
         sidebar = \"#rrggbb\"\nsurface = \"#rrggbb\"\nborder = \"#rrggbb\"\nmuted_text = \"#rrggbb\"\n\n\
         Rules: text must be comfortably readable on the background (WCAG AA or better); \
         the dark background should be very dark and the light one very light; sidebar is a \
         slightly deeper shade of the background; surface is for popovers; border is subtle; \
         muted_text is for secondary text; the accent is used for buttons with text on them, \
         so keep it saturated but not neon."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_resolve_and_read_well() {
        for theme in builtins() {
            for dark in [true, false] {
                let s = theme.swatch(dark, 1.0);
                let (bg, fg) = (luminance(s.background), luminance(s.foreground));
                let ratio = (bg.max(fg) + 0.05) / (bg.min(fg) + 0.05);
                assert!(ratio > 7.0, "{} {dark}: contrast {ratio:.1}", theme.name);
            }
        }
    }

    #[test]
    fn parses_fenced_partial_themes() {
        let text = "Here you go:\n```toml\nname = \"Harbor\"\n[dark]\nbackground = \"#0e1217\"\n\
                    foreground = \"#e6edf3\"\naccent = \"#3b82f6\"\n[light]\nbackground = \"#fff\"\n\
                    foreground = \"#14202b\"\naccent = \"#2563eb\"\n```\n";
        let file = parse(text).unwrap();
        assert_eq!(file.name, "Harbor");
        let theme = Theme {
            id: "custom:harbor".into(),
            name: file.name.clone(),
            custom: true,
            file,
        };
        let light = theme.swatch(false, 1.0);
        assert_eq!(light.background, 0xffffff);
        assert_ne!(light.border, light.background, "derived");
    }

    #[test]
    fn rejects_bad_colours() {
        let text = "name = \"X\"\n[dark]\nbackground = \"blue\"\nforeground = \"#fff\"\n\
                    accent = \"#fff\"\n[light]\nbackground = \"#fff\"\nforeground = \"#000\"\n\
                    accent = \"#000\"\n";
        assert!(parse(text).unwrap_err().to_string().contains("background"));
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = builtins()[3].file.clone();
        let id = save(dir.path(), &file, None).unwrap();
        assert_eq!(id, "custom:glacier");
        let again = save(dir.path(), &file, None).unwrap();
        assert_eq!(again, "custom:glacier-2");
        let loaded = load_custom(&themes_dir(dir.path()));
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].file, file);
        delete(dir.path(), &id).unwrap();
        assert_eq!(load_custom(&themes_dir(dir.path())).len(), 1);
    }

    #[test]
    fn mixing_and_text_colour() {
        assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
        assert_eq!(text_on(0xffffff), 0x111111);
        assert_eq!(text_on(0x1d4ed8), 0xffffff);
    }
}
