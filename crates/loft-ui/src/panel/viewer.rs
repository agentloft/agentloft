//! File viewer: syntax-highlighted code, rendered Markdown (with a source
//! toggle), images, office documents, and an external-open fallback.

use crate::panel::docs::{DocPreview, doc_kind};
use crate::store::AppStore;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::base::input::{
    FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, InputHighlighterFactory, Rope,
};
use gpui_kit::component::highlighter::SyntaxHighlighter;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, AsyncWindowContext, Context, Entity, FontWeight, HighlightStyle, Image,
    ImageFormat, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Task, WeakEntity,
    Window, div, img, prelude::FluentBuilder as _, px,
};
use loft_protocol::{FileContent, FsRoot, Request, Response};
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

enum Content {
    Loading,
    Code(Entity<EditorState>),
    Markdown {
        preview: Entity<TextViewState>,
        /// The highlighted source, built the first time it's asked for.
        source: Option<Entity<EditorState>>,
        text: Arc<str>,
    },
    Image(Arc<Image>),
    Doc(Entity<DocPreview>),
    External(String),
    Error(String),
}

pub struct FileViewer {
    store: Entity<AppStore>,
    root: FsRoot,
    pub rel_path: String,
    abs_path: Option<String>,
    size: u64,
    truncated: bool,
    content: Content,
    show_source: bool,
    /// Highlighting the file that's about to be shown, off the main thread.
    pending: Option<Task<()>>,
}

impl FileViewer {
    pub fn new(
        store: Entity<AppStore>,
        root: FsRoot,
        rel_path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            store,
            root,
            rel_path,
            abs_path: None,
            size: 0,
            truncated: false,
            content: Content::Loading,
            show_source: false,
            pending: None,
        };
        this.reload(window, cx);
        this
    }

    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.store.read(cx).client() else {
            self.content = Content::Error("Not connected to agentloftd.".into());
            cx.notify();
            return;
        };
        let fut = client.request(Request::ReadFile {
            root: self.root.clone(),
            rel_path: self.rel_path.clone(),
        });
        cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let result = fut.await;
                let _ = this.update_in(cx, |this, window, cx| match result {
                    Ok(Response::File { file }) => this.show(file, window, cx),
                    Ok(other) => {
                        this.content = Content::Error(format!("unexpected response: {other:?}"));
                        cx.notify();
                    }
                    Err(err) => {
                        this.content = Content::Error(err.message);
                        cx.notify();
                    }
                });
            },
        )
        .detach();
    }

    fn show(&mut self, file: FileContent, window: &mut Window, cx: &mut Context<Self>) {
        self.abs_path = Some(file.abs_path.clone());
        self.size = file.size;
        self.truncated = file.truncated;
        let ext = extension(&self.rel_path);
        let bytes = file.bytes.0;
        self.content = if let Some(format) = image_format(&ext) {
            Content::Image(Arc::new(Image::from_bytes(format, bytes)))
        } else if let Some(kind) = doc_kind(&self.rel_path) {
            let name = self.rel_path.clone();
            Content::Doc(cx.new(|cx| DocPreview::new(kind, &name, bytes, window, cx)))
        } else if ext == "pdf" {
            Content::External("PDF preview opens in your default viewer.".into())
        } else if file.binary {
            Content::External(format!("Binary file ({}).", human_size(file.size)))
        } else {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if matches!(ext.as_str(), "md" | "markdown" | "mdx") {
                // Rendered first; the highlighted source only when asked for.
                let preview = cx.new(|cx| {
                    TextViewState::markdown(&text, cx)
                        .selectable(true)
                        .scrollable(true)
                });
                self.content = Content::Markdown {
                    preview,
                    source: None,
                    text: text.into(),
                };
                if self.show_source {
                    self.build_source(window, cx);
                }
                self.pending = None;
                cx.notify();
                return;
            }
            let language = language_for(&self.rel_path);
            // Parse before showing, so the file appears already coloured
            // rather than plain and then repainted. The previous content stays
            // up meanwhile.
            let rope = Rope::from(text.as_str());
            let parse = cx.background_spawn(async move {
                let mut highlighter = SyntaxHighlighter::new(language);
                highlighter.update(None, &rope, None);
                highlighter
            });
            self.pending = Some(cx.spawn_in(
                window,
                async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                    let highlighter = parse.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        let source = code_editor(language, &text, highlighter, window, cx);
                        this.content = Content::Code(source);
                        this.pending = None;
                        cx.notify();
                    });
                },
            ));
            return;
        };
        self.pending = None;
        cx.notify();
    }

    /// Highlight a Markdown file's source (off the main thread) for the
    /// Source toggle.
    fn build_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Markdown {
            source: None, text, ..
        } = &self.content
        else {
            return;
        };
        let text = text.clone();
        let rope = Rope::from(&*text);
        let parse = cx.background_spawn(async move {
            let mut highlighter = SyntaxHighlighter::new("markdown");
            highlighter.update(None, &rope, None);
            highlighter
        });
        self.pending = Some(cx.spawn_in(
            window,
            async move |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
                let highlighter = parse.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    let editor = code_editor("markdown", &text, highlighter, window, cx);
                    if let Content::Markdown { source, .. } = &mut this.content {
                        *source = Some(editor);
                    }
                    this.pending = None;
                    cx.notify();
                });
            },
        ));
    }

    fn open_external(&self, cx: &mut Context<Self>) {
        if let Some(path) = &self.abs_path {
            cx.open_with_system(std::path::Path::new(path));
        }
    }

    fn reveal(&self, cx: &mut Context<Self>) {
        if let Some(path) = &self.abs_path {
            cx.reveal_path(std::path::Path::new(path));
        }
    }
}

