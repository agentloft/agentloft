//! Views an agent draws in the chat (ADR-0018).
//!
//! An agent calls `show_ui` with HTML of its own, and the thread shows it as
//! a live page: the daemon loads it in a tab of its headless browser, where
//! it can reach nothing but a few script and font hosts, and streams it to
//! whoever is looking. This crate is the part everyone shares: what a view
//! is ([`View`], [`parse`], [`partial`]), the document it is written into
//! ([`head`], [`BRIDGE_JS`]), what it may load ([`allowed_url`]) and a few
//! views to try it with ([`samples`]).

pub mod samples;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The tool's name on the Agentloft MCP server.
pub const TOOL: &str = "show_ui";

/// The largest page taken (bytes of HTML).
pub const MAX_HTML: usize = 400_000;
/// The longest title shown.
pub const MAX_TITLE: usize = 120;
/// The most a view may remember with `agentloft.setState` (bytes of JSON).
pub const MAX_STATE: usize = 64_000;
/// A view is as tall as its content, from here…
pub const MIN_HEIGHT: u32 = 24;
/// …to here; taller content scrolls inside the view.
pub const MAX_HEIGHT: u32 = 1_400;

/// In a call's input while the agent is still writing it: marks HTML that
/// is only what has arrived so far.
pub const PARTIAL: &str = "_partial";

/// The world the bridge runs in, apart from the page's own script.
pub const WORLD: &str = "agentloft";
/// The function the bridge calls to reach Agentloft (in [`WORLD`] only).
pub const BINDING: &str = "__agentloft";
/// The bridge: see `js/bridge.js`.
pub const BRIDGE_JS: &str = include_str!("js/bridge.js");
const API_JS: &str = include_str!("js/api.js");
const BASE_CSS: &str = include_str!("base.css");

/// Whether a tool call (by the name the agent's harness gives it, such as
/// `mcp__agentloft__show_ui`) is a view.
pub fn is_tool(tool: &str) -> bool {
    tool.strip_suffix(TOOL)
        .is_some_and(|head| head.ends_with("agentloft__"))
}

/// What an agent hands `show_ui`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct View {
    pub title: Option<String>,
    pub html: String,
}

/// Read a finished call. Errors are for the agent: they say what to fix.
pub fn parse(args: &Value) -> Result<View, String> {
    let html = match args {
        // The page itself, with nothing around it.
        Value::String(html) => html.as_str(),
        Value::Object(map) => match map.get("html") {
            Some(Value::String(html)) => html.as_str(),
            Some(_) => return Err("`html` must be a string: the page to show".into()),
            None if map.contains_key("body") || map.contains_key("blocks") => {
                return Err(
                    "show_ui takes a page now: { title?, html }. Write the HTML (with its CSS and JavaScript) yourself."
                        .into(),
                );
            }
            None => return Err("`html` is missing: the page to show".into()),
        },
        _ => return Err("show_ui takes { title?, html }".into()),
    };
    if html.trim().is_empty() {
        return Err("`html` is empty: give it something to show".into());
    }
    if html.len() > MAX_HTML {
        return Err(format!(
            "the page is {} bytes; the most is {MAX_HTML}. Show less, or draw from less data",
            html.len()
        ));
    }
    Ok(View {
        title: title_of(args.get("title")),
        html: html.to_owned(),
    })
}

fn title_of(value: Option<&Value>) -> Option<String> {
    let title = value?.as_str()?.trim();
    (!title.is_empty()).then(|| title.chars().take(MAX_TITLE).collect())
}

