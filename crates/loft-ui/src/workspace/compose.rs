//! What the composer takes in besides typing: a sent prompt recalled with
//! ↑ (`recall`), another thread as context (`thread_ref`: picked after
//! `@`, or dragged in from the sidebar), a selection quoted from the
//! conversation and one fenced from a terminal (`quote`). The rules are in
//! those modules; this is where they meet the composer.

use super::{DraggedThread, WorkspaceView};
use crate::recall::{Recall, Step};
use crate::store::ThreadState;
use crate::theme::Palette;
use crate::thread_ref::{Found, Listed, MAX_THREADS, ThreadRef};
use gpui_kit::assets::IconName;
use gpui_kit::base::TextSelection;
use gpui_kit::component::input::TextareaState;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::{
    AnyElement, ClipboardItem, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement as _, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, WeakEntity, Window, actions, div, px,
};
use loft_protocol::{ThreadId, ThreadStatus};
use std::collections::HashMap;

actions!(
    agentloft,
    [
        /// Put the text selected in the conversation into the composer as
        /// a Markdown quote.
        QuoteSelection
    ]
);

/// The composer's drop target, as a group its hint is styled by.
const DROP_GROUP: &str = "composer-drop";

/// What the workspace keeps for all this.
#[derive(Default)]
pub(super) struct Composing {
    /// Where each thread's composer is in the prompts sent to it (↑/↓).
    recall: HashMap<ThreadId, Recall>,
    /// The thread chips a recalled prompt brought back with it (as their
    /// lines in `pasted`): they go again when recall moves on.
    recalled_threads: HashMap<ThreadId, Vec<String>>,
}

impl WorkspaceView {
    // -- a sent prompt, recalled ---------------------------------------------

    /// ↑ (`back`) or ↓ in `thread_id`'s composer. `true`: it recalled a
    /// prompt and the key is spent; `false`: the composer keeps the key
    /// (something is being written in it).
    pub(super) fn recall_step(
        &mut self,
        thread_id: &ThreadId,
        back: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(composer) = self.composers.get(thread_id).cloned() else {
            return false;
        };
        // The keys of the composer itself, not of a field in a popover of
        // its card (the model picker's search).
        if !composer.read(cx).focus_handle(cx).is_focused(window) {
            return false;
        }
        let current = composer.read(cx).value().to_string();
        let recall = self.composing.recall.entry(thread_id.clone()).or_default();
        if !recall.shows(&current) {
            // Not, or no longer, a recalled prompt as it came: the chips
            // one brought stay, as the user's own.
            self.composing.recalled_threads.remove(thread_id);
            if !current.is_empty() {
                recall.reset();
                return false;
            }
        }
        let history = match self.store.read(cx).threads.get(thread_id) {
            Some(thread) => prompts(thread.read(cx)),
            None => Vec::new(),
        };
        let step = if back {
            recall.back(&current, &history)
        } else {
            recall.forward(&current, &history)
        };
        match step {
            Step::Pass => false,
            Step::Stay => true,
            Step::Show(text) => {
                self.recall_threads(thread_id, &text, cx);
                // An edit like any other: it can be undone, and the draft
                // follows it.
                composer.update(cx, |c, cx| {
                    c.set_selected_range(0..current.len(), cx);
                    c.replace(text, window, cx);
                });
                true
            }
        }
    }

    // -- the agent's suggested next message ---------------------------------

    /// ⇥ in an empty composer: the agent's suggested next message goes in,
    /// to send as it is or edit. `false`: there is none to take, and the
    /// key is the composer's.
    pub(super) fn take_suggestion(
        &mut self,
        thread_id: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(composer) = self.composers.get(thread_id).cloned() else {
            return false;
        };
        if !composer.read(cx).focus_handle(cx).is_focused(window)
            || !composer.read(cx).value().is_empty()
        {
            return false;
        }
        let text = self
            .store
            .read(cx)
            .threads
            .get(thread_id)
            .and_then(|t| t.read(cx).model.as_ref())
            .and_then(|m| super::suggestion(&m.thread))
            .map(str::to_owned);
        let Some(text) = text else {
            return false;
        };
        // An edit like any other: it can be undone, and the draft follows.
        composer.update(cx, |c, cx| c.replace(text, window, cx));
        true
    }

