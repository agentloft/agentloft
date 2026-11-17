//! A file's path in backticks, shown as a link to the file: its icon and
//! its name, which open it in the side panel. Only a path that is a file on
//! this machine becomes one (`open /some/file` stays code), so there is
//! nothing to guess from how the text looks.

use crate::press::ClaimsPress as _;
use crate::theme::Palette;
use gpui_kit::component::h_flex;
use gpui_kit::component::text::{
    InlineElement, InlineRenderContext, MarkdownNode, MarkdownParseContext, MarkdownPlugin,
    markdown_ast::Node,
};
use gpui_kit::{
    App, InteractiveElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use loft_protocol::ThreadId;
use std::path::{Path, PathBuf};

const NODE: &str = "file-link";
/// Longer than any path worth looking up.
const MAX_PATH: usize = 512;

/// The Markdown plugin: paths are looked up from `cwd` (the thread's
/// folder), and open in `thread`'s side panel.
pub(crate) struct FileLinks {
    pub cwd: String,
    pub thread: Option<ThreadId>,
}

/// A path that names a file.
#[derive(Debug, PartialEq)]
struct FileLink {
    /// Where the file is.
    path: PathBuf,
    /// What the side panel is asked for: the path from the thread's folder
    /// for a file inside it, the whole path otherwise.
    open: String,
    /// What is shown: the file's name, and the line if one was given.
    label: String,
}

/// A Markdown link whose target is the path of a file on this machine
/// (`/abs/path`, `~/path` or a path inside `cwd`, each with an optional
/// `:line[:column]`): what the side panel should be asked for. `None` for
/// anything else (http links, missing files), which is opened the usual way.
pub(crate) fn link_file_target(target: &str, cwd: &str) -> Option<String> {
    file_link(target, cwd, is_file).map(|l| l.open)
}

/// A Markdown link's target as a path on the computer the daemon runs on,
/// when this one can't look there: any absolute path, or one relative to
/// `cwd` (not `~/`, whose home is that computer's). Links with a scheme
/// (`https:`, `mailto:`) aren't paths.
pub(crate) fn link_path_target(target: &str, cwd: &str) -> Option<String> {
    let target = target.trim();
    let first = target.split('/').next().unwrap_or("");
    if target.starts_with("~/") || target.starts_with('#') || first.contains(':') {
        return None;
    }
    // Nothing to look at: a trailing `:line` is a line, not a name.
    let path = line_suffix(target).map_or(target, |at| &target[..at]);
    file_link(path, cwd, |_| true).map(|l| l.open)
}

/// `code` as a link, if it is the path of a file (alone, or followed by
/// `:line` or `:line:column`).
fn file_link(code: &str, cwd: &str, is_file: impl Fn(&Path) -> bool) -> Option<FileLink> {
    let code = code.trim();
    if code.is_empty() || code.len() > MAX_PATH || code.contains('\n') {
        return None;
    }
    let found = |raw: &str| {
        let relative = !raw.starts_with('/') && !raw.starts_with("~/");
        if raw.is_empty() || relative && cwd.is_empty() {
            return None;
        }
        let path = crate::items::local_image_path(raw, cwd)?;
        is_file(&path).then_some(path)
    };
    let (path, line) = match found(code) {
        Some(path) => (path, ""),
        None => {
            let at = line_suffix(code)?;
            (found(&code[..at])?, &code[at..])
        }
    };
    let name = path.file_name()?.to_string_lossy().into_owned();
    let open = match path.strip_prefix(cwd) {
        Ok(inside) if !cwd.is_empty() => inside.to_string_lossy().into_owned(),
        _ => path.to_string_lossy().into_owned(),
    };
    Some(FileLink {
        path,
        open,
        label: format!("{name}{line}"),
    })
}

/// Where a trailing `:12` or `:12:5` starts.
fn line_suffix(code: &str) -> Option<usize> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (head, last) = code.rsplit_once(':')?;
    if !digits(last) {
        return None;
    }
    match head.rsplit_once(':') {
        Some((path, line)) if digits(line) => Some(path.len()),
        _ => Some(head.len()),
    }
}

/// What the daemon said about paths in replies of threads whose files
/// aren't on this computer (a browser, a window on an SSH host), by thread
/// and path: `None` while asking.
type There = std::collections::HashMap<(ThreadId, PathBuf), Option<bool>>;

fn there() -> std::sync::MutexGuard<'static, There> {
    static THERE: std::sync::OnceLock<std::sync::Mutex<There>> = std::sync::OnceLock::new();
    THERE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The paths in `text`'s inline code (`` `…` ``) that could be files, as
