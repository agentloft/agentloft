//! The web client's files (`cargo xtask web` → `target/web`, or the app
//! bundle's copy), served from a list made by reading the folder.
//!
//! A request path is only ever compared with that list: it is never joined
//! onto a directory, so there is nothing for `..` or an encoded slash to
//! escape from. Symlinks and odd names in the folder are left out of the
//! list altogether.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Files served before a browser has signed in: the page shell and what the
/// pairing screen needs. Everything else in the bundle (the app itself)
/// needs a session.
const PUBLIC: &[&str] = &[
    "index.html",
    "boot.js",
    "app.css",
    "favicon.svg",
    "favicon.ico",
    "icon.png",
    "manifest.webmanifest",
];

const MAX_FILES: usize = 512;
const MAX_DEPTH: usize = 3;
/// How long a listing is trusted before the folder is read again (a
/// rebuild replaces the files).
const LISTING_TTL: Duration = Duration::from_secs(2);

/// Shown at `/` when the bundle hasn't been built or installed.
pub(crate) const MISSING_PAGE: &str = "<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Agentloft</title>\
<body><h1>Agentloft</h1><p>The web client isn't installed here. Build it with \
<code>cargo xtask web</code>, then reload.</p></body></html>\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asset {
    pub path: PathBuf,
    pub len: u64,
    pub content_type: &'static str,
    /// Changes when the file does.
    pub etag: String,
    pub public: bool,
    /// A gzip copy (`<name>.gz`) next to it, for browsers that accept one.
    pub gzip: Option<(PathBuf, u64)>,
}

type Listing = HashMap<String, Asset>;

pub(crate) struct Bundle {
    /// Where to look, in order; the first folder with an `index.html` wins.
    candidates: Vec<PathBuf>,
    cache: Mutex<Option<(Instant, Arc<Listing>)>>,
}

fn content_type(name: &str) -> Option<&'static str> {
    Some(match name.rsplit_once('.')?.1 {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => return None,
    })
}

fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Bundle {
    /// The usual places: `$AGENTLOFT_WEB_DIR`, then next to the daemon
    /// (`web/`), a dev build's `target/web`, and the app bundle's
    /// `Resources/web`.
    pub(crate) fn locate() -> Self {
        let mut candidates = Vec::new();
        if let Some(dir) = std::env::var_os("AGENTLOFT_WEB_DIR").filter(|d| !d.is_empty()) {
            candidates.push(PathBuf::from(dir));
        }
        if let Some(bin) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            candidates.push(bin.join("web"));
            candidates.push(bin.join("../web"));
            candidates.push(bin.join("../Resources/web"));
        }
        Self::at(candidates)
    }

    pub(crate) fn at(candidates: Vec<PathBuf>) -> Self {
        Self {
            candidates,
            cache: Mutex::new(None),
        }
    }

    fn listing(&self) -> Arc<Listing> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, listing)) = cache.as_ref()
            && at.elapsed() < LISTING_TTL
        {
            return listing.clone();
        }
        let listing = Arc::new(
            self.candidates
                .iter()
                .find(|dir| dir.join("index.html").is_file())
                .map(|dir| scan(dir))
                .unwrap_or_default(),
        );
        *cache = Some((Instant::now(), listing.clone()));
        listing
    }

    /// The bundle is there (it has an `index.html`).
    pub(crate) fn found(&self) -> bool {
        self.listing().contains_key("index.html")
    }

    /// The file a URL path names, if the bundle has it. `/` is the page.
    pub(crate) fn get(&self, url_path: &str) -> Option<Asset> {
        let name = match url_path.strip_prefix('/')? {
            "" => "index.html",
            rest => rest,
        };
        self.listing().get(name).cloned()
    }
}

