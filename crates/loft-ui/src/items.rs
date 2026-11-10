//! Timeline rows: user bubbles, assistant prose, collapsible reasoning,
//! grouped tool activity with first-class approval prompts, turn footers.

use crate::store::{AppStore, Row, ThreadState};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Entity, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px, relative,
};
use loft_client::model::ThreadModel;
use loft_protocol::{
    DecisionKind, Item, ItemBody, ItemStatus, NoticeLevel, RequestKind, RequestStatus,
    RuntimeRequest, ThreadStatus, ToolKind, TurnStatus,
};

const OUTPUT_PREVIEW_LINES: usize = 14;
/// Long messages fold to this height until "Show more".
const FOLDED_HEIGHT: f32 = 220.;

/// The file on this Mac a reply image points at: `/abs`, `~/…`,
/// `file://…`, or a path relative to the thread's folder. `None` for web
/// addresses and data URLs.
pub(crate) fn local_image_path(raw: &str, cwd: &str) -> Option<std::path::PathBuf> {
    if raw.contains("://") && !raw.starts_with("file://") || raw.starts_with("data:") {
        return None;
    }
    let path = raw.strip_prefix("file://").unwrap_or(raw);
    Some(match path.strip_prefix("~/") {
        Some(rest) => crate::host::user_home().join(rest),
        None => std::path::Path::new(cwd).join(path),
    })
}

/// Where Markdown images in a reply come from: files on this Mac load
/// from disk (see [`local_image_path`]); web addresses load as usual.
pub(crate) fn local_images(
    cwd: String,
) -> impl Fn(&gpui_kit::SharedUri) -> gpui_kit::ImageSource + Send + Sync + 'static {
    move |uri: &gpui_kit::SharedUri| match local_image_path(uri.as_ref(), &cwd) {
        // From this computer's disk, or through the daemon in a browser.
        Some(path) => crate::host::image_or_blank(&path, None),
        None => gpui_kit::ImageSource::from(uri.clone()),
    }
}

/// Width ÷ height of the image file at `path`, read from its header
/// (cached).
fn image_ratio(path: &std::path::Path) -> Option<f32> {
    thread_local! {
        static RATIOS: std::cell::RefCell<std::collections::HashMap<std::path::PathBuf, Option<f32>>> =
            Default::default();
    }
    RATIOS.with(|cache| {
        *cache
            .borrow_mut()
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                let size = imagesize::size(path).ok()?;
                (size.width > 0 && size.height > 0).then(|| size.width as f32 / size.height as f32)
            })
    })
}

/// An image that stands as a block of its own in a reply.
struct ReplyImage {
    src: String,
    alt: String,
}

const REPLY_IMAGE_NODE: &str = "reply-image";

/// The `src` and `alt` of a lone `<img …>` tag.
fn parse_img_tag(html: &str) -> Option<ReplyImage> {
    let tag = html.trim();
    let lower = tag.to_ascii_lowercase();
    if !lower.starts_with("<img") || !tag.ends_with('>') || tag[1..].contains('<') {
        return None;
    }
    let attr = |name: &str| -> Option<String> {
        let at = lower.find(&format!(" {name}="))? + name.len() + 2;
        let rest = &tag[at..];
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let end = rest[1..].find(quote)?;
        Some(rest[1..1 + end].to_string())
    };
    let src = attr("src").filter(|s| !s.is_empty())?;
    let alt = attr("alt").unwrap_or_default().replace("&quot;", "\"");
    Some(ReplyImage { src, alt })
}

/// A block that is just one image: an HTML block holding an `<img>`, or a
/// paragraph whose only content is one.
fn reply_image_block(node: &gpui_kit::component::text::markdown_ast::Node) -> Option<ReplyImage> {
    use gpui_kit::component::text::markdown_ast::Node;
    match node {
        Node::Html(html) => parse_img_tag(&html.value),
        Node::Paragraph(para) => {
            let mut found = None;
            for child in &para.children {
                match child {
                    Node::Html(html) if found.is_none() => {
                        found = Some(parse_img_tag(&html.value)?)
                    }
                    Node::Break(_) => {}
                    Node::Text(t) if t.value.trim().is_empty() => {}
                    _ => return None,
                }
            }
            found
        }
        _ => None,
    }
}

/// A reply's Markdown view: local images load from disk, and an image on
/// its own is drawn by us so it can be clicked for a large preview and
/// right-clicked to copy or save.
pub(crate) fn reply_text_view(view: TextView, cwd: String) -> TextView {
    use gpui_kit::component::menu::ContextMenuExt as _;
    use gpui_kit::component::text::MarkdownNode;
    let render_cwd = cwd.clone();
    view.image_source(local_images(cwd))
        .markdown_block_parser(|node, _| {
            let image = reply_image_block(node)?;
            let markdown = format!("![{}]({})", image.alt, image.src);
            Some(
                MarkdownNode::new(REPLY_IMAGE_NODE, image)
                    .text(markdown.clone())
                    .markdown(markdown),
            )
        })
        .markdown_block_renderer(REPLY_IMAGE_NODE, move |node, _, cx| {
            let Some(image) = node.data::<ReplyImage>() else {
                return div().into_any_element();
            };
            let p = Palette::of(cx);
            let id = SharedString::from(format!(
                "reply-image-{}",
                node.source_range().map(|r| r.start).unwrap_or_default()
            ));
            let frame = div()
                .id(id)
                .w(px(REPLY_IMAGE_WIDTH as f32))
                .max_w_full()
                .rounded(px(10.))
                .overflow_hidden()
                .border_1()
                .border_color(p.border)
                .cursor_pointer()
                .child({
                    use gpui_kit::StyledImage as _;
                    // An explicit size in its own proportions: the image
                    // element otherwise takes the picture's pixel height and
                    // leaves it floating in a tall box.
                    let ratio = local_image_path(&image.src, &render_cwd)
                        .and_then(|path| image_ratio(&path))
                        .unwrap_or(1.);
                    let w = REPLY_IMAGE_WIDTH as f32;
                    gpui_kit::img(local_images(render_cwd.clone())(
                        &gpui_kit::SharedUri::from(image.src.clone()),
                    ))
                    .w(px(w))
                    .h(px(w / ratio))
                    .object_fit(gpui_kit::ObjectFit::Contain)
                });
            match local_image_path(&image.src, &render_cwd) {
                Some(path) => {
                    let pic = crate::image_preview::Pic::file(path);
                    let open = pic.clone();
                    frame
                        .on_click(move |_, window, cx| {
                            crate::image_preview::open(open.clone(), window, cx)
                        })
                        .context_menu(crate::image_preview::menu(pic))
                        .into_any_element()
                }
                None => {
                    let url = image.src.clone();
                    frame
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .into_any_element()
                }
            }
        })
}

/// Width images in replies are shown at (the Markdown view draws images
/// without a size at text height).
const REPLY_IMAGE_WIDTH: u32 = 360;