/// `is_file` would be asked about them, not yet asked of the daemon for
/// `thread`: marked as being asked.
pub(crate) fn paths_to_ask(thread: &ThreadId, text: &str, cwd: &str) -> Vec<PathBuf> {
    const MOST: usize = 4096;
    let mut out = Vec::new();
    let mut there = there();
    if there.len() >= MOST {
        there.retain(|_, known| known.is_none());
    }
    for code in inline_code(text) {
        // Every path `file_link` would look at, the `:line` form included.
        let wanted = std::cell::RefCell::new(Vec::new());
        file_link(code, cwd, |path| {
            wanted.borrow_mut().push(path.to_path_buf());
            false
        });
        for path in wanted.into_inner() {
            let key = (thread.clone(), path);
            if !there.contains_key(&key) && out.len() < loft_protocol::MAX_FILES_EXIST {
                out.push(key.1.clone());
                there.insert(key, None);
            }
        }
    }
    out
}

/// The daemon's answers for `paths` (`None`: it couldn't say, ask again
/// later). Whether any is a file (the reply then links it).
pub(crate) fn record(thread: &ThreadId, paths: &[PathBuf], files: Option<&[bool]>) -> bool {
    let mut there = there();
    let mut any = false;
    for (ix, path) in paths.iter().enumerate() {
        let key = (thread.clone(), path.clone());
        match files {
            Some(files) => {
                let file = files.get(ix).copied().unwrap_or(false);
                any |= file;
                there.insert(key, Some(file));
            }
            None => {
                there.remove(&key);
            }
        }
    }
    any
}

/// The text of each inline code span in Markdown `text` (not fenced
/// blocks).
fn inline_code(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find('`') {
            let after = &rest[start..];
            let ticks = after.len() - after.trim_start_matches('`').len();
            let body = &after[ticks..];
            let close = "`".repeat(ticks);
            let Some(end) = body.find(&close) else {
                break;
            };
            spans.push(body[..end].trim());
            rest = &body[end + ticks..];
        }
    }
    spans
}

/// Whether `path` (in a reply of `thread`) is a file: on this computer, by
/// looking; on another, by what its daemon said (no until it has).
fn is_file_in(path: &Path, thread: Option<&ThreadId>) -> bool {
    if let Some(thread) = thread {
        let key = (thread.clone(), path.to_path_buf());
        if let Some(Some(known)) = there().get(&key) {
            return *known;
        }
        if crate::host::thread_is_elsewhere(thread) {
            return false;
        }
    }
    is_file(path)
}

/// Whether `path` is a file, remembered for a few seconds: a reply is read
/// again with every piece of it that arrives.
fn is_file(path: &Path) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use web_time::{Duration, Instant};
    const FRESH: Duration = Duration::from_secs(5);
    const MOST: usize = 2048;
    if crate::host::IS_WEB {
        return false;
    }
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, (Instant, bool)>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some((at, known)) = seen.get(path)
        && at.elapsed() < FRESH
    {
        return *known;
    }
    let found = std::fs::metadata(path).is_ok_and(|m| m.is_file());
    if seen.len() >= MOST {
        seen.clear();
    }
    seen.insert(path.to_path_buf(), (Instant::now(), found));
    found
}

impl MarkdownPlugin for FileLinks {
    fn name(&self) -> &str {
        NODE
    }

