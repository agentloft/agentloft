//! Agentloft's look: a refined neutral palette for light and dark mode,
//! appearance settings from `config.toml` (`[ui]`, reloaded live), and a
//! small `Copy` palette views read once per render (no per-row theme clones).

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use gpui_kit::{App, Hsla, Pixels, SharedString, px, rgb};
use loft_config::{Paths, UiConfig};
use std::time::SystemTime;

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
            row_gap: if compact { px(6.) } else { px(12.) },
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
    let home = cx
        .try_global::<AppearanceState>()
        .map(|s| s.paths.home.clone())
        .unwrap_or_default();
    let palette = crate::themes::find(&home, &config.palette);
    let sw = palette.swatch(
        mode.is_dark(),
        crate::themes::contrast_factor(&config.contrast),
    );
    let accent_hex = config
        .accent
        .as_deref()
        .and_then(crate::themes::parse_color)
        .unwrap_or(sw.accent);
    let accent: Hsla = rgb(accent_hex).into();
    let on_accent: Hsla = rgb(crate::themes::text_on(accent_hex)).into();
    let c = |v: u32| -> Hsla { rgb(v).into() };
    Theme::update(cx, |theme| {
        let colors = &mut theme.colors;
        colors.background = c(sw.background);
        colors.foreground = c(sw.foreground);
        colors.sidebar = c(sw.sidebar);
        colors.sidebar_foreground = c(sw.foreground);
        colors.sidebar_border = c(sw.border);
        colors.popover = c(sw.surface);
        colors.popover_foreground = c(sw.foreground);
        colors.border = c(sw.border);
        colors.input = c(sw.input);
        colors.muted = c(sw.muted);
        colors.muted_foreground = c(sw.muted_text);
        colors.list_hover = c(sw.hover);
        colors.list_active = c(sw.selected);
        colors.secondary = c(sw.bubble);
        colors.secondary_foreground = c(sw.foreground);
        colors.title_bar = c(sw.sidebar);
        colors.primary = accent;
        colors.primary_hover = accent.opacity(0.88);
        colors.primary_foreground = on_accent;
        colors.button_primary = accent;
        colors.button_primary_hover = accent.opacity(0.88);
        colors.button_primary_active = accent.opacity(0.78);
        colors.button_primary_foreground = on_accent;
        colors.ring = accent.opacity(0.55);
        colors.selection = accent.opacity(0.28);
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