fn code_editor(
    language: &str,
    text: &str,
    highlighter: SyntaxHighlighter,
    window: &mut Window,
    cx: &mut Context<FileViewer>,
) -> Entity<EditorState> {
    let language = language.to_owned();
    let text = text.to_owned();
    // Hand the editor the tree parsed in the background. If the editor ever
    // drops its highlighter and asks again, the new one parses on first use.
    let ready = Rc::new(RefCell::new(Some(highlighter)));
    let factory: InputHighlighterFactory = Rc::new(move |language| {
        let inner = ready
            .borrow_mut()
            .take()
            .unwrap_or_else(|| SyntaxHighlighter::new(language));
        Some(Box::new(Parsed(inner)) as Box<dyn InputHighlighter>)
    });
    cx.new(|cx| {
        let mut state = EditorState::new(window, cx)
            .language(language)
            .line_number(true)
            .soft_wrap(false);
        state.set_highlighter_factory(factory, cx);
        state.set_value(text, window, cx);
        state
    })
}

/// A tree-sitter highlighter that's already parsed its text. The stock one
/// parses for at most 2ms on the main thread, then shows the file plain and
/// finishes in the background after a 150ms debounce.
struct Parsed(SyntaxHighlighter);

impl InputHighlighter for Parsed {
    fn language(&self) -> SharedString {
        self.0.language().clone()
    }

    fn update(
        &mut self,
        _edit: Option<InputEdit>,
        text: &Rope,
        _folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        // The viewer is read-only, so the text only changes wholesale. When it
        // matches what was parsed this returns at once.
        self.0.update(None, text, None);
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        self.0.styles(range, resolver)
    }

    fn fold_ranges(&self, _text: &Rope) -> Vec<FoldRange> {
        let Some(tree) = self.0.tree() else {
            return Vec::new();
        };
        // Named nodes spanning three or more lines fold; walk depth-first,
        // skipping the insides of nodes too short to fold.
        let mut ranges: Vec<FoldRange> = Vec::new();
        let mut cursor = tree.walk();
        if !cursor.goto_first_child() {
            return ranges;
        }
        'walk: loop {
            let node = cursor.node();
            let (start, end) = (node.start_position().row, node.end_position().row);
            let foldable = node.is_named() && end.saturating_sub(start) >= 2;
            if foldable {
                ranges.push(FoldRange::new(start, end));
                if cursor.goto_first_child() {
                    continue;
                }
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() || cursor.depth() == 0 {
                    break 'walk;
                }
            }
        }
        ranges.sort_by_key(|r| r.start_line);
        ranges.dedup_by_key(|r| r.start_line);
        ranges
    }
}

pub fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn image_format(ext: &str) -> Option<ImageFormat> {
    Some(match ext {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        "svg" => ImageFormat::Svg,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        _ => return None,
    })
}

/// Map a file name to a gpui-kit highlighter language name.
pub fn language_for(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    match name.as_str() {
        "makefile" | "gnumakefile" => return "make",
        "cmakelists.txt" => return "cmake",
        "dockerfile" | ".bashrc" | ".zshrc" | ".profile" | ".env" => return "bash",
        "cargo.lock" => return "toml",
        _ => {}
    }
    match extension(&name).as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "json" | "jsonc" | "json5" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "py" | "pyi" => "python",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "css" | "scss" => "css",
        "html" | "htm" => "html",
        "sh" | "bash" | "zsh" | "fish" => "bash",
        "sql" => "sql",
        "lua" => "lua",
        "php" => "php",
        "zig" => "zig",
        "ex" | "exs" => "elixir",
        "scala" => "scala",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "svelte" => "svelte",
        "astro" => "astro",
        "erb" => "erb",
        "ejs" => "ejs",
        "diff" | "patch" => "diff",
        "md" | "markdown" | "mdx" => "markdown",
        "cmake" => "cmake",
        "mk" => "make",
        _ => "text",
    }
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn header_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(id)
        .gap_1()
        .px_2()
        .py(px(3.))
        .rounded(px(6.))
        .text_xs()
        .text_color(p.muted_foreground)
        .cursor_pointer()
        .hover(|d| d.bg(p.hover).text_color(p.foreground))
        .child(Icon::new(icon).xsmall())
        .child(label)
}