/// What there is of a call that is still being written: its JSON so far,
/// cut anywhere. The HTML comes out as far as it goes.
pub fn partial(json: &str) -> View {
    let mut view = View::default();
    let mut chars = json.char_indices().peekable();
    // Into the object.
    for (_, c) in chars.by_ref() {
        if c == '{' {
            break;
        }
    }
    loop {
        // A key…
        let mut started = false;
        for (_, c) in chars.by_ref() {
            if c == '"' {
                started = true;
                break;
            }
            if c == '}' {
                return view;
            }
        }
        if !started {
            return view;
        }
        let (key, closed) = string_so_far(&mut chars);
        if !closed {
            return view;
        }
        // …its colon…
        let mut colon = false;
        for (_, c) in chars.by_ref() {
            if c == ':' {
                colon = true;
                break;
            }
        }
        if !colon {
            return view;
        }
        while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
            chars.next();
        }
        // …and its value: a string we may want, or something to step over.
        match chars.peek() {
            Some((_, '"')) => {
                chars.next();
                let (text, closed) = string_so_far(&mut chars);
                match key.as_str() {
                    "html" => view.html = text,
                    "title" if closed => view.title = title_of(Some(&Value::String(text))),
                    _ => {}
                }
                if !closed {
                    return view;
                }
            }
            Some(_) => {
                let mut depth = 0usize;
                let mut in_string = false;
                let mut escaped = false;
                while let Some((_, c)) = chars.peek().copied() {
                    if in_string {
                        if escaped {
                            escaped = false;
                        } else if c == '\\' {
                            escaped = true;
                        } else if c == '"' {
                            in_string = false;
                        }
                    } else {
                        match c {
                            '"' => in_string = true,
                            '{' | '[' => depth += 1,
                            '}' | ']' if depth > 0 => depth -= 1,
                            ',' | '}' if depth == 0 => break,
                            _ => {}
                        }
                    }
                    chars.next();
                }
            }
            None => return view,
        }
    }
}

/// The JSON string the iterator is inside of, decoded as far as it goes,
/// and whether its closing quote was reached.
fn string_so_far(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> (String, bool) {
    let mut out = String::new();
    while let Some((_, c)) = chars.next() {
        match c {
            '"' => return (out, true),
            '\\' => {
                let Some((_, escape)) = chars.next() else {
                    return (out, false);
                };
                match escape {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        let Some(unit) = hex4(chars) else {
                            return (out, false);
                        };
                        // A pair of escapes makes one character past U+FFFF.
                        if (0xD800..0xDC00).contains(&unit) {
                            let mut ahead = chars.clone();
                            let low = match (ahead.next(), ahead.next()) {
                                (Some((_, '\\')), Some((_, 'u'))) => hex4(&mut ahead),
                                // Not here yet: wait for it.
                                (None, _) | (Some(_), None) => return (out, false),
                                _ => None,
                            };
                            match low {
                                Some(low) if (0xDC00..0xE000).contains(&low) => {
                                    *chars = ahead;
                                    let code = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                }
                                Some(_) => out.push('\u{fffd}'),
                                None => return (out, false),
                            }
                        } else {
                            out.push(char::from_u32(unit).unwrap_or('\u{fffd}'));
                        }
                    }
                    other => out.push(other),
                }
            }
            other => out.push(other),
        }
    }
    (out, false)
}

fn hex4(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> Option<u32> {
    let mut unit = 0u32;
    for _ in 0..4 {
        unit = unit * 16 + chars.next()?.1.to_digit(16)?;
    }
    Some(unit)
}

/// The app's look, handed to a view as CSS variables (and as
/// `agentloft.theme`), so a page can match the window it is shown in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    pub dark: bool,
    /// CSS colours.
    pub background: String,
    pub foreground: String,
    pub muted: String,
    pub border: String,
    /// A quiet fill: code blocks, hovered rows.
    pub surface: String,
    pub accent: String,
    pub accent_foreground: String,
    pub success: String,
    pub warning: String,
    pub danger: String,
    /// CSS font families.
    pub font: String,
    pub mono: String,
    /// The text size, in CSS pixels.
    pub font_size: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            dark: true,
            background: "#18181b".into(),
            foreground: "#f4f4f5".into(),
            muted: "#a1a1aa".into(),
            border: "#2e2e33".into(),
            surface: "#232327".into(),
            accent: "#6d5df6".into(),
            accent_foreground: "#ffffff".into(),
            success: "#22c55e".into(),
            warning: "#eab308".into(),
            danger: "#ef4444".into(),
            font: "system-ui, sans-serif".into(),
            mono: "ui-monospace, monospace".into(),
            font_size: 14.0,
        }
    }
}