fn scan(root: &Path) -> Listing {
    let mut files: HashMap<String, (PathBuf, u64, SystemTime)> = HashMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if files.len() >= MAX_FILES {
                break;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // `symlink_metadata`: a link is neither a file nor a directory
            // here, so nothing outside the folder is ever listed.
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !plain_name(&name) {
                continue;
            }
            let rel = format!("{prefix}{name}");
            if meta.is_dir() {
                if depth + 1 < MAX_DEPTH {
                    stack.push((entry.path(), format!("{rel}/"), depth + 1));
                }
            } else if meta.is_file() {
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.insert(rel, (entry.path(), meta.len(), modified));
            }
        }
    }
    let mut listing = Listing::new();
    for (rel, (path, len, modified)) in &files {
        let Some(content_type) = content_type(rel) else {
            continue;
        };
        let stamp = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let gzip = files
            .get(&format!("{rel}.gz"))
            .map(|(p, l, _)| (p.clone(), *l));
        listing.insert(
            rel.clone(),
            Asset {
                path: path.clone(),
                len: *len,
                content_type,
                etag: format!("\"{len:x}-{stamp:x}\""),
                public: PUBLIC.contains(&rel.as_str()),
                gzip,
            },
        );
    }
    listing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> (tempfile::TempDir, Bundle) {
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        std::fs::create_dir_all(web.join("fonts")).unwrap();
        for (name, body) in [
            ("index.html", "<!doctype html>"),
            ("boot.js", "//"),
            ("loft_web.js", "//"),
            ("loft_web_bg.wasm", "\0asm"),
            ("loft_web_bg.wasm.gz", "gz"),
            ("fonts/Inter-Regular.ttf", "ttf"),
            (".env", "SECRET=1"),
            ("notes.md", "unknown type"),
        ] {
            std::fs::write(web.join(name), body).unwrap();
        }
        std::fs::write(dir.path().join("secret.txt"), "hunter2").unwrap();
        let bundle = Bundle::at(vec![dir.path().join("missing"), web]);
        (dir, bundle)
    }

    #[test]
    fn serves_only_listed_files() {
        let (_dir, bundle) = bundle();
        assert!(bundle.found());
        assert_eq!(bundle.get("/").unwrap(), bundle.get("/index.html").unwrap());
        let wasm = bundle.get("/loft_web_bg.wasm").unwrap();
        assert_eq!(wasm.content_type, "application/wasm");
        assert!(!wasm.public, "the app needs a session");
        assert_eq!(wasm.gzip.as_ref().map(|g| g.1), Some(2));
        assert!(bundle.get("/index.html").unwrap().public);
        assert!(bundle.get("/boot.js").unwrap().public);
        assert!(bundle.get("/fonts/Inter-Regular.ttf").is_some());
        // Hidden files, unknown types and compressed copies aren't served.
        for path in ["/.env", "/notes.md", "/loft_web_bg.wasm.gz", "/nope.js"] {
            assert!(bundle.get(path).is_none(), "{path}");
        }
    }

    #[test]
    fn traversal_has_nothing_to_escape_from() {
        let (_dir, bundle) = bundle();
        for path in [
            "/../secret.txt",
            "/..%2fsecret.txt",
            "/%2e%2e/secret.txt",
            "/fonts/../../secret.txt",
            "//secret.txt",
            "/fonts/..\\..\\secret.txt",
            "secret.txt",
            "/index.html/",
            "/./index.html",
            "/index.html%00.js",
            "",
        ] {
            assert!(bundle.get(path).is_none(), "{path:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_the_folder_are_ignored() {
        let (dir, bundle) = bundle();
        let web = dir.path().join("web");
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), web.join("leak.txt")).unwrap();
        std::os::unix::fs::symlink(dir.path(), web.join("up")).unwrap();
        assert!(bundle.get("/leak.txt").is_none());
        assert!(bundle.get("/up/secret.txt").is_none());
    }

    #[test]
    fn a_missing_bundle_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = Bundle::at(vec![dir.path().join("nope")]);
        assert!(!bundle.found());
        assert!(bundle.get("/").is_none());
    }

    #[test]
    fn etag_follows_the_file() {
        let (dir, bundle) = bundle();
        let before = bundle.get("/boot.js").unwrap().etag;
        std::fs::write(dir.path().join("web/boot.js"), "// changed").unwrap();
        // Past the listing's lifetime, the new size shows.
        let fresh = Bundle::at(vec![dir.path().join("web")]);
        assert_ne!(fresh.get("/boot.js").unwrap().etag, before);
    }
}
