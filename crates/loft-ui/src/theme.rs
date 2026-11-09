//! Agentloft's look: a refined neutral palette for light and dark mode,
//! appearance settings from `config.toml` (`[ui]`, reloaded live), and a
//! small `Copy` palette views read once per render (no per-row theme clones).

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use gpui_kit::{App, Hsla, Pixels, SharedString, px, rgb};
use loft_config::{Paths, UiConfig};
use std::time::SystemTime;

/// Default accent when the user hasn't picked one.
const DEFAULT_ACCENT: u32 = 0x6e5bff;

struct Neutrals {
    background: u32,
    foreground: u32,
    sidebar: u32,
    elevated: u32,
    border: u32,
    muted: u32,
    muted_foreground: u32,
    hover: u32,
    selected: u32,
    bubble: u32,
    input: u32,
}

const DARK: Neutrals = Neutrals {
    background: 0x111113,
    foreground: 0xededef,
    sidebar: 0x0b0b0c,
    elevated: 0x19191c,
    border: 0x26262a,
    muted: 0x1c1c20,
    muted_foreground: 0x8d8d96,
    hover: 0x18181b,
    selected: 0x222227,
    bubble: 0x232328,
    input: 0x2c2c31,
};

const LIGHT: Neutrals = Neutrals {
    background: 0xffffff,
    foreground: 0x1a1a1e,
    sidebar: 0xf7f7f8,
    elevated: 0xffffff,
    border: 0xe7e7ea,
    muted: 0xf3f3f5,
    muted_foreground: 0x6f6f7a,
    hover: 0xefeff2,
    selected: 0xe8e8ec,
    bubble: 0xf1f1f4,
    input: 0xe2e2e6,
};

/// Colors and metrics a view needs for one frame. Built once per render.
#[derive(Clone)]
pub struct Palette {
    pub dark: bool,
    pub background: Hsla,
    pub foreground: Hsla,
    pub muted_foreground: Hsla,
    pub faint_foreground: Hsla,
    pub sidebar: Hsla,
    pub elevated: Hsla,
    pub border: Hsla,
    pub muted: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub bubble: Hsla,
    pub accent: Hsla,
    pub accent_foreground: Hsla,
    pub accent_soft: Hsla,
    pub info: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub warning_soft: Hsla,
    pub danger: Hsla,
    pub danger_soft: Hsla,
    pub mono: SharedString,
    pub mono_size: Pixels,
    pub content_width: Pixels,
    /// Vertical rhythm between timeline rows.
    pub row_gap: Pixels,
}

impl Palette {
    pub fn of(cx: &App) -> Self {
        let theme = cx.theme();
        let c = &theme.colors;
        let settings = cx
            .try_global::<AppearanceState>()
            .map(|s| s.config.clone())
            .unwrap_or_default();
        let compact = settings.density == "compact";
        Self {
            dark: theme.mode.is_dark(),
            background: c.background,
            foreground: c.foreground,
            muted_foreground: c.muted_foreground,
            faint_foreground: c.muted_foreground.opacity(0.7),
            sidebar: c.sidebar,
            elevated: c.popover,
            border: c.border,
            muted: c.muted,
            hover: c.list_hover,
            selected: c.list_active,
            bubble: c.secondary,
            accent: c.primary,
            accent_foreground: c.primary_foreground,
            accent_soft: c.primary.opacity(0.14),
            info: c.info,
            success: c.success,
            warning: c.warning,
            warning_soft: c.warning.opacity(0.10),
            danger: c.danger,
            danger_soft: c.danger.opacity(0.10),
            mono: theme.mono_font_family.clone(),
            mono_size: theme.mono_font_size,
            content_width: px(settings.content_width.clamp(560.0, 1400.0)),
            row_gap: if compact { px(6.) } else { px(10.) },
        }
    }
}

/// The loaded `[ui]` settings plus the config file's mtime for live reload.
pub struct AppearanceState {
    pub config: UiConfig,
    paths: Paths,
    mtime: Option<SystemTime>,
}

impl gpui_kit::Global for AppearanceState {}

/// Load settings and apply them. Call once at startup.
pub fn init(paths: Paths, cx: &mut App) {
    let (config, mtime) = load(&paths);
    cx.set_global(AppearanceState {
        config,
        paths,
        mtime,
    });
    apply(cx);
}

/// Re-read `config.toml` if it changed; returns whether anything was applied.
pub fn reload_if_changed(cx: &mut App) -> bool {
    let Some(state) = cx.try_global::<AppearanceState>() else {
        return false;
    };
    let (config, mtime) = load(&state.paths);
    if mtime == state.mtime && config == state.config {
        return false;
    }
    let changed = config != state.config;
    let state = cx.global_mut::<AppearanceState>();
    state.mtime = mtime;
    state.config = config;
    if changed {
        tracing::info!("appearance settings reloaded");
        apply(cx);
    }
    changed
}