/// Give each Markdown image in a reply a line of its own and a width, so
/// it shows as a picture rather than a text-sized glyph. Code is left alone.
pub(crate) fn sized_images(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("![") {
        return text.into();
    }
    let mut out = String::with_capacity(text.len() + 64);
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence || !line.contains("![") {
            out.push_str(line);
            continue;
        }
        // Outside lists and quotes an image becomes a block of its own,
        // which the reply view draws as a clickable picture. Inside them it
        // stays in the item's text, on a line of its own.
        let block = !nested_line(line);
        let mut rest = line;
        let mut in_code = false;
        while !rest.is_empty() {
            if rest.starts_with('`') {
                in_code = !in_code;
                out.push('`');
                rest = &rest[1..];
                continue;
            }
            if !in_code
                && rest.starts_with("![")
                && let Some(close) = rest.find("](")
                && let Some(end) = rest[close + 2..].find(')')
            {
                let alt = &rest[2..close];
                let url = rest[close + 2..close + 2 + end]
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if !alt.contains(']') && !url.is_empty() {
                    let len = out.trim_end_matches([' ', '\t']).len();
                    out.truncate(len);
                    if block {
                        // A blank line before and after: an HTML block
                        // can't interrupt a paragraph, and runs on until
                        // a blank line.
                        if !out.is_empty() && !out.ends_with("\n\n") {
                            out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
                        }
                    } else if !out.is_empty() && !out.ends_with('\n') {
                        out.push_str("  \n");
                    }
                    let alt = alt.replace('"', "&quot;");
                    let url = url.replace('"', "%22");
                    out.push_str(&format!(
                        "<img src=\"{url}\" alt=\"{alt}\" width=\"{REPLY_IMAGE_WIDTH}\">"
                    ));
                    rest = &rest[close + 2 + end + 1..];
                    if block {
                        out.push_str("\n\n");
                        rest = rest.trim_start_matches([' ', '\t']);
                    } else if !rest.trim().is_empty() {
                        out.push_str("  \n");
                        rest = rest.trim_start_matches([' ', '\t']);
                    }
                    continue;
                }
            }
            let ch = rest.chars().next().expect("non-empty");
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out.into()
}

/// A line inside a list item or a quote (indented, or starting with a
/// marker), where a blank line would break the container.
fn nested_line(line: &str) -> bool {
    if line.starts_with([' ', '\t']) {
        return true;
    }
    let t = line.trim_end();
    if t.starts_with('>') || ["- ", "* ", "+ "].iter().any(|m| t.starts_with(m)) {
        return true;
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") "))
}

/// Long enough to fold: many lines, or a lot of text.
pub(crate) fn is_long_message(text: &str) -> bool {
    text.lines().count() > 10 || text.len() > 900
}