    /// The threads sent along with the prompt now recalled (`text`) wait
    /// as chips again, in place of those the prompt recalled before
    /// brought. Chips the user added, or kept by editing, are theirs.
    fn recall_threads(&mut self, thread_id: &ThreadId, text: &str, cx: &mut Context<Self>) {
        let before = self
            .composing
            .recalled_threads
            .remove(thread_id)
            .unwrap_or_default();
        if let Some(waiting) = self.pasted.get_mut(thread_id) {
            waiting.retain(|line| !before.contains(line));
        }
        let sent_with: Vec<ThreadRef> = match self.store.read(cx).threads.get(thread_id) {
            Some(thread) if !text.is_empty() => said(thread.read(cx))
                .rev()
                .map(crate::thread_ref::split)
                .find(|(typed, _)| typed.trim() == text)
                .map(|(_, threads)| threads)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut brought = Vec::new();
        for thread in sent_with {
            let line = thread.to_text();
            let waiting = self.pasted.entry(thread_id.clone()).or_default();
            if !waiting.contains(&line) {
                waiting.push(line.clone());
                brought.push(line);
            }
        }
        if !brought.is_empty() {
            self.composing
                .recalled_threads
                .insert(thread_id.clone(), brought);
        }
        cx.notify();
    }

    /// Whether `composer` holds a recalled prompt as it was put there (no
    /// completions are offered for it: ↑ and ↓ are recall's).
    pub(super) fn shows_recalled(
        &self,
        thread_id: &ThreadId,
        composer: &Entity<TextareaState>,
        cx: &gpui_kit::App,
    ) -> bool {
        self.composing
            .recall
            .get(thread_id)
            .is_some_and(|recall| recall.shows(composer.read(cx).value().as_ref()))
    }

    // -- another thread as context -------------------------------------------

    /// The threads `@query` offers in `thread_id`'s composer.
    pub(super) fn matching_threads(
        &self,
        thread_id: &ThreadId,
        query: &str,
        cx: &gpui_kit::App,
    ) -> Vec<Found> {
        let workspace = &self.store.read(cx).workspace;
        let listed: Vec<Listed<'_>> = workspace
            .threads
            .iter()
            .map(|t| Listed {
                id: &t.id,
                title: &t.title,
                project: &t.project_id,
                project_name: workspace
                    .projects
                    .iter()
                    .find(|p| p.id == t.project_id)
                    .map_or("", |p| p.name.as_str()),
                active_ms: t.updated_at_ms,
                hidden: t.status == ThreadStatus::Archived || !t.is_started(),
            })
            .collect();
        crate::thread_ref::match_threads(&listed, thread_id, query, MAX_THREADS)
    }

    /// Add `thread` to the message being written in `to`'s composer. It
    /// waits with the large pastes (`pasted`), as the line that will be
    /// sent, and is drawn as a chip (`thread_chip`): sending needs to know
    /// nothing about it.
    pub(super) fn add_thread_ref(
        &mut self,
        to: &ThreadId,
        thread: ThreadRef,
        cx: &mut Context<Self>,
    ) {
        if &thread.id == to {
            return;
        }
        let line = thread.to_text();
        let waiting = self.pasted.entry(to.clone()).or_default();
        if !waiting.contains(&line) {
            waiting.push(line);
        }
        cx.notify();
    }

    /// A thread waiting in the composer (entry `ix` of `pasted`): its
    /// chip, with a ✕ to take it out again.
    pub(super) fn thread_chip(
        &self,
        thread_id: &ThreadId,
        ix: usize,
        thread: &ThreadRef,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tid = thread_id.clone();
        crate::thread_ref::chip(thread, p)
            .pr_1()
            .child(
                div()
                    .id(SharedString::from(format!("thread-ref-remove-{ix}")))
                    .cursor_pointer()
                    .p(px(3.))
                    .rounded(px(5.))
                    .text_color(p.muted_foreground)
                    .hover(|d| d.bg(p.selected).text_color(p.foreground))
                    .child(Icon::new(IconName::X).xsmall())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(list) = this.pasted.get_mut(&tid)
                            && ix < list.len()
                        {
                            list.remove(ix);
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// While something is dragged: the composer as a place to drop it. A
    /// thread (from the sidebar or a pane's title bar) becomes context for
    /// the message. Drawn last, over the pane's own drop zones, which
    /// cover the composer too; files dropped here are still attached.
    pub(super) fn composer_drop_target(
        &self,
        thread_id: &ThreadId,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !cx.has_active_drag() {
            return None;
        }
        let accent = p.accent;
        let surface = p.elevated.opacity(0.92);
        let (own, for_thread, for_files) =
            (thread_id.clone(), thread_id.clone(), thread_id.clone());
        // What a thread dropped here becomes, shown while one is over the
        // composer. An icon, not words: a drag's style reaches what is
        // painted with it, and text takes its colour when it is laid out.
        let hint = gpui_kit::svg()
            .path(IconName::MessageSquarePlus.path())
            .size(px(22.))
            .text_color(gpui_kit::transparent_black())
            .drag_over::<DraggedThread>(move |s, _, _, _| s.text_color(accent))
            .group_drag_over::<DraggedThread>(DROP_GROUP, move |s| s.text_color(accent));
        let target = div()
            .group(DROP_GROUP)
            .absolute()
            .inset_0()
            .rounded(px(18.))
            // The zones underneath are not also being dropped on.
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            // A thread is no context for itself.
            .can_drop(move |dragged, _, _| {
                dragged
                    .downcast_ref::<DraggedThread>()
                    .is_none_or(|thread| thread.id != own)
            })
            .drag_over::<DraggedThread>(move |s, _, _, _| {
                s.bg(surface).border_2().border_color(accent.opacity(0.7))
            })
            .drag_over::<gpui_kit::ExternalPaths>(move |s, _, _, _| {
                s.border_1().border_color(accent)
            })
            .child(hint)
            .on_drop(cx.listener(move |this, dragged: &DraggedThread, _, cx| {
                let thread = ThreadRef::new(dragged.id.clone(), &dragged.title);
                this.add_thread_ref(&for_thread, thread, cx);
            }))
            .on_drop(
                cx.listener(move |this, paths: &gpui_kit::ExternalPaths, _, cx| {
                    this.attach_paths(for_files.clone(), paths.paths().to_vec(), cx);
                }),
            );
        Some(gpui_kit::deferred(target).into_any_element())
    }

    // -- a selection, into the message ---------------------------------------

    /// Put `block` after what is typed in `thread_id`'s composer, then
    /// `after`, and the caret after that. One edit, undone with ⌘Z.
    fn insert_block(
        &mut self,
        thread_id: &ThreadId,
        block: &str,
        after: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.store.read(cx).threads.get(thread_id).cloned() else {
            return;
        };
        let composer = self.composer_for(&thread, window, cx);
        composer.update(cx, |c, cx| {
            let typed = c.value().to_string();
            let added = crate::quote::appended(&typed, block, after);
            c.set_selected_range(typed.len()..typed.len(), cx);
            c.insert(added, window, cx);
            c.focus(window, cx);
        });
        cx.notify();
    }

    /// `text` (selected in the conversation) as a quote in the selected
    /// thread's composer.
    pub(crate) fn quote_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.store.read(cx).selected.clone() else {
            return;
        };
        if let Some(quote) = crate::quote::markdown_quote(text) {
            self.insert_block(&thread_id, &quote, "\n\n", window, cx);
        }
    }

    /// The Quote shortcut: whatever is selected in the conversation. (In a
    /// terminal the same keys are the terminal's own action, which ends in
    /// `add_terminal_selection`.)
    pub(super) fn quote_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = TextSelection::selected_text(window, cx);
        self.quote_text(&text, window, cx);
    }

    /// `text` (selected in one of `thread_id`'s terminals) as a code block
    /// in its composer.
    pub(super) fn add_terminal_selection(
        &mut self,
        thread_id: &ThreadId,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(block) = crate::quote::fenced_block(text) {
            self.insert_block(thread_id, &block, "\n", window, cx);
        }
    }

    // -- dev ------------------------------------------------------------------

    /// Dev/test hook `do:<step> <step> …`: input in order, through the
    /// window's own event handling, with the composer focused first.
    /// A step is `wait=<ms>`, `click=x,y`, `rclick=x,y`, `move=x,y`,
    /// `drag=x,y,x2,y2` (or its parts `press=x,y`, `hold=x,y`, `release=x,y`),
    /// all in window points, `'text` (typed a character at a time) or a
    /// keystroke (`up`, `enter`, `cmd-k`).
    pub(super) fn dev_script(
        &mut self,
        script: &str,
        thread: &Entity<ThreadState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_kit::{
            Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
            PlatformInput, point,
        };
        let steps: Vec<String> = script.split_whitespace().map(str::to_owned).collect();
        let composer = self.composer_for(thread, window, cx);
        composer.update(cx, |c, cx| c.focus(window, cx));
        let os = cx.platform_info().os;
        cx.spawn_in(window, async move |_, cx| {
            let executor = cx.background_executor().clone();
            let pause = |ms: u64| executor.timer(std::time::Duration::from_millis(ms));
            let modifiers = Modifiers::default();
            let moved = |position, pressed_button| {
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button,
                    modifiers,
                })
            };
            let down = |position, button| {
                PlatformInput::MouseDown(MouseDownEvent {
                    button,
                    position,
                    modifiers,
                    click_count: 1,
                    first_mouse: false,
                })
            };
            let up = |position, button| {
                PlatformInput::MouseUp(MouseUpEvent {
                    button,
                    position,
                    modifiers,
                    click_count: 1,
                })
            };
            for step in steps {
                let (name, arg) = step.split_once('=').unwrap_or((step.as_str(), ""));
                let n: Vec<f32> = arg.split(',').filter_map(|v| v.parse().ok()).collect();
                let at = |i: usize| {
                    let v = |i: usize| px(n.get(i).copied().unwrap_or(0.));
                    point(v(i), v(i + 1))
                };
                let left = MouseButton::Left;
                let events: Vec<PlatformInput> = match name {
                    "wait" => {
                        pause(n.first().copied().unwrap_or(500.) as u64).await;
                        continue;
                    }
                    "move" => vec![moved(at(0), None)],
                    // A drag in parts, to look at it half way.
                    "press" => vec![moved(at(0), None), down(at(0), left)],
                    "hold" => vec![moved(at(0), Some(left))],
                    "release" => vec![up(at(0), left)],
                    "click" => vec![moved(at(0), None), down(at(0), left), up(at(0), left)],
                    "rclick" => {
                        let right = MouseButton::Right;
                        vec![moved(at(0), None), down(at(0), right), up(at(0), right)]
                    }
                    "drag" => {
                        // Out of the press, across in three moves, let go.
                        let (from, to) = (at(0), at(2));
                        let part = |f: f32| from + point((to.x - from.x) * f, (to.y - from.y) * f);
                        vec![
                            moved(from, None),
                            down(from, left),
                            moved(part(0.1), Some(left)),
                            moved(part(0.5), Some(left)),
                            moved(to, Some(left)),
                            up(to, left),
                        ]
                    }
                    _ => {
                        let keys: Vec<Keystroke> = match step.strip_prefix('\'') {
                            Some(text) => text
                                .chars()
                                .map(|c| Keystroke {
                                    modifiers,
                                    key: c.to_string(),
                                    key_char: Some(c.to_string()),
                                })
                                .collect(),
                            None => Keystroke::parse_with_os(&step, os)
                                .ok()
                                .into_iter()
                                .collect(),
                        };
                        for key in keys {
                            let _ = cx.update(|window, cx| {
                                window.dispatch_keystroke(key, cx);
                            });
                            pause(30).await;
                        }
                        pause(120).await;
                        continue;
                    }
                };
                for event in events {
                    let _ = cx.update(|window, cx| {
                        window.dispatch_event(event, cx);
                    });
                    // A frame between two: a drag's drop zones are drawn
                    // once it has started.
                    pause(80).await;
                }
            }
            tracing::info!("dev: script done");
        })
        .detach();
    }
}