    fn parse(&self, node: &Node, _: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::InlineCode(code) = node else {
            return None;
        };
        let link = file_link(&code.value, &self.cwd, |path| {
            is_file_in(path, self.thread.as_ref())
        })?;
        // Selecting and copying it gives the path as it was written.
        Some(MarkdownNode::new(NODE, link).text(code.value.clone()))
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let link = node.data::<FileLink>()?;
        let p = Palette::of(cx);
        let id = SharedString::from(format!(
            "file-link-{}-{}",
            node.source_range().map(|r| r.start).unwrap_or_default(),
            link.open
        ));
        let (thread, open, path) = (self.thread.clone(), link.open.clone(), link.path.clone());
        let tip = SharedString::from(link.path.to_string_lossy().into_owned());
        let size = context.font_size();
        Some(InlineElement::new(
            h_flex()
                .id(id)
                .h(context.line_height())
                .gap(px(4.))
                .items_center()
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|d| d.bg(p.hover))
                .child(crate::file_icons::icon(&link.label, false, false, size))
                // The colour links have in the text around it.
                .child(div().text_color(p.accent).child(link.label.clone()))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                })
                // The press is the link's, not the start of a text selection
                // in the message around it (which would otherwise keep
                // following the pointer after the click opened the file).
                .claims_press()
                .on_click(move |_, window, cx| {
                    let store = crate::windows::store_of(window, cx);
                    match (store, &thread) {
                        (Some(store), Some(thread)) => {
                            let (thread, open) = (thread.clone(), open.clone());
                            store.update(cx, |s, cx| s.open_file(thread, open, cx));
                        }
                        // Nowhere of ours to show it: the system's own app.
                        _ => cx.open_url(&format!("file://{}", path.to_string_lossy())),
                    }
                }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_elsewhere_are_asked_once_and_linked_by_the_answer() {
        let spans = inline_code(
            "Run `cargo test` on `/srv/app/main.rs:12`.\n```\n`not this`\n```\n``a`b``",
        );
        assert_eq!(spans, ["cargo test", "/srv/app/main.rs:12", "a`b"]);

        let thread = ThreadId::new();
        let text = "See `/srv/app/main.rs:12` and `notes.md`, then `ls`.";
        let asked = paths_to_ask(&thread, text, "/srv/app");
        // Every form `file_link` looks at: the whole, then without `:12`.
        assert!(asked.contains(&PathBuf::from("/srv/app/main.rs:12")));
        assert!(asked.contains(&PathBuf::from("/srv/app/main.rs")));
        assert!(asked.contains(&PathBuf::from("/srv/app/notes.md")));
        // Asked once: while asking, and after, nothing new.
        assert!(paths_to_ask(&thread, text, "/srv/app").is_empty());

        let answers: Vec<bool> = asked
            .iter()
            .map(|p| p == Path::new("/srv/app/main.rs"))
            .collect();
        assert!(record(&thread, &asked, Some(&answers)));
        assert!(is_file_in(Path::new("/srv/app/main.rs"), Some(&thread)));
        assert!(!is_file_in(Path::new("/srv/app/notes.md"), Some(&thread)));
        let link = file_link("/srv/app/main.rs:12", "/srv/app", |p| {
            is_file_in(p, Some(&thread))
        })
        .unwrap();
        assert_eq!(
            (link.open.as_str(), link.label.as_str()),
            ("main.rs", "main.rs:12")
        );

        // No answer (not connected): asked again next time.
        let other = ThreadId::new();
        let asked = paths_to_ask(&other, "`/x/y.rs`", "/x");
        assert!(!record(&other, &asked, None));
        assert_eq!(paths_to_ask(&other, "`/x/y.rs`", "/x"), asked);
    }

    fn link(code: &str) -> Option<(String, String)> {
        let files = [
            "/w/proj/src/main.rs",
            "/w/other/clip one.mp4",
            "/w/proj/a:1",
        ];
        file_link(code, "/w/proj", |p| files.iter().any(|f| Path::new(f) == p))
            .map(|l| (l.open, l.label))
    }

    #[test]
    fn only_paths_of_files_are_links() {
        let found = |open: &str, label: &str| Some((open.to_string(), label.to_string()));
        // Inside the thread's folder: asked for from there, however written.
        assert_eq!(link("src/main.rs"), found("src/main.rs", "main.rs"));
        assert_eq!(link("/w/proj/src/main.rs"), found("src/main.rs", "main.rs"));
        // Outside it: by its whole path.
        assert_eq!(
            link("/w/other/clip one.mp4"),
            found("/w/other/clip one.mp4", "clip one.mp4")
        );
        // A line (and column) after the path stays on the name.
        assert_eq!(link("src/main.rs:12"), found("src/main.rs", "main.rs:12"));
        assert_eq!(
            link("src/main.rs:12:5"),
            found("src/main.rs", "main.rs:12:5")
        );
        // A file whose own name ends that way is that file.
        assert_eq!(link("a:1"), found("a:1", "a:1"));
        // Commands, folders, names of nothing, and other code are code.
        assert_eq!(link("open /w/proj/src/main.rs"), None);
        assert_eq!(link("src"), None);
        assert_eq!(link("src/nope.rs"), None);
        assert_eq!(link("let x = 1;"), None);
        assert_eq!(link(""), None);
    }

    #[test]
    fn a_relative_path_needs_a_folder() {
        assert_eq!(file_link("main.rs", "", |_| true), None);
        assert!(file_link("/main.rs", "", |_| true).is_some());
    }

    #[test]
    fn a_markdown_link_to_a_file_opens_in_the_panel() {
        // A Markdown link's target names the side panel's file, with any
        // `:line` suffix dropped. (The real `is_file` runs here; these paths
        // don't exist, so it never matches.)
        assert_eq!(link_file_target("/does/not/exist.rs", "/w/proj"), None);
        assert_eq!(link_file_target("https://pi.dev", "/w/proj"), None);
        // On another computer, paths are taken at their word.
        assert_eq!(
            link_path_target("/srv/app/main.rs:12", "/srv/app").as_deref(),
            Some("main.rs")
        );
        assert_eq!(link_path_target("https://pi.dev/x", "/srv/app"), None);
        assert_eq!(link_path_target("mailto:me@x.y", "/srv/app"), None);
        assert_eq!(link_path_target("~/notes.md", "/srv/app"), None);
    }
}
