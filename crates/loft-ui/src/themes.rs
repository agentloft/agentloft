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

fn spec(bg: &str, fg: &str, accent: &str) -> SwatchSpec {
    SwatchSpec {
        background: bg.into(),
        foreground: fg.into(),
        accent: accent.into(),
        ..Default::default()
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

/// The themes that ship with the app.
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
            "graphite",
            "Graphite",
            "Cool greys, slate accent",
            spec("#121417", "#e8eaed", "#64748b"),
            spec("#fbfbfc", "#1c1f24", "#475569"),
        ),
        t(
            "paper",
            "Paper",
            "Black and white, nothing else",
            spec("#141414", "#f5f5f5", "#f5f5f5"),
            spec("#ffffff", "#141414", "#141414"),
        ),
        t(
            "sandstone",
            "Sandstone",
            "Warm stone with a copper accent",
            spec("#17130f", "#efe7db", "#d9822b"),
            spec("#fbf8f3", "#2b241c", "#c2410c"),
        ),
        t(
            "fjord",
            "Fjord",
            "Deep sea blues",
            spec("#0c1218", "#e3eaf2", "#3b82f6"),
            spec("#f6f9fc", "#132131", "#2563eb"),
        ),
        t(
            "moss",
            "Moss",
            "Forest greens",
            spec("#0f1411", "#e4ece6", "#2fb36b"),
            spec("#f6faf7", "#16241b", "#15803d"),
        ),
        t(
            "orchid",
            "Orchid",
            "Plum with a magenta accent",
            spec("#150f18", "#f0e6f3", "#c84fd8"),
            spec("#fcf7fd", "#2a1730", "#a21caf"),
        ),
        t(
            "rosewood",
            "Rosewood",
            "Dusky rose",
            spec("#171012", "#f2e4e7", "#f0476b"),
            spec("#fdf6f7", "#2d1418", "#e11d48"),
        ),
        t(
            "nocturne",
            "Nocturne",
            "Ink blue, crisp contrast",
            spec("#090b12", "#f4f5fb", "#818cf8"),
            spec("#ffffff", "#0b0d17", "#4f46e5"),
        ),
        t(
            "amber-crt",
            "Amber CRT",
            "Phosphor amber on charcoal",
            spec("#0d0b08", "#ffcf87", "#ffb000"),
            spec("#fff8ec", "#3a2a10", "#b45309"),
        ),
    ]
}

pub fn themes_dir(home: &Path) -> PathBuf {
    home.join("themes")
}

/// Your themes, from `dir` (sorted by name). Broken files are skipped.
pub fn load_custom(dir: &Path) -> Vec<Theme> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Theme> = read
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            let file = parse(&text).ok()?;
            let id = e.path().file_stem()?.to_string_lossy().into_owned();
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
    std::fs::create_dir_all(&dir)?;
    let stem = match existing.and_then(|id| id.strip_prefix("custom:")) {
        Some(stem) => stem.to_owned(),
        None => {
            let base = slug(&file.name);
            let mut stem = base.clone();
            let mut n = 2;
            while dir.join(format!("{stem}.toml")).exists() {
                stem = format!("{base}-{n}");
                n += 1;
            }
            stem
        }
    };
    std::fs::write(dir.join(format!("{stem}.toml")), to_toml(file))?;
    Ok(format!("custom:{stem}"))
}

pub fn delete(home: &Path, id: &str) -> anyhow::Result<()> {
    if let Some(stem) = id.strip_prefix("custom:") {
        std::fs::remove_file(themes_dir(home).join(format!("{stem}.toml")))?;
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
        assert_eq!(id, "custom:sandstone");
        let again = save(dir.path(), &file, None).unwrap();
        assert_eq!(again, "custom:sandstone-2");
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