/// The conversation's context menu: what Copy offers for a selection, and
/// Quote next to it. (Both are there, disabled, with nothing selected.)
pub(super) fn timeline_menu(
    view: WeakEntity<WorkspaceView>,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, window, cx| {
        let text = TextSelection::selected_text(window, cx).trim().to_owned();
        let nothing = text.is_empty();
        let (copied, quoted, view) = (text.clone(), text, view.clone());
        menu.item(
            PopupMenuItem::new("Copy")
                .icon(IconName::Copy)
                .action(Box::new(gpui_kit::component::input::Copy))
                .disabled(nothing)
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                }),
        )
        .item(
            PopupMenuItem::new("Quote in Message")
                .icon(IconName::TextQuote)
                .action(Box::new(QuoteSelection))
                .disabled(nothing)
                .on_click(move |_, window, cx| {
                    let (view, text) = (view.clone(), quoted.clone());
                    // After the menu closes, so the composer keeps focus.
                    window.defer(cx, move |window, cx| {
                        let _ = view.update(cx, |v, cx| v.quote_text(&text, window, cx));
                    });
                }),
        )
    }
}

/// The prompts sent in a thread, oldest first, for ↑ to step through: as
/// typed, without the lines that stand for thread chips (`recall_threads`
/// brings those back as chips).
fn prompts(thread: &ThreadState) -> Vec<String> {
    crate::recall::history(said(thread).map(|text| crate::thread_ref::split(text).0))
}

/// What the user said to this thread's agent (not what an agent said to
/// one of its own), oldest first.
fn said(thread: &ThreadState) -> impl DoubleEndedIterator<Item = &str> {
    let items = thread.model.as_ref().map_or(&[][..], |m| &m.items[..]);
    items.iter().filter_map(|item| match &item.body {
        loft_protocol::ItemBody::UserMessage { text, .. } if item.parent_id.is_none() => {
            Some(text.as_str())
        }
        _ => None,
    })
}