/// An icon-only header action, named in its tooltip.
fn header_icon(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex_none()
        .p(px(5.))
        .rounded(px(6.))
        .text_color(p.muted_foreground)
        .cursor_pointer()
        .hover(|d| d.bg(p.hover).text_color(p.foreground))
        .child(Icon::new(icon).small())
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx)
        })
}

impl Render for FileViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let is_markdown = matches!(self.content, Content::Markdown { .. });
        let (dir, file) = match self.rel_path.rsplit_once('/') {
            Some((d, f)) => (format!("{d}/"), f.to_owned()),
            None => (String::new(), self.rel_path.clone()),
        };
        let name: SharedString = file.into();
        let header = h_flex()
            .h(px(36.))
            .flex_shrink_0()
            .px_3()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .child(
                        div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .text_color(p.faint_foreground)
                            .child(dir),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(name),
                    ),
            )
            .when(self.size > 0, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(human_size(self.size)),
                )
            })
            .when(is_markdown, |d| {
                d.child(
                    header_icon(
                        "md-toggle",
                        if self.show_source {
                            IconName::Eye
                        } else {
                            IconName::Code
                        },
                        if self.show_source {
                            "Show preview"
                        } else {
                            "Show source"
                        },
                        &p,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_source = !this.show_source;
                        if this.show_source {
                            this.build_source(window, cx);
                        }
                        cx.notify();
                    })),
                )
            })
            .child(
                header_icon("reload", IconName::RefreshCw, "Reload", &p)
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
            .child(
                header_icon("reveal", IconName::FolderOpen, "Reveal in Finder", &p)
                    .on_click(cx.listener(|this, _, _, cx| this.reveal(cx))),
            )
            .child(
                header_icon(
                    "external",
                    IconName::ExternalLink,
                    "Open with default app",
                    &p,
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_external(cx))),
            );

        let body = match &self.content {
            Content::Loading => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Spinner::new().color(p.muted_foreground))
                .into_any_element(),
            Content::Code(editor) => div()
                .flex_1()
                .min_h_0()
                .child(Editor::new(editor).readonly(true).bordered(false).h_full())
                .into_any_element(),
            Content::Markdown {
                preview, source, ..
            } => match (self.show_source, source) {
                (true, Some(source)) => div()
                    .flex_1()
                    .min_h_0()
                    .child(Editor::new(source).readonly(true).bordered(false).h_full())
                    .into_any_element(),
                (true, None) => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(Spinner::new().color(p.muted_foreground))
                    .into_any_element(),
                // Pinned to the panel's size so a long document scrolls
                // inside it instead of stretching the window's layout.
                (false, _) => div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        div().absolute().inset_0().px_6().py_4().child(
                            TextView::new(preview)
                                .selectable(true)
                                .scrollable(true)
                                .size_full(),
                        ),
                    )
                    .into_any_element(),
            },
            Content::Image(image) => div()
                .id("viewer-image")
                .context_menu(crate::image_preview::menu(match &self.abs_path {
                    Some(path) => crate::image_preview::Pic::file(path.into()),
                    None => crate::image_preview::Pic::Loaded {
                        image: image.clone(),
                        name: self
                            .rel_path
                            .rsplit('/')
                            .next()
                            .unwrap_or("image")
                            .to_owned(),
                    },
                }))
                .flex_1()
                .min_h_0()
                .p_4()
                .flex()
                .items_center()
                .justify_center()
                .bg(p.muted)
                .child(
                    img(image.clone())
                        .max_w_full()
                        .max_h_full()
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            Content::Doc(doc) => div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(div().absolute().inset_0().child(doc.clone()))
                .into_any_element(),
            Content::External(reason) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(Icon::new(IconName::File).large())
                .child(reason.clone())
                .child(
                    header_button(
                        "external-big",
                        IconName::ExternalLink,
                        "Open with default app",
                        &p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.open_external(cx))),
                )
                .into_any_element(),
            Content::Error(err) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .px_6()
                .text_sm()
                .text_color(p.danger)
                .child(Icon::new(IconName::CircleAlert).large())
                .child(err.clone())
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(header)
            .when(self.truncated, |d| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .bg(p.warning_soft)
                        .text_color(p.warning)
                        .child("Large file — showing the first 10 MB."),
                )
            })
            .child(body)
            .font_weight(FontWeight::NORMAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_by_name_and_extension() {
        assert_eq!(language_for("src/main.rs"), "rust");
        assert_eq!(language_for("web/App.TSX"), "tsx");
        assert_eq!(language_for("Makefile"), "make");
        assert_eq!(language_for("notes.unknown"), "text");
        assert_eq!(human_size(2048), "2.0 KB");
    }
}