/// Text that renders as itself in the Markdown view: punctuation escaped,
/// line breaks kept.
pub(crate) fn plain_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut prev_blank = true;
    for (i, line) in text.split('\n').enumerate() {
        let blank = line.trim().is_empty();
        if i > 0 {
            // Around a blank line: a paragraph break; else a hard break.
            out.push_str(if blank || prev_blank { "\n" } else { "  \n" });
        }
        prev_blank = blank;
        let indent = line.len() - line.trim_start().len();
        // Leading spaces would turn into a code block; keep them visible.
        for _ in 0..indent {
            out.push('\u{a0}');
        }
        for c in line.trim_start().chars() {
            if c.is_ascii_punctuation() {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

pub fn render_row(
    thread: &Entity<ThreadState>,
    store: &Entity<AppStore>,
    p: &Palette,
    ix: usize,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let state = thread.read(cx);
    let (Some(model), Some(row)) = (state.model.as_ref(), state.rows.get(ix)) else {
        return div().into_any_element();
    };
    let content = match row {
        // Your messages say where they're drawn (the turn map lights the
        // ticks of those in view).
        Row::Item(i) if matches!(model.items[*i].body, ItemBody::UserMessage { .. }) => {
            let drawn = state.drawn_messages.clone();
            div()
                .relative()
                .child(render_item(thread, state, &model.items[*i], p))
                // Said while the agent was working: marked as a steer, in
                // the space a message keeps above itself.
                .when(crate::store::is_steer(&model.items, *i), |d| {
                    d.child(
                        h_flex()
                            .absolute()
                            .top_0()
                            .right(px(4.))
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .text_color(p.faint_foreground)
                            .child(Icon::new(IconName::CornerDownRight).xsmall())
                            .child("Steer"),
                    )
                })
                .child(
                    gpui_kit::canvas(
                        move |bounds, _, _| {
                            drawn.borrow_mut().insert(ix, bounds);
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .into_any_element()
        }
        Row::Item(i) => render_item(thread, state, &model.items[*i], p),
        Row::ToolGroup { members } => {
            let items: Vec<&Item> = members.iter().map(|i| &model.items[*i]).collect();
            render_tool_group(thread, store, state, model, &items, p, cx)
        }
        Row::Work {
            turn_ix,
            open,
            steps,
        } => render_work_header(thread, model, *turn_ix, *open, *steps, p),
        Row::Pending => render_pending(p),
        Row::Waiting => render_waiting(&model.thread.waiting_on, p),
        Row::Card(ix) => crate::info_cards::render(thread, store, state, *ix, p),
        Row::TurnFooter { turn_ix } => {
            let agents = store.read(cx).available_harnesses();
            render_turn_footer(store, model, *turn_ix, agents, p)
        }
    };
    let footer = matches!(row, Row::TurnFooter { .. });
    // Under a reply, absorb the Markdown view's trailing paragraph gap.
    let after_prose = footer
        && ix > 0
        && matches!(state.rows.get(ix - 1), Some(Row::Item(i))
            if matches!(model.items[*i].body, ItemBody::AssistantMessage { .. }));
    div()
        .w_full()
        .flex()
        .justify_center()
        .child(
            div()
                .w_full()
                .max_w(p.content_width)
                .px_6()
                // The footer sits right under the reply it belongs to (the
                // Markdown view already ends with paragraph spacing).
                .when(footer, |d| d.pt_0().pb(p.row_gap))
                .when(after_prose, |d| d.mt(px(-2.)))
                .when(!footer, |d| d.py(p.row_gap / 2.))
                .child(content),
        )
        .into_any_element()
}

pub(crate) fn render_item(
    thread: &Entity<ThreadState>,
    state: &ThreadState,
    item: &Item,
    p: &Palette,
) -> AnyElement {
    match &item.body {
        ItemBody::UserMessage { text, attachments } => {
            use gpui_kit::StyledImage as _;
            use gpui_kit::component::menu::ContextMenuExt as _;
            let mut images = h_flex().gap_2().justify_end().flex_wrap();
            for (ix, a) in attachments.iter().enumerate() {
                let path = std::path::PathBuf::from(&a.path);
                images = images.child(
                    div()
                        .id(SharedString::from(format!("att-{}-{ix}", item.id)))
                        .size(px(120.))
                        .rounded(px(12.))
                        .overflow_hidden()
                        .border_1()
                        .border_color(p.border)
                        .cursor_pointer()
                        .child(
                            gpui_kit::img(crate::host::image_or_blank(&path, None))
                                .size_full()
                                .object_fit(gpui_kit::ObjectFit::Cover),
                        )
                        .on_click({
                            let pic = crate::image_preview::Pic::File {
                                path: path.clone(),
                                name: a.name.clone(),
                            };
                            move |_, window, cx| crate::image_preview::open(pic.clone(), window, cx)
                        })
                        .context_menu(crate::image_preview::menu(
                            crate::image_preview::Pic::File {
                                path: path.clone(),
                                name: a.name.clone(),
                            },
                        )),
                );
            }
            v_flex()
                .items_end()
                .gap_1p5()
                .pt_5()
                .when(!attachments.is_empty(), |d| d.child(images))
                .when(!text.is_empty(), |d| {
                    let group = SharedString::from(format!("user-{}", item.id));
                    let copy_id = format!("copy-user-{}", item.id);
                    let pinned = just_copied(&copy_id);
                    let long = is_long_message(text);
                    let fold_key = loft_protocol::ItemId(format!("fold:{}", item.id.0));
                    let open = state.is_expanded(&fold_key, false);
                    let folded = long && !open;
                    // Selectable: the text goes through the Markdown view,
                    // escaped so it shows exactly as typed.
                    let body = TextView::markdown(
                        SharedString::from(format!("user-md-{}", item.id)),
                        plain_markdown(text),
                    )
                    .selectable(true);
                    let toggle = long.then(|| {
                        let (thread, key) = (thread.clone(), fold_key.clone());
                        div()
                            .id(SharedString::from(format!("fold-{}", item.id)))
                            .px_1p5()
                            .py(px(2.))
                            .rounded(px(5.))
                            .text_xs()
                            .cursor_pointer()
                            .text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                            .child(if open { "Show less" } else { "Show more" })
                            .on_click(move |_, _, cx| {
                                let key = key.clone();
                                thread.update(cx, |t, cx| t.toggle(key, false, cx));
                            })
                    });
                    d.child(
                        v_flex()
                            .group(group.clone())
                            .w_full()
                            .items_end()
                            .gap_0p5()
                            .child(
                                div()
                                    .relative()
                                    .max_w(relative(0.82))
                                    .px_4()
                                    .py(px(9.))
                                    .rounded(px(18.))
                                    .bg(p.bubble)
                                    .text_color(p.foreground)
                                    .when(folded, |d| d.max_h(px(FOLDED_HEIGHT)).overflow_hidden())
                                    .child(body)
                                    // Fade the cut-off edge into the bubble.
                                    .when(folded, |d| {
                                        d.child(
                                            div()
                                                .absolute()
                                                .left_0()
                                                .right_0()
                                                .bottom_0()
                                                .h(px(48.))
                                                .bg(gpui_kit::linear_gradient(
                                                    180.,
                                                    gpui_kit::linear_color_stop(
                                                        p.bubble.opacity(0.),
                                                        0.,
                                                    ),
                                                    gpui_kit::linear_color_stop(p.bubble, 1.),
                                                )),
                                        )
                                    }),
                            )
                            // Under the bubble: show more, and copy on hover
                            // (or while it shows ✓).
                            .child(
                                h_flex().gap_1().items_center().children(toggle).child(
                                    div()
                                        .when(!pinned, |d| d.invisible())
                                        .group_hover(group, |d| d.visible())
                                        .child(copy_button(
                                            copy_id,
                                            text.clone(),
                                            "Copy message",
                                            p,
                                        )),
                                ),
                            ),
                    )
                })
                .into_any_element()
        }

        ItemBody::AssistantMessage { text } => {
            let cwd = state
                .model
                .as_ref()
                .map(|m| m.thread.cwd.clone())
                .unwrap_or_default();
            match state.markdown.get(&item.id) {
                Some(md) => {
                    reply_text_view(TextView::new(md).stream_fade(true), cwd).into_any_element()
                }
                None => reply_text_view(
                    TextView::markdown(
                        SharedString::from(format!("md-{}", item.id)),
                        sized_images(text).into_owned(),
                    ),
                    cwd,
                )
                .into_any_element(),
            }
        }

        ItemBody::Reasoning { text } => {
            let thinking = item.status == ItemStatus::InProgress;
            let open = state.is_expanded(&item.id, false);
            let key = item.id.clone();
            let thread = thread.clone();
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .id(SharedString::from(format!("reason-{}", item.id)))
                        .gap_1p5()
                        .items_center()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .cursor_pointer()
                        .hover(|d| d.text_color(p.foreground))
                        .child(Icon::new(IconName::Brain).small())
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(if thinking && text.is_empty() {
                                    "Thinking…".to_string()
                                } else {
                                    reasoning_summary(text).unwrap_or_else(|| "Thought".into())
                                }),
                        )
                        .when(!text.is_empty(), |d| d.child(chevron(open)))
                        .on_click(move |_, _, cx| {
                            let key = key.clone();
                            thread.update(cx, |t, cx| t.toggle(key, false, cx));
                        }),
                )
                .when(open && !text.is_empty(), |d| {
                    d.child(
                        div()
                            .ml(px(7.))
                            .pl_3()
                            .border_l_1()
                            .border_color(p.border)
                            .text_sm()
                            .text_color(p.muted_foreground)
                            .child(text.clone()),
                    )
                })
                .into_any_element()
        }

        ItemBody::Notice { level, text } => h_flex()
            .justify_center()
            .text_xs()
            .text_color(match level {
                NoticeLevel::Info => p.faint_foreground,
                NoticeLevel::Warning => p.warning,
            })
            .child(text.clone())
            .into_any_element(),

        ItemBody::Error { text } => h_flex()
            .gap_2()
            .items_start()
            .px_3()
            .py_2()
            .rounded(px(10.))
            .border_1()
            .border_color(p.danger.opacity(0.35))
            .bg(p.danger_soft)
            .text_sm()
            .text_color(p.foreground)
            .child(
                div()
                    .pt(px(2.))
                    .text_color(p.danger)
                    .child(Icon::new(IconName::CircleAlert).small()),
            )
            .child(div().flex_1().child(text.clone()))
            .into_any_element(),

        // Tool calls are rendered as groups (`Row::ToolGroup`).
        ItemBody::ToolCall { .. } => div().into_any_element(),
    }
}

// -- turn work ---------------------------------------------------------------

/// "1m 58s", "12s", "1h 4m".
pub(crate) fn human_duration(ms: i64) -> String {
    let secs = (ms.max(0) / 1000) as u64;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

fn now_ms() -> i64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The header over a turn's work: live while it runs, a fold once done.
fn render_work_header(
    thread: &Entity<ThreadState>,
    model: &ThreadModel,
    turn_ix: usize,
    open: bool,
    steps: usize,
    p: &Palette,
) -> AnyElement {
    let Some(turn) = model.turns.get(turn_ix) else {
        return div().into_any_element();
    };
    let running = !turn.status.is_terminal();
    let elapsed = turn.ended_at_ms.unwrap_or_else(now_ms) - turn.started_at_ms;
    let label = if running {
        format!("Working for {}", human_duration(elapsed))
    } else {
        format!("Worked for {}", human_duration(elapsed))
    };
    let turn_id = turn.id.clone();
    let thread = thread.clone();
    // The words (and the chevron) are the whole of it: what you click.
    h_flex()
        .pt_1()
        .text_sm()
        .text_color(p.muted_foreground)
        .child(
            h_flex()
                .id(SharedString::from(format!("work-{}", turn.id.0)))
                .flex_none()
                .gap_1()
                .items_center()
                .child(label)
                .when(!running && steps > 0, |d| {
                    d.child(chevron(open))
                        .cursor_pointer()
                        .hover(|d| d.text_color(p.foreground))
                        .on_click(move |_, _, cx| {
                            thread.update(cx, |t, cx| t.toggle_work(&turn_id, cx));
                        })
                }),
        )
        .into_any_element()
}

/// The agent hasn't said anything yet (or is between steps).
fn render_pending(p: &Palette) -> AnyElement {
    use gpui_kit::{Animation, AnimationExt as _, pulsating_between};
    h_flex()
        .gap_1p5()
        .items_center()
        .text_sm()
        .text_color(p.muted_foreground)
        .child(Icon::new(IconName::Brain).small())
        .child("Thinking…")
        .with_animation(
            "pending-pulse",
            Animation::new(std::time::Duration::from_millis(1400))
                .repeat()
                .with_easing(pulsating_between(0.45, 1.0)),
            |d, delta| d.opacity(delta),
        )
        .into_any_element()
}

/// The turn is over, but work the agent started in the background isn't:
/// say what it's waiting on.
fn render_waiting(tasks: &[String], p: &Palette) -> AnyElement {
    use gpui_kit::{Animation, AnimationExt as _, pulsating_between};
    let what = match tasks {
        [one] => one.clone(),
        many => format!("{} background tasks", many.len()),
    };
    h_flex()
        .gap_1p5()
        .items_center()
        .text_sm()
        .text_color(p.muted_foreground)
        .child(Icon::new(IconName::Hourglass).small())
        .child(div().flex_none().child("Waiting on"))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(p.foreground)
                .child(what),
        )
        .with_animation(
            "waiting-pulse",
            Animation::new(std::time::Duration::from_millis(1800))
                .repeat()
                .with_easing(pulsating_between(0.5, 1.0)),
            |d, delta| d.opacity(delta),
        )
        .into_any_element()
}

/// A thought's first line, as its label ("Checking the parser").
fn reasoning_summary(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.trim_matches(|c: char| c == '*' || c == '#' || c == '_' || c.is_whitespace());
    if line.is_empty() {
        return None;
    }
    let mut out: String = line.chars().take(90).collect();
    if line.chars().count() > 90 {
        out.push('…');
    }
    Some(out)
}

// -- tool activity -----------------------------------------------------------

fn render_tool_group(
    thread: &Entity<ThreadState>,
    store: &Entity<AppStore>,
    state: &ThreadState,
    model: &ThreadModel,
    items: &[&Item],
    p: &Palette,
    cx: &App,
) -> AnyElement {
    let cwd = model.thread.cwd.as_str();
    let open_requests: Vec<&RuntimeRequest> = items
        .iter()
        .filter_map(|i| model.request_for_item(&i.id))
        .filter(|r| r.status == RequestStatus::Open)
        .collect();
    let running = items.iter().any(|i| i.status == ItemStatus::InProgress);
    // The group's own open state: not the first tool's (they used to share
    // a key, so opening one opened the other).
    let key = loft_protocol::ItemId(format!("group:{}", items[0].id.0));

    let mut col = v_flex().gap_1();
    if items.len() > 1 && items.iter().all(|i| crate::store::is_agent(i)) {
        col = col.child(agents_group(thread, store, state, model, items, p, cx));
    } else if items.len() == 1 {
        col = col.child(tool_line(thread, store, state, model, items[0], p, cx));
    } else {
        // Closed until opened: opening while a command runs and closing
        // when it ends made the list jump with every command.
        let open = state.is_expanded(&key, false);
        let thread_h = thread.clone();
        let toggle_key = key.clone();
        col = col.child(
            h_flex()
                .id(SharedString::from(format!("group-{key}")))
                .gap_1p5()
                .items_center()
                .text_sm()
                .text_color(p.muted_foreground)
                .cursor_pointer()
                .hover(|d| d.text_color(p.foreground))
                .child(status_icon(running, false, false, p))
                .child(summarize(items))
                .child(chevron(open))
                .on_click(move |_, _, cx| {
                    let key = toggle_key.clone();
                    thread_h.update(cx, |t, cx| t.toggle(key, false, cx));
                }),
        );
        if open {
            let mut list = v_flex()
                .gap_1()
                .pl(px(7.))
                .ml(px(7.))
                .border_l_1()
                .border_color(p.border);
            for item in items {
                list = list.child(tool_line(thread, store, state, model, item, p, cx));
            }
            col = col.child(list);
        }
    }
    for request in open_requests {
        col = col.child(approval_card(request, store, p, cwd));
    }
    col.into_any_element()
}

/// Several agents started together (subagents and/or threads): their
/// logos stacked and a count, then their cards, open while any runs.
fn agents_group(
    thread: &Entity<ThreadState>,
    store: &Entity<AppStore>,
    state: &ThreadState,
    model: &ThreadModel,
    items: &[&Item],
    p: &Palette,
    cx: &App,
) -> AnyElement {
    let live = store_live(store, model, cx);
    // (agent harness, running, failed) per card.
    let looks: Vec<(String, bool, bool)> = items
        .iter()
        .map(|item| {
            let running = crate::store::agent_running(model, item, &live);
            let spawned = match &item.body {
                ItemBody::ToolCall { tool, output, .. }
                    if tool.ends_with("agentloft__spawn_thread") =>
                {
                    output
                        .as_deref()
                        .and_then(crate::store::spawned_thread_id)
                        .and_then(|id| {
                            store
                                .read(cx)
                                .workspace
                                .threads
                                .iter()
                                .find(|t| t.id == id)
                                .cloned()
                        })
                }
                _ => None,
            };
            let harness = spawned
                .as_ref()
                .map(|t| t.harness.as_str().to_owned())
                .unwrap_or_else(|| model.thread.harness.as_str().to_owned());
            let failed = match &spawned {
                Some(t) => t.status == ThreadStatus::Error,
                None => {
                    matches!(item.status, ItemStatus::Failed)
                        || matches!(&item.body, ItemBody::ToolCall { is_error: true, .. })
                }
            };
            (harness, running, failed)
        })
        .collect();
    let running = looks.iter().filter(|l| l.1).count();
    let failed = looks.iter().filter(|l| !l.1 && l.2).count();
    let done = looks.len() - running - failed;
    let all_threads = items.iter().all(|i| {
        matches!(&i.body, ItemBody::ToolCall { tool, .. } if tool.ends_with("agentloft__spawn_thread"))
    });
    let noun = if all_threads { "threads" } else { "agents" };
    let summary = [(running, "running"), (done, "done"), (failed, "failed")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, w)| format!("{n} {w}"))
        .collect::<Vec<_>>()
        .join(" · ");
    let key = loft_protocol::ItemId(format!("agents:{}", items[0].id.0));
    let open = state.is_expanded(&key, running > 0);
    // Logos overlapping, like a stack of cards.
    let mut stack = h_flex().flex_none().items_center();
    for (ix, (harness, _, _)) in looks.iter().take(3).enumerate() {
        stack = stack.child(
            div()
                .size(px(26.))
                .when(ix > 0, |d| d.ml(px(-9.)))
                .rounded_full()
                .bg(p.muted)
                .border_2()
                .border_color(p.background)
                .flex()
                .items_center()
                .justify_center()
                .child(crate::brand::agent_logo(harness, px(14.), p)),
        );
    }
    let thread_h = thread.clone();
    let toggle_key = key.clone();
    let default_open = running > 0;
    let header = h_flex()
        .id(SharedString::from(format!("agents-{}", items[0].id)))
        .gap_3()
        .items_center()
        .px_2()
        .py_1p5()
        .ml(px(-8.))
        .rounded(px(10.))
        .cursor_pointer()
        .hover(|d| d.bg(p.hover))
        .child(stack)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.foreground)
                        .child(format!("{} {noun}", items.len())),
                )
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .when(running > 0, |d| {
                            d.child(div().size(px(6.)).rounded_full().bg(p.accent))
                        })
                        .child(summary),
                ),
        )
        .child(chevron(open))
        .on_click(move |_, _, cx| {
            let key = toggle_key.clone();
            thread_h.update(cx, |t, cx| t.toggle(key, default_open, cx));
        });
    let mut card = v_flex().gap_1().child(header);
    if open {
        let mut list = v_flex()
            .px_3()
            .py_1()
            .rounded(px(12.))
            .border_1()
            .border_color(p.border)
            .bg(p.muted.opacity(0.35));
        for item in items {
            list = list.child(
                div()
                    .ml(px(8.))
                    .child(tool_line(thread, store, state, model, item, p, cx)),
            );
        }
        card = card.child(list);
    }
    card.into_any_element()
}

