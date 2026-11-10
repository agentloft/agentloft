//! Coloured file and folder icons (Material Icon Theme, MIT), looked up by
//! file name, then by extension (longest first: `test.ts` before `ts`),
//! and folders by name.

use gpui_kit::{AnyElement, Image, ImageFormat, IntoElement, Pixels, Styled as _, img};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[path = "file_icons_table.rs"]
mod table;

struct Maps {
    svgs: HashMap<&'static str, &'static [u8]>,
    extensions: HashMap<&'static str, &'static str>,
    file_names: HashMap<&'static str, &'static str>,
    folders: HashMap<&'static str, &'static str>,
}

fn maps() -> &'static Maps {
    static MAPS: OnceLock<Maps> = OnceLock::new();
    MAPS.get_or_init(|| Maps {
        svgs: table::SVGS.iter().copied().collect(),
        extensions: table::EXTENSIONS.iter().copied().collect(),
        file_names: table::FILE_NAMES.iter().copied().collect(),
        folders: table::FOLDERS.iter().copied().collect(),
    })
}

/// The icon name for an entry (an SVG in `assets/file-icons`).
pub fn icon_name(name: &str, is_dir: bool, open: bool) -> String {
    let m = maps();
    let lower = name.to_ascii_lowercase();
    if is_dir {
        let base = m.folders.get(lower.as_str()).copied().unwrap_or("folder");
        return if open {
            format!("{base}-open")
        } else {
            base.to_owned()
        };
    }
    if let Some(icon) = m.file_names.get(lower.as_str()) {
        return (*icon).to_owned();
    }
    // `a.test.ts` → try `test.ts`, then `ts`.
    let mut rest = lower.as_str();
    while let Some((_, ext)) = rest.split_once('.') {
        if let Some(icon) = m.extensions.get(ext) {
            return (*icon).to_owned();
        }
        rest = ext;
    }
    "file".to_owned()
}

fn image(icon: &str) -> Option<Arc<Image>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Image>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(img) = cache.get(icon) {
        return Some(img.clone());
    }
    let bytes = maps().svgs.get(icon)?;
    let img = Arc::new(Image::from_bytes(ImageFormat::Svg, bytes.to_vec()));
    cache.insert(icon.to_owned(), img.clone());
    Some(img)
}

/// The icon for a file or folder at `size`.
pub fn icon(name: &str, is_dir: bool, open: bool, size: Pixels) -> AnyElement {
    let icon = icon_name(name, is_dir, open);
    let image = image(&icon)
        .or_else(|| image(if is_dir { "folder" } else { "file" }))
        .expect("the fallback icons are bundled");
    img(image).size(size).flex_shrink_0().into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups() {
        assert_eq!(icon_name("main.rs", false, false), "rust");
        assert_eq!(icon_name("Cargo.lock", false, false), "lock");
        assert_eq!(icon_name("README.md", false, false), "readme");
        assert_eq!(icon_name("App.tsx", false, false), "react_ts");
        assert_eq!(icon_name("Dockerfile", false, false), "docker");
        assert_eq!(icon_name("x.unknownext", false, false), "file");
        assert_eq!(icon_name("Makefile", false, false), "makefile");
        assert_eq!(icon_name("src", true, false), "folder-src");
        assert_eq!(icon_name("src", true, true), "folder-src-open");
        assert_eq!(icon_name("whatever", true, false), "folder");
        // Every name the lookups can produce is bundled.
        let m = maps();
        for icon in m.extensions.values().chain(m.file_names.values()) {
            assert!(m.svgs.contains_key(icon), "{icon}");
        }
        for icon in m.folders.values() {
            assert!(m.svgs.contains_key(icon), "{icon}");
            assert!(
                m.svgs.contains_key(format!("{icon}-open").as_str()),
                "{icon}-open"
            );
        }
    }
}
