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
use std::time::Duration;
use web_time::Instant;

/// How long a lookup is trusted before the folder is checked again.
const FRESH: Duration = Duration::from_secs(30);

thread_local! {
    static FOUND: RefCell<HashMap<PathBuf, (Instant, Option<PathBuf>)>> = RefCell::new(HashMap::new());
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
    let found = loft_config::icons::find(root);
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
        .filter(|p| loft_config::icons::drawable(p))
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
    fn initials_read_like_names() {
        assert_eq!(initials("Matter & Why"), "MW");
        assert_eq!(initials("agentloft"), "AT");
        assert_eq!(initials("stickfigure-studio"), "SS");
    }
}
