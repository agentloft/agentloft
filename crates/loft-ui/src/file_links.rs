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
        let link = file_link(&code.value, &self.cwd, is_file)?;
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
}