/// The threads this one's agent started that are running now.
fn store_live(
    store: &Entity<AppStore>,
    model: &ThreadModel,
    cx: &App,
) -> std::collections::HashSet<loft_protocol::ThreadId> {
    store
        .read(cx)
        .workspace
        .threads
        .iter()
        .filter(|t| {
            t.parent.as_ref().is_some_and(|p| {
                p.thread_id == model.thread.id && p.kind == loft_protocol::ForkKind::Spawned
            }) && (matches!(
                t.status,
                ThreadStatus::Running | ThreadStatus::WaitingApproval
            ) || t.is_waiting())
        })
        .map(|t| t.id.clone())
        .collect()
}

/// One tool call. Subagents render as agent cards; file tools open the file.
pub(crate) fn tool_line(
    thread: &Entity<ThreadState>,
    store: &Entity<AppStore>,
    state: &ThreadState,
    model: &ThreadModel,
    item: &Item,
    p: &Palette,
    cx: &App,
) -> AnyElement {
    let ItemBody::ToolCall {
        tool,
        tool_kind,
        title,
        input,
        output,
        is_error,
    } = &item.body
    else {
        return div().into_any_element();
    };
    if *tool_kind == ToolKind::Subagent {
        return agent_card(store, model, item, title, p);
    }
    // An agent this one started in its own thread (orchestration).
    if tool.ends_with("agentloft__spawn_thread") && !*is_error && item.status != ItemStatus::Failed
    {
        return spawned_card(store, model, item, input, output.as_deref(), p, cx);
    }
    let cwd = model.thread.cwd.as_str();
    let running = item.status == ItemStatus::InProgress;
    let failed = matches!(item.status, ItemStatus::Failed) || *is_error;
    let declined = item.status == ItemStatus::Declined;
    let output = output.as_ref().filter(|o| !o.trim().is_empty());
    // Read/Edit/Write open the file in the panel; others expand their output.
    let file = matches!(tool_kind, ToolKind::FileRead | ToolKind::FileChange)
        .then(|| {
            ["file_path", "notebook_path", "path"]
                .iter()
                .find_map(|k| input.get(*k).and_then(|v| v.as_str()))
        })
        .flatten()
        .map(|f| relative_to(f, cwd))
        .filter(|f| !f.starts_with('/'));
    let open = file.is_none() && output.is_some() && state.is_expanded(&item.id, false);
    let mut detail = relative_to(
        title.strip_prefix(&format!("{tool} ")).unwrap_or(title),
        cwd,
    );
    if detail == *tool {
        // MCP tools have no natural title: show their arguments instead.
        detail = match input {
            serde_json::Value::Object(map) if !map.is_empty() => map
                .iter()
                .map(|(k, v)| match v {
                    serde_json::Value::String(s) => format!("{k}: {s}"),
                    other => format!("{k}: {other}"),
                })
                .collect::<Vec<_>>()
                .join("  "),
            _ => String::new(),
        };
    }
    // Orchestration tools name the threads they touch, not their ids.
    let mut label = tool_label(tool);
    let mut plain = false;
    if let Some(op) = tool.strip_prefix("mcp__agentloft__") {
        let title_of = |id: &str| {
            store
                .read(cx)
                .workspace
                .threads
                .iter()
                .find(|t| t.id.0 == id)
                .map(|t| t.title.clone())
                .unwrap_or_else(|| id.chars().take(8).collect())
        };
        let one = input
            .get("thread_id")
            .and_then(|v| v.as_str())
            .map(title_of);
        let named = match op {
            "wait_for_threads" => Some((
                if item.status == ItemStatus::InProgress {
                    "Waiting for"
                } else {
                    "Waited for"
                },
                input
                    .get("thread_ids")
                    .and_then(|v| v.as_array())
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|v| v.as_str())
                            .map(|id| format!("“{}”", title_of(id)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default(),
            )),
            "send_to_thread" => Some((
                "Messaged",
                one.map(|t| format!("“{t}”")).unwrap_or_default(),
            )),
            "stop_thread" => Some(("Stopped", one.map(|t| format!("“{t}”")).unwrap_or_default())),
            "thread_status" => Some(("Checked", one.map(|t| format!("“{t}”")).unwrap_or_default())),
            "list_agents" => Some(("Listed agents", String::new())),
            _ => None,
        };
        if let Some((verb, what)) = named {
            label = verb.to_owned();
            detail = what;
            plain = true;
        }
    }
    let clickable = file.is_some() || output.is_some();

    let row = h_flex()
        .id(SharedString::from(format!("tool-{}", item.id)))
        .gap_1p5()
        .items_center()
        .text_sm()
        .min_w_0()
        .rounded(px(6.))
        .child(status_icon(running, failed, declined, p))
        .child(
            div()
                .flex_shrink_0()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.foreground)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(!plain, |d| {
                    d.font_family(p.mono.clone()).text_size(p.mono_size)
                })
                .text_color(if file.is_some() {
                    p.accent
                } else {
                    p.muted_foreground
                })
                .child(detail),
        )
        .when(declined, |d| {
            d.child(div().text_xs().text_color(p.warning).child("denied"))
        });
    let row = if clickable {
        let thread = thread.clone();
        let store = store.clone();
        let key = item.id.clone();
        let thread_id = model.thread.id.clone();
        row.cursor_pointer()
            .hover(|d| d.bg(p.hover))
            .on_click(move |_, _, cx| match &file {
                Some(path) => {
                    let (thread_id, path) = (thread_id.clone(), path.clone());
                    store.update(cx, |s, cx| s.open_file(thread_id, path, cx));
                }
                None => {
                    let key = key.clone();
                    thread.update(cx, |t, cx| t.toggle(key, false, cx));
                }
            })
            .into_any_element()
    } else {
        row.into_any_element()
    };

    v_flex()
        .gap_1()
        .child(row)
        .when_some(output.filter(|_| open), |d, out| {
            let lines: Vec<&str> = out.lines().collect();
            let shown = lines
                .iter()
                .take(OUTPUT_PREVIEW_LINES)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            let more = lines.len().saturating_sub(OUTPUT_PREVIEW_LINES);
            d.child(
                v_flex()
                    .ml(px(20.))
                    .px_3()
                    .py_2()
                    .rounded(px(8.))
                    .bg(p.muted)
                    .font_family(p.mono.clone())
                    .text_size(p.mono_size)
                    .text_color(if failed { p.danger } else { p.muted_foreground })
                    .child(shown)
                    .when(more > 0, |d| {
                        d.child(
                            div()
                                .pt_1()
                                .text_color(p.faint_foreground)
                                .child(format!("… {more} more lines")),
                        )
                    }),
            )
        })
        .into_any_element()
}