impl Theme {
    /// The same look with nothing in it that isn't a colour or a font
    /// name: a theme comes from a client, and goes into a stylesheet.
    pub fn cleaned(&self) -> Self {
        let fallback = Self::default();
        let colour = |given: &str, fallback: &str| {
            let given = given.trim();
            let plain = !given.is_empty()
                && given.len() <= 64
                && given
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "#(),.% -/".contains(c));
            if plain { given } else { fallback }.to_owned()
        };
        let font = |given: &str, fallback: &str| {
            let cleaned: String = given
                .chars()
                .filter(|c| c.is_alphanumeric() || " ,-_.".contains(*c))
                .take(200)
                .collect();
            if cleaned.trim().is_empty() {
                fallback.to_owned()
            } else {
                cleaned
            }
        };
        Self {
            dark: self.dark,
            background: colour(&self.background, &fallback.background),
            foreground: colour(&self.foreground, &fallback.foreground),
            muted: colour(&self.muted, &fallback.muted),
            border: colour(&self.border, &fallback.border),
            surface: colour(&self.surface, &fallback.surface),
            accent: colour(&self.accent, &fallback.accent),
            accent_foreground: colour(&self.accent_foreground, &fallback.accent_foreground),
            success: colour(&self.success, &fallback.success),
            warning: colour(&self.warning, &fallback.warning),
            danger: colour(&self.danger, &fallback.danger),
            font: font(&self.font, &fallback.font),
            mono: font(&self.mono, &fallback.mono),
            font_size: if self.font_size.is_finite() {
                self.font_size.clamp(10.0, 24.0)
            } else {
                fallback.font_size
            },
        }
    }

    /// The variables a page styles itself with.
    fn css(&self) -> String {
        let t = self.cleaned();
        format!(
            ":root{{color-scheme:{};--bg:{};--fg:{};--muted:{};--border:{};--surface:{};--accent:{};--accent-fg:{};--success:{};--warning:{};--danger:{};--font:{};--mono:{};--font-size:{}px}}",
            if t.dark { "dark" } else { "light" },
            t.background,
            t.foreground,
            t.muted,
            t.border,
            t.surface,
            t.accent,
            t.accent_foreground,
            t.success,
            t.warning,
            t.danger,
            t.font,
            t.mono,
            t.font_size,
        )
    }
}

/// JSON that is safe inside a `<script>`: nothing in it can end the element.
fn script_json(value: &impl Serialize) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "null".into())
        .replace('<', "\\u003c")
}

/// What is written into a view's page before the agent's own HTML: the
/// doctype, the theme's variables, the base styles and `window.agentloft`.
/// The agent's HTML follows as it is, a fragment or a whole document (the
/// parser folds a second `<html>` or `<body>` into the first).
pub fn head(theme: &Theme, state: &Value) -> String {
    let api = API_JS
        .replace("__STATE__", &script_json(state))
        .replace("__THEME__", &script_json(&theme.cleaned()));
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><style id=\"agentloft-theme\">{}</style><style id=\"agentloft-base\">{BASE_CSS}</style><script>{api}</script></head><body>",
        theme.css(),
    )
}

/// Script (for the page's own world) that gives a page already showing a
/// new look, and tells it so (`agentloft:theme` on `window`).
pub fn theme_script(theme: &Theme) -> String {
    format!(
        "(()=>{{const s=document.getElementById('agentloft-theme');if(s)s.textContent={};if(window.agentloft)window.agentloft.theme={};window.dispatchEvent(new Event('agentloft:theme'));}})()",
        script_json(&theme.css()),
        script_json(&theme.cleaned()),
    )
}

/// The hosts a view may load scripts, styles and fonts from. Nothing else
/// on the network is reachable from a view.
pub const ALLOWED_HOSTS: &[&str] = &[
    "cdn.jsdelivr.net",
    "unpkg.com",
    "cdnjs.cloudflare.com",
    "esm.sh",
    "esm.run",
    "cdn.skypack.dev",
    "ga.jspm.io",
    "cdn.tailwindcss.com",
    "d3js.org",
    "cdn.plot.ly",
    "fonts.googleapis.com",
    "fonts.gstatic.com",
];

/// Whether a view may fetch `url`: `https`, from one of [`ALLOWED_HOSTS`].
pub fn allowed_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // No user names, no ports: just the host.
    let host = authority.to_ascii_lowercase();
    ALLOWED_HOSTS.contains(&host.as_str())
}