/// Toggle light/dark and save it as `ui.theme`, as Settings does; the
/// config file is the source of truth, so an unsaved switch would be undone
/// by the next settings reload.
pub fn toggle_mode(cx: &mut App) {
    let next = if cx.theme().mode.is_dark() {
        "light"
    } else {
        "dark"
    };
    let Some(state) = cx.try_global::<AppearanceState>() else {
        return;
    };
    let paths = state.paths.clone();
    match loft_config::write_setting(
        &paths,
        &["ui", "theme"],
        loft_config::SettingValue::Str(next.into()),
    ) {
        Ok(()) => {
            reload_if_changed(cx);
        }
        Err(err) => {
            // Still switch for this session.
            tracing::warn!("couldn't save the theme: {err:#}");
            cx.global_mut::<AppearanceState>().config.theme = next.into();
            apply(cx);
        }
    }
}

/// Apply the current settings (call again when the system appearance flips).
pub fn apply(cx: &mut App) {
    let config = cx
        .try_global::<AppearanceState>()
        .map(|s| s.config.clone())
        .unwrap_or_default();
    let mode = match config.theme.as_str() {
        "light" => ThemeMode::Light,
        "dark" => ThemeMode::Dark,
        _ => ThemeMode::from(cx.window_appearance()),
    };
    Theme::change(mode, None, cx);
    let neutrals = if mode.is_dark() { &DARK } else { &LIGHT };
    let accent: Hsla = config
        .accent
        .as_deref()
        .and_then(parse_hex)
        .unwrap_or_else(|| rgb(DEFAULT_ACCENT).into());
    Theme::update(cx, |theme| {
        let c = &mut theme.colors;
        c.background = rgb(neutrals.background).into();
        c.foreground = rgb(neutrals.foreground).into();
        c.sidebar = rgb(neutrals.sidebar).into();
        c.sidebar_foreground = rgb(neutrals.foreground).into();
        c.sidebar_border = rgb(neutrals.border).into();
        c.popover = rgb(neutrals.elevated).into();
        c.popover_foreground = rgb(neutrals.foreground).into();
        c.border = rgb(neutrals.border).into();
        c.input = rgb(neutrals.input).into();
        c.muted = rgb(neutrals.muted).into();
        c.muted_foreground = rgb(neutrals.muted_foreground).into();
        c.list_hover = rgb(neutrals.hover).into();
        c.list_active = rgb(neutrals.selected).into();
        c.secondary = rgb(neutrals.bubble).into();
        c.secondary_foreground = rgb(neutrals.foreground).into();
        c.title_bar = rgb(neutrals.sidebar).into();
        c.primary = accent;
        c.primary_hover = accent.opacity(0.88);
        c.primary_foreground = gpui_kit::white();
        c.button_primary = accent;
        c.button_primary_hover = accent.opacity(0.88);
        c.button_primary_active = accent.opacity(0.78);
        c.button_primary_foreground = gpui_kit::white();
        c.ring = accent.opacity(0.55);
        c.selection = accent.opacity(0.28);
        if let Some(family) = &config.font_family {
            theme.font_family = family.clone().into();
        }
        if let Some(family) = &config.mono_font_family {
            theme.mono_font_family = family.clone().into();
        }
        theme.font_size = px(config.font_size.clamp(10.0, 24.0));
        theme.mono_font_size = px(config.mono_font_size.clamp(9.0, 22.0));
        theme.radius = px(8.);
        theme.radius_lg = px(14.);
    });
    cx.refresh_windows();
}

fn load(paths: &Paths) -> (UiConfig, Option<SystemTime>) {
    let mtime = std::fs::metadata(paths.config_file())
        .and_then(|m| m.modified())
        .ok();
    let config = match loft_config::Config::load(paths) {
        Ok(c) => c.ui,
        Err(err) => {
            tracing::warn!(%err, "invalid config.toml; keeping defaults");
            UiConfig::default()
        }
    };
    (config, mtime)
}

pub fn parse_hex(s: &str) -> Option<Hsla> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(|v| rgb(v).into())
}

/// "now", "5m", "3h", "2d", "4w".
pub fn relative_time(ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    let secs = ((now - ms) / 1000).max(0);
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=604_799 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert!(parse_hex("#6e5bff").is_some());
        assert!(parse_hex("6e5bff").is_some());
        assert!(parse_hex("#fff").is_none());
        assert!(parse_hex("zzzzzz").is_none());
    }
}