/// A subagent (Task/Agent tool call): description, live progress, open button.
/// An agent's mark in a circle with its status dot, which pulses while
/// it runs. The dot sits on top of the circle (drawn after it), never cut
/// by its border.
pub(crate) fn agent_avatar(
    id: &str,
    harness: &str,
    size: f32,
    dot: gpui_kit::Hsla,
    running: bool,
    p: &Palette,
) -> AnyElement {
    use gpui_kit::{Animation, AnimationExt as _, ease_in_out};
    let dot_size = (size * 0.34).round().max(7.);
    let ring = 2.;
    let circle = div()
        .absolute()
        .inset_0()
        .rounded_full()
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .flex()
        .items_center()
        .justify_center()
        .child(crate::brand::agent_logo(harness, px(size * 0.53), p));
    let badge = div()
        .absolute()
        .right(px(-ring))
        .bottom(px(-ring))
        .size(px(dot_size + ring * 2.))
        .rounded_full()
        .bg(p.background)
        .flex()
        .items_center()
        .justify_center()
        .child(div().size(px(dot_size)).rounded_full().bg(dot));
    let mut avatar = div().relative().flex_none().size(px(size)).child(circle);
    if running {
        // A soft halo that swells and fades behind the dot.
        let halo = div()
            .absolute()
            .right(px(-ring))
            .bottom(px(-ring))
            .size(px(dot_size + ring * 2.))
            .rounded_full()
            .bg(dot)
            .with_animation(
                SharedString::from(format!("{id}-halo")),
                Animation::new(std::time::Duration::from_millis(1400))
                    .repeat()
                    .with_easing(ease_in_out),
                move |d, t| {
                    let grow = dot_size * 0.9 * t;
                    d.right(px(-ring - grow / 2.))
                        .bottom(px(-ring - grow / 2.))
                        .size(px(dot_size + ring * 2. + grow))
                        .opacity(0.45 * (1. - t))
                },
            );
        avatar = avatar.child(halo);
    }
    avatar.child(badge).into_any_element()
}

/// How a subagent (its Agent tool call `item`) is doing.
pub(crate) struct SubagentStatus {
    pub running: bool,
    pub failed: bool,
    /// What it is doing now, else its answer.
    pub subtitle: String,
    pub elapsed: Option<String>,
}

