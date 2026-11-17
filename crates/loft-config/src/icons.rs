//! Finding a project's icon on disk: its favicon in the usual places, the
//! icon a page links to, else an icon-like image in an icon-ish folder.
//! Shared by the desktop app (which looks itself) and the daemon (which
//! looks on a browser session's behalf, ADR-0016).

use std::path::{Path, PathBuf};

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

/// Whether `path` is an image we can draw, of a sensible size.
pub fn drawable(path: &Path) -> bool {
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

/// Folders worth looking in for an icon (case aside), a few levels down.
const ICON_DIRS: &[&str] = &[
    "assets",
    "asset",
    "icon",
    "icons",
    "public",
    "static",
    "resources",
    "images",
    "img",
    "res",
    "branding",
    "brand",
    "logo",
    "logos",
    "media",
    "app",
    "src",
    "web",
    "www",
    "docs",
    "png",
    "svg",
    "ico",
    "favicon",
    "favicons",
];

/// Whether a file name reads like an icon for this project.
fn icon_like(stem: &str, project: &str) -> bool {
    let s = stem.to_ascii_lowercase();
    let project = project.to_ascii_lowercase();
    s.contains("favicon")
        || s.starts_with("icon")
        || s.starts_with("appicon")
        || s.starts_with("app-icon")
        || s.starts_with("app_icon")
        || s.starts_with("logo")
        || (!project.is_empty() && s.starts_with(&project))
}

/// The size a file name mentions (`icon-256`, `icon_128x128@2x`), if any.
fn named_size(stem: &str) -> Option<u32> {
    let digits: String = stem
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let n: u32 = digits.parse().ok()?;
    Some(if stem.contains("@2x") { n * 2 } else { n })
}

/// Icon-like images in icon-ish folders, best first: SVG, then the size
/// nearest 256 px, then the shallowest.
fn scan(root: &Path) -> Option<PathBuf> {
    let project = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut found: Vec<(u32, u32, usize, PathBuf)> = Vec::new();
    fn walk(dir: &Path, depth: usize, project: &str, found: &mut Vec<(u32, u32, usize, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten().take(400) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            if path.is_dir() {
                let iconish = ICON_DIRS.contains(&lower.as_str())
                    || lower.ends_with(".iconset")
                    || lower.ends_with(".appiconset")
                    || lower.ends_with(".xcassets");
                if depth < 3 && iconish {
                    walk(&path, depth + 1, project, found);
                }
                continue;
            }
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Inside an icon set every image is the icon.
            let in_set = dir
                .extension()
                .is_some_and(|e| e == "iconset" || e == "appiconset");
            if !(in_set || icon_like(&stem, project)) || !drawable(&path) {
                continue;
            }
            let svg = u32::from(!lower.ends_with(".svg"));
            let size = named_size(&stem).map_or(1000, |n| n.abs_diff(256));
            found.push((svg, size, depth, path));
        }
    }
    walk(root, 0, &project, &mut found);
    found.sort();
    found.into_iter().next().map(|(.., p)| p)
}

/// The icon in a project folder, if it has one. Reads the disk each time;
/// callers cache.
pub fn find(root: &Path) -> Option<PathBuf> {
    CANDIDATES
        .iter()
        .map(|c| root.join(c))
        .find(|p| drawable(p))
        .or_else(|| PAGES.iter().find_map(|page| linked_icon(root, page)))
        .or_else(|| scan(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_favicons_where_projects_keep_them() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find(dir.path()), None);
        // A page linking its icon.
        std::fs::create_dir_all(dir.path().join("public/img")).unwrap();
        std::fs::write(dir.path().join("public/img/Logo.svg"), "<svg/>").unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            r#"<html><head><LINK rel="icon" href="/img/Logo.svg"></head></html>"#,
        )
        .unwrap();
        assert_eq!(
            find(dir.path()),
            Some(dir.path().join("public/img/Logo.svg"))
        );
        // A favicon at a known spot wins.
        std::fs::write(dir.path().join("public/favicon.png"), [1u8, 2, 3]).unwrap();
        assert_eq!(
            find(dir.path()),
            Some(dir.path().join("public/favicon.png"))
        );
    }

    #[test]
    fn finds_app_icons_in_icon_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("myapp");
        for f in [
            "assets/icon/icon-1024.png",
            "assets/icon/png/myapp-256.png",
            "assets/icon/AppIcon.iconset/icon_16x16.png",
            "node_modules/x/logo.png",
            "src/main.rs",
        ] {
            let path = root.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, [1u8, 2, 3]).unwrap();
        }
        // The one nearest 256 px.
        assert_eq!(
            find(&root),
            Some(root.join("assets/icon/png/myapp-256.png"))
        );
    }
}