/// The address a view's page lives at. It is never fetched: the daemon
/// answers for it. A real, per-view origin gives the page storage of its
/// own and a secure context.
pub fn origin(item_id: &str) -> String {
    let label: String = item_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(40)
        .collect::<String>()
        .to_ascii_lowercase();
    format!("https://v{label}.view.agentloft.invalid/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_call_reads_into_a_view() {
        let view = parse(&json!({ "title": " Sizes ", "html": "<p>hi</p>" })).unwrap();
        assert_eq!(view.title.as_deref(), Some("Sizes"));
        assert_eq!(view.html, "<p>hi</p>");
        // The page alone will do.
        assert_eq!(parse(&json!("<b>x</b>")).unwrap().html, "<b>x</b>");
        assert!(
            parse(&json!({ "html": "x", "title": 3 }))
                .unwrap()
                .title
                .is_none()
        );
    }

    #[test]
    fn what_agents_get_wrong_is_named() {
        let err = |v: Value| parse(&v).unwrap_err();
        assert!(err(json!({})).contains("`html` is missing"));
        assert!(err(json!({ "html": "  " })).contains("empty"));
        assert!(err(json!({ "html": 3 })).contains("must be a string"));
        assert!(err(json!({ "body": [] })).contains("takes a page now"));
        assert!(err(json!(3)).contains("{ title?, html }"));
        let big = "x".repeat(MAX_HTML + 1);
        assert!(err(json!({ "html": big })).contains("bytes"));
    }

    #[test]
    fn only_our_tool_is_a_view() {
        assert!(is_tool("mcp__agentloft__show_ui"));
        assert!(!is_tool("mcp__other__show_ui"));
        assert!(!is_tool("show_ui"));
        assert!(!is_tool("mcp__agentloft__browser_click"));
    }

    #[test]
    fn a_call_still_being_written_gives_what_it_has() {
        let whole = r#"{"title":"Sizes","html":"<p class=\"a\">caf\u00e9 \ud83d\ude00\nline</p><script>x<1</script>"}"#;
        let full = partial(whole);
        assert_eq!(full.title.as_deref(), Some("Sizes"));
        assert_eq!(
            full.html,
            "<p class=\"a\">café 😀\nline</p><script>x<1</script>"
        );
        // Cut anywhere, it is a prefix of the whole and never garbage.
        for cut in 0..whole.len() {
            if !whole.is_char_boundary(cut) {
                continue;
            }
            let part = partial(&whole[..cut]);
            assert!(
                full.html.starts_with(&part.html),
                "cut at {cut}: {:?}",
                part.html
            );
            assert!(part.title.is_none() || part.title == full.title);
        }
        // The title may come second, and other keys are stepped over.
        let later = partial(r#"{"n":[1,{"a":"}"}],"html":"<i>","title":"T"}"#);
        assert_eq!(
            (later.html.as_str(), later.title.as_deref()),
            ("<i>", Some("T"))
        );
        assert_eq!(partial("").html, "");
        assert_eq!(partial(r#"{"html""#).html, "");
    }

    #[test]
    fn the_head_carries_theme_state_and_api() {
        let theme = Theme {
            accent: "#ff0000".into(),
            font: "Inter</style><script>alert(1)</script>".into(),
            background: "url(javascript:1);}body{display:none".into(),
            ..Theme::default()
        };
        let head = head(&theme, &json!({ "note": "</script><b>" }));
        assert!(head.starts_with("<!doctype html>"));
        assert!(head.ends_with("<body>"));
        assert!(head.contains("--accent:#ff0000"));
        assert!(head.contains("window.agentloft"));
        // Nothing a client or a view put in can end a tag early.
        assert!(!head.contains("</script><b>"));
        assert!(head.contains("\\u003c/script>\\u003cb>"));
        assert!(!head.contains("alert(1)</script>"));
        assert!(!head.contains("display:none"));
        assert_eq!(head.matches("</script>").count(), 1);
        let script = theme_script(&theme);
        assert!(script.contains("agentloft:theme") && script.contains("#ff0000"));
    }

    #[test]
    fn only_listed_hosts_load() {
        assert!(allowed_url("https://cdn.jsdelivr.net/npm/chart.js"));
        assert!(allowed_url(
            "https://fonts.googleapis.com/css2?family=Inter"
        ));
        assert!(allowed_url("https://UNPKG.com/d3"));
        assert!(!allowed_url("http://cdn.jsdelivr.net/npm/chart.js"));
        assert!(!allowed_url("https://evil.example/x.js"));
        assert!(!allowed_url("https://cdn.jsdelivr.net.evil.example/x.js"));
        assert!(!allowed_url("https://user@cdn.jsdelivr.net/x.js"));
        assert!(!allowed_url("https://cdn.jsdelivr.net:8443/x.js"));
        assert!(!allowed_url("data:text/html,hi"));
    }

    #[test]
    fn each_view_has_an_origin_of_its_own() {
        let a = origin("01A1-c8e7-B410");
        assert_eq!(a, "https://v01a1c8e7b410.view.agentloft.invalid/");
        assert_ne!(a, origin("01a1c8e7b411"));
    }
}