pub(crate) fn subagent_status(model: &ThreadModel, item: &Item) -> SubagentStatus {
    let children: Vec<&Item> = model
        .items
        .iter()
        .filter(|i| i.parent_id.as_ref() == Some(&item.id))
        .collect();
    // Launched in the background: the call returns at once with a receipt;
    // the agent's own steps tell how it is going.
    let background = matches!(&item.body, ItemBody::ToolCall { output: Some(out), .. }
        if out.trim_start().starts_with("Async agent launched"));
    let stopped = item.status == ItemStatus::Declined;
    let running = !stopped
        && (item.status == ItemStatus::InProgress
            || (background && children.iter().any(|c| c.status == ItemStatus::InProgress)));
    let failed = matches!(item.status, ItemStatus::Failed | ItemStatus::Declined)
        || matches!(&item.body, ItemBody::ToolCall { is_error: true, .. });
    // Under the title: what it is doing now, else what it answered.
    let latest = children.iter().rev().find_map(|i| match &i.body {
        ItemBody::ToolCall { title, .. } => Some(title.clone()),
        _ => None,
    });
    let answer = if background {
        // Its last words, once it has stopped.
        children.iter().rev().find_map(|i| match &i.body {
            ItemBody::AssistantMessage { text } => first_line(text),
            _ => None,
        })
    } else {
        match &item.body {
            ItemBody::ToolCall {
                output: Some(out), ..
            } => first_line(&crate::panel::agent::clean_report(out)),
            _ => None,
        }
    };
    let subtitle = if running {
        latest.unwrap_or_else(|| "Working…".into())
    } else if stopped {
        "Stopped".into()
    } else {
        answer.unwrap_or_else(|| {
            if failed {
                "Failed".into()
            } else {
                "Done".into()
            }
        })
    };
    let elapsed = item.started_at_ms.map(|start| {
        let end = if running {
            now_ms()
        } else if background {
            children
                .iter()
                .map(|c| c.updated_at_ms)
                .max()
                .unwrap_or(item.updated_at_ms)
        } else {
            item.updated_at_ms
        };
        human_duration(end - start)
    });
    SubagentStatus {
        running,
        failed,
        subtitle,
        elapsed,
    }
}

fn agent_card(
    store: &Entity<AppStore>,
    model: &ThreadModel,
    item: &Item,
    title: &str,
    p: &Palette,
) -> AnyElement {
    let SubagentStatus {
        running,
        failed,
        subtitle,
        elapsed,
    } = subagent_status(model, item);
    let dot = if running {
        p.accent
    } else if failed {
        p.danger
    } else {
        p.success
    };
    let store = store.clone();
    let (thread_id, item_id) = (model.thread.id.clone(), item.id.clone());
    h_flex()
        .id(SharedString::from(format!("agent-{}", item.id)))
        .gap_3()
        .items_center()
        .px_2()
        .py_1p5()
        .ml(px(-8.))
        .rounded(px(10.))
        .cursor_pointer()
        .hover(|d| d.bg(p.hover))
        .child(agent_avatar(
            &format!("agent-dot-{}", item.id),
            model.thread.harness.as_str(),
            30.,
            dot,
            running,
            p,
        ))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.foreground)
                        .truncate()
                        .child(title.to_owned()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .truncate()
                        .child(subtitle),
                ),
        )
        .children(elapsed.map(|e| {
            div()
                .flex_none()
                .text_xs()
                .text_color(p.faint_foreground)
                .child(e)
        }))
        .child(
            Icon::new(IconName::ChevronRight)
                .xsmall()
                .text_color(p.faint_foreground),
        )
        .on_click(move |_, _, cx| {
            let (thread_id, item_id) = (thread_id.clone(), item_id.clone());
            store.update(cx, |s, cx| s.open_agent(thread_id, item_id, cx));
        })
        .into_any_element()
}

/// A thread another agent started (`spawn_thread`): its agent, title and
/// live status; click to open it (⌘-click: beside this one).
fn spawned_card(
    store: &Entity<AppStore>,
    model: &ThreadModel,
    item: &Item,
    input: &serde_json::Value,
    output: Option<&str>,
    p: &Palette,
    cx: &App,
) -> AnyElement {
    let child = output
        .and_then(crate::store::spawned_thread_id)
        .and_then(|id| {
            store
                .read(cx)
                .workspace
                .threads
                .iter()
                .find(|t| t.id == id)
                .cloned()
        });
    let asked = |k: &str| {
        input
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    let (harness, title, subtitle, dot, running) = match &child {
        Some(t) => {
            let (what, dot, running) = match t.status {
                _ if t.is_waiting() => ("Waiting on background work", p.accent, true),
                ThreadStatus::Running => ("Working", p.accent, true),
                ThreadStatus::WaitingApproval => ("Needs your approval", p.warning, false),
                ThreadStatus::Error => ("Failed", p.danger, false),
                _ => ("Done", p.success, false),
            };
            let mut subtitle = crate::workspace::harness_name(&t.harness).to_owned();
            if let Some(m) = &t.model {
                subtitle.push_str(&format!(
                    " · {}",
                    store.read(cx).model_display(&t.harness, m)
                ));
            }
            if let Some(e) = &t.effort {
                subtitle.push_str(&format!(" · {e}"));
            }
            (
                t.harness.as_str().to_owned(),
                t.title.clone(),
                format!("{what} · {subtitle}"),
                dot,
                running,
            )
        }
        None => (
            asked("agent")
                .unwrap_or(model.thread.harness.as_str())
                .to_owned(),
            asked("title")
                .map(str::to_owned)
                .or_else(|| asked("prompt").and_then(first_line))
                .unwrap_or_else(|| "New agent".into()),
            "Starting agent…".into(),
            p.accent,
            item.status == ItemStatus::InProgress,
        ),
    };
    let target = child.map(|t| t.id);
    let store = store.clone();
    h_flex()
        .id(SharedString::from(format!("spawned-{}", item.id)))
        .gap_3()
        .items_center()
        .px_2()
        .py_1p5()
        .ml(px(-8.))
        .rounded(px(10.))
        .when(target.is_some(), |d| {
            d.cursor_pointer().hover(|d| d.bg(p.hover))
        })
        .child(agent_avatar(
            &format!("spawned-dot-{}", item.id),
            &harness,
            30.,
            dot,
            running,
            p,
        ))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.foreground)
                        .truncate()
                        .child(title),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .truncate()
                        .child(subtitle),
                ),
        )
        .when(target.is_some(), |d| {
            d.child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child("Open thread")
                    .child(Icon::new(IconName::ChevronRight).xsmall()),
            )
        })
        .when_some(target, |d, id| {
            d.on_click(move |e: &gpui_kit::ClickEvent, _, cx| {
                let id = id.clone();
                if e.modifiers().platform {
                    store.update(cx, |s, cx| s.open_pane(id, cx));
                } else {
                    store.update(cx, |s, cx| s.select_thread(id, cx));
                }
            })
        })
        .into_any_element()
}

/// The first line of `text` with its Markdown marks dropped, if any.
fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let plain: String = line
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '#'))
        .collect();
    let plain = plain.trim();
    (!plain.is_empty()).then(|| plain.chars().take(160).collect())
}

