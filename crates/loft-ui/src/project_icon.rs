//! A project's icon: the image chosen for it, else its favicon (found in
//! the usual places), else its initials on a colour of its own.

use crate::theme::Palette;
use gpui_kit::{
    AnyElement, Hsla, IntoElement as _, ObjectFit, ParentElement as _, Styled as _,
    StyledImage as _, div, img, px,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where web and app projects keep their icon, best first.
const CANDIDATES: &[&str] = &[
    "favicon.svg",
    "favicon.png",
    "favicon.ico",
    "public/favicon.svg",
    "public/favicon.png",
    "public/favicon.ico",
    "public/icon.svg",
    "public/icon.png",
    "static/favicon.svg",
    "static/favicon.png",
    "static/favicon.ico",
    "app/icon.svg",
    "app/icon.png",
    "app/favicon.ico",
    "src/app/icon.svg",
    "src/app/icon.png",
    "src/app/favicon.ico",
    "src/favicon.svg",
    "src/favicon.ico",
    "assets/icon.svg",
    "assets/icon.png",
    "assets/logo.svg",
    "assets/logo.png",
    "icon.svg",
    "icon.png",
    "logo.svg",
    "logo.png",
];

/// Pages whose `<link rel="icon" href="…">` names the icon.
const PAGES: &[&str] = &["index.html", "public/index.html", "src/index.html"];

/// Images we can draw (GPUI decodes these).
const DRAWABLE: &[&str] = &["svg", "png", "ico", "jpg", "jpeg", "webp", "gif"];

/// How long a lookup is trusted before the folder is checked again.
const FRESH: Duration = Duration::from_secs(30);

thread_local! {
    static FOUND: RefCell<HashMap<PathBuf, (Instant, Option<PathBuf>)>> = RefCell::new(HashMap::new());
}

fn drawable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| DRAWABLE.contains(&e.to_ascii_lowercase().as_str()))
        && std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() < 2_000_000)
}

/// The favicon a page links to (`<link rel="icon" href="/x.svg">`), if any.
fn linked_icon(root: &Path, page: &str) -> Option<PathBuf> {
    let html = std::fs::read_to_string(root.join(page)).ok()?;
    let lower = html.to_ascii_lowercase();
    let mut at = 0;
    while let Some(i) = lower[at..].find("<link") {
        let start = at + i;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &lower[start..end];
        at = end;
        if !(tag.contains("rel=\"icon\"")
            || tag.contains("rel='icon'")
            || tag.contains("shortcut icon"))
        {
            continue;
        }
        let href = ["href=\"", "href='"].iter().find_map(|k| {
            let i = tag.find(k)? + k.len();
            let quote = k.chars().last()?;
            let j = tag[i..].find(quote)? + i;
            // From the original text (case kept).
            Some(html[start + i..start + j].to_owned())
        })?;
        if href.starts_with("http") || href.starts_with("data:") {
            continue;
        }
        let rel = href.trim_start_matches('/');
        // `/x` is served from `public/` in most setups.
        for base in [
            root.to_path_buf(),
            root.join("public"),
            root.join(Path::new(page).parent()?),
        ] {
            let path = base.join(rel);
            if drawable(&path) {
                return Some(path);
            }
        }
    }
    None
}

/// The favicon in a project folder, if it has one.
pub fn favicon(root: &Path) -> Option<PathBuf> {
    let cached = FOUND.with(|f| {
        f.borrow()
            .get(root)
            .filter(|(at, _)| at.elapsed() < FRESH)
            .map(|(_, found)| found.clone())
    });
    if let Some(found) = cached {
        return found;
    }
    let found = CANDIDATES
        .iter()
        .map(|c| root.join(c))
        .find(|p| drawable(p))
        .or_else(|| PAGES.iter().find_map(|page| linked_icon(root, page)));
    FOUND.with(|f| {
        f.borrow_mut()
            .insert(root.to_path_buf(), (Instant::now(), found.clone()))
    });
    found
}

/// The project's image: chosen, else its favicon.
pub fn image(project: &loft_protocol::Project) -> Option<PathBuf> {
    project
        .icon
        .as_ref()
        .map(PathBuf::from)
        .filter(|p| drawable(p))
        .or_else(|| favicon(Path::new(&project.root_path)))
}

/// "Matter & Why" → "MW"; "agentloft" → "AT" (first and last letter).
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let first = |w: &str| w.chars().next().map(|c| c.to_ascii_uppercase());
    match words.as_slice() {
        [] => "?".into(),
        [one] => {
            let chars: Vec<char> = one.chars().collect();
            let mut s = String::new();
            s.extend(chars.first().map(|c| c.to_ascii_uppercase()));
            if chars.len() > 1 {
                s.extend(chars.last().map(|c| c.to_ascii_uppercase()));
            }
            s
        }
        [a, b, ..] => first(a).into_iter().chain(first(b)).collect(),
    }
}

/// A steady colour per project name.
pub fn color(name: &str) -> Hsla {
    const HUES: [f32; 8] = [205., 150., 0., 85., 38., 270., 175., 325.];
    let hash = name
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    gpui_kit::hsla(HUES[hash as usize % HUES.len()] / 360., 0.75, 0.62, 1.)
}

/// The project's icon, `size` points square.
pub fn badge(project: &loft_protocol::Project, size: f32, p: &Palette) -> AnyElement {
    match image(project) {
        Some(path) => div()
            .size(px(size))
            .flex_none()
            .rounded(px(size * 0.24))
            .overflow_hidden()
            .child(img(path).size_full().object_fit(ObjectFit::Contain))
            .into_any_element(),
        None => initials_badge(&project.name, size, p),
    }
}

/// Initials on the project's colour.
pub fn initials_badge(name: &str, size: f32, _p: &Palette) -> AnyElement {
    let c = color(name);
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(size * 0.24))
        .bg(c.opacity(0.18))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(size * 0.45))
        .font_weight(gpui_kit::FontWeight::BOLD)
        .text_color(c)
        .child(initials(name))
        .into_any_element()
}

/// Forget cached lookups (after an icon changes).
pub fn forget() {
    FOUND.with(|f| f.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_favicons_where_projects_keep_them() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(favicon(dir.path()), None);
        // A page linking its icon.
        std::fs::create_dir_all(dir.path().join("public/img")).unwrap();
        std::fs::write(dir.path().join("public/img/Logo.svg"), "<svg/>").unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            r#"<html><head><LINK rel="icon" href="/img/Logo.svg"></head></html>"#,
        )
        .unwrap();
        forget();
        assert_eq!(
            favicon(dir.path()),
            Some(dir.path().join("public/img/Logo.svg"))
        );
        // A favicon at a known spot wins.
        std::fs::write(dir.path().join("public/favicon.png"), [1u8, 2, 3]).unwrap();
        forget();
        assert_eq!(
            favicon(dir.path()),
            Some(dir.path().join("public/favicon.png"))
        );
    }

    #[test]
    fn initials_read_like_names() {
        assert_eq!(initials("Matter & Why"), "MW");
        assert_eq!(initials("agentloft"), "AT");
        assert_eq!(initials("stickfigure-studio"), "SS");
    }
}