pub(crate) fn approval_card(
    req: &RuntimeRequest,
    store: &Entity<AppStore>,
    p: &Palette,
    cwd: &str,
) -> AnyElement {
    let input = req.detail.get("input");
    let pick = |k: &str| input.and_then(|i| i.get(k)).and_then(|v| v.as_str());
    let (headline, body) = match req.kind {
        RequestKind::FileChangeApproval => (
            "Allow Claude to edit this file?",
            pick("file_path").map(|f| relative_to(f, cwd)),
        ),
        RequestKind::Question => ("Claude has a question", None),
        RequestKind::ToolApproval => match pick("command") {
            Some(cmd) => ("Allow Claude to run this command?", Some(cmd.to_owned())),
            None => ("Allow Claude to use this tool?", None),
        },
    };
    let summary = req
        .title
        .split_once(": ")
        .map(|(_, rest)| rest.to_owned())
        .unwrap_or_else(|| req.title.clone());
    let reason = req
        .detail
        .get("decision_reason")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let mut buttons = h_flex().gap_2().pt_1().flex_wrap();
    for decision in &req.options {
        let decision = *decision;
        let store = store.clone();
        let (thread_id, request_id) = (req.thread_id.clone(), req.id.clone());
        // The default keys, spelled for the device in hand (⌘↩ on a Mac,
        // Ctrl+Enter elsewhere).
        let hint = match decision {
            DecisionKind::AllowOnce => crate::keymap::display("secondary-enter"),
            DecisionKind::AllowSession => crate::keymap::display("secondary-shift-enter"),
            DecisionKind::Deny => crate::keymap::display("secondary-shift-backspace"),
            DecisionKind::DenyAndStop => String::new(),
        };
        let label = if hint.is_empty() {
            decision.label().to_owned()
        } else {
            format!("{}  {hint}", decision.label())
        };
        let button = Button::new(SharedString::from(format!("{}-{decision:?}", req.id)))
            .cursor_pointer()
            .label(label)
            .small();
        let button = match decision {
            DecisionKind::AllowOnce => button.primary(),
            DecisionKind::AllowSession => button.outline(),
            DecisionKind::Deny => button.ghost(),
            DecisionKind::DenyAndStop => button.ghost().danger(),
        };
        buttons = buttons.child(button.on_click(move |_, _, cx| {
            let (thread_id, request_id) = (thread_id.clone(), request_id.clone());
            store.update(cx, |s, cx| s.respond(thread_id, request_id, decision, cx));
        }));
    }

    v_flex()
        .mt_2()
        .p_3()
        .gap_2()
        .rounded(px(12.))
        .border_1()
        .border_color(p.warning.opacity(0.35))
        .bg(p.warning_soft)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .text_color(p.warning)
                        .child(Icon::new(IconName::ShieldCheck).small()),
                )
                .child(div().font_weight(FontWeight::SEMIBOLD).child(headline)),
        )
        .child(
            div()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(summary),
        )
        .when_some(body, |d, body| {
            d.child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(px(8.))
                    .bg(p.background)
                    .border_1()
                    .border_color(p.border)
                    .font_family(p.mono.clone())
                    .text_size(p.mono_size)
                    .child(body),
            )
        })
        .when_some(reason, |d, r| {
            d.child(div().text_xs().text_color(p.faint_foreground).child(r))
        })
        .child(buttons)
        .into_any_element()
}

fn render_turn_footer(
    store: &Entity<AppStore>,
    model: &ThreadModel,
    turn_ix: usize,
    agents: Vec<(&'static str, &'static str)>,
    p: &Palette,
) -> AnyElement {
    use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
    let Some(turn) = model.turns.get(turn_ix) else {
        return div().into_any_element();
    };
    let (icon, label, color) = match turn.status {
        TurnStatus::Completed => (IconName::CircleCheck, "Done", p.faint_foreground),
        TurnStatus::Interrupted => (IconName::CircleStop, "Stopped", p.faint_foreground),
        TurnStatus::Failed => (IconName::CircleX, "Failed", p.danger),
        TurnStatus::Running => return div().into_any_element(),
    };
    let mut parts = vec![label.to_owned()];
    if let Some(u) = &turn.usage {
        if let Some(ms) = u.duration_ms {
            parts.push(format!("{:.1}s", ms as f64 / 1000.0));
        }
        let tokens = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens + u.output_tokens;
        if tokens > 0 {
            parts.push(format!("{} tokens", compact_count(tokens)));
        }
        if let Some(cost) = u.cost_usd {
            // A turn's own cost can be a fraction of a cent's tenth.
            parts.push(if cost > 0. && cost < 0.0005 {
                "<$0.001".to_owned()
            } else {
                format!("${cost:.3}")
            });
        }
    }
    let idle = model.running_turn().is_none();

    // Copy: the turn's reply (main-thread assistant text).
    let reply: Vec<&str> = model
        .items
        .iter()
        .filter(|i| i.turn_id.as_ref() == Some(&turn.id) && i.parent_id.is_none())
        .filter_map(|i| match &i.body {
            ItemBody::AssistantMessage { text } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let copy = (!reply.is_empty()).then(|| {
        copy_button(
            format!("copy-turn-{}", turn.id),
            reply.join("\n\n"),
            "Copy response",
            p,
        )
    });

    // Fork the conversation (and files) as of the end of this turn.
    let fork = idle.then(|| {
        let (store, thread_id, turn_id) = (store.clone(), model.thread.id.clone(), turn.id.clone());
        let current = model.thread.harness.clone();
        Button::new(SharedString::from(format!("fork-turn-{}", turn.id)))
            .cursor_pointer()
            .icon(IconName::GitFork)
            .ghost()
            .xsmall()
            .tooltip("Fork from here")
            .dropdown_menu(move |mut menu, _, _| {
                for &(id, name) in &agents {
                    let (store, t, at) = (store.clone(), thread_id.clone(), turn_id.clone());
                    let label = if id == current.as_str() {
                        format!("Fork with {name} (same conversation)")
                    } else {
                        format!("Fork with {name} (handoff summary)")
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        let h = loft_protocol::HarnessId(id.into());
                        let (t, at) = (t.clone(), Some(at.clone()));
                        store.update(cx, |s, cx| s.fork_thread(t, h, at, cx));
                    }));
                }
                menu
            })
    });

    // Undo: restore files to before this turn.
    let can_undo = turn.checkpoint_before.is_some() && idle;
    let undo = can_undo.then(|| {
        let (store, thread_id, turn_id) = (store.clone(), model.thread.id.clone(), turn.id.clone());
        let later = model.turns.len().saturating_sub(turn_ix + 1);
        Button::new(SharedString::from(format!("undo-{}", turn.id)))
            .cursor_pointer()
            .icon(IconName::RotateCw)
            .ghost()
            .xsmall()
            .tooltip("Undo this turn's changes")
            .on_click(move |_, window, cx| {
                confirm_revert(
                    store.clone(),
                    thread_id.clone(),
                    turn_id.clone(),
                    later,
                    window,
                    cx,
                )
            })
    });
    h_flex()
        .gap_1p5()
        .items_center()
        .text_xs()
        .text_color(color)
        .child(Icon::new(icon).xsmall())
        .child(parts.join(" · "))
        .child(
            h_flex()
                .ml_1()
                .gap_px()
                .items_center()
                .text_color(p.faint_foreground)
                .children(copy)
                .children(fork)
                .children(undo),
        )
        .into_any_element()
}

/// Which copy button was clicked last, and when (for its ✓).
static COPIED: std::sync::Mutex<Option<(String, web_time::Instant)>> = std::sync::Mutex::new(None);
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1500);

pub(crate) fn just_copied(id: &str) -> bool {
    COPIED
        .lock()
        .ok()
        .and_then(|c| c.clone())
        .is_some_and(|(last, at)| last == id && at.elapsed() < COPIED_FOR)
}

/// A small copy-to-clipboard icon button; it shows ✓ "Copied" briefly.
fn copy_button(
    id: String,
    text: String,
    tooltip: &'static str,
    p: &Palette,
) -> gpui_kit::component::button::Button {
    let copied = just_copied(&id);
    let key = id.clone();
    Button::new(SharedString::from(id))
        .cursor_pointer()
        .icon(if copied {
            IconName::Check
        } else {
            IconName::Copy
        })
        .ghost()
        .xsmall()
        .when(copied, |b| b.text_color(p.success))
        .tooltip(if copied { "Copied" } else { tooltip })
        .on_click(move |_, window, cx| {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text.clone()));
            mark_copied(key.clone(), window, cx);
        })
}

/// Remember that copy button `id` was just used (it shows ✓ for a moment).
pub(crate) fn mark_copied(id: String, window: &mut Window, cx: &mut App) {
    if let Ok(mut c) = COPIED.lock() {
        *c = Some((id, web_time::Instant::now()));
    }
    window.refresh();
    // Back to the copy icon afterwards.
    cx.spawn(async move |cx| {
        cx.background_executor().timer(COPIED_FOR).await;
        cx.update(|cx| cx.refresh_windows());
    })
    .detach();
}

pub(crate) fn confirm_revert(
    store: Entity<AppStore>,
    thread_id: loft_protocol::ThreadId,
    turn_id: loft_protocol::TurnId,
    later_turns: usize,
    window: &mut Window,
    cx: &mut App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
    let scope = match later_turns {
        0 => "the changes made in this turn".to_owned(),
        1 => "the changes made in this turn and the one after it".to_owned(),
        n => format!("the changes made in this turn and the {n} after it"),
    };
    window.open_dialog(cx, move |dialog, _, _| {
        let dialog = dialog.centered();
        let (store, thread_id, turn_id) = (store.clone(), thread_id.clone(), turn_id.clone());
        dialog
            .title("Undo changes?")
            .child(format!(
                "Files go back to how they were before this turn, discarding {scope}. The conversation is kept and the agent is told."
            ))
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().child(Button::new("cancel")
.cursor_pointer().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("ok")
.cursor_pointer().danger().label("Undo changes"))),
            )
            .on_ok(move |_, _, cx| {
                let (t, u) = (thread_id.clone(), turn_id.clone());
                store.update(cx, |s, cx| s.revert_turn(t, u, cx));
                true
            })
    });
}

// -- helpers -------------------------------------------------------------------

fn status_icon(running: bool, failed: bool, declined: bool, p: &Palette) -> AnyElement {
    let slot = div()
        .size(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0();
    if running {
        slot.child(Spinner::new().xsmall().color(p.muted_foreground))
            .into_any_element()
    } else if failed {
        slot.text_color(p.danger)
            .child(Icon::new(IconName::CircleX).xsmall())
            .into_any_element()
    } else if declined {
        slot.text_color(p.warning)
            .child(Icon::new(IconName::Ban).xsmall())
            .into_any_element()
    } else {
        slot.text_color(p.faint_foreground)
            .child(Icon::new(IconName::Check).xsmall())
            .into_any_element()
    }
}

fn chevron(open: bool) -> AnyElement {
    Icon::new(if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .xsmall()
    .into_any_element()
}

/// "Ran 2 commands, read 3 files" for a run of tool calls.
pub fn summarize(items: &[&Item]) -> String {
    let mut counts: Vec<(ToolKind, usize)> = Vec::new();
    for item in items {
        if let ItemBody::ToolCall { tool_kind, .. } = &item.body {
            match counts.iter_mut().find(|(k, _)| k == tool_kind) {
                Some((_, n)) => *n += 1,
                None => counts.push((*tool_kind, 1)),
            }
        }
    }
    let phrase = |kind: ToolKind, n: usize| -> String {
        let (verb, one, many) = match kind {
            ToolKind::Command => ("ran", "command", "commands"),
            ToolKind::FileChange => ("edited", "file", "files"),
            ToolKind::FileRead => ("read", "file", "files"),
            ToolKind::Search => ("ran", "search", "searches"),
            ToolKind::WebFetch => ("fetched", "page", "pages"),
            ToolKind::Mcp => ("called", "MCP tool", "MCP tools"),
            ToolKind::Subagent => ("ran", "agent", "agents"),
            ToolKind::Todo => ("updated", "todo list", "todo lists"),
            ToolKind::Other => ("used", "tool", "tools"),
        };
        format!("{verb} {n} {}", if n == 1 { one } else { many })
    };
    let text = counts
        .iter()
        .map(|(k, n)| phrase(*k, *n))
        .collect::<Vec<_>>()
        .join(", ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => text,
    }
}

/// Short display name for a tool (`mcp__server__tool` → `server·tool`).
fn tool_label(tool: &str) -> String {
    match tool.strip_prefix("mcp__") {
        Some(rest) => rest.replacen("__", "·", 1),
        None => tool.to_owned(),
    }
}

/// Show paths inside the thread's working directory relative to it.
pub fn relative_to(text: &str, cwd: &str) -> String {
    let prefix = format!("{}/", cwd.trim_end_matches('/'));
    let mut out = text.replace(&prefix, "");
    // macOS canonical paths: /private/tmp ↔ /tmp, /private/var ↔ /var.
    if let Some(short) = prefix.strip_prefix("/private") {
        out = out.replace(short, "");
    }
    out
}

fn compact_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::*;

    #[test]
    fn reply_images_get_a_line_and_a_width() {
        let out = sized_images("Here: ![Loft](/tmp/a.png) nice");
        assert_eq!(
            out,
            "Here:\n\n<img src=\"/tmp/a.png\" alt=\"Loft\" width=\"360\">\n\nnice"
        );
        assert_eq!(
            sized_images("**1. Loft**\n![Loft](/tmp/a.png)\n\n**2.**"),
            "**1. Loft**\n\n<img src=\"/tmp/a.png\" alt=\"Loft\" width=\"360\">\n\n\n\n**2.**"
        );
        // In a list it stays in the item.
        assert_eq!(
            sized_images("- see ![x](/a.png)"),
            "- see  \n<img src=\"/a.png\" alt=\"x\" width=\"360\">"
        );
        assert_eq!(sized_images("`![x](y)`"), "`![x](y)`");
        assert_eq!(sized_images("```\n![x](y)\n```\n"), "```\n![x](y)\n```\n");
        assert_eq!(sized_images("no images"), "no images");
    }

    #[test]
    fn lone_img_tags_parse() {
        let img = parse_img_tag("<img src=\"/tmp/a.png\" alt=\"A &quot;b&quot;\" width=\"360\">")
            .unwrap();
        assert_eq!(
            (img.src.as_str(), img.alt.as_str()),
            ("/tmp/a.png", "A \"b\"")
        );
        assert!(parse_img_tag("<img alt=\"x\">").is_none());
        assert!(parse_img_tag("<img src=\"a\"> <b>").is_none());
        assert!(parse_img_tag("<p>hi</p>").is_none());
        assert_eq!(local_image_path("https://x.dev/a.png", "/w"), None);
        assert_eq!(
            local_image_path("a.png", "/w"),
            Some(std::path::PathBuf::from("/w/a.png"))
        );
    }

    fn tool(kind: ToolKind) -> Item {
        Item {
            id: ItemId::new(),
            thread_id: ThreadId::new(),
            turn_id: None,
            ordinal: 0,
            status: ItemStatus::Completed,
            body: ItemBody::ToolCall {
                tool: "x".into(),
                tool_kind: kind,
                title: "t".into(),
                input: serde_json::Value::Null,
                output: None,
                is_error: false,
            },
            updated_at_ms: 0,
            parent_id: None,
            started_at_ms: None,
        }
    }

    #[test]
    fn plain_text_survives_markdown() {
        assert_eq!(plain_markdown("a*b*"), "a\\*b\\*");
        assert_eq!(plain_markdown("# hi\nnext"), "\\# hi  \nnext");
        assert_eq!(plain_markdown("p1\n\np2"), "p1\n\np2");
        assert!(plain_markdown("    code").starts_with('\u{a0}'));
        assert!(is_long_message(&"line\n".repeat(20)));
        assert!(!is_long_message("short"));
    }

    #[test]
    fn summaries_read_naturally() {
        let items = [
            tool(ToolKind::Command),
            tool(ToolKind::FileRead),
            tool(ToolKind::Command),
        ];
        let refs: Vec<&Item> = items.iter().collect();
        assert_eq!(summarize(&refs), "Ran 2 commands, read 1 file");
        assert_eq!(summarize(&[&tool(ToolKind::FileChange)]), "Edited 1 file");
    }

    #[test]
    fn paths_become_relative() {
        assert_eq!(
            relative_to("/private/tmp/repo/src/a.rs", "/private/tmp/repo"),
            "src/a.rs"
        );
        assert_eq!(relative_to("cat /tmp/repo/x", "/private/tmp/repo"), "cat x");
        assert_eq!(compact_count(12_345), "12.3k");
    }
}
