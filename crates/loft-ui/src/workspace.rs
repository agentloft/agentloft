//! The main window: an integrated sidebar (search, projects → threads,
//! status), a breadcrumb header, the conversation timeline and a floating
//! composer.

use crate::items::render_row;
use crate::panel::{Tab, WorkbenchPanel};
use crate::press::ClaimsPress as _;
use crate::spinner::Spinner;
use crate::store::{AppStore, ConnState, StoreEvent, ThreadState};
use crate::theme::{self, Palette, relative_time};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, ElementId, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, actions, div,
    prelude::FluentBuilder as _, px,
};
use loft_protocol::{HarnessId, ProjectId, ThreadId, ThreadStatus, ThreadWorkspace};
use std::collections::HashMap;

actions!(
    agentloft,
    [
        NewThread,
        NewChat,
        Interrupt,
        ToggleSidebar,
        ToggleTheme,
        OpenSettings,
        AddProject,
        FocusSearch,
        About,
        Quit,
        TogglePanel,
        ApproveOnce,
        ApproveSession,
        DenyRequest,
        ShowFiles,
        ShowChanges,
        ShowTerminal,
        ToggleTerminal,
        ShowBrowser,
        NewWindow,
        NewTab,
        OpenPalette,
        SteerMessage
    ]
);

// The composer's recall, thread context and quoting (`workspace/compose.rs`).
mod compose;
pub use compose::QuoteSelection;

const CONTEXT: &str = "Workspace";
const SIDEBAR_WIDTH: f32 = 272.;
const HEADER_HEIGHT: f32 = 52.;
/// Leaves room for the macOS traffic lights in the transparent title bar
/// (a browser tab has none, so only the usual margin there).
const TRAFFIC_LIGHTS_INSET: f32 = if crate::host::IS_WEB { 16. } else { 78. };

pub fn bind_keys(cx: &mut gpui_kit::App) {
    // Every shortcut, with the user's own keys (Settings → Shortcuts).
    crate::keymap::apply(cx);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

/// The native menu bar.
pub fn app_menus() -> Vec<gpui_kit::Menu> {
    use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
    use gpui_kit::{Menu, MenuItem, OsAction, SystemMenuType};
    vec![
        Menu::new("Agentloft").items([
            MenuItem::action("About Agentloft", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit Agentloft", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Thread", NewThread),
            MenuItem::action("New Chat", NewChat),
            MenuItem::action("New Pane", NewTab),
            MenuItem::action("New Window", NewWindow),
            MenuItem::separator(),
            MenuItem::action("Add Project…", AddProject),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Panel", TogglePanel),
            MenuItem::separator(),
            MenuItem::action("Files", ShowFiles),
            MenuItem::action("Changes", ShowChanges),
            MenuItem::action("Terminal", ToggleTerminal),
            MenuItem::action("Terminal in Side Panel", ShowTerminal),
            MenuItem::action("Browser", ShowBrowser),
            MenuItem::separator(),
            MenuItem::action("Command Palette…", OpenPalette),
            MenuItem::action("Search Threads", FocusSearch),
            MenuItem::separator(),
            MenuItem::action("Toggle Light/Dark", ToggleTheme),
        ]),
        Menu::new("Agent").items([MenuItem::action("Stop Turn", Interrupt)]),
    ]
}

pub struct WorkspaceView {
    pub(crate) store: Entity<AppStore>,
    /// The footer's usage button: each agent's plan limits, on hover.
    plan_limits: Entity<crate::plan_limits::PlanLimits>,
    /// The Automations page, made the first time it is opened.
    automations_page: Option<Entity<crate::automations::AutomationsPage>>,
    composers: HashMap<ThreadId, Entity<TextareaState>>,
    /// Threads whose composer has something typed in it right now (a
    /// draft is saved a moment later).
    typed: std::collections::HashSet<ThreadId>,
    /// Images waiting in each thread's composer.
    attachments: HashMap<ThreadId, Vec<crate::attachments::PendingImage>>,
    /// Large pastes held as chips until sent, per thread.
    pasted: HashMap<ThreadId, Vec<String>>,
    /// Notes left on the browser page, waiting to be sent.
    page_notes: HashMap<ThreadId, Vec<loft_browser::Annotation>>,
    /// Notes left on documents open in a thread's panel, not sent yet: the
    /// file (as its tab has it) and the note.
    doc_notes: HashMap<ThreadId, Vec<(String, crate::panel::docs::DocNote)>>,
    composer_harness: HashMap<ThreadId, HarnessId>,
    observed: HashMap<ThreadId, Subscription>,
    search: Entity<InputState>,
    query: String,
    /// The sidebar lists archived threads too.
    show_archived: bool,
    /// Sidebar groups showing their threads idle for over 12 hours: the
    /// Threads list (`None`) or a project's.
    show_older: std::collections::HashSet<Option<ProjectId>>,
    /// The sidebar thread whose ··· menu is open.
    row_menu: Option<ThreadId>,
    /// Threads sidebar: only this project's threads (`None` = all).
    project_filter: Option<ProjectId>,
    /// Sidebar threads picked together (shift-click), in sidebar order.
    multi: Vec<ThreadId>,
    /// Where a shift-click range starts: the last thread clicked.
    anchor: Option<ThreadId>,
    /// The sidebar's thread rows as last drawn, top to bottom.
    visible_rows: Vec<ThreadId>,
    /// The sidebar thread being renamed in place, and its field.
    renaming: Option<(ThreadId, Entity<InputState>, Subscription)>,
    /// Threads whose messages match the sidebar search, and the search
    /// those answer.
    search_hits: HashMap<ThreadId, loft_protocol::SearchHit>,
    search_seq: u64,
    /// The open `/` or `@` completion popover.
    completion: Option<crate::completion::Completion>,
    completion_seq: u64,
    /// Sent prompts being recalled with ↑ (`workspace/compose.rs`).
    composing: compose::Composing,
    /// Open the folder browser on the next render (asked for without a
    /// window at hand).
    pending_add_project: bool,
    /// The floating mini browser, shown when an agent drives the browser
    /// while the panel isn't showing it.
    browser_preview: bool,
    /// Where the mini browser was dragged to, as its distance from the
    /// conversation's right and bottom edges (`None`: its usual corner).
    browser_preview_at: Option<gpui_kit::Point<gpui_kit::Pixels>>,
    /// The conversation's size as last laid out (the mini browser stays in it).
    preview_room: std::rc::Rc<std::cell::Cell<gpui_kit::Size<gpui_kit::Pixels>>>,
    /// Whether we listen to the browser's events yet (annotations).
    /// The composer's model picker (for the selected thread) and whether
    /// it's open.
    model_picker: Option<Entity<crate::model_picker::ModelPicker>>,
    picker_open: bool,
    /// Pending debounced draft saves, per thread.
    draft_saves: HashMap<ThreadId, gpui_kit::Task<()>>,
    /// The forms for open questions and plans (kept while open, so what
    /// was picked or typed survives switching threads).
    ask_forms: HashMap<loft_protocol::RequestId, Entity<crate::ask::AskForm>>,
    /// Threads whose saved draft has been put back in their composer.
    drafts_restored: std::collections::HashSet<ThreadId>,
    /// The new-worktree branch chooser and whether it's open.
    branch_picker: Option<Entity<crate::branch_picker::BranchPicker>>,
    branch_picker_open: bool,
    pub(crate) sidebar_open: bool,
    pub panel: Entity<WorkbenchPanel>,
    pub(crate) panel_open: bool,
    /// Terminals docked under the conversation (Ctrl+`).
    dock: Entity<crate::panel::dock::TerminalDock>,
    pub(crate) dock_open: bool,
    dock_slide: Slide,
    /// The dock's height as last laid out (it slides, and reopens, at it).
    dock_height: std::rc::Rc<std::cell::Cell<f32>>,
    /// The thread's project actions in the header (`actions.rs`).
    pub(crate) actions_bar: crate::actions::ActionsBar,
    /// Where the details button is (its card opens under it).
    details_anchor: std::rc::Rc<std::cell::Cell<Option<gpui_kit::Bounds<gpui_kit::Pixels>>>>,
    sidebar_slide: Slide,
    /// How far open the sidebar is drawn this frame (0–1).
    sidebar_amount: f32,
    panel_slide: Slide,
    details_slide: Slide,
    /// How far open the details card is drawn this frame (0–1).
    details_amount: f32,
    /// The card floats over the conversation's right margin (one pane):
    /// how much of the margin it takes, which the conversation keeps clear.
    /// Zero when it docks as a column beside the panes instead.
    details_inset: f32,
    /// The side panel's width as last laid out (it slides, and reopens, at
    /// this width). Zero until it first opens, when it gets a share of the room.
    panel_width: std::rc::Rc<std::cell::Cell<f32>>,
    /// The conversation's width as last laid out (the turn map needs a margin).
    /// Each pane's conversation width (the turn map needs a margin).
    timeline_widths: HashMap<ThreadId, std::rc::Rc<std::cell::Cell<f32>>>,
    /// The pane area's size, measured: how many panes fit.
    pane_room: std::rc::Rc<std::cell::Cell<(f32, f32)>>,
    /// While drawing a pane: whether it is the focused one (popovers and
    /// the mini browser show only there).
    rendering_focused: bool,
    /// The turn-map tick being hovered (its peek card shows).
    peek: Option<(ThreadId, usize)>,
    /// The details card is docked open (shown while the window is wide).
    details_open: bool,
    details_card: Option<Entity<crate::details::DetailsCard>>,
    /// Width of the conversation area (with the dock), to decide docking.
    details_room: std::rc::Rc<std::cell::Cell<f32>>,
    /// Dev/test hook: `AGENTLOFT_DEV_TAB=files|changes|agent|file:<path>|docnote:<spec>`
    /// opens a panel tab once the first thread has loaded.
    dev_tab: Option<String>,
    /// Last window title set (the native tab label).
    window_title: String,
    _subs: Vec<Subscription>,
}

impl WorkspaceView {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search threads"));
        let panel = cx.new(|cx| WorkbenchPanel::new(store.clone(), window, cx));
        let dock = cx.new(|cx| crate::panel::dock::TerminalDock::new(panel.clone(), cx));
        let subs = vec![
            cx.subscribe_in(&store, window, |this, _, event, window, cx| {
                this.on_store_event(event, window, cx)
            }),
            // Notes and screenshots from a thread's browser go to its composer.
            cx.subscribe_in(&panel, window, |this, _, event, window, cx| match event {
                crate::panel::PanelEvent::Annotation(thread_id, note) => {
                    this.add_annotation(thread_id, note, window, cx)
                }
                crate::panel::PanelEvent::DocNote(thread_id, path, note) => {
                    this.add_doc_note(thread_id, path, note, cx)
                }
                crate::panel::PanelEvent::AddToMessage(thread_id, text) => {
                    this.add_terminal_selection(thread_id, text, window, cx)
                }
                crate::panel::PanelEvent::Maximized(_) => cx.notify(),
                crate::panel::PanelEvent::ShowPanel => {
                    this.panel_open = true;
                    cx.notify();
                }
                crate::panel::PanelEvent::ShowDock => {
                    this.dock_open = true;
                    cx.notify();
                }
                crate::panel::PanelEvent::HideDock => this.hide_dock(window, cx),
                // Done annotating with no notes waiting: clear the page.
                crate::panel::PanelEvent::AnnotatingStopped(thread_id) => {
                    if this.page_notes.get(thread_id).is_none_or(|n| n.is_empty())
                        && let Some(browser) = this.panel.read(cx).existing_browser(thread_id, cx)
                    {
                        browser.update(cx, |b, _| b.clear_annotations());
                    }
                }
                crate::panel::PanelEvent::Screenshot(thread_id, png) => {
                    if let Some(image) =
                        crate::attachments::PendingImage::png("screenshot.png", png.clone())
                    {
                        this.add_images(thread_id, vec![image], cx);
                    }
                }
            }),
            cx.observe(&store, |_, _, cx| cx.notify()),
            // The window title is the native tab label; hidden tabs don't
            // render, so keep it current from store changes.
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.update_window_title(window, cx)
            }),
            cx.subscribe_in(&search, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query = state.read(cx).value().trim().to_lowercase();
                    this.search_messages(cx);
                    cx.notify();
                }
            }),
            // Follow the system light/dark switch when theme = "system".
            cx.observe_window_appearance(window, |_, _, cx| theme::apply(cx)),
            // Switching native tabs: refresh the tab strip. Back from
            // another app: the project's actions may have been edited.
            cx.observe_window_activation(window, |this, _, cx| {
                this.actions_bar.stale();
                cx.notify()
            }),
        ];
        Self {
            plan_limits: cx.new(|_| crate::plan_limits::PlanLimits::new(store.clone())),
            store,
            automations_page: None,
            composers: HashMap::new(),
            typed: Default::default(),
            attachments: HashMap::new(),
            pasted: HashMap::new(),
            page_notes: HashMap::new(),
            doc_notes: HashMap::new(),
            composer_harness: HashMap::new(),
            observed: HashMap::new(),
            search,
            query: String::new(),
            show_archived: false,
            show_older: Default::default(),
            row_menu: None,
            project_filter: None,
            multi: Vec::new(),
            anchor: None,
            visible_rows: Vec::new(),
            renaming: None,
            search_hits: HashMap::new(),
            search_seq: 0,
            completion: None,
            completion_seq: 0,
            composing: Default::default(),
            pending_add_project: false,
            browser_preview: false,
            browser_preview_at: None,
            preview_room: Default::default(),
            model_picker: None,
            picker_open: false,
            branch_picker: None,
            branch_picker_open: false,
            draft_saves: HashMap::new(),
            ask_forms: HashMap::new(),
            drafts_restored: Default::default(),
            sidebar_open: true,
            panel,
            panel_open: false,
            dock,
            dock_open: false,
            dock_slide: Slide::default(),
            dock_height: std::rc::Rc::new(std::cell::Cell::new(DOCK_HEIGHT)),
            actions_bar: Default::default(),
            details_anchor: Default::default(),
            details_open: false,
            peek: None,
            timeline_widths: Default::default(),
            pane_room: Default::default(),
            rendering_focused: true,
            sidebar_slide: Slide::default(),
            sidebar_amount: 1.,
            panel_slide: Slide::default(),
            details_slide: Slide::default(),
            details_amount: 0.,
            details_inset: 0.,
            panel_width: std::rc::Rc::new(std::cell::Cell::new(0.)),
            details_card: None,
            details_room: Default::default(),
            window_title: String::new(),
            dev_tab: take_dev_tab(),
            _subs: subs,
        }
    }

    fn update_window_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let title = store
            .selected
            .as_ref()
            .and_then(|id| store.workspace.threads.iter().find(|t| &t.id == id))
            .map(|t| {
                let project = store
                    .workspace
                    .projects
                    .iter()
                    .find(|p| p.id == t.project_id)
                    .map(|p| p.name.as_str())
                    .unwrap_or("");
                format!("{} — {project}", t.title)
            })
            .unwrap_or_else(|| "Agentloft".into());
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
            // Sibling tabs draw this title in their tab strips.
            cx.defer(crate::windows::refresh_all);
        }
    }

    pub(crate) fn on_store_event(
        &mut self,
        event: &StoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            StoreEvent::OpenFile {
                thread_id,
                rel_path,
            } => {
                self.panel_open = true;
                let (t, r) = (thread_id.clone(), rel_path.clone());
                self.panel
                    .update(cx, |panel, cx| panel.open_file(&t, r, window, cx));
            }
            StoreEvent::OpenTurnDiff {
                thread_id,
                turn_id,
                path,
            } => {
                self.panel_open = true;
                let (t, u, path) = (thread_id.clone(), turn_id.clone(), path.clone());
                self.panel.update(cx, |panel, cx| {
                    panel.open_turn_diff(&t, u, path, window, cx)
                });
            }
            StoreEvent::OpenAgent { thread_id, item_id } => {
                self.panel_open = true;
                let (t, i) = (thread_id.clone(), item_id.clone());
                self.panel
                    .update(cx, |panel, cx| panel.open_agent(&t, i, window, cx));
            }
            StoreEvent::BrowserCall {
                call_id,
                thread_id,
                command,
            } => {
                self.run_browser_call(*call_id, thread_id.clone(), command.clone(), window, cx);
            }
            StoreEvent::Notify { message, error } => {
                use gpui_kit::component::WindowExt as _;
                use gpui_kit::component::notification::Notification;
                let note = Notification::new().message(message.clone());
                let note = if *error {
                    // The same error again replaces its toast rather than
                    // stacking another.
                    struct ErrorToast;
                    note.id1::<ErrorToast>(gpui_kit::SharedString::from(message.clone()))
                        .with_type(gpui_kit::component::notification::NotificationType::Error)
                } else {
                    note.with_type(gpui_kit::component::notification::NotificationType::Success)
                };
                window.push_notification(note, cx);
            }
            StoreEvent::Progress { message } => {
                use gpui_kit::component::WindowExt as _;
                use gpui_kit::component::notification::{Notification, NotificationType};
                struct ProgressToast;
                window.remove_notification::<ProgressToast>(cx);
                if let Some(message) = message {
                    window.push_notification(
                        Notification::new()
                            .message(message.clone())
                            .id::<ProgressToast>()
                            .with_type(NotificationType::Info)
                            .autohide(false),
                        cx,
                    );
                }
            }
            StoreEvent::Browse { url } => {
                self.panel_open = true;
                self.panel
                    .update(cx, |p, cx| p.show(Tab::Browser, window, cx));
                if crate::host::IS_WEB {
                    if let Some(page) = self.panel.update(cx, |p, cx| p.page(window, cx)) {
                        page.update(cx, |v, cx| v.go(url, cx));
                    }
                } else if let Some(browser) = self.panel.update(cx, |p, cx| p.browser(window, cx)) {
                    browser.update(cx, |b, cx| b.navigate(url, cx));
                }
            }
            StoreEvent::EditQueued { thread_id, id } => {
                self.edit_queued(thread_id, *id, window, cx);
            }
            StoreEvent::Unsent { thread_id } => self.take_unsent(thread_id, window, cx),
            StoreEvent::ConfirmArchive { thread_id, reason } => {
                confirm_archive(
                    thread_id.clone(),
                    reason.clone(),
                    self.store.clone(),
                    window,
                    cx,
                );
            }
        }
        cx.notify();
    }

    fn apply_dev_tab(
        &mut self,
        dev: &str,
        thread: &Entity<ThreadState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (thread_id, agent) = {
            let state = thread.read(cx);
            let model = state.model.as_ref().expect("checked");
            let agent = model.items.iter().rev().find(|i| {
                matches!(&i.body, loft_protocol::ItemBody::ToolCall { tool_kind, .. }
                    if *tool_kind == loft_protocol::ToolKind::Subagent)
            });
            (model.thread.id.clone(), agent.map(|i| i.id.clone()))
        };
        self.panel_open = true;
        match dev.split_once(':') {
            // `all:<hook>++<hook>`: several hooks, in order.
            Some(("all", hooks)) => {
                for hook in hooks.split("++") {
                    self.apply_dev_tab(hook, thread, window, cx);
                }
            }
            // `do:<step> <step> …`: keys, typing and the pointer, in order
            // (`dev_script` in workspace/compose.rs lists the steps).
            Some(("do", script)) => {
                self.panel_open = false;
                self.dev_script(script, thread, window, cx);
            }
            Some(("type", text)) => {
                let composer = self.composer_for(thread, window, cx);
                let text = text.to_owned();
                let end = text.len();
                composer.update(cx, |c, cx| {
                    c.set_value(text, window, cx);
                    c.focus(window, cx);
                    c.set_selected_range(end..end, cx);
                });
                // Once the thread's commands are in.
                let tid = thread_id.clone();
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(2000))
                        .await;
                    let _ =
                        this.update(cx, |this, cx| this.refresh_completion(&tid, &composer, cx));
                })
                .detach();
            }
            Some(("viewclick", spec)) => {
                // `x,y[,times[,gap_ms[,after_ms]]]`: click in one of the
                // thread's views, x and y from its top left. Goes through
                // the window's own event handling, so it tests what a real
                // click does without the system's leave to post one.
                let n: Vec<f32> = spec
                    .split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect();
                let at = |i: usize, or: f32| n.get(i).copied().unwrap_or(or);
                let (x, y, times) = (at(0, 10.), at(1, 10.), at(2, 1.) as usize);
                let gap = std::time::Duration::from_millis(at(3, 150.) as u64);
                let after = std::time::Duration::from_millis(at(4, 3000.) as u64);
                let thread = thread.clone();
                // As the user has it: no side panel taking room from the chat.
                self.panel_open = false;
                cx.spawn_in(window, async move |_, cx| {
                    use gpui_kit::{
                        Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
                        PlatformInput, point,
                    };
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1500))
                        .await;
                    // The thread's last view, scrolled into sight.
                    let _ = cx.update(|_, cx| {
                        thread.update(cx, |state, cx| {
                            if let Some(item) = state.last_view_item() {
                                state.reveal(item, cx);
                            }
                        })
                    });
                    cx.background_executor().timer(after).await;
                    for i in 0..times {
                        let Ok(Some(bounds)) = cx.update(|_, cx| thread.read(cx).a_view_bounds(cx))
                        else {
                            tracing::info!("dev: no view to click in");
                            return;
                        };
                        let position = bounds.origin + point(px(x), px(y));
                        let (button, modifiers) = (MouseButton::Left, Modifiers::default());
                        let steps = [
                            PlatformInput::MouseMove(MouseMoveEvent {
                                position,
                                pressed_button: None,
                                modifiers,
                            }),
                            PlatformInput::MouseDown(MouseDownEvent {
                                button,
                                position,
                                modifiers,
                                click_count: 1,
                                first_mouse: false,
                            }),
                            PlatformInput::MouseUp(MouseUpEvent {
                                button,
                                position,
                                modifiers,
                                click_count: 1,
                            }),
                        ];
                        for step in steps {
                            let _ = cx.update(|window, cx| {
                                window.dispatch_event(step, cx);
                            });
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(40))
                                .await;
                        }
                        tracing::info!(click = i + 1, "dev: clicked in a view");
                        cx.background_executor().timer(gap).await;
                    }
                })
                .detach();
            }
            Some(("click", spec)) => {
                self.panel_open = false;
                // Several, one after another: `x,y,after_ms;x,y,after_ms`; a step
                // `move x,y,after_ms` only moves the pointer.
                for click in spec.split(';') {
                    dev_click(click.to_owned(), window, cx);
                }
            }
            Some(("hover", spec)) => {
                // `x,y[,after_ms]`: the pointer comes to rest at window
                // coordinates (hover cards and tooltips, for a screenshot).
                let n: Vec<f32> = spec
                    .split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect();
                let at = |i: usize, or: f32| n.get(i).copied().unwrap_or(or);
                let position = gpui_kit::point(px(at(0, 10.)), px(at(1, 10.)));
                let after = std::time::Duration::from_millis(at(2, 2500.) as u64);
                cx.spawn_in(window, async move |_, cx| {
                    cx.background_executor().timer(after).await;
                    let _ = cx.update(|window, cx| {
                        window.dispatch_event(
                            gpui_kit::PlatformInput::MouseMove(gpui_kit::MouseMoveEvent {
                                position,
                                pressed_button: None,
                                modifiers: gpui_kit::Modifiers::default(),
                            }),
                            cx,
                        );
                    });
                })
                .detach();
            }
            Some(("wheel", spec)) => {
                // `x,y,dy[,after_ms]`: one wheel turn of `dy` pixels at
                // window coordinates, as a trackpad sends it, with the
                // details card open (its lists scroll).
                self.panel_open = false;
                self.details_open = true;
                let n: Vec<f32> = spec
                    .split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect();
                let at = |i: usize, or: f32| n.get(i).copied().unwrap_or(or);
                let position = gpui_kit::point(px(at(0, 10.)), px(at(1, 10.)));
                let dy = at(2, -60.);
                let after = std::time::Duration::from_millis(at(3, 2500.) as u64);
                cx.spawn_in(window, async move |_, cx| {
                    cx.background_executor().timer(after).await;
                    let _ = cx.update(|window, cx| {
                        window.dispatch_event(
                            gpui_kit::PlatformInput::MouseMove(gpui_kit::MouseMoveEvent {
                                position,
                                pressed_button: None,
                                modifiers: gpui_kit::Modifiers::default(),
                            }),
                            cx,
                        );
                        window.dispatch_event(
                            gpui_kit::PlatformInput::Scroll(gpui_kit::ScrollEvent {
                                position,
                                delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
                                    px(0.),
                                    px(dy),
                                )),
                                ..Default::default()
                            }),
                            cx,
                        );
                    });
                })
                .detach();
            }
            Some(("viewkeys", spec)) => {
                // `x,y,after_ms`: click a field of the thread's view, type
                // "abc", press Cmd+A, type "z" (so the field holds "z" if
                // the shortcut reached the page), all as the system sends
                // them.
                let n: Vec<f32> = spec
                    .split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect();
                let at = |i: usize, or: f32| n.get(i).copied().unwrap_or(or);
                let (x, y) = (at(0, 10.), at(1, 10.));
                let after = std::time::Duration::from_millis(at(2, 3000.) as u64);
                let thread = thread.clone();
                self.panel_open = false;
                cx.spawn_in(window, async move |_, cx| {
                    use gpui_kit::{Modifiers, MouseMoveEvent, PlatformInput, point};
                    let executor = cx.background_executor().clone();
                    let pause = |ms: u64| executor.timer(std::time::Duration::from_millis(ms));
                    pause(1500).await;
                    let _ = cx.update(|_, cx| {
                        thread.update(cx, |state, cx| {
                            if let Some(item) = state.last_view_item() {
                                state.reveal(item, cx);
                            }
                        })
                    });
                    executor.timer(after).await;
                    let Ok(Some((embed, bounds))) = cx.update(|_, cx| {
                        let state = thread.read(cx);
                        state.a_view_embed().zip(state.a_view_bounds(cx))
                    }) else {
                        tracing::info!("dev: no native view to type in");
                        return;
                    };
                    let _ = cx.update(|window, cx| {
                        window.dispatch_event(
                            PlatformInput::MouseMove(MouseMoveEvent {
                                position: bounds.origin + point(px(x), px(y)),
                                pressed_button: None,
                                modifiers: Modifiers::default(),
                            }),
                            cx,
                        );
                    });
                    pause(500).await;
                    let host =
                        |cx: &mut gpui_kit::AsyncWindowContext,
                         act: &dyn Fn(&loft_browser::ViewHost)| {
                            let _ = cx.update(|_, cx| {
                                if let Some(host) = embed.read(cx).host() {
                                    act(host);
                                }
                            });
                        };
                    host(cx, &|h| h.click_at(x, y));
                    pause(500).await;
                    host(cx, &|h| h.type_keys("abc", false));
                    pause(500).await;
                    host(cx, &|h| h.type_keys("a", true));
                    pause(500).await;
                    host(cx, &|h| h.type_keys("z", false));
                    tracing::info!("dev: typed abc, Cmd+A, z in a native view");
                })
                .detach();
            }
            Some(("viewnative", spec)) => {
                // `x,y[,times[,gap_ms[,after_ms]]]`: real clicks in a view
                // that is a page of the app's own, through the window as
                // the system sends them (the pointer moved onto it first,
                // which is what makes the page live).
                let n: Vec<f32> = spec
                    .split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect();
                let at = |i: usize, or: f32| n.get(i).copied().unwrap_or(or);
                let (x, y, times) = (at(0, 10.), at(1, 10.), at(2, 1.) as usize);
                let gap = std::time::Duration::from_millis(at(3, 150.) as u64);
                let after = std::time::Duration::from_millis(at(4, 3000.) as u64);
                // `…,wheel_lines,back(0|1),away(0|1),return_after_ms`.
                let (wheel, back, away) = (at(5, 0.) as i32, at(6, 0.) > 0., at(7, 0.) > 0.);
                let return_after = at(8, 0.) as u64;
                // `…,cold(0|1)`: click the picture, without the pointer
                // having come onto the view first.
                let cold = at(9, 0.) > 0.;
                // `…,again(0|1)`: a while after the pointer left, it comes
                // onto the view once more.
                let again = at(10, 0.) > 0.;
                let thread = thread.clone();
                self.panel_open = false;
                cx.spawn_in(window, async move |_, cx| {
                    use gpui_kit::{Modifiers, MouseMoveEvent, PlatformInput, point};
                    let executor = cx.background_executor().clone();
                    let pause = |ms: u64| executor.timer(std::time::Duration::from_millis(ms));
                    pause(1500).await;
                    let _ = cx.update(|_, cx| {
                        thread.update(cx, |state, cx| {
                            if let Some(item) = state.last_view_item() {
                                state.reveal(item, cx);
                            }
                        })
                    });
                    executor.timer(after).await;
                    let Ok(Some((embed, bounds))) = cx.update(|_, cx| {
                        let state = thread.read(cx);
                        state.a_view_embed().zip(state.a_view_bounds(cx))
                    }) else {
                        tracing::info!("dev: no native view to click in");
                        return;
                    };
                    if !cold {
                        let _ = cx.update(|window, cx| {
                            window.dispatch_event(
                                PlatformInput::MouseMove(MouseMoveEvent {
                                    position: bounds.origin + point(px(x), px(y)),
                                    pressed_button: None,
                                    modifiers: Modifiers::default(),
                                }),
                                cx,
                            );
                        });
                    }
                    pause(500).await;
                    for i in 0..times {
                        let _ = cx.update(|_, cx| {
                            if let Some(host) = embed.read(cx).host() {
                                if cold {
                                    host.click_in_window(bounds.origin + point(px(x), px(y)));
                                } else {
                                    host.click_at(x, y);
                                }
                            }
                        });
                        tracing::info!(click = i + 1, "dev: clicked in a native view");
                        executor.timer(gap).await;
                    }
                    // Then, if asked: the wheel over it, the pointer back on
                    // it, the pointer away from it.
                    if wheel != 0 {
                        let _ = cx.update(|_, cx| {
                            if let Some(host) = embed.read(cx).host() {
                                host.wheel_at(x, y, wheel);
                            }
                        });
                        tracing::info!(wheel, "dev: turned the wheel over a native view");
                        pause(900).await;
                    }
                    let move_to = |position| {
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position,
                            pressed_button: None,
                            modifiers: Modifiers::default(),
                        })
                    };
                    if back {
                        let at = cx
                            .update(|_, cx| thread.read(cx).a_view_bounds(cx))
                            .ok()
                            .flatten()
                            .map(|b| b.origin + point(px(x), px(40.)));
                        if let Some(at) = at {
                            let _ = cx.update(|window, cx| {
                                window.dispatch_event(move_to(at), cx);
                            });
                            tracing::info!("dev: the pointer is back on the view");
                            pause(900).await;
                        }
                    }
                    if away {
                        let _ = cx.update(|window, cx| {
                            window.dispatch_event(move_to(point(px(600.), px(20.))), cx);
                        });
                        tracing::info!("dev: the pointer left the view");
                    }
                    if again {
                        pause(1500).await;
                        let at = cx
                            .update(|_, cx| thread.read(cx).a_view_bounds(cx))
                            .ok()
                            .flatten()
                            .map(|b| b.origin + point(px(x), px(y)));
                        if let Some(at) = at {
                            let _ = cx.update(|window, cx| {
                                window.dispatch_event(move_to(at), cx);
                            });
                            tracing::info!("dev: the pointer came onto the view again");
                        }
                    }
                    // After a while out of sight, back to the view.
                    if return_after > 0 {
                        pause(return_after).await;
                        let _ = cx.update(|_, cx| {
                            thread.update(cx, |state, cx| {
                                if let Some(item) = state.last_view_item() {
                                    state.reveal(item, cx);
                                }
                            })
                        });
                        tracing::info!("dev: scrolled back to the view");
                    }
                })
                .detach();
            }
            Some(("addproject", text)) => {
                let text = text.to_owned();
                cx.defer_in(window, move |this, window, cx| {
                    let picker = this.add_project(window, cx);
                    picker.update(cx, |p, cx| p.type_text(&text, window, cx));
                });
            }
            Some(("automations", what)) => {
                // `automations:list|new|edit`: the Automations page, its
                // editor on a new one or on the first.
                self.panel_open = false;
                self.store.update(cx, |s, _| s.automations_open = true);
                let store = self.store.clone();
                let page = self
                    .automations_page
                    .get_or_insert_with(|| {
                        cx.new(|_| crate::automations::AutomationsPage::new(store))
                    })
                    .clone();
                // `automations:new;click:x,y`: then a click in the window.
                let (what, click) = match what.split_once(";click:") {
                    Some((w, c)) => (w.to_owned(), Some(c.to_owned())),
                    None => (what.to_owned(), None),
                };
                page.update(cx, |page, cx| page.dev_open(&what, window, cx));
                if let Some(spec) = click {
                    dev_click(spec, window, cx);
                }
            }
            Some(("settings", section)) => {
                // `settings:<section>@end`: scrolled to its end.
                let section = match section.strip_suffix("@end") {
                    Some(section) => {
                        crate::settings::dev_scroll_to_end();
                        section
                    }
                    None => section,
                };
                let section = crate::settings::Section::parse(section);
                crate::settings::open(self.store.clone(), section, cx);
                // `AGENTLOFT_DEV_CLICK=x,y[,after_ms]`: then a click in it.
                if let Ok(spec) = std::env::var("AGENTLOFT_DEV_CLICK") {
                    dev_click(spec, window, cx);
                }
            }
            Some(("attach", path)) => {
                let paths = vec![std::path::PathBuf::from(path)];
                self.attach_paths(thread_id.clone(), paths, cx);
            }
            Some(("fork", harness)) => {
                let (t, h) = (thread_id.clone(), HarnessId(harness.into()));
                self.store.update(cx, |s, cx| s.fork_thread(t, h, None, cx));
            }
            Some(("annotate", url)) => {
                self.panel
                    .update(cx, |p, cx| p.show(Tab::Browser, window, cx));
                if let Some(browser) = self.panel.update(cx, |p, cx| p.browser(window, cx)) {
                    browser.update(cx, |b, cx| {
                        b.navigate(url, cx);
                        b.set_annotating(true, cx);
                    });
                }
            }
            Some(("panes", n)) => {
                // The n most recent threads, side by side (tests).
                self.panel_open = false;
                let n: usize = n.parse().unwrap_or(2);
                let others: Vec<ThreadId> = self
                    .store
                    .read(cx)
                    .workspace
                    .threads
                    .iter()
                    .filter(|t| {
                        t.id != thread_id && t.status != ThreadStatus::Archived && t.is_started()
                    })
                    .take(n.saturating_sub(1))
                    .map(|t| t.id.clone())
                    .collect();
                self.store.update(cx, |s, cx| {
                    for id in &others {
                        s.ensure_thread(id, cx);
                    }
                    // Side by side, then stacked under the first ones.
                    let all: Vec<ThreadId> =
                        std::iter::once(thread_id.clone()).chain(others).collect();
                    let cols = all.len().div_ceil(2).max(1);
                    let mut layout: Vec<Vec<ThreadId>> = vec![Vec::new(); cols.min(all.len())];
                    for (i, id) in all.into_iter().enumerate() {
                        let n = layout.len();
                        layout[i % n].push(id);
                    }
                    s.layout = layout;
                    cx.notify();
                });
            }
            Some(("card", which)) => {
                self.panel_open = false;
                let kind = match which {
                    "usage" => loft_protocol::AgentInfoKind::Usage,
                    "status" => loft_protocol::AgentInfoKind::Status,
                    _ => loft_protocol::AgentInfoKind::Context,
                };
                let client = self.store.read(cx).client();
                thread.update(cx, |t, cx| {
                    t.add_card(kind, client, cx);
                    // Unfold every section (tests).
                    let ix = t.cards.len() - 1;
                    t.card_ui(
                        ix,
                        |c| c.expanded = vec!["messages", "mcp", "memory", "skills", "agents"],
                        cx,
                    );
                });
            }
            Some(("peek", n)) => {
                self.panel_open = false;
                self.peek = n.parse().ok().map(|n| (thread_id.clone(), n));
            }
            Some(("sendstop", text)) => {
                // Send, then stop whatever subagents are running (tests).
                self.panel_open = false;
                self.details_open = true;
                let id = thread_id.clone();
                let text = text.to_owned();
                self.store
                    .update(cx, |s, cx| s.send_message(id.clone(), text, Vec::new(), cx));
                let (store, th) = (self.store.clone(), thread.clone());
                cx.spawn(async move |_, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(25))
                        .await;
                    cx.update(|cx| {
                        let running: Vec<_> = th
                            .read(cx)
                            .model
                            .as_ref()
                            .map(|m| {
                                m.items
                                    .iter()
                                    .filter(|i| {
                                        i.parent_id.is_none()
                                            && crate::items::subagent_status(m, i).running
                                            && matches!(&i.body, loft_protocol::ItemBody::ToolCall { tool_kind, .. }
                                                if *tool_kind == loft_protocol::ToolKind::Subagent)
                                    })
                                    .map(|i| i.id.clone())
                                    .collect()
                            })
                            .unwrap_or_default();
                        for item in running {
                            store.update(cx, |s, cx| s.stop_subagent(id.clone(), item, cx));
                        }
                    });
                })
                .detach();
            }
            Some(("send", text)) => {
                self.panel_open = false;
                let id = thread_id.clone();
                let text = text.to_owned();
                self.store
                    .update(cx, |s, cx| s.send_message(id, text, Vec::new(), cx));
            }
            // `queue:first|second|…` queues each.
            Some(("queue", text)) => {
                self.panel_open = false;
                for part in text.split('|') {
                    let queued = crate::store::Outgoing::new(part.to_owned(), Vec::new());
                    let id = thread_id.clone();
                    self.store.update(cx, |s, cx| s.enqueue(id, queued, cx));
                }
            }
            Some(("file", path)) => {
                let path = path.to_owned();
                self.panel
                    .update(cx, |p, cx| p.open_file(&thread_id, path, window, cx));
            }
            Some(("docnote", spec)) => {
                // `path|x,y;x,y|note[|send]`: open a document, turn notes on,
                // click each point (window coordinates) and leave the note
                // there; with `send`, send the message after.
                let mut parts = spec.split('|');
                let path = parts.next().unwrap_or_default().to_owned();
                let points: Vec<(f32, f32)> = parts
                    .next()
                    .unwrap_or_default()
                    .split(';')
                    .filter_map(|p| {
                        let (x, y) = p.split_once(',')?;
                        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
                    })
                    .collect();
                let note = parts.next().unwrap_or("A note").to_owned();
                let send = parts.next() == Some("send");
                self.panel.update(cx, |p, cx| {
                    p.open_file(&thread_id, path.clone(), window, cx)
                });
                let (tid, thread) = (thread_id.clone(), thread.clone());
                cx.spawn_in(window, async move |this, cx| {
                    let executor = cx.background_executor().clone();
                    let pause = |ms: u64| executor.timer(std::time::Duration::from_millis(ms));
                    let doc = |this: &Self, cx: &gpui_kit::App| {
                        this.panel.read(cx).doc_preview(&tid, &path, cx)
                    };
                    // The file is read through the daemon, then parsed.
                    pause(2500).await;
                    let _ = this.update(cx, |this, cx| {
                        if let Some(doc) = doc(this, cx) {
                            doc.update(cx, |d, cx| d.set_annotating(true, cx));
                        }
                    });
                    pause(600).await;
                    for (x, y) in points {
                        use gpui_kit::{
                            Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput,
                        };
                        let position = gpui_kit::point(px(x), px(y));
                        let (button, modifiers) = (MouseButton::Left, Modifiers::default());
                        let steps = [
                            PlatformInput::MouseDown(MouseDownEvent {
                                button,
                                position,
                                modifiers,
                                click_count: 1,
                                first_mouse: false,
                            }),
                            PlatformInput::MouseUp(MouseUpEvent {
                                button,
                                position,
                                modifiers,
                                click_count: 1,
                            }),
                        ];
                        for step in steps {
                            let _ = cx.update(|window, cx| {
                                window.dispatch_event(step, cx);
                            });
                            pause(60).await;
                        }
                        pause(300).await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            if let Some(doc) = doc(this, cx) {
                                doc.update(cx, |d, cx| d.finish_note(&note, window, cx));
                            }
                        });
                        pause(300).await;
                    }
                    tracing::info!("dev: notes left on a document");
                    if send {
                        // A PDF's page pictures are drawn first.
                        pause(2500).await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            let composer = this.composer_for(&thread, window, cx);
                            this.submit(&tid, &composer, window, cx, false);
                        });
                    }
                })
                .detach();
            }
            // Run one of the project's actions, by name.
            Some(("action", name)) => {
                self.panel_open = false;
                self.run_action(thread_id.clone(), name.to_owned(), window, cx);
            }
            // The palette with a query typed, a moment after launch (once
            // what it lists for the thread, like its project's actions, is
            // in).
            Some(("palette", query)) => {
                self.panel_open = false;
                let query = query.to_owned();
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1500))
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        crate::palette::open_with(cx.entity(), &this.store, &query, window, cx)
                    });
                })
                .detach();
            }
            _ => match dev {
                "branches" => self.branch_picker_open = true,
                "work" => {
                    // Unfold every finished turn's work.
                    self.panel_open = false;
                    let turns: Vec<_> = thread
                        .read(cx)
                        .model
                        .as_ref()
                        .map(|m| m.turns.iter().map(|t| t.id.clone()).collect())
                        .unwrap_or_default();
                    thread.update(cx, |t, cx| {
                        for turn in &turns {
                            t.toggle_work(turn, cx);
                        }
                    });
                }
                "closesidebar" => {
                    // Shut the sidebar a moment after launch (animation tests).
                    self.panel_open = false;
                    cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(5))
                            .await;
                        let _ = this.update(cx, |this, cx| {
                            this.sidebar_open = false;
                            cx.notify();
                        });
                    })
                    .detach();
                }
                "maximize" => {
                    self.panel
                        .update(cx, |p, cx| p.show(Tab::Files, window, cx));
                    self.panel.update(cx, |p, cx| p.set_maximized(true, cx));
                }
                "details" => {
                    self.panel_open = false;
                    self.details_open = true;
                }
                "addproject" => {
                    self.panel_open = false;
                    cx.defer_in(window, |this, window, cx| {
                        this.add_project(window, cx);
                    });
                }
                "models" => {
                    self.panel_open = false;
                    let picker = self.model_picker_for(&thread_id, window, cx);
                    picker.update(cx, |p, cx| p.reset(window, cx));
                    self.picker_open = true;
                }
                "changes" => self
                    .panel
                    .update(cx, |p, cx| p.show(Tab::Changes, window, cx)),
                "terminal" => self
                    .panel
                    .update(cx, |p, cx| p.show(Tab::Terminal, window, cx)),
                // A shell in the side panel, and the panel shut a moment
                // later: what size the shell is told as the panel slides
                // away (`list_terminals` on the daemon's socket says).
                "terminalshut" => {
                    self.panel
                        .update(cx, |p, cx| p.show(Tab::Terminal, window, cx));
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(2500))
                            .await;
                        let _ = this.update(cx, |this, cx| {
                            this.panel_open = false;
                            cx.notify();
                        });
                    })
                    .detach();
                }
                "dock" => self.toggle_terminal(window, cx),
                "browser" => self
                    .panel
                    .update(cx, |p, cx| p.show(Tab::Browser, window, cx)),
                // Exercise the thread menu's actions (screenshots/tests).
                "merge" => {
                    let t = thread_id.clone();
                    self.store.update(cx, |s, cx| s.merge_thread(t, cx));
                }
                "archive" => {
                    let t = thread_id.clone();
                    self.store
                        .update(cx, |s, cx| s.archive_thread(t, false, cx));
                }
                "palette" => crate::palette::open(cx.entity(), &self.store, window, cx),
                "archived" => self.show_archived = true,
                "rename" => self.start_rename(thread_id.clone(), window, cx),
                "undo" => {
                    let last = thread
                        .read(cx)
                        .model
                        .as_ref()
                        .and_then(|m| Some((m.turns.last()?.id.clone(), 0)));
                    if let Some((turn, later)) = last {
                        crate::items::confirm_revert(
                            self.store.clone(),
                            thread_id.clone(),
                            turn,
                            later,
                            window,
                            cx,
                        );
                    }
                }
                "newtab" => {
                    // A second thread in a native tab next to this one.
                    let other = self
                        .store
                        .read(cx)
                        .workspace
                        .threads
                        .iter()
                        .filter(|t| t.id != thread_id && t.status != ThreadStatus::Archived)
                        .map(|t| t.id.clone())
                        .next();
                    if let Some(other) = other {
                        self.store.update(cx, |s, cx| s.open_pane(other, cx));
                    }
                }
                "agent" => {
                    if let Some(item) = agent {
                        self.panel
                            .update(cx, |p, cx| p.open_agent(&thread_id, item, window, cx));
                    }
                }
                _ => self
                    .panel
                    .update(cx, |p, cx| p.show(Tab::Files, window, cx)),
            },
        }
    }

    /// An agent drives the browser: show it, run the command, answer.
    fn browser_in_panel(&self, cx: &gpui_kit::App) -> bool {
        self.panel_open && self.panel.read(cx).active_tab(cx) == Some(Tab::Browser)
    }

    /// A note left on the page: it waits as a chip in the composer, and a
    /// picture of the page with its numbered pins is attached.
    fn add_annotation(
        &mut self,
        thread_id: &ThreadId,
        note: &loft_browser::Annotation,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_id = thread_id.clone();
        self.page_notes
            .entry(thread_id.clone())
            .or_default()
            .push(note.clone());
        cx.notify();
        let Some(browser) = self.panel.read(cx).existing_browser(&thread_id, cx) else {
            return;
        };
        let Some(task) = browser.update(cx, |b, cx| b.capture(cx)) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let Some(png) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                // Only while the notes are still waiting.
                if this.page_notes.get(&thread_id).is_none_or(|n| n.is_empty()) {
                    return;
                }
                let Some(image) = crate::attachments::PendingImage::png(PAGE_NOTES_IMAGE, png)
                else {
                    return;
                };
                let list = this.attachments.entry(thread_id.clone()).or_default();
                match list.iter().position(|a| a.name == PAGE_NOTES_IMAGE) {
                    Some(ix) => list[ix] = image,
                    None if list.len() < crate::attachments::MAX_IMAGES => list.push(image),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A note left on a document: it waits as a chip in the composer. For a
    /// PDF, a picture of each page with notes, pins drawn on, is attached.
    fn add_doc_note(
        &mut self,
        thread_id: &ThreadId,
        path: &str,
        note: &crate::panel::docs::DocNote,
        cx: &mut Context<Self>,
    ) {
        self.doc_notes
            .entry(thread_id.clone())
            .or_default()
            .push((path.to_owned(), note.clone()));
        self.refresh_doc_pictures(thread_id, path, cx);
        cx.notify();
    }

    /// The pictures attached for a PDF's notes, made again from the notes
    /// its preview still shows.
    fn refresh_doc_pictures(&mut self, thread_id: &ThreadId, path: &str, cx: &mut Context<Self>) {
        let prefix = doc_notes_image_prefix(path);
        let task = self
            .panel
            .read(cx)
            .doc_preview(thread_id, path, cx)
            .and_then(|doc| doc.read(cx).note_pictures(cx));
        let Some(task) = task else {
            // No notes on pages left (or not a PDF): no pictures either.
            if let Some(images) = self.attachments.get_mut(thread_id) {
                images.retain(|a| !a.name.starts_with(&prefix));
            }
            return;
        };
        let (thread_id, path) = (thread_id.clone(), path.to_owned());
        cx.spawn(async move |this, cx| {
            let pictures = task.await;
            let _ = this.update(cx, |this, cx| {
                // Only while notes on this file are still waiting.
                let waiting = this
                    .doc_notes
                    .get(&thread_id)
                    .is_some_and(|notes| notes.iter().any(|(p, _)| *p == path));
                let list = this.attachments.entry(thread_id.clone()).or_default();
                list.retain(|a| !a.name.starts_with(&prefix));
                if waiting {
                    for (page, png) in pictures {
                        if list.len() >= crate::attachments::MAX_IMAGES {
                            break;
                        }
                        let name = format!("{prefix}{page}.png");
                        if let Some(image) = crate::attachments::PendingImage::png(&name, png) {
                            list.push(image);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Chips for notes left on the page and on documents, each removable.
    fn note_chips(
        &self,
        thread_id: &ThreadId,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let page_notes = self
            .page_notes
            .get(thread_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let doc_notes = self
            .doc_notes
            .get(thread_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if page_notes.is_empty() && doc_notes.is_empty() {
            return None;
        }
        let chip = |id: String, n: u32, what: String, note: String, tooltip: String| {
            h_flex()
                .id(SharedString::from(id))
                .gap_1p5()
                .items_center()
                .max_w(px(300.))
                .pl_1()
                .pr_1()
                .py(px(3.))
                .rounded(px(8.))
                .border_1()
                .border_color(p.accent.opacity(0.35))
                .bg(p.accent.opacity(0.08))
                .text_xs()
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
                })
                .child(
                    div()
                        .flex_none()
                        .size(px(16.))
                        .rounded(px(8.))
                        .bg(p.accent)
                        .text_color(p.accent_foreground)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(10.))
                        .font_weight(FontWeight::BOLD)
                        .child(n.max(1).to_string()),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(px(120.))
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.foreground)
                        .child(what),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(p.muted_foreground)
                        .child(note),
                )
        };
        let remove = |id: String| {
            div()
                .id(SharedString::from(id))
                .flex_none()
                .cursor_pointer()
                .p(px(2.))
                .rounded(px(5.))
                .text_color(p.muted_foreground)
                .hover(|d| d.bg(p.selected).text_color(p.foreground))
                .child(Icon::new(IconName::X).xsmall())
        };
        let mut row = h_flex().gap_1p5().flex_wrap().px_3().pt_2();
        for (ix, note) in page_notes.iter().enumerate() {
            let tid = thread_id.clone();
            let what = if note.is_area() {
                "Area".to_owned()
            } else if note.label.is_empty() {
                match note.tag.as_str() {
                    "svg" => "Icon",
                    "img" | "picture" => "Image",
                    "video" => "Video",
                    "a" => "Link",
                    "button" => "Button",
                    "input" | "textarea" | "select" => "Field",
                    _ => "Element",
                }
                .to_owned()
            } else {
                note.label.chars().take(24).collect()
            };
            row = row.child(
                chip(
                    format!("note-{ix}"),
                    note.n,
                    what,
                    note.note.clone(),
                    describe_note(note),
                )
                .child(
                    remove(format!("note-remove-{ix}"))
                        .claims_press()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.remove_note(&tid, ix, cx);
                        })),
                ),
            );
        }
        for (ix, (path, note)) in doc_notes.iter().enumerate() {
            let tid = thread_id.clone();
            let file = path.rsplit('/').next().unwrap_or(path);
            row = row.child(
                chip(
                    format!("doc-note-{ix}"),
                    note.n,
                    note.chip.clone(),
                    note.note.clone(),
                    format!("{file}: {}", note.what),
                )
                .child(
                    remove(format!("doc-note-remove-{ix}"))
                        .claims_press()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.remove_doc_note(&tid, ix, cx);
                        })),
                ),
            );
        }
        Some(row.into_any_element())
    }

    fn remove_doc_note(&mut self, thread_id: &ThreadId, ix: usize, cx: &mut Context<Self>) {
        let Some(list) = self.doc_notes.get_mut(thread_id) else {
            return;
        };
        if ix >= list.len() {
            return;
        }
        let (path, note) = list.remove(ix);
        // Its pin leaves the document, and its page's picture is made again.
        if let Some(doc) = self.panel.read(cx).doc_preview(thread_id, &path, cx) {
            doc.update(cx, |doc, cx| doc.remove_note(note.n, cx));
        }
        self.refresh_doc_pictures(thread_id, &path, cx);
        cx.notify();
    }

    fn remove_note(&mut self, thread_id: &ThreadId, ix: usize, cx: &mut Context<Self>) {
        if let Some(list) = self.page_notes.get_mut(thread_id)
            && ix < list.len()
        {
            let note = list.remove(ix);
            let empty = list.is_empty();
            if empty {
                // No notes left: the picture of them goes too.
                if let Some(images) = self.attachments.get_mut(thread_id) {
                    images.retain(|a| a.name != PAGE_NOTES_IMAGE);
                }
            }
            // And its pin leaves the page.
            if let Some(browser) = self.panel.read(cx).existing_browser(thread_id, cx) {
                browser.update(cx, |b, _| {
                    if empty {
                        b.clear_annotations();
                    } else {
                        b.remove_annotation(note.n);
                    }
                });
            }
        }
        cx.notify();
    }

    fn run_browser_call(
        &mut self,
        call_id: u64,
        thread_id: ThreadId,
        command: serde_json::Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let command: loft_browser::BrowserCommand = match serde_json::from_value(command) {
            Ok(c) => c,
            Err(err) => {
                client.respond_call(
                    call_id,
                    Err(loft_protocol::ErrorBody::new(
                        loft_protocol::ErrorCode::BadRequest,
                        format!("invalid browser command: {err}"),
                    )),
                );
                return;
            }
        };
        // Let the user watch: in the panel if it shows the browser, else in
        // a floating mini preview.
        if !self.browser_in_panel(cx) {
            self.browser_preview = true;
        }
        let Some(browser) = self
            .panel
            .update(cx, |p, cx| p.browser_for(&thread_id, window, cx))
        else {
            client.respond_call(
                call_id,
                Err(loft_protocol::ErrorBody::new(
                    loft_protocol::ErrorCode::NotFound,
                    "the thread isn't open in this window",
                )),
            );
            return;
        };
        let task = browser.update(cx, |b, cx| b.execute(command, window, cx));
        cx.spawn(async move |_, _| {
            let result = task.await.map_err(|err| {
                loft_protocol::ErrorBody::new(
                    loft_protocol::ErrorCode::Internal,
                    format!("{err:#}"),
                )
            });
            client.respond_call(call_id, result);
        })
        .detach();
        cx.notify();
    }

    /// Whether the conversation area is wide enough to dock the details card.
    fn can_dock(&self) -> bool {
        self.details_room.get() >= DOCK_MIN
    }

    /// The conversation, with the details card docked beside it when open
    /// and there is room.
    fn with_details_dock(
        &mut self,
        body: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.store.read(cx).selected.clone();
        let amount = self.details_amount;
        let float = self.details_inset > 0.;
        let dock = match (&selected, amount > 0.) {
            (Some(id), true) => {
                if self
                    .details_card
                    .as_ref()
                    .is_none_or(|c| c.read(cx).thread_id() != id)
                {
                    self.details_card = crate::details::docked(
                        window,
                        cx.weak_entity(),
                        self.store.clone(),
                        id.clone(),
                        cx,
                    );
                }
                self.details_card.clone()
            }
            _ => None,
        };
        let room = self.details_room.clone();
        h_flex()
            .size_full()
            .relative()
            .child(
                gpui_kit::canvas(
                    move |bounds, window, _| {
                        let width = f32::from(bounds.size.width);
                        let before = room.get();
                        room.set(width);
                        // Crossing the line docks or undocks the card.
                        if (before >= DOCK_MIN) != (width >= DOCK_MIN) {
                            window.refresh();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(body),
            )
            .children(dock.map(|card| {
                div()
                    .when(float, |d| d.absolute().top_0().right_0())
                    .flex_none()
                    .w(px(DOCK_WIDTH * amount))
                    .h_full()
                    .overflow_hidden()
                    .child(
                        // As tall as the conversation at most: a long card scrolls.
                        v_flex()
                            .w(px(DOCK_WIDTH))
                            .h_full()
                            .pt_3()
                            .pr_3()
                            .pb_3()
                            .child(card),
                    )
                    .into_any_element()
            }))
            .into_any_element()
    }

    /// The conversation with the terminal dock under it (sliding in and
    /// out, then resizable by its top edge).
    fn with_terminal_dock(
        &mut self,
        body: AnyElement,
        show: bool,
        p: &Palette,
        window: &mut Window,
    ) -> AnyElement {
        let amount = self.dock_slide.update(show, window);
        if amount <= 0. {
            return body;
        }
        let height = self.dock_height.get();
        if amount < 1. {
            // Mid-slide: the dock at its full height, partly shown (so the
            // shell isn't resized every frame).
            return v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().w_full().child(body))
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .h(px(height * amount))
                        .overflow_hidden()
                        .border_t_1()
                        .border_color(p.border)
                        .child(div().w_full().h(px(height)).child(self.dock.clone())),
                )
                .into_any_element();
        }
        let measured = self.dock_height.clone();
        div()
            .size_full()
            .child(
                v_resizable("terminal-dock")
                    .child(resizable_panel().child(body))
                    .child(
                        resizable_panel()
                            .size(px(height))
                            .size_range(px(DOCK_MIN_HEIGHT)..px(2000.))
                            .child(
                                div()
                                    .size_full()
                                    .relative()
                                    .border_t_1()
                                    .border_color(p.border)
                                    .child(self.dock.clone())
                                    .child(
                                        gpui_kit::canvas(
                                            move |bounds, _, _| {
                                                measured.set(f32::from(bounds.size.height))
                                            },
                                            |_, _, _, _| {},
                                        )
                                        .absolute()
                                        .size_full(),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Show a panel tab (never closes the panel, unlike [`Self::show_tab`]).
    pub(crate) fn open_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.panel_open = true;
        self.panel
            .update(cx, |panel, cx| panel.show(tab, window, cx));
        cx.notify();
    }

    pub(crate) fn show_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let already = self.panel_open && self.panel.read(cx).active_tab(cx).as_ref() == Some(&tab);
        if already {
            self.panel_open = false;
        } else {
            self.panel_open = true;
            self.panel
                .update(cx, |panel, cx| panel.show(tab, window, cx));
        }
        cx.notify();
    }

    /// Ctrl+`: open the terminal dock (with a shell) and focus it; when
    /// open, focus it, or hide it if it has focus already.
    pub(crate) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).selected.is_none() {
            return;
        }
        if self.dock_open && self.panel.read(cx).docked_focused(window, cx) {
            self.hide_dock(window, cx);
            return;
        }
        self.dock_open = true;
        self.panel
            .update(cx, |p, cx| p.ensure_docked(true, window, cx));
        cx.notify();
    }

    /// Hide the terminal dock, handing focus back to the composer.
    fn hide_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let had_focus = self.panel.read(cx).docked_focused(window, cx);
        self.dock_open = false;
        if had_focus
            && let Some(id) = self.store.read(cx).selected.clone()
            && let Some(composer) = self.composers.get(&id)
        {
            composer.update(cx, |c, cx| c.focus(window, cx));
        }
        cx.notify();
    }

    fn composer_for(
        &mut self,
        thread: &Entity<ThreadState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        let id = thread.read(cx).id.clone();
        if !self.observed.contains_key(&id) {
            let sub = cx.observe(thread, |_, _, cx| cx.notify());
            self.observed.insert(id.clone(), sub);
        }
        let harness_now = thread
            .read(cx)
            .model
            .as_ref()
            .map(|m| m.thread.harness.clone());
        if let Some(c) = self.composers.get(&id).cloned() {
            // Keep the placeholder in sync after a harness switch.
            if let Some(h) = harness_now
                && self.composer_harness.get(&id) != Some(&h)
            {
                c.update(cx, |c, cx| {
                    c.set_placeholder(placeholder_for(&h), window, cx)
                });
                self.composer_harness.insert(id.clone(), h);
            }
            return c;
        }
        let thread_harness = thread
            .read(cx)
            .model
            .as_ref()
            .map(|m| m.thread.harness.clone())
            .unwrap_or_else(HarnessId::claude);
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 12)
                .submit_on_enter(true)
                .placeholder(placeholder_for(&thread_harness))
        });
        let thread_id = id.clone();
        let sub =
            cx.subscribe_in(
                &composer,
                window,
                move |this, state, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => {
                        this.submit(&thread_id, state, window, cx, false);
                    }
                    InputEvent::Change => {
                        // (A prompt recalled with ↑ is not being typed:
                        // nothing to complete, and ↑↓ stay recall's.)
                        if !this.shows_recalled(&thread_id, state, cx) {
                            this.refresh_completion(&thread_id, state, cx);
                        }
                        this.queue_draft_save(&thread_id, state, cx);
                        // The sidebar lists a new thread once something is
                        // typed for it (and stops when that is deleted).
                        let typed = !state.read(cx).value().trim().is_empty();
                        let was = if typed {
                            !this.typed.insert(thread_id.clone())
                        } else {
                            this.typed.remove(&thread_id)
                        };
                        if was != typed {
                            cx.notify();
                        }
                    }
                    InputEvent::Blur if this.completion.take().is_some() => cx.notify(),
                    _ => {}
                },
            );
        self._subs.push(sub);
        // A fork's handoff digest, for the user to review and send.
        if let Some(draft) = self.store.update(cx, |s, _| s.drafts.remove(&id)) {
            composer.update(cx, |c, cx| {
                c.set_value(draft, window, cx);
                c.focus(window, cx);
            });
        }
        self.composer_harness.insert(id.clone(), thread_harness);
        self.composers.insert(id, composer.clone());
        composer
    }

    /// Save the composer's text as the thread's draft once typing pauses.
    fn queue_draft_save(
        &mut self,
        thread_id: &ThreadId,
        composer: &Entity<TextareaState>,
        cx: &mut Context<Self>,
    ) {
        let id = thread_id.clone();
        let composer = composer.downgrade();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.draft_saves.remove(&id);
                let Some(composer) = composer.upgrade() else {
                    return;
                };
                let text = composer.read(cx).value().to_string();
                let saved = this
                    .store
                    .read(cx)
                    .workspace
                    .threads
                    .iter()
                    .find(|t| t.id == id)
                    .and_then(|t| t.draft.clone())
                    .unwrap_or_default();
                if text.trim() != saved.trim() {
                    this.store
                        .update(cx, |s, cx| s.set_draft(id.clone(), text, cx));
                }
            });
        });
        // Replacing the task cancels the previous wait.
        self.draft_saves.insert(thread_id.clone(), task);
    }

    /// ⌘↩ in the composer: with nothing typed and an approval open, approve
    /// it; otherwise send what's typed into the running turn right away.
    fn steer_or_approve(
        &mut self,
        thread_id: &ThreadId,
        composer: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let empty = composer.read(cx).value().trim().is_empty()
            && self.attachments.get(thread_id).is_none_or(|a| a.is_empty())
            && self.pasted.get(thread_id).is_none_or(|p| p.is_empty())
            && self.page_notes.get(thread_id).is_none_or(|n| n.is_empty())
            && self.doc_notes.get(thread_id).is_none_or(|n| n.is_empty());
        if empty && self.open_request(cx).is_some() {
            self.answer(loft_protocol::DecisionKind::AllowOnce, cx);
            return;
        }
        self.submit(thread_id, composer, window, cx, true);
    }

    /// Send what's typed. While the agent works it waits in the queue, or
    /// with `steer` goes into the running turn now.
    fn submit(
        &mut self,
        thread_id: &ThreadId,
        composer: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
        steer: bool,
    ) {
        let typed = composer.read(cx).value().trim().to_owned();
        let has_images = self
            .attachments
            .get(thread_id)
            .is_some_and(|a| !a.is_empty());
        let pasted = self.pasted.get(thread_id).cloned().unwrap_or_default();
        let notes = self.page_notes.get(thread_id).cloned().unwrap_or_default();
        let doc_notes = self.doc_notes.get(thread_id).cloned().unwrap_or_default();
        // Reports shown as cards, not sent: Claude's `/context` and
        // `/usage`, Codex's `/status`.
        let harness = self
            .store
            .read(cx)
            .workspace
            .threads
            .iter()
            .find(|t| &t.id == thread_id)
            .map(|t| t.harness.as_str().to_owned())
            .unwrap_or_default();
        let report = {
            use loft_protocol::{AgentInfoKind, HarnessId};
            match (harness.as_str(), typed.as_str()) {
                (HarnessId::CLAUDE, "/context") => Some(AgentInfoKind::Context),
                (HarnessId::CLAUDE, "/usage" | "/cost") => Some(AgentInfoKind::Usage),
                (HarnessId::CODEX, "/status" | "/usage") => Some(AgentInfoKind::Status),
                _ => None,
            }
        };
        // Codex commands Agentloft runs itself.
        let no_notes = notes.is_empty() && doc_notes.is_empty();
        if harness == loft_protocol::HarnessId::CODEX && pasted.is_empty() && no_notes {
            let done = match typed.as_str() {
                "/model" => {
                    let picker = self.model_picker_for(thread_id, window, cx);
                    picker.update(cx, |p, cx| p.reset(window, cx));
                    self.picker_open = true;
                    true
                }
                "/diff" => {
                    self.open_tab(Tab::Changes, window, cx);
                    true
                }
                "/new" => {
                    self.new_thread(None, cx);
                    true
                }
                _ => false,
            };
            if done {
                composer.update(cx, |c, cx| c.set_value("", window, cx));
                cx.notify();
                return;
            }
        }
        if let Some(kind) = report.filter(|_| pasted.is_empty() && no_notes)
            && let Some(thread) = self.store.read(cx).threads.get(thread_id).cloned()
        {
            composer.update(cx, |c, cx| c.set_value("", window, cx));
            let client = self.store.read(cx).client();
            thread.update(cx, |t, cx| t.add_card(kind, client, cx));
            return;
        }
        if typed.is_empty() && !has_images && pasted.is_empty() && no_notes {
            return;
        }
        self.pasted.remove(thread_id);
        if !notes.is_empty() {
            self.page_notes.remove(thread_id);
            // Sent: take the pins off the page and leave annotation mode.
            if let Some(browser) = self.panel.read(cx).existing_browser(thread_id, cx) {
                browser.update(cx, |b, cx| {
                    b.clear_annotations();
                    b.set_annotating(false, cx);
                });
            }
        }
        if !doc_notes.is_empty() {
            self.doc_notes.remove(thread_id);
            // Sent: take the pins off the documents and stop annotating them.
            let mut files: Vec<&str> = Vec::new();
            for (path, _) in &doc_notes {
                if !files.contains(&path.as_str()) {
                    files.push(path);
                }
            }
            for path in files {
                if let Some(doc) = self.panel.read(cx).doc_preview(thread_id, path, cx) {
                    doc.update(cx, |doc, cx| {
                        doc.clear_notes(cx);
                        doc.set_annotating(false, cx);
                    });
                }
            }
        }
        let images = self.attachments.remove(thread_id).unwrap_or_default();
        // Pasted blocks follow what was typed.
        let mut parts = Vec::new();
        if !typed.is_empty() {
            parts.push(typed);
        }
        parts.extend(pasted.into_iter().map(|p| p.trim_end().to_owned()));
        if !notes.is_empty() {
            parts.push(notes_block(&notes));
        }
        if !doc_notes.is_empty() {
            let pictured: Vec<&str> = images.iter().map(|a| a.name.as_str()).collect();
            parts.push(doc_notes_block(&doc_notes, &pictured));
        }
        let text = parts.join("\n\n");
        composer.update(cx, |c, cx| c.set_value("", window, cx));
        let attachments: Vec<loft_protocol::Attachment> =
            images.iter().map(|a| a.to_attachment()).collect();
        let id = thread_id.clone();
        if self.is_running(thread_id, cx) {
            let message = crate::store::Outgoing::new(text, images);
            self.store.update(cx, |store, cx| {
                if steer {
                    store.steer(id, message, cx);
                } else {
                    // The daemon sends it when the current turn completes.
                    store.enqueue(id, message, cx);
                }
            });
        } else {
            self.store.update(cx, |store, cx| {
                store.send_message(id, text, attachments, cx)
            });
        }
    }

    /// The thread at a glance: a tick per message you sent, down the left
    /// edge. Hover one to peek at it and its reply; click to go there.
    fn turn_map(
        &self,
        thread: &Entity<ThreadState>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = thread.read(cx);
        let model = state.model.as_ref()?;
        let tid = state.id.clone();
        let items = &model.items;
        // A tick per message you sent: its row, and its item.
        let marks: Vec<(usize, usize)> = state
            .rows
            .iter()
            .enumerate()
            .filter_map(|(row_ix, row)| match row {
                crate::store::Row::Item(i)
                    if matches!(items[*i].body, loft_protocol::ItemBody::UserMessage { .. }) =>
                {
                    Some((row_ix, *i))
                }
                _ => None,
            })
            .collect();
        if marks.len() < 2 {
            return None;
        }
        // Only in the margin beside the text, never over it: rows keep at
        // least 24 pt clear, so a narrow pane gets a slimmer rail.
        let width = self.timeline_widths.get(&tid).map_or(0., |w| w.get());
        let content = f32::from(p.content_width).min(width);
        let margin = (width - content) / 2. + 24.;
        if width <= 0. || margin < 20. {
            return None;
        }
        let compact = margin < 40.;
        let (inset, long, mid, short) = if compact {
            (4., 15., 12., 7.)
        } else {
            (12., 20., 16., 10.)
        };
        // Lit: the messages on screen; with none in view (deep in a long
        // reply), the last one scrolled past.
        let visible = state.visible_messages.borrow().clone();
        let lit: Vec<usize> = if visible.is_empty() {
            let top = state.scroller.read(cx).logical_scroll_top().item_ix;
            vec![marks.iter().rposition(|(row, ..)| *row <= top).unwrap_or(0)]
        } else {
            marks
                .iter()
                .enumerate()
                .filter(|(_, (row, ..))| visible.contains(row))
                .map(|(ix, _)| ix)
                .collect()
        };
        let spacing = match marks.len() {
            0..=40 => 14.,
            41..=80 => 8.,
            _ => 5.,
        };
        let scroller = state.scroller.clone();
        let mut rail = v_flex()
            .w(px(if compact { 22. } else { 34. }))
            .items_start();
        // The card's words are made for the tick under the pointer alone:
        // they take a walk through the thread's items, and this runs for
        // every tick on every paint.
        let peeked = self
            .peek
            .as_ref()
            .filter(|(thread, _)| *thread == tid)
            .and_then(|(_, ix)| Some((*ix, turn_peek(items, marks.get(*ix)?.1))));
        for (ix, (row, _)) in marks.into_iter().enumerate() {
            let peek = peeked.as_ref().filter(|(peeked, _)| *peeked == ix);
            let hovered = peek.is_some();
            let is_current = lit.contains(&ix);
            let scroller = scroller.clone();
            let card = peek.map(|(_, (title, reply))| {
                let (title, reply) = (title.clone(), reply.clone());
                v_flex()
                    .absolute()
                    .left(px(if compact { 26. } else { 38. }))
                    .top(px(-24.))
                    .w(px(320.))
                    .gap_1()
                    .px_3()
                    .py_2p5()
                    .rounded(px(12.))
                    .bg(p.elevated)
                    .border_1()
                    .border_color(p.border)
                    .shadow_lg()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.foreground)
                            .truncate()
                            .child(title),
                    )
                    .when(!reply.is_empty(), |d| {
                        d.child(
                            div()
                                .text_sm()
                                .text_color(p.muted_foreground)
                                .line_clamp(3)
                                .child(reply),
                        )
                    })
            });
            rail = rail.child(
                h_flex()
                    .id(SharedString::from(format!("turn-mark-{ix}")))
                    .relative()
                    .w_full()
                    .h(px(spacing))
                    .pl(px(inset))
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .h(px(2.))
                            .rounded(px(1.))
                            .w(px(if is_current {
                                long
                            } else if hovered {
                                mid
                            } else {
                                short
                            }))
                            .bg(if is_current || hovered {
                                p.foreground.opacity(0.85)
                            } else {
                                p.muted_foreground.opacity(0.45)
                            }),
                    )
                    .children(card)
                    .on_hover(cx.listener({
                        let peek_id = tid.clone();
                        move |this, on: &bool, _, cx| {
                            if *on {
                                this.peek = Some((peek_id.clone(), ix));
                            } else if this.peek.as_ref() == Some(&(peek_id.clone(), ix)) {
                                this.peek = None;
                            }
                            cx.notify();
                        }
                    }))
                    .on_click(move |_, _, cx| {
                        scroller.update(cx, |s, cx| {
                            s.scroll_to_item(row, cx);
                        });
                    }),
            );
        }
        Some(
            v_flex()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .justify_center()
                .child(rail)
                .into_any_element(),
        )
    }

    /// Put a queued message back in the composer (text and images).
    fn edit_queued(
        &mut self,
        thread_id: &ThreadId,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(q) = self.store.read(cx).queued(thread_id, id, cx) else {
            return;
        };
        self.store.update(cx, |s, cx| s.dequeue(thread_id, id, cx));
        // Threads it carried as context come back as chips.
        let (text, threads) = crate::thread_ref::split(&q.text);
        self.back_to_composer(thread_id, &text, window, cx);
        if !threads.is_empty() {
            let waiting = self.pasted.entry(thread_id.clone()).or_default();
            for thread in threads {
                let line = thread.to_text();
                if !waiting.contains(&line) {
                    waiting.push(line);
                }
            }
        }
        // Its images are the daemon's by now: read them back.
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let thread_id = thread_id.clone();
        cx.spawn(async move |this, cx| {
            let mut images = Vec::new();
            for a in q.attachments {
                let mut data = Vec::new();
                loop {
                    let part = client
                        .request(loft_protocol::Request::ReadPath {
                            path: a.path.clone(),
                            thread_id: None,
                            offset: data.len() as u64,
                        })
                        .await;
                    let Ok(loft_protocol::Response::File { file }) = part else {
                        data.clear();
                        break;
                    };
                    data.extend_from_slice(&file.bytes.0);
                    if !file.truncated || file.bytes.0.is_empty() {
                        break;
                    }
                }
                images.extend(crate::attachments::PendingImage::stored(
                    a.name,
                    std::path::Path::new(&a.path),
                    data,
                ));
            }
            let _ = this.update(cx, |this, cx| this.add_images(&thread_id, images, cx));
        })
        .detach();
    }

    /// Put `text` in a thread's composer, ahead of what is typed there.
    fn back_to_composer(
        &mut self,
        thread_id: &ThreadId,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.store.read(cx).threads.get(thread_id).cloned() else {
            return;
        };
        let composer = self.composer_for(&thread, window, cx);
        composer.update(cx, |c, cx| {
            let current = c.value().to_string();
            let text = if current.trim().is_empty() {
                text.to_owned()
            } else {
                format!("{}\n\n{}", text, current.trim_end())
            };
            let end = text.len();
            c.set_value(text, window, cx);
            c.set_selected_range(end..end, cx);
            c.focus(window, cx);
        });
    }

    /// Messages the daemon didn't take go back where they were typed.
    fn take_unsent(&mut self, thread_id: &ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let unsent = self
            .store
            .update(cx, |s, _| s.unsent.remove(thread_id))
            .unwrap_or_default();
        for message in unsent {
            self.back_to_composer(thread_id, &message.text, window, cx);
            self.add_images(thread_id, message.images, cx);
        }
    }

    /// Chips for large pastes waiting in the composer.
    fn paste_chips(
        &self,
        thread_id: &ThreadId,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pasted = self.pasted.get(thread_id).filter(|v| !v.is_empty())?;
        let mut row = h_flex().gap_2().flex_wrap().px_3().pt_2();
        for (ix, text) in pasted.iter().enumerate() {
            // Another thread, added as context: its own chip.
            if let Some(thread) = crate::thread_ref::parse(text) {
                row = row.child(self.thread_chip(thread_id, ix, &thread, p, cx));
                continue;
            }
            let lines = text.lines().count();
            let preview: String = text
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .chars()
                .take(40)
                .collect();
            let (open_text, tid) = (text.clone(), thread_id.clone());
            row =
                row.child(
                    h_flex()
                        .id(SharedString::from(format!("paste-{ix}")))
                        .gap_2()
                        .items_center()
                        .max_w(px(320.))
                        .pl_2()
                        .pr_1()
                        .py_1()
                        .rounded(px(9.))
                        .border_1()
                        .border_color(p.border)
                        .bg(p.muted)
                        .cursor_pointer()
                        .hover(|d| d.bg(p.hover))
                        .child(Icon::new(IconName::FileText).small().text_color(p.accent))
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(div().text_xs().font_weight(FontWeight::MEDIUM).child(
                                    format!(
                                        "Pasted text · {lines} line{} · {}",
                                        if lines == 1 { "" } else { "s" },
                                        human_bytes(text.len())
                                    ),
                                ))
                                .child(
                                    div()
                                        .truncate()
                                        .text_xs()
                                        .text_color(p.faint_foreground)
                                        .child(preview),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("paste-remove-{ix}")))
                                .cursor_pointer()
                                .p(px(3.))
                                .rounded(px(5.))
                                .text_color(p.muted_foreground)
                                .hover(|d| d.bg(p.selected).text_color(p.foreground))
                                .child(Icon::new(IconName::X).xsmall())
                                .claims_press()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(list) = this.pasted.get_mut(&tid)
                                        && ix < list.len()
                                    {
                                        list.remove(ix);
                                    }
                                    cx.notify();
                                })),
                        )
                        .on_click(move |_, window, cx| show_pasted(open_text.clone(), window, cx)),
                );
        }
        Some(row.into_any_element())
    }

    fn is_running(&self, thread_id: &ThreadId, cx: &gpui_kit::App) -> bool {
        self.store
            .read(cx)
            .threads
            .get(thread_id)
            .and_then(|t| {
                t.read(cx)
                    .model
                    .as_ref()
                    .map(|m| m.running_turn().is_some())
            })
            .unwrap_or(false)
    }

    /// The oldest open approval in the selected thread.
    fn open_request(&self, cx: &gpui_kit::App) -> Option<loft_protocol::RuntimeRequest> {
        let store = self.store.read(cx);
        let id = store.selected.as_ref()?;
        let thread = store.threads.get(id)?.read(cx);
        thread
            .model
            .as_ref()?
            .open_requests()
            .min_by_key(|r| r.opened_at_ms)
            .cloned()
    }

    /// The forms for `thread`'s open questions and plans, made as they
    /// open; forms whose request closed are dropped.
    fn ask_forms_for(
        &mut self,
        thread: &Entity<ThreadState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Entity<crate::ask::AskForm>> {
        let (open, agent) = {
            let state = thread.read(cx);
            let Some(model) = state.model.as_ref() else {
                return Vec::new();
            };
            let open: Vec<loft_protocol::RuntimeRequest> = model
                .open_requests()
                .filter(|r| crate::ask::is_form(r))
                .cloned()
                .collect();
            (open, harness_name(&model.thread.harness))
        };
        // Drop forms whose request is no longer open anywhere.
        let store = self.store.read(cx);
        self.ask_forms.retain(|id, form| {
            let thread_id = &form.read(cx).request.thread_id;
            store
                .threads
                .get(thread_id)
                .and_then(|t| t.read(cx).model.as_ref())
                .is_some_and(|m| m.open_requests().any(|r| &r.id == id))
        });
        let mut forms = Vec::new();
        for req in open {
            let form = match self.ask_forms.get(&req.id) {
                Some(f) => f.clone(),
                None => {
                    let store = self.store.clone();
                    let id = req.id.clone();
                    let f = cx.new(|cx| crate::ask::AskForm::new(req, store, agent, window, cx));
                    self.ask_forms.insert(id, f.clone());
                    f
                }
            };
            forms.push(form);
        }
        forms
    }

    fn answer(&mut self, decision: loft_protocol::DecisionKind, cx: &mut Context<Self>) {
        let Some(req) = self.open_request(cx) else {
            return;
        };
        if !req.options.contains(&decision) {
            return;
        }
        // Questions and plans answer through their form (the answers, the
        // feedback typed).
        if let Some(form) = self.ask_forms.get(&req.id).cloned() {
            form.update(cx, |f, cx| f.decide(decision, cx));
            return;
        }
        self.store
            .update(cx, |s, cx| s.respond(req.thread_id, req.id, decision, cx));
    }

    /// Add Project: the built-in folder browser.
    pub(crate) fn add_project(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<crate::folder_picker::FolderPicker> {
        use crate::folder_picker::{FolderPicker, FolderPickerEvent};
        use gpui_kit::component::WindowExt as _;
        let store = self.store.clone();
        let picker = cx.new(|cx| FolderPicker::new(store, window, cx));
        let sub = cx.subscribe_in(&picker, window, |this, _, event, window, cx| {
            if let FolderPickerEvent::Pick(path) = event {
                let path = path.to_string_lossy().into_owned();
                this.store.update(cx, |s, cx| s.add_project(path, cx));
            }
            window.close_dialog(cx);
        });
        self._subs.push(sub);
        let focus = picker.read(cx).focus_handle(cx);
        let content = picker.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let dialog = dialog.centered();
            dialog
                .w(px(620.))
                .p_0()
                .close_button(false)
                .child(content.clone())
        });
        focus.focus(window, cx);
        picker
    }

    /// Recompute the `/` or `@` completion for the composer's cursor.
    fn refresh_completion(
        &mut self,
        thread_id: &ThreadId,
        composer: &Entity<TextareaState>,
        cx: &mut Context<Self>,
    ) {
        use crate::completion::{
            Completion, Kind, MAX_ITEMS, bare_commands, match_commands, trigger,
        };
        let (text, cursor) = {
            let c = composer.read(cx);
            (c.value().to_string(), c.cursor())
        };
        self.completion_seq += 1;
        let Some((kind, token, query)) = trigger(&text, cursor) else {
            if self.completion.take().is_some() {
                cx.notify();
            }
            return;
        };
        let previous = self
            .completion
            .as_ref()
            .filter(|c| c.kind == kind)
            .map(|c| c.selected)
            .unwrap_or(0);
        let scroll = self
            .completion
            .as_ref()
            .filter(|c| c.kind == kind)
            .map(|c| c.scroll.clone())
            .unwrap_or_default();
        match kind {
            Kind::Slash => {
                // This thread's commands, else another of its harness's.
                let store = self.store.read(cx);
                let thread = store.workspace.threads.iter().find(|t| &t.id == thread_id);
                let has = |t: &&loft_protocol::Thread| {
                    !t.command_info.is_empty() || !t.slash_commands.is_empty()
                };
                let asked = thread.and_then(|t| {
                    store
                        .commands
                        .get(&(t.harness.as_str().to_owned(), t.cwd.clone()))
                        .cloned()
                        .flatten()
                        .filter(|l| !l.is_empty())
                });
                let commands = thread
                    .filter(has)
                    .map(|t| {
                        if t.command_info.is_empty() {
                            bare_commands(&t.slash_commands)
                        } else {
                            t.command_info.clone()
                        }
                    })
                    .or(asked)
                    .or_else(|| {
                        let harness = &thread?.harness;
                        store
                            .workspace
                            .threads
                            .iter()
                            .find(|t| &t.harness == harness && has(t))
                            .map(|t| {
                                if t.command_info.is_empty() {
                                    bare_commands(&t.slash_commands)
                                } else {
                                    t.command_info.clone()
                                }
                            })
                    })
                    .unwrap_or_else(|| match thread.map(|t| t.harness.as_str()) {
                        // Known before Codex answers.
                        Some(loft_protocol::HarnessId::CODEX) => {
                            loft_protocol::codex_builtin_commands()
                        }
                        _ => Vec::new(),
                    });
                let info = match_commands(&commands, &query);
                let items: Vec<String> = info.iter().map(|c| c.name.clone()).collect();
                // Keep the chosen row in view (a new list starts at the top).
                if !items.is_empty() {
                    scroll.scroll_to_item(previous.min(items.len() - 1));
                }
                self.completion = (!items.is_empty()).then(|| Completion {
                    thread_id: thread_id.clone(),
                    kind,
                    token,
                    selected: previous.min(items.len() - 1),
                    items,
                    info,
                    threads: Vec::new(),
                    scroll,
                });
                cx.notify();
            }
            Kind::File => {
                let Some(client) = self.store.read(cx).client() else {
                    return;
                };
                let seq = self.completion_seq;
                // Other threads by title, listed after the files.
                let threads = self.matching_threads(thread_id, &query, cx);
                let body = loft_protocol::Request::FindFiles {
                    root: loft_protocol::FsRoot::Thread(thread_id.clone()),
                    query,
                    limit: MAX_ITEMS as u32,
                };
                let thread_id = thread_id.clone();
                cx.spawn(async move |this, cx| {
                    let result = client.request(body).await;
                    let _ = this.update(cx, |this, cx| {
                        // A newer keystroke already moved on.
                        if this.completion_seq != seq {
                            return;
                        }
                        let items = match result {
                            Ok(loft_protocol::Response::Paths { paths }) => paths,
                            _ => Vec::new(),
                        };
                        let rows = items.len() + threads.len();
                        this.completion = (rows > 0).then(|| Completion {
                            thread_id,
                            kind,
                            token,
                            selected: previous.min(rows.saturating_sub(1)),
                            items,
                            info: Vec::new(),
                            threads,
                            scroll,
                        });
                        if let Some(completion) = &this.completion {
                            completion.scroll_to_selected();
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    /// Replace the token with the chosen completion.
    fn accept_completion(
        &mut self,
        ix: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut c) = self.completion.take() else {
            return;
        };
        if let Some(ix) = ix {
            c.selected = ix;
        }
        // A thread: what was typed for it goes, and it waits as a chip.
        if let Some(thread) = c.picked_thread().cloned() {
            if let Some(composer) = self.composers.get(&c.thread_id).cloned() {
                composer.update(cx, |state, cx| {
                    state.set_selected_range(c.token.clone(), cx);
                    state.replace("", window, cx);
                    state.focus(window, cx);
                });
            }
            self.add_thread_ref(&c.thread_id, thread, cx);
            return;
        }
        let (Some(text), Some(composer)) =
            (c.insertion(), self.composers.get(&c.thread_id).cloned())
        else {
            return;
        };
        composer.update(cx, |state, cx| {
            state.set_selected_range(c.token.clone(), cx);
            state.replace(text, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn add_images(
        &mut self,
        thread_id: &ThreadId,
        images: Vec<crate::attachments::PendingImage>,
        cx: &mut Context<Self>,
    ) {
        let list = self.attachments.entry(thread_id.clone()).or_default();
        let room = crate::attachments::MAX_IMAGES.saturating_sub(list.len());
        list.extend(images.into_iter().take(room));
        cx.notify();
    }

    /// Attach image files (from a drop or the picker), read off-thread.
    fn attach_paths(
        &mut self,
        thread_id: ThreadId,
        paths: Vec<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let images = cx
                .background_executor()
                .spawn(async move { crate::attachments::read_paths(&paths) })
                .await;
            let _ = this.update(cx, |this, cx| this.add_images(&thread_id, images, cx));
        })
        .detach();
    }

    fn pick_images(&mut self, thread_id: ThreadId, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |this, cx| this.attach_paths(thread_id, paths, cx));
            }
        })
        .detach();
    }

    pub(crate) fn new_thread(&mut self, project: Option<ProjectId>, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let project = project
            .or_else(|| {
                let selected = store.selected.as_ref()?;
                store
                    .workspace
                    .thread(selected)
                    .map(|t| t.project_id.clone())
            })
            .or_else(|| store.workspace.projects.first().map(|p| p.id.clone()));
        match project {
            Some(id) => self.store.update(cx, |s, cx| s.new_thread(id, cx)),
            // No window here: open the folder browser on the next render.
            None => {
                self.pending_add_project = true;
                cx.notify();
            }
        }
    }

    /// How many columns, and panes per column, fit the pane area.
    fn pane_room(&self) -> (usize, usize) {
        let (w, h) = self.pane_room.get();
        if w <= 0. {
            return (4, 2);
        }
        let cols = ((w / PANE_MIN_W).floor() as usize).clamp(1, MAX_PANES);
        let rows = ((h / PANE_MIN_H).floor() as usize).clamp(1, 3);
        (cols, rows)
    }

    /// The window's threads: columns side by side, each a stack of panes,
    /// with dividers to drag. Drop a thread (from the sidebar, or a pane's
    /// title bar) on a pane's left or right edge for a new column, its top
    /// or bottom to stack it there, or its middle to swap.
    fn render_panes(
        &mut self,
        focused: Entity<ThreadState>,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused_id = focused.read(cx).id.clone();
        let (max_cols, max_rows) = self.pane_room();
        let layout: Vec<Vec<ThreadId>> = self.store.update(cx, |s, _| {
            s.max_cols = max_cols;
            s.max_rows = max_rows;
            if s.pane_at(&focused_id).is_none() {
                s.layout.push(vec![focused_id.clone()]);
            }
            // The focused pane is always one that fits.
            if let Some((c, r)) = s.pane_at(&focused_id) {
                if c >= max_cols {
                    s.layout.swap(c, max_cols - 1);
                }
                let c = c.min(max_cols - 1);
                if r >= max_rows {
                    s.layout[c].swap(r, max_rows - 1);
                }
            }
            s.layout
                .iter()
                .take(max_cols)
                .map(|col| col.iter().take(max_rows).cloned().collect())
                .collect()
        });
        let total: usize = layout.iter().map(Vec::len).sum();
        let multi = total > 1;
        // One conversation spans the width (its scrollbar at the edge) with
        // the card over its margin; several make room for it as a column.
        self.details_inset = if multi {
            0.
        } else {
            DOCK_WIDTH * self.details_amount
        };
        let dragging = cx.has_active_drag();
        // Resizing keeps while the arrangement stays; a new one starts even.
        let shape: String = layout
            .iter()
            .map(|col| {
                col.iter()
                    .map(|t| t.as_str().get(..8).unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("+")
            })
            .collect::<Vec<_>>()
            .join("|");
        let mut columns = h_resizable(SharedString::from(format!("panes-{shape}")));
        for (c, col) in layout.iter().enumerate() {
            let mut stack = v_resizable(SharedString::from(format!("pane-col-{c}-{shape}")));
            for (r, id) in col.iter().enumerate() {
                let thread = self.store.update(cx, |s, cx| s.ensure_thread(id, cx));
                let pane = self.render_pane(
                    &thread,
                    (c, r),
                    id == &focused_id,
                    multi,
                    dragging,
                    p,
                    window,
                    cx,
                );
                stack = stack.child(
                    resizable_panel()
                        .size_range(px(PANE_MIN_H * 0.6)..px(10_000.))
                        .child(pane),
                );
            }
            columns = columns.child(
                resizable_panel()
                    .size_range(px(PANE_MIN_W * 0.7)..px(10_000.))
                    .child(stack),
            );
        }
        let room = self.pane_room.clone();
        div()
            .size_full()
            .relative()
            .child(if multi {
                columns.into_any_element()
            } else {
                // One pane: no dividers.
                let thread = self
                    .store
                    .update(cx, |s, cx| s.ensure_thread(&focused_id, cx));
                self.render_pane(&thread, (0, 0), true, false, dragging, p, window, cx)
            })
            .child(
                gpui_kit::canvas(
                    move |bounds, window, _| {
                        let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                        if room.get() != size {
                            room.set(size);
                            window.refresh();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }

    /// One pane: its title bar (when there are several), the thread, and,
    /// while a thread is dragged, where it can land.
    #[allow(clippy::too_many_arguments)]
    fn render_pane(
        &mut self,
        thread: &Entity<ThreadState>,
        at: (usize, usize),
        is_focused: bool,
        multi: bool,
        dragging: bool,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = thread.read(cx).id.clone();
        self.rendering_focused = is_focused;
        let body = self.render_thread(thread.clone(), p, window, cx);
        self.rendering_focused = true;
        let header = multi.then(|| self.pane_header(thread, at.0 * 10 + at.1, is_focused, p, cx));
        let overlay = dragging.then(|| self.pane_drop_zones(at, p, cx));
        let store = self.store.clone();
        let focus_id = id.clone();
        v_flex()
            .id(SharedString::from(format!("pane-{}", id.as_str())))
            .size_full()
            .relative()
            // Clicking anywhere in a pane focuses it.
            .capture_any_mouse_down(move |_, _, cx| {
                let id = focus_id.clone();
                store.update(cx, |s, cx| {
                    if s.selected.as_ref() != Some(&id) {
                        s.select_thread(id, cx);
                    }
                });
            })
            .children(header)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .children(overlay)
            .into_any_element()
    }

    /// A pane's title bar: agent, title, close; drag it to move the pane.
    fn pane_header(
        &self,
        thread: &Entity<ThreadState>,
        ix: usize,
        focused: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = thread.read(cx);
        let (title, harness, running) = match state.model.as_ref() {
            Some(m) => (
                m.thread.title.clone(),
                m.thread.harness.as_str().to_owned(),
                m.running_turn().is_some(),
            ),
            None => ("Loading…".to_owned(), String::new(), false),
        };
        let id = state.id.clone();
        let drag = DraggedThread {
            id: id.clone(),
            title: title.clone(),
        };
        let store = self.store.clone();
        h_flex()
            .id(SharedString::from(format!("pane-head-{ix}")))
            .group("pane-head")
            .h(px(34.))
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .text_sm()
            .cursor_grab()
            .when(focused, |d| d.bg(p.background).text_color(p.foreground))
            .when(!focused, |d| d.bg(p.sidebar).text_color(p.muted_foreground))
            .child(crate::brand::agent_logo(&harness, px(13.), p))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(focused, |d| d.font_weight(FontWeight::MEDIUM))
                    .child(title),
            )
            .when(running, |d| {
                d.child(Spinner::new().xsmall().color(p.accent))
            })
            .child(
                div()
                    .id(SharedString::from(format!("pane-close-{ix}")))
                    .flex_none()
                    .p(px(3.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .text_color(p.muted_foreground)
                    .when(!focused, |d| {
                        d.invisible().group_hover("pane-head", |d| d.visible())
                    })
                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    .child(Icon::new(IconName::X).xsmall())
                    .claims_press()
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        let id = id.clone();
                        store.update(cx, |s, cx| s.close_pane(&id, cx));
                    }),
            )
            .on_drag(drag, |d, _, _, cx| cx.new(|_| d.clone()))
            .into_any_element()
    }

    /// While a thread is dragged: where it can land on the pane at `at`.
    fn pane_drop_zones(
        &self,
        at: (usize, usize),
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::store::DropZone;
        let accent = p.accent;
        let zone = |zone: DropZone, label: &'static str, cx: &mut Context<Self>| {
            div()
                .id(SharedString::from(format!(
                    "drop-{zone:?}-{}-{}",
                    at.0, at.1
                )))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(gpui_kit::transparent_black())
                .drag_over::<DraggedThread>(move |s, _, _, _| {
                    s.bg(accent.opacity(0.16))
                        .border_2()
                        .border_color(accent.opacity(0.7))
                        .text_color(accent)
                })
                .child(label)
                .on_drop(cx.listener(move |this, d: &DraggedThread, _, cx| {
                    let id = d.id.clone();
                    this.store
                        .update(cx, |s, cx| s.place_pane(id, at, zone, MAX_PANES, cx));
                }))
        };
        // Edges for placing, the middle for swapping.
        h_flex()
            .absolute()
            .inset_0()
            .child(
                zone(DropZone::Left, "Place left", cx)
                    .h_full()
                    .w(gpui_kit::relative(0.22)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .child(
                        zone(DropZone::Top, "Stack above", cx)
                            .w_full()
                            .h(gpui_kit::relative(0.25)),
                    )
                    .child(zone(DropZone::Center, "Swap in", cx).w_full().flex_1())
                    .child(
                        zone(DropZone::Bottom, "Stack below", cx)
                            .w_full()
                            .h(gpui_kit::relative(0.25)),
                    ),
            )
            .child(
                zone(DropZone::Right, "Place right", cx)
                    .h_full()
                    .w(gpui_kit::relative(0.22)),
            )
            .into_any_element()
    }

    /// A new thread in a pane of its own (⌘T).
    pub(crate) fn new_pane(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |s, _| s.expect_new_pane());
        self.new_thread(None, cx);
    }

    pub(crate) fn interrupt(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.store.read(cx).selected.clone()
            && self.is_running(&id, cx)
        {
            self.store.update(cx, |s, cx| s.interrupt(id, cx));
        }
    }

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        let path = self.store.read(cx).paths().config_file();
        // A starting config to edit by hand (the desktop app's own disk).
        if !crate::host::IS_WEB && !path.exists() {
            let _ = std::fs::write(&path, SETTINGS_TEMPLATE);
        }
        crate::settings::open(self.store.clone(), None, cx);
    }

    // -- sidebar ------------------------------------------------------------

    /// Rename a thread in place: its sidebar title becomes a field (Enter
    /// or clicking away saves, Esc cancels).
    pub(crate) fn start_rename(
        &mut self,
        id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming.as_ref().is_some_and(|(r, _, _)| *r == id) {
            return;
        }
        let title = self
            .store
            .read(cx)
            .workspace
            .threads
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        // A folded project opens so the row shows.
        if let Some(project) = self
            .store
            .read(cx)
            .workspace
            .threads
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.project_id.clone())
        {
            self.store.update(cx, |s, cx| {
                s.set_collapsed(std::slice::from_ref(&project), false, cx)
            });
            // So do older threads.
            self.show_older.insert(None);
            self.show_older.insert(Some(project));
        }
        self.sidebar_open = true;
        let input = cx.new(|cx| {
            let mut s = InputState::new(window, cx);
            s.set_value(title.clone(), window, cx);
            s
        });
        let end = title.len();
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.set_selected_range(0..end, cx);
        });
        let sub = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.finish_rename(true, cx);
            }
        });
        self.renaming = Some((id.clone(), input, sub));
        // A menu that just closed hands focus back to where it opened from;
        // take it again once that's done.
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(60))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some((r, input, _)) = &this.renaming
                    && *r == id
                    && !gpui_kit::Focusable::focus_handle(input.read(cx), cx).is_focused(window)
                {
                    let end = input.read(cx).value().len();
                    input.update(cx, |s, cx| {
                        s.focus(window, cx);
                        s.set_selected_range(0..end, cx);
                    });
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// End an in-place rename, saving the new title or not.
    fn finish_rename(&mut self, save: bool, cx: &mut Context<Self>) {
        let Some((id, input, _sub)) = self.renaming.take() else {
            return;
        };
        let title = input.read(cx).value().trim().to_owned();
        let old = self
            .store
            .read(cx)
            .workspace
            .threads
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone());
        if save && !title.is_empty() && old.as_deref() != Some(title.as_str()) {
            self.store
                .update(cx, |s, cx| s.rename_thread(id, title, cx));
        }
        cx.notify();
    }

    /// Shift-click: pick the threads from the last one clicked (or the open
    /// one) to `id`, as the sidebar lists them.
    fn select_range(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        let from = self
            .anchor
            .clone()
            .or_else(|| self.store.read(cx).selected.clone());
        let at = |t: &ThreadId| self.visible_rows.iter().position(|r| r == t);
        self.multi = match (from.as_ref().and_then(at), at(&id)) {
            (Some(a), Some(b)) => self.visible_rows[a.min(b)..=a.max(b)].to_vec(),
            _ => vec![id.clone()],
        };
        if self.anchor.is_none() {
            self.anchor = from;
        }
        cx.notify();
    }

    /// Look for the sidebar query in every thread's messages (after a
    /// short pause in typing).
    fn search_messages(&mut self, cx: &mut Context<Self>) {
        self.search_seq += 1;
        let seq = self.search_seq;
        let query = self.query.clone();
        if query.is_empty() {
            self.search_hits.clear();
            return;
        }
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(140))
                .await;
            if this
                .update(cx, |this, _| this.search_seq != seq)
                .unwrap_or(true)
            {
                return;
            }
            let result = client
                .request(loft_protocol::Request::SearchThreads { query, limit: 200 })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.search_seq != seq {
                    return;
                }
                this.search_hits = match result {
                    Ok(loft_protocol::Response::SearchHits { hits }) => {
                        hits.into_iter().map(|h| (h.thread_id.clone(), h)).collect()
                    }
                    _ => HashMap::new(),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Does the sidebar search match this thread: its title, project,
    /// agent, branch or (from the core) its messages.
    fn search_matches(&self, t: &loft_protocol::Thread, project: &str) -> bool {
        let q = &self.query;
        q.is_empty()
            || t.title.to_lowercase().contains(q)
            || project.to_lowercase().contains(q)
            || harness_name(&t.harness).to_lowercase().contains(q)
            || t.workspace
                .branch()
                .is_some_and(|b| b.to_lowercase().contains(q))
            || self.search_hits.contains_key(&t.id)
    }

    /// Threads sidebar: show every project's threads, or one's.
    fn project_filter_button(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
        let (projects, chats): (Vec<loft_protocol::Project>, Option<ProjectId>) = {
            let store = self.store.read(cx);
            let mut list: Vec<_> = store
                .workspace
                .projects
                .iter()
                .filter(|p| !store.is_chats(p))
                .cloned()
                .collect();
            list.sort_by_key(|p| p.name.to_lowercase());
            let chats = store
                .workspace
                .projects
                .iter()
                .find(|p| store.is_chats(p))
                .map(|p| p.id.clone());
            (list, chats)
        };
        let current = self.project_filter.clone();
        let view = cx.entity().downgrade();
        let _ = p;
        Button::new("project-filter")
            .cursor_pointer()
            .ghost()
            .xsmall()
            .icon(IconName::ListFilter)
            .when(current.is_some(), |b| {
                gpui_kit::component::Selectable::selected(b, true)
            })
            .tooltip("Filter by project")
            .dropdown_menu(move |mut menu, _, _| {
                let set = |id: Option<ProjectId>| {
                    let v = view.clone();
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                        let id = id.clone();
                        let _ = v.update(cx, |this, cx| {
                            this.project_filter = id;
                            cx.notify();
                        });
                    }
                };
                // Built like the project rows below, so all line up.
                let plain = |icon: IconName, label: &'static str| {
                    PopupMenuItem::element(move |_, cx| {
                        let p = Palette::of(cx);
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .size(px(16.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(Icon::new(icon).xsmall().text_color(p.muted_foreground)),
                            )
                            .child(label)
                    })
                };
                menu = menu.item(
                    plain(IconName::FolderOpen, "All projects")
                        .checked(current.is_none())
                        .on_click(set(None)),
                );
                if let Some(chats) = &chats {
                    menu = menu.item(
                        plain(IconName::MessageSquareDashed, "No project")
                            .checked(current.as_ref() == Some(chats))
                            .on_click(set(Some(chats.clone()))),
                    );
                }
                menu = menu.separator();
                // Each with its icon (favicon, chosen image or initials).
                for project in &projects {
                    let row = project.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_, cx| {
                            let p = Palette::of(cx);
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(crate::project_icon::badge(&row, 16., &p))
                                .child(row.name.clone())
                        })
                        .checked(current.as_ref() == Some(&project.id))
                        .on_click(set(Some(project.id.clone()))),
                    );
                }
                menu
            })
            .into_any_element()
    }

    /// A run of sidebar rows. With `older` (the group's key), threads idle
    /// for over 12 hours fold under an "Older" row, returned apart so the
    /// caller can place it (the flat list keeps it at the bottom).
    fn render_rows(
        &mut self,
        rows: Vec<Nested<'_>>,
        older: Option<Option<ProjectId>>,
        subtitle: &dyn Fn(&loft_protocol::Thread) -> Option<Subtitle>,
        drawn: &mut Vec<ThreadId>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> (gpui_kit::Div, Option<gpui_kit::Div>) {
        let (recent, old) = match &older {
            Some(_) => split_older(rows, &self.store.read(cx).unread, now_ms()),
            None => (rows, Vec::new()),
        };
        let mut list = v_flex().gap_px();
        for row in &recent {
            list = list.child(self.sidebar_row(row, subtitle, drawn, p, cx));
        }
        let Some(key) = older.filter(|_| !old.is_empty()) else {
            return (list, None);
        };
        let open = self.show_older.contains(&key);
        let count = old.iter().filter(|r| r.depth == 0).count();
        let toggle_id = SharedString::from(format!(
            "older-{}",
            key.as_ref().map_or("threads", |k| k.as_str())
        ));
        let mut folded = v_flex().gap_px().child(
            h_flex()
                .id(toggle_id)
                .gap_1p5()
                .items_center()
                .pl(px(12.))
                .pr_2()
                .py(px(5.))
                .rounded(px(7.))
                .cursor_pointer()
                .text_xs()
                .text_color(p.faint_foreground)
                .hover(|d| d.bg(p.hover).text_color(p.muted_foreground))
                .child(Icon::new(IconName::Clock).xsmall())
                .child(format!("Older ({count})"))
                .child(div().flex_1())
                .child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall(),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.show_older.remove(&key) {
                        this.show_older.insert(key.clone());
                    }
                    cx.notify();
                })),
        );
        if open {
            for row in &old {
                folded = folded.child(self.sidebar_row(row, subtitle, drawn, p, cx));
            }
        }
        (list, Some(folded))
    }

    /// One thread's sidebar row.
    fn sidebar_row(
        &self,
        row: &Nested<'_>,
        subtitle: &dyn Fn(&loft_protocol::Thread) -> Option<Subtitle>,
        drawn: &mut Vec<ThreadId>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let thread = row.thread;
        let id = &thread.id;
        drawn.push(id.clone());
        let (selected, unread) = {
            let store = self.store.read(cx);
            (
                store.selected.as_ref() == Some(id),
                store.unread.contains(id),
            )
        };
        let multi = &self.multi;
        thread_row(
            &self.store,
            thread,
            row.depth,
            row.under,
            selected || multi.contains(id),
            unread,
            self.row_menu.as_ref() == Some(id),
            self.renaming
                .as_ref()
                .filter(|(r, _, _)| r == id)
                .map(|(_, input, _)| input.clone()),
            // A message matched (not just the title): show where.
            self.search_hits
                .get(id)
                .filter(|_| !thread.title.to_lowercase().contains(&self.query)),
            (row.depth == 0).then(|| subtitle(thread)).flatten(),
            (multi.len() > 1 && multi.contains(id)).then(|| multi.clone()),
            p,
            cx,
        )
    }

    fn render_sidebar(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (projects, all_threads, conn, unread, collapsed) = {
            let store = self.store.read(cx);
            (
                store.workspace.projects.clone(),
                // Without each agent's commands: nothing here reads them,
                // and with them this copy, made for every frame, was half
                // a megabyte for a couple of dozen threads.
                store
                    .workspace
                    .threads
                    .iter()
                    .map(loft_protocol::Thread::without_commands)
                    .collect::<Vec<_>>(),
                store.conn.clone(),
                store.unread.clone(),
                store.collapsed.clone(),
            )
        };
        // Drafts (new threads not sent yet) go first; everything else stays
        // most recent first. A draft keeps its creation time so typing
        // doesn't re-sort, which would otherwise sink it under any thread
        // that did something since.
        let mut all_threads = all_threads;
        all_threads.sort_by_key(|t| t.is_started() || t.parent.is_some());
        let all_projects: Vec<ProjectId> = projects.iter().map(|p| p.id.clone()).collect();
        let query = self.query.clone();
        // Rows picked together (gone threads drop out).
        self.multi
            .retain(|id| all_threads.iter().any(|t| &t.id == id));
        let mut drawn: Vec<ThreadId> = Vec::new();
        let mut list = v_flex().gap_4().px_2().pb_4();
        // Kept at the foot of the list, however short it is: the flat
        // list's "Older" row and the archived threads.
        let mut foot = v_flex().gap_px().px_2().pb_2();
        // Chats (threads without a project) come first.
        let chats: Vec<ProjectId> = {
            let store = self.store.read(cx);
            projects
                .iter()
                .filter(|p| store.is_chats(p))
                .map(|p| p.id.clone())
                .collect()
        };
        let mut projects = projects;
        // Then pinned projects, in pin order; then the rest as they were.
        let pinned = self.store.read(cx).pinned_projects.clone();
        projects.sort_by_key(|p| {
            (
                !chats.contains(&p.id),
                pinned.iter().position(|x| *x == p.id).unwrap_or(usize::MAX),
            )
        });

        // Under a top-level row's title (when projects are mixed): its project.
        let subtitle = |t: &loft_protocol::Thread| {
            Some(match projects.iter().find(|p| p.id == t.project_id) {
                Some(pr) if !chats.contains(&pr.id) => Subtitle::Project(pr.clone()),
                _ => Subtitle::NoProject,
            })
        };
        let folded_threads = if query.is_empty() {
            self.store.read(cx).folded_threads.clone()
        } else {
            Default::default()
        };

        // Pinned threads first (in pin order), with what's under them.
        let pinned_threads = self.store.read(cx).pinned_threads.clone();
        let mut pinned: Vec<&loft_protocol::Thread> = pinned_threads
            .iter()
            .filter_map(|id| all_threads.iter().find(|t| &t.id == id))
            .filter(|t| t.status != ThreadStatus::Archived)
            .collect();
        let mut at = 0;
        while at < pinned.len() {
            let parent = pinned[at].id.clone();
            let children: Vec<_> = all_threads
                .iter()
                .filter(|t| {
                    t.status != ThreadStatus::Archived
                        && t.parent
                            .as_ref()
                            .is_some_and(|p| !p.detached && p.thread_id == parent)
                        && !pinned.iter().any(|x| x.id == t.id)
                })
                .collect();
            pinned.extend(children);
            at += 1;
        }
        // Kept out of the lists below either way.
        let pinned_rows: std::collections::HashSet<ThreadId> =
            pinned.iter().map(|t| t.id.clone()).collect();
        let pinned: Vec<_> = pinned
            .into_iter()
            .filter(|t| self.lists_thread(t))
            .filter(|t| {
                let name = projects
                    .iter()
                    .find(|p| p.id == t.project_id)
                    .map_or("", |p| p.name.as_str());
                self.search_matches(t, name)
            })
            .collect();
        if !pinned.is_empty() {
            let rows = nest_forks(pinned, &folded_threads);
            let (rows, _) = self.render_rows(rows, None, &subtitle, &mut drawn, p, cx);
            list = list.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .px_2()
                            .text_xs()
                            .text_color(p.faint_foreground)
                            .child(Icon::new(IconName::Pin).xsmall())
                            .child("Pinned"),
                    )
                    .child(rows),
            );
        }

        // One list, most recent first, each row with its project.
        let flat = !theme::sidebar_by_project(cx);
        if flat {
            let filter = self
                .project_filter
                .clone()
                .filter(|f| projects.iter().any(|p| &p.id == f));
            let threads: Vec<_> = all_threads
                .iter()
                .filter(|t| t.status != ThreadStatus::Archived)
                .filter(|t| !pinned_rows.contains(&t.id))
                .filter(|t| filter.as_ref().is_none_or(|f| &t.project_id == f))
                .filter(|t| self.lists_thread(t))
                .filter(|t| {
                    let name = projects
                        .iter()
                        .find(|p| p.id == t.project_id)
                        .map_or("", |p| p.name.as_str());
                    self.search_matches(t, name)
                })
                .collect();
            let rows = nest_forks(threads, &folded_threads);
            let (rows, older) = self.render_rows(
                rows,
                query.is_empty().then_some(None),
                &subtitle,
                &mut drawn,
                p,
                cx,
            );
            list = list.child(rows);
            foot = foot.children(older);
        }

        for project in projects.iter().filter(|_| !flat) {
            let project_id = project.id.clone();
            let is_chats = chats.contains(&project.id);
            let threads: Vec<_> = all_threads
                .iter()
                .filter(|t| t.project_id == project.id && t.status != ThreadStatus::Archived)
                .filter(|t| !pinned_rows.contains(&t.id))
                .filter(|t| self.lists_thread(t))
                .filter(|t| self.search_matches(t, &project.name))
                .collect();
            if (is_chats || !query.is_empty()) && threads.is_empty() {
                continue;
            }
            // Searching shows every match, folded or not.
            let folded = query.is_empty() && collapsed.contains(&project.id);
            let header = project_header(
                &self.store,
                project,
                &threads,
                folded,
                &unread,
                &all_projects,
                is_chats,
                p,
                cx.listener(move |this, _, _, cx| {
                    if is_chats {
                        this.store.update(cx, |s, cx| s.new_chat(cx));
                    } else {
                        this.new_thread(Some(project_id.clone()), cx)
                    }
                }),
            );
            let mut rows = v_flex().gap_px();
            if !folded {
                let (recent, older) = self.render_rows(
                    nest_forks(threads, &folded_threads),
                    query.is_empty().then(|| Some(project.id.clone())),
                    &|_| None,
                    &mut drawn,
                    p,
                    cx,
                );
                rows = recent.children(older);
            }
            list = list.child(v_flex().gap_1().child(header).child(rows));
        }

        self.visible_rows = drawn;

        // Archived threads: hidden unless asked for; each can be restored.
        let archived: Vec<_> = all_threads
            .iter()
            .filter(|t| t.status == ThreadStatus::Archived)
            .filter(|t| {
                let project = projects
                    .iter()
                    .find(|p| p.id == t.project_id)
                    .map_or("", |p| p.name.as_str());
                self.search_matches(t, project)
            })
            .cloned()
            .collect();
        if !archived.is_empty() {
            let open = self.show_archived;
            let mut section = v_flex().gap_px().child(
                // Styled like the "Older" row above it.
                h_flex()
                    .id("archived-toggle")
                    .gap_1p5()
                    .items_center()
                    .pl(px(12.))
                    .pr_2()
                    .py(px(5.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .hover(|d| d.bg(p.hover).text_color(p.muted_foreground))
                    .child(Icon::new(IconName::Archive).xsmall())
                    .child(format!("Archived ({})", archived.len()))
                    .child(div().flex_1())
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_archived = !this.show_archived;
                        cx.notify();
                    })),
            );
            if open {
                for t in archived {
                    let id = t.id.clone();
                    section = section.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .pl(px(26.))
                            .pr_2()
                            .py(px(5.))
                            .text_sm()
                            .text_color(p.muted_foreground)
                            .child(div().flex_1().min_w_0().truncate().child(t.title.clone()))
                            .child(
                                div()
                                    .id(SharedString::from(format!("unarchive-{}", t.id)))
                                    .px_1p5()
                                    .rounded(px(5.))
                                    .text_xs()
                                    .cursor_pointer()
                                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                                    .child("Restore")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let id = id.clone();
                                        this.store.update(cx, |s, cx| s.unarchive_thread(id, cx));
                                    })),
                            ),
                    );
                }
            }
            foot = foot.child(section);
        }

        let (dot, status): (_, SharedString) = match &conn {
            ConnState::Connecting => (p.warning, "Connecting…".into()),
            ConnState::Connected => (p.success, "Connected".into()),
            ConnState::Disconnected(_) => (p.danger, "Reconnecting…".into()),
        };
        let dark = p.dark;

        let search = Input::new(&self.search)
            .prefix(
                Icon::new(IconName::Search)
                    .small()
                    .text_color(p.muted_foreground),
            )
            .cleanable(true);
        // A browser tab has no traffic lights: search takes their place in
        // the top strip instead of a row of its own.
        let (search_on_top, search_below) = if crate::host::IS_WEB {
            (Some(search), None)
        } else {
            (None, Some(search))
        };

        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(p.sidebar)
            .border_r_1()
            .border_color(p.border)
            // Title-bar strip: traffic lights on the left, actions on the right.
            .child(
                h_flex()
                    .id("sidebar-titlebar")
                    .on_click(titlebar_double_click)
                    .h(px(HEADER_HEIGHT))
                    .pl(px(if search_on_top.is_some() {
                        12.
                    } else {
                        TRAFFIC_LIGHTS_INSET
                    }))
                    .pr_2()
                    .items_center()
                    .justify_end()
                    .gap_1()
                    .children(
                        search_on_top.map(|search| div().flex_1().min_w_0().mr_1().child(search)),
                    )
                    .child(icon_button(
                        "toggle-sidebar",
                        IconName::PanelLeft,
                        p,
                        cx.listener(|this, _, _, cx| {
                            this.sidebar_open = false;
                            cx.notify();
                        }),
                    ))
                    .child(icon_button(
                        "new-thread",
                        IconName::SquarePen,
                        p,
                        // Where it goes: a project, or none.
                        cx.listener(|this, _, window, cx| {
                            crate::new_in::open(cx.entity(), &this.store, window, cx)
                        }),
                    )),
            )
            .children(search_below.map(|search| div().px_3().pb_2().child(search)))
            .child(self.automations_entry(p, cx))
            .child(
                h_flex()
                    .px_4()
                    .pb_2()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child(if flat {
                        // Filtered to a project: say which.
                        match &self.project_filter {
                            Some(f) => self
                                .store
                                .read(cx)
                                .workspace
                                .projects
                                .iter()
                                .find(|p| &p.id == f)
                                .map(|p| {
                                    if self.store.read(cx).is_chats(p) {
                                        "NO PROJECT".to_owned()
                                    } else {
                                        p.name.to_uppercase()
                                    }
                                })
                                .unwrap_or_else(|| "THREADS".into()),
                            None => "THREADS".into(),
                        }
                    } else {
                        "PROJECTS".to_owned()
                    })
                    .child(
                        h_flex()
                            .gap_0p5()
                            .when(flat, |d| d.child(self.project_filter_button(p, cx)))
                            .child(icon_button(
                                "add-project",
                                IconName::FolderPlus,
                                p,
                                // New, local folder, git URL or GitHub.
                                cx.listener(|this, _, window, cx| {
                                    crate::new_in::open_sources(
                                        cx.entity(),
                                        &this.store,
                                        window,
                                        cx,
                                    )
                                }),
                            )),
                    ),
            )
            .child(
                v_flex()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list.flex_shrink_0())
                    // Pushes the foot down when the list is short.
                    .child(div().flex_1())
                    .child(foot.flex_shrink_0()),
            )
            .child(
                h_flex()
                    .px_3()
                    .h(px(40.))
                    .items_center()
                    .gap_2()
                    .border_t_1()
                    .border_color(p.border)
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(crate::brand::app_icon(px(18.)))
                    .child(div().size(px(6.)).rounded_full().bg(dot))
                    .child(div().flex_1().child(status))
                    .child(self.usage_button(p, cx))
                    .child(icon_button(
                        "toggle-theme",
                        if dark { IconName::Sun } else { IconName::Moon },
                        p,
                        cx.listener(|_, _, _, cx| theme::toggle_mode(cx)),
                    ))
                    .child(icon_button(
                        "settings",
                        IconName::Settings,
                        p,
                        cx.listener(|this, _, _, cx| this.open_settings(cx)),
                    )),
            )
            .into_any_element()
    }

    /// The footer's usage button: plan limits in a card on hover (and the
    /// busiest one's percent beside the icon once known); a click opens
    /// Settings ▸ Usage.
    fn usage_button(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let limits = self.plan_limits.clone();
        // Redrawn when the answers arrive (the percent beside the icon).
        let peak = limits.read(cx).peak();
        let card = limits.clone();
        h_flex()
            .id("sidebar-usage")
            .h(px(26.))
            .px(px(6.))
            .gap_1()
            .items_center()
            .rounded(px(7.))
            .text_color(match peak {
                Some(pct) if pct >= 80. => p.warning,
                _ => p.muted_foreground,
            })
            .cursor_pointer()
            .hover(|d| d.bg(p.hover).text_color(p.foreground))
            .child(Icon::new(IconName::Gauge).small())
            .children(peak.map(|pct| format!("{}%", pct.round() as i64)))
            // Asked as the pointer arrives, so the card opens with answers.
            .on_hover(move |hovered, _, cx| {
                if *hovered {
                    limits.update(cx, |l, cx| l.refresh_if_stale(cx));
                }
            })
            .tooltip(move |_, _| card.clone().into())
            .on_click(cx.listener(|this, _, _, cx| {
                crate::settings::open(
                    this.store.clone(),
                    Some(crate::settings::Section::Usage),
                    cx,
                );
            }))
            .into_any_element()
    }

    /// The sidebar's way to the Automations page: how many are on, and
    /// whether one of their runs is going.
    fn automations_entry(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let open = store.automations_open;
        let on = store
            .workspace
            .automations
            .iter()
            .filter(|a| a.spec.enabled)
            .count();
        let running = store.workspace.threads.iter().any(|t| {
            t.automation_id.is_some()
                && (matches!(
                    t.status,
                    ThreadStatus::Running | ThreadStatus::WaitingApproval
                ) || t.is_waiting())
        });
        div()
            .px_2()
            .pb_2()
            .child(
                h_flex()
                    .id("sidebar-automations")
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py(px(5.))
                    .rounded(px(7.))
                    .text_sm()
                    .cursor_pointer()
                    .when(open, |d| d.bg(p.selected).text_color(p.foreground))
                    .when(!open, |d| {
                        d.text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    })
                    .child(Icon::new(IconName::Clock).small())
                    .child(div().flex_1().child("Automations"))
                    .when(running, |d| {
                        d.child(Spinner::new().xsmall().color(p.accent))
                    })
                    .when(on > 0, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(p.faint_foreground)
                                .child(on.to_string()),
                        )
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.store.update(cx, |s, cx| {
                            s.automations_open = !s.automations_open;
                            cx.notify();
                        });
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    // -- main pane ----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn render_header(
        &mut self,
        title: Option<(String, loft_protocol::Thread)>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_thread = title.is_some();
        let actions = title
            .as_ref()
            .and_then(|(_, thread)| self.actions_control(thread, p, cx));
        h_flex()
            .id("titlebar")
            .on_click(titlebar_double_click)
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .px_4()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            // Room for the traffic lights, growing as the sidebar slides shut.
            .when(self.sidebar_amount < 1., |d| {
                d.pl(px(
                    16. + (TRAFFIC_LIGHTS_INSET - 16.) * (1. - self.sidebar_amount)
                ))
            })
            .when(!self.sidebar_open, |d| {
                d.child(icon_button(
                    "show-sidebar",
                    IconName::PanelLeft,
                    p,
                    cx.listener(|this, _, _, cx| {
                        this.sidebar_open = true;
                        cx.notify();
                    }),
                ))
            })
            .when_some(title, |d, (project, thread)| {
                let chip = harness_name(&thread.harness).to_owned();
                let title = thread.title.clone();
                let branch_chip = match &thread.workspace {
                    ThreadWorkspace::Worktree { branch, .. } => {
                        Some(chip_label(IconName::GitBranch, branch.clone(), p))
                    }
                    ThreadWorkspace::NewWorktree { .. } => Some(chip_label(
                        IconName::GitBranch,
                        "New worktree".to_owned(),
                        p,
                    )),
                    // Cleaned up; the next message makes a new one.
                    ThreadWorkspace::Released { base, .. } => {
                        Some(chip_label(IconName::GitMerge, format!("In {base}"), p))
                    }
                    ThreadWorkspace::Local => None,
                };
                let pr_badge = thread
                    .pull_request
                    .as_ref()
                    .map(|pr| crate::pull_request::badge(pr, p));
                d.child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .text_sm()
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(p.muted_foreground)
                                .child(project),
                        )
                        .child(div().text_color(p.faint_foreground).child("/"))
                        .child(
                            div()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(title),
                        ),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .px_2()
                        .py(px(3.))
                        .rounded(px(7.))
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(crate::brand::agent_logo(
                            thread.harness.as_str(),
                            px(12.),
                            p,
                        ))
                        .child(chip),
                )
                .children(branch_chip)
                .children(pr_badge)
            })
            .children(actions)
            .when(has_thread, |d| {
                let anchor = self.details_anchor.clone();
                let measure = anchor.clone();
                let docked = self.details_open && self.can_dock() || crate::details::is_over(cx);
                d.child(
                    div()
                        .relative()
                        .child(toggle_button(
                            "hdr-details",
                            IconName::ScrollText,
                            docked,
                            p,
                            cx.listener(move |this, _, window, cx| {
                                // Wide enough: dock it (and keep it); else a popover.
                                if this.can_dock() {
                                    this.details_open = !this.details_open;
                                    cx.notify();
                                    return;
                                }
                                let Some(id) = this.store.read(cx).selected.clone() else {
                                    return;
                                };
                                if let Some(at) = anchor.get() {
                                    let (workspace, store) = (cx.weak_entity(), this.store.clone());
                                    crate::details::open(window, at, workspace, store, id, cx);
                                }
                            }),
                        ))
                        .child(
                            gpui_kit::canvas(
                                move |bounds, _, _| measure.set(Some(bounds)),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        ),
                )
                .child(toggle_button(
                    "hdr-terminal",
                    IconName::PanelBottom,
                    self.dock_open,
                    p,
                    cx.listener(|this, _, window, cx| {
                        if this.dock_open {
                            this.hide_dock(window, cx);
                        } else {
                            this.toggle_terminal(window, cx);
                        }
                    }),
                ))
                .child(toggle_button(
                    "hdr-panel",
                    IconName::PanelRight,
                    self.panel_open,
                    p,
                    cx.listener(|this, _, _, cx| {
                        this.panel_open = !this.panel_open;
                        cx.notify();
                    }),
                ))
            })
            .into_any_element()
    }

    /// "Forked from …" marker above a fork's timeline (ADR-0011).
    fn fork_bar(
        &self,
        parent: loft_protocol::ThreadParent,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = self
            .store
            .read(cx)
            .workspace
            .threads
            .iter()
            .find(|t| t.id == parent.thread_id)
            .map(|t| t.title.clone());
        // The thread it came from may have been deleted since: say so, and
        // don't offer to open it.
        let exists = title.is_some();
        let from = match title {
            Some(title) => format!("“{title}”"),
            None => "a deleted thread".into(),
        };
        let spawned = parent.kind == loft_protocol::ForkKind::Spawned;
        let how = match parent.kind {
            loft_protocol::ForkKind::Native => "full context",
            loft_protocol::ForkKind::Handoff => "handoff",
            loft_protocol::ForkKind::Spawned => "by its agent",
        };
        let id = parent.thread_id.clone();
        h_flex()
            .justify_center()
            .py_1p5()
            .child(
                h_flex()
                    .id("fork-bar")
                    .gap_1p5()
                    .items_center()
                    .px_2p5()
                    .py_1()
                    .rounded(px(999.))
                    .border_1()
                    .border_color(p.border)
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(
                        Icon::new(if spawned {
                            IconName::Bot
                        } else {
                            IconName::GitFork
                        })
                        .xsmall(),
                    )
                    .child(div().max_w(px(420.)).truncate().child(if spawned {
                        format!("Started from {from}")
                    } else {
                        format!("Forked from {from}")
                    }))
                    .child(
                        div()
                            .text_color(p.faint_foreground)
                            .child(format!("· {how}")),
                    )
                    .when(exists, |d| {
                        d.cursor_pointer()
                            .hover(|d| d.text_color(p.foreground).bg(p.hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = id.clone();
                                this.store.update(cx, |s, cx| s.select_thread(id, cx));
                            }))
                    }),
            )
            .into_any_element()
    }

    fn render_thread(
        &mut self,
        thread: Entity<ThreadState>,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let composer = self.composer_for(&thread, window, cx);
        // The agent's commands and skills, asked up front so `/` lists them
        // before the thread's session has started.
        if self.rendering_focused {
            let ask = thread.read(cx).model.as_ref().and_then(|m| {
                (matches!(
                    m.thread.harness.as_str(),
                    loft_protocol::HarnessId::CODEX | loft_protocol::HarnessId::CLAUDE
                ) && m.thread.command_info.is_empty())
                .then(|| (m.thread.harness.as_str().to_owned(), m.thread.cwd.clone()))
            });
            if let Some((harness, cwd)) = ask {
                self.store
                    .update(cx, |s, cx| s.ensure_commands(&harness, &cwd, cx));
            }
        }
        let tl_width = self
            .timeline_widths
            .entry(thread.read(cx).id.clone())
            .or_default()
            .clone();
        let preview = if self.rendering_focused {
            self.browser_preview_card(p, window, cx)
        } else {
            None
        };
        // Put a saved draft back, once, when the thread first loads.
        let saved = thread.read(cx).model.as_ref().map(|m| {
            (
                m.thread.id.clone(),
                m.thread.draft_text().map(str::to_owned),
            )
        });
        if let Some((tid, draft)) = saved
            && self.drafts_restored.insert(tid)
            && let Some(draft) = draft
            && composer.read(cx).value().is_empty()
        {
            composer.update(cx, |c, cx| c.set_value(draft, window, cx));
        }
        // Where a pending worktree starts, and the controls to change it.
        let pending = thread.read(cx).model.as_ref().and_then(|m| {
            (m.items.is_empty()
                && m.thread.parent.is_none()
                && m.thread.workspace.is_new_worktree())
            .then(|| {
                (
                    m.thread.id.clone(),
                    m.thread.project_id.clone(),
                    m.thread.workspace.clone(),
                )
            })
        });
        // A thread working in the checkout: the branch checked out there.
        let local_branch = thread.read(cx).model.as_ref().and_then(|m| {
            (m.items.is_empty() && m.thread.workspace == ThreadWorkspace::Local)
                .then(|| m.thread.project_id.clone())
        });
        let local_branch = local_branch.and_then(|project_id| {
            self.store
                .update(cx, |s, cx| s.ensure_branches(&project_id, cx));
            self.store
                .read(cx)
                .branches
                .get(&project_id)
                .and_then(|l| l.current.clone())
        });
        let (worktree_base, base_row) = match pending {
            Some((tid, project_id, workspace)) => {
                self.store
                    .update(cx, |s, cx| s.ensure_branches(&project_id, cx));
                let list = self
                    .store
                    .read(cx)
                    .branches
                    .get(&project_id)
                    .cloned()
                    .unwrap_or_default();
                let base = crate::branch_picker::effective_base(&workspace, &list);
                let row =
                    self.worktree_base_row(&tid, &project_id, base.clone(), &list, p, window, cx);
                (base, Some(row))
            }
            None => (None, None),
        };
        let fork_parent = thread
            .read(cx)
            .model
            .as_ref()
            .and_then(|m| m.thread.parent.clone());
        let fork_bar = fork_parent.map(|parent| self.fork_bar(parent, p, cx));
        let turn_map = self.turn_map(&thread, p, cx);
        let ask_forms = self.ask_forms_for(&thread, window, cx);
        let state = thread.read(cx);
        let Some(model) = state.model.as_ref() else {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Spinner::new().color(p.muted_foreground))
                .into_any_element();
        };
        let thread_id = model.thread.id.clone();
        let running = model.running_turn().is_some();
        let stop_hint = crate::keymap::hint("stop", cx);
        // Open approvals whose card isn't on screen (a background subagent's
        // step, or a step inside folded work) are pinned above the composer.
        let shown_items: std::collections::HashSet<usize> = state
            .rows
            .iter()
            .flat_map(|r| match r {
                crate::store::Row::Item(i) => vec![*i],
                crate::store::Row::ToolGroup { members } => members.clone(),
                _ => vec![],
            })
            .collect();
        let pinned: Vec<AnyElement> = model
            .open_requests()
            .filter(|req| !crate::ask::is_form(req))
            .filter(|req| {
                let ix = req
                    .item_id
                    .as_ref()
                    .and_then(|id| model.items.iter().position(|i| &i.id == id));
                ix.is_none_or(|ix| !shown_items.contains(&ix))
            })
            .map(|req| {
                // Say whose request it is when a subagent asks.
                let from = req
                    .item_id
                    .as_ref()
                    .and_then(|id| model.items.iter().find(|i| &i.id == id))
                    .and_then(|i| i.parent_id.as_ref())
                    .and_then(|pid| model.items.iter().find(|i| &i.id == pid))
                    .and_then(|parent| match &parent.body {
                        loft_protocol::ItemBody::ToolCall { title, .. } => Some(title.clone()),
                        _ => None,
                    });
                v_flex()
                    .gap_1()
                    .children(from.map(|title| {
                        h_flex()
                            .gap_1p5()
                            .px_1()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child(crate::brand::agent_logo(
                                model.thread.harness.as_str(),
                                px(12.),
                                p,
                            ))
                            .child(format!("Subagent “{title}” asks"))
                    }))
                    .child(crate::items::approval_card(
                        req,
                        &self.store,
                        p,
                        &model.thread.cwd,
                    ))
                    .into_any_element()
            })
            .collect();
        let pinned: Vec<AnyElement> = ask_forms
            .into_iter()
            .map(|f| f.into_any_element())
            .chain(pinned)
            .collect();
        let waiting = pinned.is_empty() && model.open_requests().next().is_some();
        // Nothing said yet, and nothing waiting to be.
        let empty = model.items.is_empty()
            && !state
                .rows
                .iter()
                .any(|r| matches!(r, crate::store::Row::Queued(_)));
        let harness_choices = self.store.read(cx).available_harnesses();
        // A chat (no project): no worktrees, and it says so.
        let is_chat = {
            let store = self.store.read(cx);
            store
                .workspace
                .projects
                .iter()
                .find(|p| p.id == model.thread.project_id)
                .is_some_and(|p| store.is_chats(p))
        };
        let known_models = self
            .store
            .read(cx)
            .models
            .get(&model.thread.harness)
            .cloned()
            .unwrap_or_default();
        // The model in use: the chosen one (by alias or resolved id), else
        // the agent's default.
        let model_name = match model.thread.model.clone() {
            Some(m) => known_models
                .iter()
                .find(|k| k.id == m || k.resolved_id.as_deref() == Some(m.as_str()))
                .map(|k| k.display_name.clone())
                .unwrap_or_else(|| pretty_model(&m)),
            None => known_models
                .iter()
                .find(|k| k.is_default)
                .map(|k| k.display_name.clone())
                .unwrap_or_else(|| harness_name(&model.thread.harness).into()),
        };
        let effort_name = model
            .thread
            .effort
            .as_deref()
            .map(|e| crate::model_picker::effort_label(e).to_owned());
        let current_mode = model.thread.permission_mode;
        let harness = model.thread.harness.clone();
        let scroller_state = state.scroller.clone();
        let following = scroller_state.read(cx).is_following_tail();
        // Clear of the details card floating over the right margin.
        let inset = self.details_inset;
        let pad = px(24. + inset);
        let pending = self
            .attachments
            .get(&thread_id)
            .cloned()
            .unwrap_or_default();
        let strip = crate::attachments::strip(&pending, p, {
            let view = cx.entity().downgrade();
            let id = thread_id.clone();
            move |ix, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    if let Some(list) = this.attachments.get_mut(&id)
                        && ix < list.len()
                    {
                        list.remove(ix);
                    }
                    cx.notify();
                });
            }
        });

        let timeline = if empty {
            v_flex()
                .flex_1()
                // As wide as the column and no wider: what doesn't fit on
                // a line wraps onto the next.
                .w_full()
                .min_w_0()
                .px_6()
                // Centred on the column left clear of the details card,
                // like the composer below.
                .pr(pad)
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(p.muted_foreground)
                .child(
                    div()
                        .max_w_full()
                        .text_xl()
                        .text_center()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.foreground)
                        .child(if is_chat {
                            "What's on your mind?"
                        } else {
                            "What should we work on?"
                        }),
                )
                .when_some(model.thread.parent.clone(), |d, parent| {
                    let title = self
                        .store
                        .read(cx)
                        .workspace
                        .threads
                        .iter()
                        .find(|t| t.id == parent.thread_id)
                        .map(|t| format!("“{}”", t.title))
                        .unwrap_or_else(|| "a deleted thread".into());
                    let how = match parent.kind {
                        loft_protocol::ForkKind::Native => format!(
                            "{} continues with the full conversation.",
                            harness_name(&model.thread.harness)
                        ),
                        loft_protocol::ForkKind::Handoff => {
                            "It starts from a handoff summary. Review it below, then send.".into()
                        }
                        loft_protocol::ForkKind::Spawned => {
                            "Started by that thread's agent.".into()
                        }
                    };
                    d.child(
                        h_flex()
                            .mt_2()
                            .gap_1p5()
                            .items_center()
                            .text_sm()
                            .child(Icon::new(IconName::GitFork).small())
                            .child(format!("Forked from {title}")),
                    )
                    .child(div().text_xs().child(how))
                })
                .when(model.thread.parent.is_none(), |d| {
                    d.child(harness_picker(
                        &model.thread.id,
                        &model.thread.harness,
                        harness_choices.clone(),
                        &self.store,
                        p,
                    ))
                    .when(!is_chat, |d| {
                        d.child(workspace_picker(
                            &model.thread.id,
                            &model.thread.workspace,
                            &self.store,
                            p,
                        ))
                        .children(base_row)
                    })
                })
                .child(
                    // Said in full: on a second line if one isn't enough.
                    div()
                        .w_full()
                        .max_w(px(520.))
                        .text_center()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(match &model.thread.workspace {
                            _ if is_chat => {
                                "A chat, not tied to a project: the agent works in an empty folder of its own"
                                    .to_owned()
                            }
                            ThreadWorkspace::NewWorktree { .. } => match &worktree_base {
                                Some(b) => format!(
                                    "Your first message creates a worktree on a new branch from {}",
                                    base_label(b)
                                ),
                                None => "Your first message creates an isolated worktree on a \
                                         new branch"
                                    .to_owned(),
                            },
                            ThreadWorkspace::Local => match &local_branch {
                                Some(b) => format!(
                                    "Works in {} on {b}, as it is now",
                                    tilde(&model.thread.cwd)
                                ),
                                None => format!("Works in {}", tilde(&model.thread.cwd)),
                            },
                            _ => format!("Works in {}", tilde(&model.thread.cwd)),
                        }),
                )
                .into_any_element()
        } else {
            let thread = thread.clone();
            let store = self.store.clone();
            let palette = p.clone();
            v_flex()
                .flex_1()
                .min_h_0()
                .children(fork_bar)
                .child(
                    MessageScroller::new("timeline", scroller_state, move |ix, window, cx| {
                        render_row(&thread, &store, &palette, ix, window, cx)
                    })
                    // It shows on every move, and following a working agent
                    // moves it all the time: only once you scroll away.
                    .scrollbar(!following)
                    .with_bottom_fade(p.background)
                    // Rows space themselves (`Palette::row_gap`); the
                    // scroller's own 32px gap under each row is too much.
                    .with_row_style(
                        gpui_kit::StyleRefinement::default()
                            .pb(px(0.))
                            .pr(px(inset)),
                    )
                    // The jump button's row spans the width; a right margin
                    // of the card's inset centres it on the rows instead.
                    .with_jump_button_style(gpui_kit::StyleRefinement::default().mr(px(inset)))
                    .flex_1(),
                )
                // Right click: Copy, and Quote, for what is selected.
                .context_menu(compose::timeline_menu(cx.entity().downgrade()))
                .into_any_element()
        };

        let drop_accent = p.accent;
        let action = if running {
            let id = thread_id.clone();
            round_button(
                "stop",
                IconName::Square,
                p.danger,
                gpui_kit::white(),
                cx.listener(move |this, _, _, cx| {
                    let id = id.clone();
                    this.store.update(cx, |s, cx| s.interrupt(id, cx));
                }),
            )
        } else {
            let id = thread_id.clone();
            let composer_h = composer.clone();
            round_button(
                "send",
                IconName::ArrowUp,
                p.accent,
                p.accent_foreground,
                cx.listener(move |this, _, window, cx| {
                    this.submit(&id, &composer_h, window, cx, false);
                }),
            )
        };

        v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            // Dragging the mini browser by its title bar moves it, kept
            // inside the conversation.
            .on_drag_move(cx.listener(
                |this, e: &gpui_kit::DragMoveEvent<DraggedPreview>, _, cx| {
                    let grab = e.drag(cx).grab.get();
                    let room = e.bounds;
                    // The card's top left (its border is outside the bar).
                    let left = e.event.position.x - grab.x - px(1.) - room.origin.x;
                    let top = e.event.position.y - grab.y - px(1.) - room.origin.y;
                    let at = gpui_kit::point(
                        room.size.width - left - px(PREVIEW_CARD_W),
                        room.size.height - top - px(PREVIEW_CARD_H),
                    );
                    this.browser_preview_at = Some(keep_preview_in(at, room.size));
                    cx.notify();
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .relative()
                    .child(timeline)
                    .children(turn_map)
                    .child({
                        let width = tl_width.clone();
                        let (drawn, visible) = {
                            let t = thread.read(cx);
                            (t.drawn_messages.clone(), t.visible_messages.clone())
                        };
                        gpui_kit::canvas(
                            move |bounds, window, _| {
                                let w = f32::from(bounds.size.width);
                                if (width.get() - w).abs() > 0.5 {
                                    width.set(w);
                                    window.refresh();
                                }
                                // Your messages drawn this frame that are in
                                // view (the list draws a little beyond it).
                                let mut now: Vec<usize> = drawn
                                    .borrow_mut()
                                    .drain()
                                    .filter(|(_, b)| {
                                        b.bottom() > bounds.top() && b.top() < bounds.bottom()
                                    })
                                    .map(|(row, _)| row)
                                    .collect();
                                now.sort_unstable();
                                if *visible.borrow() != now {
                                    *visible.borrow_mut() = now;
                                    window.refresh();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full()
                    }),
            )
            .child(
                div()
                    .px_6()
                    .pr(pad)
                    .pb_5()
                    .pt_1()
                    .flex()
                    .justify_center()
                    .child(
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .max_w(p.content_width)
                            .gap_1()
                            .when(!pinned.is_empty(), |d| {
                                d.child(v_flex().gap_2().pb_2().children(pinned))
                            })
                            .when(waiting, |d| {
                                d.child(
                                    h_flex()
                                        .gap_1p5()
                                        .px_2()
                                        .text_xs()
                                        .text_color(p.warning)
                                        .child(Icon::new(IconName::ShieldCheck).xsmall())
                                        .child(format!(
                                            "{} is waiting for your approval",
                                            harness_name(&model.thread.harness)
                                        )),
                                )
                            })
                            .children(
                                self.completion
                                    .as_ref()
                                    .filter(|c| c.thread_id == thread_id)
                                    .map(|c| {
                                        let view = cx.entity().downgrade();
                                        crate::completion::popover(c, p, move |ix, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.accept_completion(Some(ix), window, cx)
                                            });
                                        })
                                    }),
                            )
                            .child(
                                v_flex()
                                    .id("composer-card")
                                    .p_2()
                                    .gap_2()
                                    .rounded(px(18.))
                                    .border_1()
                                    .border_color(p.border)
                                    .bg(p.elevated)
                                    .shadow_lg()
                                    // While completions show, ↑/↓/⏎/⇥/esc drive them
                                    // instead of the textarea.
                                    // With nothing typed (or a recalled prompt
                                    // as it came), they recall sent prompts.
                                    .capture_action({
                                        let id = thread_id.clone();
                                        cx.listener(
                                            move |this,
                                                  _: &gpui_kit::component::input::MoveUp,
                                                  window,
                                                  cx| {
                                                if let Some(c) = &mut this.completion {
                                                    c.move_by(-1);
                                                    cx.stop_propagation();
                                                    cx.notify();
                                                } else if this.recall_step(&id, true, window, cx) {
                                                    cx.stop_propagation();
                                                }
                                            },
                                        )
                                    })
                                    .capture_action({
                                        let id = thread_id.clone();
                                        cx.listener(
                                            move |this,
                                                  _: &gpui_kit::component::input::MoveDown,
                                                  window,
                                                  cx| {
                                                if let Some(c) = &mut this.completion {
                                                    c.move_by(1);
                                                    cx.stop_propagation();
                                                    cx.notify();
                                                } else if this.recall_step(&id, false, window, cx) {
                                                    cx.stop_propagation();
                                                }
                                            },
                                        )
                                    })
                                    .capture_action(cx.listener(
                                        |this,
                                         _: &gpui_kit::component::input::Enter,
                                         window,
                                         cx| {
                                            if this.completion.is_some() {
                                                cx.stop_propagation();
                                                this.accept_completion(None, window, cx);
                                            }
                                        },
                                    ))
                                    .capture_action(cx.listener(
                                        |this,
                                         _: &gpui_kit::component::input::IndentInline,
                                         window,
                                         cx| {
                                            if this.completion.is_some() {
                                                cx.stop_propagation();
                                                this.accept_completion(None, window, cx);
                                            }
                                        },
                                    ))
                                    .capture_action(cx.listener(
                                        |this, _: &gpui_kit::component::input::Escape, _, cx| {
                                            if this.completion.take().is_some() {
                                                cx.stop_propagation();
                                                cx.notify();
                                            }
                                        },
                                    ))
                                    // ⌘V with an image on the clipboard attaches it;
                                    // text pastes fall through to the textarea.
                                    .capture_action({
                                        let id = thread_id.clone();
                                        cx.listener(
                                            move |this,
                                                  _: &gpui_kit::component::input::Paste,
                                                  _,
                                                  cx| {
                                                let Some(item) = cx.read_from_clipboard() else {
                                                    return;
                                                };
                                                let images =
                                                    crate::attachments::from_clipboard(&item);
                                                if !images.is_empty() {
                                                    cx.stop_propagation();
                                                    this.add_images(&id, images, cx);
                                                } else if let Some(text) = item.text()
                                                    && is_large_paste(&text)
                                                {
                                                    // A chip, not a wall of text.
                                                    cx.stop_propagation();
                                                    this.pasted
                                                        .entry(id.clone())
                                                        .or_default()
                                                        .push(text);
                                                    cx.notify();
                                                }
                                            },
                                        )
                                    })
                                    .drag_over::<gpui_kit::ExternalPaths>(move |d, _, _, _| {
                                        d.border_color(drop_accent)
                                    })
                                    .on_drop({
                                        let id = thread_id.clone();
                                        cx.listener(
                                            move |this, paths: &gpui_kit::ExternalPaths, _, cx| {
                                                this.attach_paths(
                                                    id.clone(),
                                                    paths.paths().to_vec(),
                                                    cx,
                                                );
                                            },
                                        )
                                    })
                                    // A thread dragged here is context for
                                    // the message (over the pane's drop zones).
                                    .children(self.composer_drop_target(&thread_id, p, cx))
                                    .children(strip)
                                    .children(self.note_chips(&thread_id, p, cx))
                                    .children(self.paste_chips(&thread_id, p, cx))
                                    .child(
                                        div()
                                            .key_context("Composer")
                                            .px_2()
                                            .pt_1()
                                            // ⌘↩ (Ctrl+Enter): into the running turn now.
                                            .on_action({
                                                let id = thread_id.clone();
                                                let composer = composer.clone();
                                                cx.listener(
                                                    move |this, _: &SteerMessage, window, cx| {
                                                        this.steer_or_approve(
                                                            &id, &composer, window, cx,
                                                        );
                                                    },
                                                )
                                            })
                                            .child(Textarea::new(&composer).appearance(false)),
                                    )
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .justify_between()
                                            .gap_2()
                                            .child(
                                                h_flex()
                                                    .gap_1()
                                                    // In a narrow column the
                                                    // model's name gives way,
                                                    // never the send button.
                                                    .flex_1()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    // Picking files uses this
                                                    // computer's file dialog.
                                                    .when(!crate::host::IS_WEB, |d| {
                                                        let id = thread_id.clone();
                                                        d.child(icon_button(
                                                            "attach",
                                                            IconName::Paperclip,
                                                            p,
                                                            cx.listener(move |this, _, _, cx| {
                                                                this.pick_images(id.clone(), cx)
                                                            }),
                                                        ))
                                                    })
                                                    .child(self.model_picker_button(
                                                        &thread_id,
                                                        &harness,
                                                        model_name,
                                                        effort_name,
                                                        current_mode,
                                                        p,
                                                        window,
                                                        cx,
                                                    ))
                                                    .children(self.context_button(
                                                        &thread_id, &harness, p, cx,
                                                    )),
                                            )
                                            .child(
                                                h_flex()
                                                    .flex_none()
                                                    .gap_2()
                                                    .items_center()
                                                    .when(running && !stop_hint.is_empty(), |d| {
                                                        d.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(p.faint_foreground)
                                                                .child(format!(
                                                                    "{stop_hint} to stop"
                                                                )),
                                                        )
                                                    })
                                                    .child(action),
                                            ),
                                    ),
                            ),
                    ),
            )
            // Last, so it floats over the conversation.
            .when(preview.is_some(), |d| {
                let room = self.preview_room.clone();
                d.child(
                    gpui_kit::canvas(move |bounds, _, _| room.set(bounds.size), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
            })
            .children(preview)
            .into_any_element()
    }

    /// Whether the sidebar lists `thread`. A new thread isn't one yet: it
    /// shows once it has a message, or something typed for one (a saved
    /// draft, text in its composer, or pictures waiting to go with it).
    fn lists_thread(&self, thread: &loft_protocol::Thread) -> bool {
        thread.is_started()
            || thread.parent.is_some()
            || thread.draft_text().is_some()
            || self.typed.contains(&thread.id)
            || self
                .attachments
                .get(&thread.id)
                .is_some_and(|waiting| !waiting.is_empty())
    }

    /// "From ⎇ main ▾ · Local | origin" under the workspace picker.
    #[allow(clippy::too_many_arguments)]
    fn worktree_base_row(
        &mut self,
        thread_id: &ThreadId,
        project_id: &loft_protocol::ProjectId,
        base: Option<loft_protocol::WorktreeBase>,
        list: &loft_protocol::BranchList,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::popover::Popover;
        let picker = match &self.branch_picker {
            Some(b) if b.read(cx).thread_id() == thread_id => b.clone(),
            _ => {
                let (store, t, pid) = (self.store.clone(), thread_id.clone(), project_id.clone());
                let picker =
                    cx.new(|cx| crate::branch_picker::BranchPicker::new(store, t, pid, window, cx));
                let sub = cx.subscribe(&picker, |this, _, event, cx| match event {
                    crate::branch_picker::BranchPickerEvent::Close => {
                        this.branch_picker_open = false;
                        cx.notify();
                    }
                });
                self._subs.push(sub);
                self.branch_picker = Some(picker.clone());
                if self.branch_picker_open {
                    picker.update(cx, |p, cx| p.reset(window, cx));
                }
                picker
            }
        };
        let focus = picker.read(cx).search_focus(cx);
        let open = self.branch_picker_open && self.rendering_focused;
        let branch = base.as_ref().map(|b| b.branch.clone());
        let trigger = Button::new("worktree-base")
            .cursor_pointer()
            .ghost()
            .xsmall()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(Icon::new(IconName::GitBranch).xsmall().text_color(p.accent))
                    .child(
                        div()
                            .max_w(px(220.))
                            .truncate()
                            .text_color(p.foreground)
                            .child(branch.clone().unwrap_or_else(|| "Choose a branch".into())),
                    )
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .text_color(p.faint_foreground),
                    ),
            );
        let branch_popover = Popover::new("worktree-base-popover")
            .anchor(gpui_kit::Anchor::TopCenter)
            .offset(px(6.))
            .p_0()
            .open(open)
            .track_focus(&focus)
            .on_open_change(cx.listener({
                let picker = picker.clone();
                move |this, open: &bool, window, cx| {
                    this.branch_picker_open = *open;
                    if *open {
                        picker.update(cx, |p, cx| p.reset(window, cx));
                    }
                    cx.notify();
                }
            }))
            .trigger(trigger)
            .content(move |_, _, _| picker.clone());

        // Local copy or the remote's. Switching lands on that source's main
        // branch; the branch list then shows only its branches.
        let remote = crate::branch_picker::main_remote(list);
        let from_remote = base.as_ref().is_some_and(|b| b.remote.is_some());
        let mut source = h_flex().gap(px(2.)).p(px(2.)).rounded(px(8.)).bg(p.muted);
        for (is_remote, label, icon, available) in [
            (
                false,
                "Local".to_owned(),
                IconName::Laptop,
                !list.local.is_empty(),
            ),
            (
                true,
                remote.clone().unwrap_or_else(|| "origin".into()),
                IconName::Cloud,
                remote.is_some(),
            ),
        ] {
            let active = from_remote == is_remote;
            let (store, t) = (self.store.clone(), thread_id.clone());
            let target = crate::branch_picker::source_default(
                list,
                if is_remote { remote.clone() } else { None },
            );
            source = source.child(
                h_flex()
                    .id(SharedString::from(format!("base-source-{is_remote}")))
                    .gap_1()
                    .items_center()
                    .px_2()
                    .py(px(2.))
                    .rounded(px(6.))
                    .text_xs()
                    .when(active, |d| {
                        d.bg(p.elevated)
                            .text_color(p.foreground)
                            .font_weight(FontWeight::MEDIUM)
                            .shadow_sm()
                    })
                    .when(!active && available, |d| {
                        d.cursor_pointer()
                            .text_color(p.muted_foreground)
                            .hover(|d| d.text_color(p.foreground))
                    })
                    .when(!available, |d| {
                        d.text_color(p.faint_foreground).opacity(0.5)
                    })
                    .child(Icon::new(icon).xsmall())
                    .child(label)
                    .when(!active && available, |d| {
                        d.on_click(move |_, _, cx| {
                            let (t, target) = (t.clone(), target.clone());
                            store.update(cx, |s, cx| s.set_thread_workspace(t, true, target, cx));
                        })
                    }),
            );
        }
        h_flex()
            .gap_2()
            .items_center()
            .text_xs()
            .text_color(p.faint_foreground)
            .child("From")
            .child(branch_popover)
            .child(source)
            .into_any_element()
    }

    /// The floating mini browser (bottom right of the conversation).
    fn browser_preview_card(
        &mut self,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.browser_preview || self.browser_in_panel(cx) {
            return None;
        }
        let thread_id = self.store.read(cx).selected.clone()?;
        let browser = self.panel.read(cx).existing_browser(&thread_id, cx)?;
        const W: f32 = PREVIEW_W;
        const H: f32 = PREVIEW_H;
        let (title, surface) = {
            let b = browser.read(cx);
            let title = b
                .title()
                .map(str::to_owned)
                .or_else(|| b.url().map(str::to_owned))
                .unwrap_or_else(|| "Browser".into());
            (title, b.preview_surface(W)?)
        };
        let loading = browser.read(cx).is_loading();
        let at = match self.browser_preview_at {
            Some(at) => keep_preview_in(at, self.preview_room.get()),
            None => gpui_kit::point(px(20.), px(150.)),
        };
        Some(
            v_flex()
                .absolute()
                .right(at.x)
                .bottom(at.y)
                .w(px(W + 2.))
                .rounded(px(12.))
                .overflow_hidden()
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .shadow_lg()
                .child(
                    h_flex()
                        .id("preview-bar")
                        .h(px(30.))
                        .px_2()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .border_b_1()
                        .border_color(p.border)
                        .cursor(gpui_kit::CursorStyle::OpenHand)
                        .on_drag(DraggedPreview::default(), |d, grab, _, cx| {
                            d.grab.set(grab);
                            cx.new(|_| d.clone())
                        })
                        .child(if loading {
                            Spinner::new()
                                .xsmall()
                                .color(p.muted_foreground)
                                .into_any_element()
                        } else {
                            Icon::new(IconName::Globe)
                                .xsmall()
                                .text_color(p.muted_foreground)
                                .into_any_element()
                        })
                        .child(div().flex_1().min_w_0().truncate().child(title))
                        .child(
                            div()
                                .id("preview-expand")
                                .p(px(3.))
                                .rounded(px(5.))
                                .cursor_pointer()
                                .text_color(p.muted_foreground)
                                .hover(|d| d.bg(p.hover).text_color(p.foreground))
                                .child(Icon::new(IconName::Maximize2).xsmall())
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(
                                        "Open in the side panel",
                                    )
                                    .build(window, cx)
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.browser_preview = false;
                                    this.panel_open = true;
                                    this.panel
                                        .update(cx, |p, cx| p.show(Tab::Browser, window, cx));
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("preview-close")
                                .p(px(3.))
                                .rounded(px(5.))
                                .cursor_pointer()
                                .text_color(p.muted_foreground)
                                .hover(|d| d.bg(p.hover).text_color(p.foreground))
                                .child(Icon::new(IconName::X).xsmall())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.browser_preview = false;
                                    cx.notify();
                                })),
                        ),
                )
                .child(div().w(px(W)).h(px(H)).flex().flex_col().child(surface))
                .into_any_element(),
        )
    }

    /// The picker for `thread_id`, made on first use and remade when the
    /// selected thread changes.
    fn model_picker_for(
        &mut self,
        thread_id: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<crate::model_picker::ModelPicker> {
        if let Some(picker) = &self.model_picker
            && picker.read(cx).thread_id() == thread_id
        {
            return picker.clone();
        }
        let store = self.store.clone();
        let id = thread_id.clone();
        let picker = cx.new(|cx| crate::model_picker::ModelPicker::new(store, id, window, cx));
        let sub = cx.subscribe_in(&picker, window, |this, _, event, window, cx| match event {
            crate::model_picker::PickerEvent::Close => {
                this.picker_open = false;
                if let Some(id) = this.store.read(cx).selected.clone()
                    && let Some(composer) = this.composers.get(&id)
                {
                    composer.update(cx, |c, cx| c.focus(window, cx));
                }
                cx.notify();
            }
        });
        self._subs.push(sub);
        self.picker_open = false;
        self.model_picker = Some(picker.clone());
        picker
    }

    /// Composer chip showing the agent, model and effort; opens the picker.
    #[allow(clippy::too_many_arguments)]
    fn model_picker_button(
        &mut self,
        thread_id: &ThreadId,
        harness: &HarnessId,
        model_name: String,
        effort_name: Option<String>,
        mode: Option<loft_protocol::PermissionMode>,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::popover::Popover;
        let picker = self.model_picker_for(thread_id, window, cx);
        let focus = picker.read(cx).search_focus(cx);
        let open = self.picker_open && self.rendering_focused;
        let trigger = Button::new("model-picker")
            .cursor_pointer()
            .ghost()
            .xsmall()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(crate::brand::agent_logo(harness.as_str(), px(13.), p))
                    .child(div().text_color(p.foreground).child(model_name))
                    .when_some(effort_name, |d, effort| {
                        d.child(div().text_color(p.faint_foreground).child("·"))
                            .child(div().text_color(p.muted_foreground).child(effort))
                    })
                    // The permission mode, when one is chosen.
                    .when(mode.is_some(), |d| {
                        let full = mode == Some(loft_protocol::PermissionMode::FullAccess);
                        d.child(div().text_color(p.faint_foreground).child("·"))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .text_color(if full { p.warning } else { p.muted_foreground })
                                    .child(Icon::new(crate::model_picker::mode_icon(mode)).xsmall())
                                    .child(crate::model_picker::mode_short(mode)),
                            )
                    })
                    .child(
                        Icon::new(if open {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall()
                        .text_color(p.faint_foreground),
                    ),
            );
        Popover::new("model-picker-popover")
            .anchor(gpui_kit::Anchor::BottomLeft)
            .offset(px(8.))
            .p_0()
            .open(open)
            .track_focus(&focus)
            .on_open_change(cx.listener({
                let picker = picker.clone();
                move |this, open: &bool, window, cx| {
                    this.picker_open = *open;
                    if *open {
                        picker.update(cx, |p, cx| p.reset(window, cx));
                    }
                    cx.notify();
                }
            }))
            .trigger(trigger)
            .content(move |_, _, _| picker.clone())
            .into_any_element()
    }

    /// A small ring by the model chip, filled as far as the agent's context
    /// is; it opens the thread's numbers (context, tokens used, cost) and
    /// the compact-when-idle switch. Nothing until a turn has reported any.
    fn context_button(
        &self,
        thread_id: &ThreadId,
        harness: &HarnessId,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use gpui_kit::component::Disableable as _;
        use gpui_kit::component::button::{Button, ButtonVariants as _};
        use gpui_kit::component::popover::Popover;
        let store = self.store.clone();
        let id = thread_id.clone();
        let stats = thread_stats(&store, &id, cx)?;
        let fraction = stats.fraction();
        let tip = match (fraction, stats.context) {
            (Some(f), _) => format!("Context {}% full", (f * 100.).round() as i64),
            (None, Some(c)) => format!("Context {} tokens", short_tokens(c.tokens)),
            (None, None) => format!("{} tokens used", short_tokens(stats.total())),
        };
        let trigger = Button::new("context-ring")
            .cursor_pointer()
            .ghost()
            .xsmall()
            .compact()
            .tooltip(tip)
            .child(context_ring(
                fraction.unwrap_or(0.),
                context_color(fraction, p),
                p.faint_foreground.opacity(0.45),
            ));
        let agent = harness_name(harness);
        let compacts = crate::model_picker::can_compact(harness);
        Some(
            Popover::new("context-popover")
                .anchor(gpui_kit::Anchor::BottomLeft)
                .offset(px(8.))
                .p_0()
                .trigger(trigger)
                .content(move |_, _, cx| {
                    let p = Palette::of(cx);
                    let stats = thread_stats(&store, &id, cx).unwrap_or_default();
                    let fraction = stats.fraction();
                    let color = context_color(fraction, &p);
                    let numbers = match (stats.context, fraction) {
                        (Some(c), Some(f)) => format!(
                            "{}% · {} / {}",
                            (f * 100.).round() as i64,
                            short_tokens(c.tokens),
                            short_tokens(c.window.unwrap_or_default())
                        ),
                        (Some(c), None) => format!("{} tokens", short_tokens(c.tokens)),
                        (None, _) => "Not reported yet".into(),
                    };
                    let row = |label: &'static str, value: String, p: &Palette| {
                        h_flex()
                            .items_center()
                            .text_xs()
                            .child(div().flex_1().text_color(p.muted_foreground).child(label))
                            .child(div().text_color(p.foreground).child(value))
                    };
                    let total = stats.total();
                    let split = [
                        (stats.input, "input"),
                        (stats.cache_read, "cached"),
                        (stats.cache_write, "cache write"),
                        (stats.output, "output"),
                    ]
                    .iter()
                    .filter(|(n, _)| *n > 0)
                    .map(|(n, what)| format!("{} {what}", short_tokens(*n)))
                    .collect::<Vec<_>>()
                    .join(" · ");
                    v_flex()
                        .w(px(340.))
                        .rounded(px(12.))
                        .overflow_hidden()
                        .child(
                            v_flex()
                                .gap_2()
                                .px_3()
                                .py_2p5()
                                .child(
                                    h_flex()
                                        .items_center()
                                        .text_sm()
                                        .child(
                                            div()
                                                .flex_1()
                                                .font_weight(FontWeight::MEDIUM)
                                                .child("Context window"),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(p.muted_foreground)
                                                .child(numbers),
                                        ),
                                )
                                .when_some(fraction, |d, f| {
                                    d.child(
                                        div().h(px(6.)).w_full().rounded_full().bg(p.muted).child(
                                            div()
                                                .h_full()
                                                .rounded_full()
                                                .w(gpui_kit::relative(f.clamp(0., 1.)))
                                                .bg(color),
                                        ),
                                    )
                                })
                                .when(compacts, |d| {
                                    let busy = store
                                        .read(cx)
                                        .threads
                                        .get(&id)
                                        .and_then(|t| t.read(cx).model.as_ref())
                                        .is_some_and(|m| {
                                            m.thread.status != loft_protocol::ThreadStatus::Idle
                                        });
                                    let (store, id) = (store.clone(), id.clone());
                                    let popover = cx.entity().downgrade();
                                    d.child(
                                        h_flex()
                                            .gap_3()
                                            .items_center()
                                            .child(
                                                // Wraps, so the button
                                                // beside it keeps its room.
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_xs()
                                                    .text_color(p.muted_foreground)
                                                    .child(format!(
                                                        "{agent} also compacts by itself when the context fills up."
                                                    )),
                                            )
                                            .child(
                                                Button::new("context-compact-now")
                                                    .flex_none()
                                                    .cursor_pointer()
                                                    .outline()
                                                    .xsmall()
                                                    .icon(IconName::FoldVertical)
                                                    .label("Compact now")
                                                    .disabled(busy)
                                                    .tooltip(if busy {
                                                        "Waits for the running turn to end"
                                                    } else {
                                                        "Summarize the conversation so far (/compact)"
                                                    })
                                                    .on_click(move |_, window, cx| {
                                                        let id = id.clone();
                                                        store.update(cx, |s, cx| {
                                                            s.send_message(
                                                                id,
                                                                "/compact".into(),
                                                                Vec::new(),
                                                                cx,
                                                            )
                                                        });
                                                        // Done here: close the popover.
                                                        let _ = popover.update(cx, |s, cx| {
                                                            s.dismiss(window, cx)
                                                        });
                                                    }),
                                            ),
                                    )
                                }),
                        )
                        // What the thread has cost so far.
                        .when(total > 0 || stats.cost.is_some(), |d| {
                            d.child(
                                v_flex()
                                    .gap_1()
                                    .px_3()
                                    .py_2()
                                    .border_t_1()
                                    .border_color(p.border)
                                    .when(total > 0, |d| {
                                        d.child(row(
                                            "Tokens used in this thread",
                                            short_tokens(total),
                                            &p,
                                        ))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(p.faint_foreground)
                                                .child(split),
                                        )
                                    })
                                    .when_some(stats.cost, |d, cost| {
                                        d.child(row(
                                            if stats.estimated {
                                                "Cost (API estimate)"
                                            } else {
                                                "Cost"
                                            },
                                            format_cost(cost),
                                            &p,
                                        ))
                                    })
                                    .child({
                                        let store = store.clone();
                                        let popover = cx.entity().downgrade();
                                        h_flex()
                                            .id("context-usage-link")
                                            .gap_1()
                                            .items_center()
                                            .pt_1()
                                            .text_xs()
                                            .text_color(p.accent)
                                            .cursor_pointer()
                                            .hover(|d| d.underline())
                                            .child("All threads, by day, and plan limits")
                                            .child(Icon::new(IconName::ChevronRight).xsmall())
                                            .on_click(move |_, window, cx| {
                                                let _ = popover
                                                    .update(cx, |s, cx| s.dismiss(window, cx));
                                                crate::settings::open(
                                                    store.clone(),
                                                    Some(crate::settings::Section::Usage),
                                                    cx,
                                                );
                                            })
                                    }),
                            )
                        })
                        .when(compacts, |d| {
                            d.child(crate::model_picker::compaction_row(
                                &store,
                                "context-compact",
                                &p,
                                cx,
                            ))
                        })
                })
                .into_any_element(),
        )
    }

    fn render_welcome(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let has_projects = !self.store.read(cx).workspace.projects.is_empty();
        // Which agents are ready (onboarding): version, or how to install.
        let statuses = self.store.read(cx).harness_status.clone();
        let mut agents = v_flex().gap_1p5().pt_4().text_sm();
        for (id, name, install) in [
            (
                HarnessId::CLAUDE,
                "Claude Code",
                "npm i -g @anthropic-ai/claude-code",
            ),
            (HarnessId::CODEX, "Codex", "npm i -g @openai/codex"),
            (HarnessId::OPENCODE, "OpenCode", ""),
            (HarnessId::GEMINI, "Gemini CLI", ""),
        ] {
            let Some(status) = statuses.get(&HarnessId(id.into())) else {
                continue;
            };
            // Optional ACP agents are listed only when present.
            if install.is_empty() && !status.installed {
                continue;
            }
            let ok = status.installed && status.error.is_none();
            agents = agents.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(if ok {
                            IconName::CircleCheck
                        } else {
                            IconName::CircleAlert
                        })
                        .small()
                        .text_color(if ok {
                            p.success
                        } else {
                            p.warning
                        }),
                    )
                    .child(div().font_weight(FontWeight::MEDIUM).child(name))
                    .child(
                        div()
                            .text_color(p.muted_foreground)
                            .when(ok, |d| d.child(status.version.clone().unwrap_or_default()))
                            .when(!ok, |d| {
                                d.child("not installed — ").child(
                                    div().font_family(p.mono.clone()).text_xs().child(install),
                                )
                            }),
                    ),
            );
        }
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .child(crate::brand::app_icon(px(88.)))
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Welcome to Agentloft"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(if has_projects {
                        "Pick a thread on the left, or start a new one."
                    } else {
                        "Add a project folder to start working with Claude."
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_2()
                    .child(pill_button(
                        "welcome-add",
                        IconName::FolderPlus,
                        "Add project",
                        crate::keymap::hint("add_project", cx),
                        p,
                        cx.listener(|this, _, window, cx| {
                            crate::new_in::open_sources(cx.entity(), &this.store, window, cx)
                        }),
                    ))
                    .when(has_projects, |d| {
                        d.child(pill_button(
                            "welcome-new",
                            IconName::SquarePen,
                            "New thread",
                            crate::keymap::hint("new_thread", cx),
                            p,
                            cx.listener(|this, _, _, cx| this.new_thread(None, cx)),
                        ))
                    }),
            )
            .child(agents)
            .into_any_element()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::panel::page::frame_drawn(window);
        let p = Palette::of(cx);
        if std::mem::take(&mut self.pending_add_project) {
            cx.defer_in(window, |this, window, cx| {
                this.add_project(window, cx);
            });
        }
        let automations_open = self.store.read(cx).automations_open;
        // The Automations page takes the thread's place until one is picked.
        let selected = self
            .store
            .read(cx)
            .selected_thread()
            .filter(|_| !automations_open);
        if let Some(thread) = &selected
            && thread.read(cx).model.is_some()
            && let Some(dev) = self.dev_tab.take()
        {
            self.apply_dev_tab(&dev, thread, window, cx);
        }
        let disconnected = matches!(self.store.read(cx).conn, ConnState::Disconnected(_));

        let header_title = selected.as_ref().and_then(|t| {
            let model = t.read(cx).model.as_ref()?;
            let store = self.store.read(cx);
            let project = store
                .workspace
                .projects
                .iter()
                .find(|p| p.id == model.thread.project_id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            Some((project, model.thread.without_commands()))
        });

        self.details_amount = self
            .details_slide
            .update(self.details_open && self.can_dock(), window);
        self.details_inset = 0.;
        let body = match selected {
            _ if automations_open => {
                let store = self.store.clone();
                self.automations_page
                    .get_or_insert_with(|| {
                        cx.new(|_| crate::automations::AutomationsPage::new(store))
                    })
                    .clone()
                    .into_any_element()
            }
            Some(thread) => self.render_panes(thread, &p, window, cx),
            None => self.render_welcome(&p, cx),
        };
        let body = self.with_details_dock(body, window, cx);
        let show_panel = self.panel_open && header_title.is_some();
        let show_dock = self.dock_open && header_title.is_some();
        if show_dock && self.panel.read(cx).dock_len(cx) == 0 {
            // A thread picked with the dock open gets a shell of its own.
            self.panel
                .update(cx, |p, cx| p.ensure_docked(false, window, cx));
        }
        let body = self.with_terminal_dock(body, show_dock, &p, window);
        let mut body_slot = Some(body);
        let side = self.sidebar_slide.update(self.sidebar_open, window);
        self.sidebar_amount = side;
        // What the conversation and the side panel have to share.
        let room = f32::from(window.viewport_size().width) - SIDEBAR_WIDTH * side;
        // Not enough for both at a usable width: the panel takes the
        // conversation's place, as when it is asked to fill the window,
        // rather than squeeze the conversation to a sliver.
        let cramped = room < CONVERSATION_MIN + PANEL_FLOOR;
        if self.panel.read(cx).cramped != cramped {
            self.panel.update(cx, |panel, _| panel.cramped = cramped);
        }
        let maximized = show_panel && (cramped || self.panel.read(cx).maximized);
        // The conversation keeps its width: the panel is as wide as what
        // is left allows, whether it is being dragged or was left wide in
        // a window that has since shrunk. In a tight window that is less
        // than the panel would otherwise go down to.
        let panel_max = (room - CONVERSATION_MIN).max(PANEL_FLOOR);
        let panel_min = panel_max.min(PANEL_MIN);
        if show_panel && self.panel_width.get() <= 0. {
            self.panel_width.set(default_panel_width(room));
        }
        if show_panel && !maximized {
            self.panel_width
                .set(self.panel_width.get().clamp(panel_min, panel_max));
        }
        let header = self.render_header(header_title, &p, cx);
        let sidebar = (side > 0.).then(|| {
            div()
                .flex_none()
                .h_full()
                .w(px(SIDEBAR_WIDTH * side))
                .overflow_hidden()
                .child(self.render_sidebar(&p, cx))
        });
        let panel_amount = self.panel_slide.update(show_panel && !maximized, window);
        let panel_sliding = panel_amount > 0. && panel_amount < 1.;
        let panel_width = self.panel_width.clone();

        v_flex()
            .key_context(CONTEXT)
            // Esc lets go of threads picked together in the sidebar.
            .on_key_down(cx.listener(|this, e: &gpui_kit::KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" && !this.multi.is_empty() {
                    this.multi.clear();
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &NewThread, _, cx| this.new_thread(None, cx)))
            .on_action(
                cx.listener(|this, _: &NewChat, _, cx| {
                    this.store.update(cx, |s, cx| s.new_chat(cx))
                }),
            )
            .on_action(cx.listener(|this, _: &Interrupt, _, cx| this.interrupt(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .on_action(cx.listener(|_, _: &ToggleTheme, _, cx| theme::toggle_mode(cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.open_settings(cx)))
            .on_action(cx.listener(|this, _: &AddProject, window, cx| {
                crate::new_in::open_sources(cx.entity(), &this.store, window, cx)
            }))
            .on_action(cx.listener(|_, _: &About, window, cx| show_about(window, cx)))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                crate::palette::open(cx.entity(), &this.store, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &QuoteSelection, window, cx| {
                    this.quote_selection(window, cx)
                }),
            )
            .on_action(cx.listener(|_, _: &NewWindow, _, cx| {
                crate::windows::open(None, crate::windows::Placement::Window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.new_pane(cx)))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.sidebar_open = true;
                this.search.update(cx, |s, cx| s.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &ApproveOnce, _, cx| {
                this.answer(loft_protocol::DecisionKind::AllowOnce, cx)
            }))
            .on_action(cx.listener(|this, _: &ApproveSession, _, cx| {
                this.answer(loft_protocol::DecisionKind::AllowSession, cx)
            }))
            .on_action(cx.listener(|this, _: &DenyRequest, _, cx| {
                this.answer(loft_protocol::DecisionKind::Deny, cx)
            }))
            .on_action(cx.listener(|this, _: &TogglePanel, _, cx| {
                this.panel_open = !this.panel_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowFiles, w, cx| this.show_tab(Tab::Files, w, cx)))
            .on_action(
                cx.listener(|this, _: &ShowChanges, w, cx| this.show_tab(Tab::Changes, w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ShowTerminal, w, cx| this.show_tab(Tab::Terminal, w, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleTerminal, w, cx| this.toggle_terminal(w, cx)))
            .on_action(
                cx.listener(|this, _: &ShowBrowser, w, cx| this.show_tab(Tab::Browser, w, cx)),
            )
            .size_full()
            .bg(p.background)
            .text_color(p.foreground)
            .child(
                h_flex().flex_1().min_h_0().children(sidebar).child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(header)
                        .when(disconnected, |d| {
                            d.child(
                                h_flex()
                                    .justify_center()
                                    .gap_2()
                                    .py_1p5()
                                    .bg(p.warning_soft)
                                    .text_xs()
                                    .text_color(p.warning)
                                    .child(Spinner::new().xsmall().color(p.warning))
                                    .child("Lost connection to agentloftd — reconnecting…"),
                            )
                        })
                        .when(maximized, |d| {
                            d.child(div().flex_1().min_h_0().w_full().child(self.panel.clone()))
                        })
                        .when(!maximized && panel_sliding, |d| {
                            // Mid-slide: a plain row at the panel's width.
                            let shown = self.panel_width.get() * panel_amount;
                            // A web page is a native view (or the picture of
                            // one): nothing can clip it, so it is made as
                            // narrow as what shows. Everything else keeps
                            // the panel's full width and is cut off at the
                            // edge, as the dock is. Squeezed, a terminal was
                            // told a new width on every frame of the slide,
                            // and left two columns wide once the panel had
                            // shut: whatever a program printed until it
                            // opened again was wrapped at two.
                            let squeezed = self.panel.read(cx).active_tab(cx) == Some(Tab::Browser);
                            let panel = if squeezed {
                                div()
                                    .flex_none()
                                    .h_full()
                                    .w(px(shown))
                                    .overflow_hidden()
                                    .border_l_1()
                                    .border_color(p.border)
                                    .child(self.panel.clone())
                            } else {
                                div()
                                    .flex_none()
                                    .h_full()
                                    .w(px(shown))
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .flex_none()
                                            .h_full()
                                            .w(px(self.panel_width.get()))
                                            .border_l_1()
                                            .border_color(p.border)
                                            .child(self.panel.clone()),
                                    )
                            };
                            d.child(
                                h_flex()
                                    .flex_1()
                                    .min_h_0()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .h_full()
                                            .children(body_slot.take()),
                                    )
                                    .child(panel),
                            )
                        })
                        .when(!maximized && !panel_sliding, |d| {
                            // A box holding exactly the room under the header:
                            // the group is "100% tall", and would otherwise
                            // count the header too (and grow with tall content),
                            // pushing the bottom of the panel off the window.
                            d.child(
                                div().flex_1().min_h_0().w_full().child(
                                    h_resizable("workbench")
                                        .child(
                                            resizable_panel()
                                                // Beside the panel, the
                                                // conversation keeps a width
                                                // it can be read and written
                                                // at; alone, it has the lot.
                                                .when(show_panel, |d| {
                                                    d.size_range(px(CONVERSATION_MIN)..px(100_000.))
                                                })
                                                .children(body_slot.take()),
                                        )
                                        .child(
                                            resizable_panel()
                                                .size(px(self.panel_width.get()))
                                                .size_range(px(panel_min)..px(panel_max))
                                                .visible(show_panel)
                                                .child(
                                                    div()
                                                        .size_full()
                                                        .relative()
                                                        .border_l_1()
                                                        .border_color(p.border)
                                                        .child(self.panel.clone())
                                                        .child(
                                                            gpui_kit::canvas(
                                                                move |bounds, _, _| {
                                                                    panel_width.set(f32::from(
                                                                        bounds.size.width,
                                                                    ))
                                                                },
                                                                |_, _, _, _| {},
                                                            )
                                                            .absolute()
                                                            .size_full(),
                                                        ),
                                                ),
                                        ),
                                ),
                            )
                        }),
                ),
            )
            // Where there is one window only (a browser tab): settings and
            // menus that otherwise get a window of their own.
            .children(crate::settings::overlay(cx))
            .children(crate::details::overlay(window, cx))
            .children(loft_browser::menu::overlay(window, cx))
    }
}

fn show_about(window: &mut Window, cx: &mut gpui_kit::App) {
    use gpui_kit::component::WindowExt as _;
    let p = Palette::of(cx);
    window.open_dialog(cx, move |dialog, _, _| {
        let dialog = dialog.centered();
        dialog.child(
            v_flex()
                .items_center()
                .gap_2()
                .py_4()
                .child(crate::brand::app_icon(px(96.)))
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Agentloft"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child(format!("Version {}", crate::brand::VERSION)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child("Run coding agents side by side — natively."),
                )
                .child(
                    div()
                        .pt_2()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child("Apache-2.0 · github.com/agentloft/agentloft"),
                ),
        )
    });
}

// -- small building blocks -------------------------------------------------------

type ClickHandler = Box<dyn Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static>;

fn icon_button(
    id: impl Into<SharedString>,
    icon: IconName,
    p: &Palette,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> AnyElement {
    let handler: ClickHandler = Box::new(on_click);
    div()
        .id(ElementId::Name(id.into()))
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .text_color(p.muted_foreground)
        .cursor_pointer()
        .hover(|d| d.bg(p.hover).text_color(p.foreground))
        .child(Icon::new(icon).small())
        .on_click(move |e, w, cx| handler(e, w, cx))
        .into_any_element()
}

/// Double-clicking our drawn title bar does what macOS is set to do
/// (zoom or minimize), like a native title bar.
fn titlebar_double_click(event: &gpui_kit::ClickEvent, window: &mut Window, _: &mut gpui_kit::App) {
    if event.click_count() == 2 {
        window.titlebar_double_click();
    }
}

fn toggle_button(
    id: &'static str,
    icon: IconName,
    on: bool,
    p: &Palette,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .cursor_pointer()
        .when(on, |d| d.bg(p.selected).text_color(p.foreground))
        .when(!on, |d| {
            d.text_color(p.muted_foreground)
                .hover(|d| d.bg(p.hover).text_color(p.foreground))
        })
        .child(Icon::new(icon).small())
        .on_click(on_click)
        .into_any_element()
}

fn round_button(
    id: &'static str,
    icon: IconName,
    bg: gpui_kit::Hsla,
    fg: gpui_kit::Hsla,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .size(px(30.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|d| d.opacity(0.85))
        .child(Icon::new(icon).small())
        .on_click(on_click)
        .into_any_element()
}

fn pill_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    shortcut: String,
    p: &Palette,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> AnyElement {
    h_flex()
        .id(id)
        .gap_2()
        .items_center()
        .px_3()
        .py_1p5()
        .rounded(px(10.))
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .text_sm()
        .cursor_pointer()
        .hover(|d| d.bg(p.hover))
        .child(Icon::new(icon).small().text_color(p.muted_foreground))
        .child(label)
        .child(
            div()
                .text_xs()
                .text_color(p.faint_foreground)
                .child(shortcut),
        )
        .on_click(on_click)
        .into_any_element()
}

/// Messages waiting for the current turn to end, each removable.
pub(crate) fn mode_label(mode: Option<loft_protocol::PermissionMode>) -> &'static str {
    use loft_protocol::PermissionMode::*;
    match mode {
        None => "Default permissions",
        Some(ReadOnly) => "Read-only (plan)",
        Some(AskAll) => "Ask before acting",
        Some(AcceptEdits) => "Auto-accept edits",
        Some(Auto) => "Auto",
        Some(FullAccess) => "Full access",
    }
}

fn chip_label(icon: IconName, label: impl Into<SharedString>, p: &Palette) -> AnyElement {
    h_flex()
        .gap_1()
        .items_center()
        .px_2()
        .py(px(3.))
        .rounded(px(7.))
        .text_xs()
        .text_color(p.muted_foreground)
        .hover(|d| d.bg(p.hover))
        .child(Icon::new(icon).xsmall())
        .child(label.into())
        .into_any_element()
}

/// Forks listed under their parent (ADR-0011), keeping the recency order
/// among siblings. Returns `(thread, depth)`.
/// A sidebar row: the thread, how deep it nests, and (if it has threads
/// under it) whether they're folded, how many, and whether any is busy.
struct Nested<'a> {
    thread: &'a loft_protocol::Thread,
    depth: usize,
    under: Option<Under>,
}

#[derive(Clone, Copy)]
struct Under {
    folded: bool,
    count: usize,
    busy: bool,
}

/// Forks and spawned threads under their parents (unless detached); a
/// folded thread's descendants are left out.
fn nest_forks<'a>(
    threads: Vec<&'a loft_protocol::Thread>,
    folded: &std::collections::HashSet<ThreadId>,
) -> Vec<Nested<'a>> {
    let present: std::collections::HashSet<&ThreadId> = threads.iter().map(|t| &t.id).collect();
    let parent_of = |t: &loft_protocol::Thread| {
        t.parent
            .as_ref()
            .filter(|p| !p.detached)
            .map(|p| &p.thread_id)
            .filter(|id| present.contains(id))
            .cloned()
    };
    fn descendants<'a>(
        t: &loft_protocol::Thread,
        all: &[&'a loft_protocol::Thread],
        parent_of: &dyn Fn(&loft_protocol::Thread) -> Option<ThreadId>,
        out: &mut Vec<&'a loft_protocol::Thread>,
    ) {
        for child in all.iter().filter(|c| parent_of(c).as_ref() == Some(&t.id)) {
            out.push(child);
            descendants(child, all, parent_of, out);
        }
    }
    fn push<'a>(
        t: &'a loft_protocol::Thread,
        depth: usize,
        all: &[&'a loft_protocol::Thread],
        parent_of: &dyn Fn(&loft_protocol::Thread) -> Option<ThreadId>,
        folded: &std::collections::HashSet<ThreadId>,
        out: &mut Vec<Nested<'a>>,
    ) {
        let mut below = Vec::new();
        descendants(t, all, parent_of, &mut below);
        let is_folded = folded.contains(&t.id);
        let under = (!below.is_empty()).then(|| Under {
            folded: is_folded,
            count: below.len(),
            busy: below.iter().any(|c| {
                matches!(
                    c.status,
                    ThreadStatus::Running | ThreadStatus::WaitingApproval
                ) || c.is_waiting()
            }),
        });
        out.push(Nested {
            thread: t,
            depth,
            under,
        });
        if is_folded {
            return;
        }
        for child in all.iter().filter(|c| parent_of(c).as_ref() == Some(&t.id)) {
            push(child, depth + 1, all, parent_of, folded, out);
        }
    }
    let mut out = Vec::with_capacity(threads.len());
    for root in threads.iter().filter(|t| parent_of(t).is_none()) {
        push(root, 0, &threads, &parent_of, folded, &mut out);
    }
    out
}

/// How long a thread sits idle before the sidebar folds it under "Older".
const OLDER_AFTER_MS: i64 = 12 * 60 * 60 * 1000;

/// Sidebar rows split into recent and older ones, a top-level thread and
/// the threads under it kept together. They stay recent while any of them
/// did something in the last 12 hours, is at work or unread, or is a draft.
fn split_older<'a>(
    rows: Vec<Nested<'a>>,
    unread: &std::collections::HashSet<ThreadId>,
    now: i64,
) -> (Vec<Nested<'a>>, Vec<Nested<'a>>) {
    let live = |r: &Nested| {
        let t = r.thread;
        now - t.updated_at_ms < OLDER_AFTER_MS
            || matches!(
                t.status,
                ThreadStatus::Running | ThreadStatus::WaitingApproval
            )
            || t.is_waiting()
            || !t.is_started()
            || unread.contains(&t.id)
            || r.under.is_some_and(|u| u.busy)
    };
    let (mut recent, mut old) = (Vec::new(), Vec::new());
    let mut family: Vec<Nested<'a>> = Vec::new();
    let mut flush = |family: &mut Vec<Nested<'a>>| {
        if family.iter().any(live) {
            recent.append(family);
        } else {
            old.append(family);
        }
    };
    for row in rows {
        if row.depth == 0 {
            flush(&mut family);
        }
        family.push(row);
    }
    flush(&mut family);
    (recent, old)
}

/// A project's sidebar heading. Clicking it folds or unfolds its threads;
/// folded, it still says how many there are and whether any need you.
#[allow(clippy::too_many_arguments)]
fn project_header(
    store: &Entity<AppStore>,
    project: &loft_protocol::Project,
    threads: &[&loft_protocol::Thread],
    folded: bool,
    unread: &std::collections::HashSet<ThreadId>,
    all_projects: &[ProjectId],
    chats: bool,
    p: &Palette,
    on_new: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> AnyElement {
    let id = project.id.clone();
    let group = SharedString::from(format!("project-{}", project.id));
    let waiting = threads
        .iter()
        .any(|t| t.status == ThreadStatus::WaitingApproval);
    let running = threads
        .iter()
        .any(|t| t.status == ThreadStatus::Running || t.is_waiting());
    let fresh = threads.iter().any(|t| unread.contains(&t.id));
    // Folded: what you'd otherwise miss.
    let summary: Option<AnyElement> = folded.then(|| {
        h_flex()
            .gap_1p5()
            .items_center()
            .when(waiting, |d| {
                d.child(div().size(px(7.)).rounded_full().bg(p.warning))
            })
            .when(running && !waiting, |d| {
                d.child(Spinner::new().xsmall().color(p.accent))
            })
            .when(fresh && !running && !waiting, |d| {
                d.child(div().size(px(7.)).rounded_full().bg(p.accent))
            })
            .when(!threads.is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(threads.len().to_string()),
                )
            })
            .into_any_element()
    });
    let toggle = {
        let (store, id) = (store.clone(), id.clone());
        move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
            let id = id.clone();
            store.update(cx, |s, cx| s.set_collapsed(&[id], !folded, cx));
        }
    };
    let menu = {
        let (store, all) = (store.clone(), all_projects.to_vec());
        let (this_id, this_name, threads) = (id.clone(), project.name.clone(), threads.len());
        move |menu: gpui_kit::component::menu::PopupMenu,
              _: &mut Window,
              cx: &mut Context<gpui_kit::component::menu::PopupMenu>| {
            use gpui_kit::component::menu::PopupMenuItem;
            let (s1, a1, s2, a2) = (store.clone(), all.clone(), store.clone(), all.clone());
            // This project: pin, its settings, removing it (not for Chats).
            let menu = if chats {
                menu
            } else {
                let pinned = store.read(cx).pinned_projects.contains(&this_id);
                let (s3, i3) = (store.clone(), this_id.clone());
                let (s4, i4) = (store.clone(), this_id.clone());
                let (s5, i5, n5) = (store.clone(), this_id.clone(), this_name.clone());
                menu.item(
                    PopupMenuItem::new(if pinned { "Unpin" } else { "Pin" })
                        .icon(if pinned {
                            IconName::PinOff
                        } else {
                            IconName::Pin
                        })
                        .on_click(move |_, _, cx| {
                            let id = i3.clone();
                            s3.update(cx, |s, cx| s.toggle_pinned(id, cx));
                        }),
                )
                .item(
                    PopupMenuItem::new("Project Settings…")
                        .icon(IconName::Settings)
                        .on_click(move |_, _, cx| {
                            crate::settings::open_project(s4.clone(), i4.clone(), cx)
                        }),
                )
                .item(
                    PopupMenuItem::new("Remove Project…")
                        .icon(IconName::Trash)
                        .on_click(move |_, window, cx| {
                            confirm_remove_project(
                                s5.clone(),
                                i5.clone(),
                                n5.clone(),
                                threads,
                                window,
                                cx,
                            )
                        }),
                )
                .separator()
            };
            menu.item(
                PopupMenuItem::new("Collapse All")
                    .icon(IconName::ChevronsDownUp)
                    .on_click(move |_, _, cx| {
                        s1.update(cx, |s, cx| s.set_collapsed(&a1, true, cx))
                    }),
            )
            .item(
                PopupMenuItem::new("Expand All")
                    .icon(IconName::ChevronsUpDown)
                    .on_click(move |_, _, cx| {
                        s2.update(cx, |s, cx| s.set_collapsed(&a2, false, cx))
                    }),
            )
        }
    };
    h_flex()
        .id(SharedString::from(format!("project-head-{}", project.id)))
        .group(group.clone())
        .gap_2()
        .items_center()
        .pl_2()
        .pr_1()
        .h(px(28.))
        .rounded(px(7.))
        .cursor_pointer()
        .text_color(p.muted_foreground)
        .hover(|d| d.text_color(p.foreground))
        // The folder turns into a chevron under the pointer.
        .child(
            div()
                .relative()
                .size(px(14.))
                .flex_none()
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .group_hover(group.clone(), |d| d.invisible())
                        // The project's icon (favicon, chosen image or
                        // initials); chats get a bubble.
                        .child(if chats {
                            Icon::new(IconName::MessageSquare)
                                .xsmall()
                                .into_any_element()
                        } else {
                            crate::project_icon::badge(project, 14., p)
                        }),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .invisible()
                        .group_hover(group.clone(), |d| d.visible())
                        .child(
                            Icon::new(if folded {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall(),
                        ),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .child(project.name.clone()),
        )
        .children(summary)
        .child(
            div()
                .id(SharedString::from(format!("new-wrap-{}", project.id)))
                .claims_press()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(icon_button(
                    format!("new-{}", project.id),
                    IconName::Plus,
                    p,
                    move |e, w, cx| {
                        cx.stop_propagation();
                        on_new(e, w, cx)
                    },
                )),
        )
        .on_click(toggle)
        .context_menu(menu)
        .into_any_element()
}

/// The line under a thread's title in the Threads sidebar: its project.
enum Subtitle {
    Project(loft_protocol::Project),
    NoProject,
}

fn subtitle_line(sub: &Subtitle, branch: Option<&str>, p: &Palette) -> AnyElement {
    let (badge, name): (AnyElement, String) = match sub {
        Subtitle::Project(project) => (
            crate::project_icon::badge(project, 15., p),
            project.name.clone(),
        ),
        Subtitle::NoProject => (
            Icon::new(IconName::MessageSquareDashed)
                .xsmall()
                .text_color(p.faint_foreground)
                .into_any_element(),
            "No project".into(),
        ),
    };
    h_flex()
        .gap_1p5()
        .items_center()
        .min_w_0()
        .text_xs()
        .text_color(p.muted_foreground)
        .child(badge)
        .child(div().flex_none().max_w(px(140.)).truncate().child(name))
        .when_some(branch, |d, b| {
            d.child(
                h_flex()
                    .min_w_0()
                    .gap_1()
                    .items_center()
                    .text_color(p.faint_foreground)
                    .child(Icon::new(IconName::GitBranch).xsmall())
                    .child(div().truncate().child(b.to_owned())),
            )
        })
        .into_any_element()
}

/// When the running turn started: the open thread's turn if we have it,
/// else when the thread last changed status (its turn start, mostly).
/// The sidebar badge of a thread waiting on the user: what it waits on,
/// when the thread is loaded here.
fn waiting_label(
    store: &Entity<AppStore>,
    thread: &loft_protocol::Thread,
    cx: &gpui_kit::App,
) -> &'static str {
    let kind = store
        .read(cx)
        .threads
        .get(&thread.id)
        .and_then(|t| t.read(cx).model.as_ref())
        .and_then(|m| m.open_requests().map(|r| r.kind).next());
    match kind {
        Some(loft_protocol::RequestKind::Question) => "Answer",
        Some(loft_protocol::RequestKind::Plan) => "Review plan",
        _ => "Approve",
    }
}

fn running_since(
    store: &Entity<AppStore>,
    thread: &loft_protocol::Thread,
    cx: &gpui_kit::App,
) -> i64 {
    store
        .read(cx)
        .threads
        .get(&thread.id)
        .and_then(|t| {
            t.read(cx)
                .model
                .as_ref()?
                .running_turn()
                .map(|t| t.started_at_ms)
        })
        .unwrap_or(thread.updated_at_ms)
}

/// "8s", "4m", "1h 12m": short enough for a sidebar row.
fn short_duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

fn now_ms() -> i64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn thread_row(
    store: &Entity<AppStore>,
    thread: &loft_protocol::Thread,
    depth: usize,
    under: Option<Under>,
    active: bool,
    unread: bool,
    menu_open: bool,
    renaming: Option<Entity<InputState>>,
    hit: Option<&loft_protocol::SearchHit>,
    subtitle: Option<Subtitle>,
    bulk: Option<Vec<ThreadId>>,
    p: &Palette,
    cx: &mut Context<WorkspaceView>,
) -> AnyElement {
    let id = thread.id.clone();
    let editing = renaming.is_some();
    let branch = thread.workspace.branch().map(str::to_owned);
    let trailing: AnyElement = match thread.status {
        // The spinner already repaints the sidebar every frame, so the
        // elapsed time stays current without a timer of its own.
        ThreadStatus::Running => h_flex()
            .gap_1()
            .items_center()
            .text_xs()
            .text_color(p.accent)
            .child(Spinner::new().xsmall().color(p.accent))
            .child(format!(
                "Working {}",
                short_duration(now_ms() - running_since(store, thread, cx))
            ))
            .into_any_element(),
        ThreadStatus::WaitingApproval => div()
            .px_1p5()
            .rounded(px(5.))
            .bg(p.warning_soft)
            .text_xs()
            .text_color(p.warning)
            .child(waiting_label(store, thread, cx))
            .into_any_element(),
        ThreadStatus::Error => Icon::new(IconName::CircleAlert)
            .xsmall()
            .text_color(p.danger)
            .into_any_element(),
        // Its turn is over but background work it started isn't.
        _ if thread.is_waiting() => h_flex()
            .gap_1()
            .items_center()
            .text_xs()
            .text_color(p.accent)
            .child(Icon::new(IconName::Hourglass).xsmall())
            .child("Waiting")
            .into_any_element(),
        // Finished while you were elsewhere.
        _ if unread => div()
            .size(px(7.))
            .mr_1()
            .rounded_full()
            .bg(p.accent)
            .into_any_element(),
        _ if !thread.is_started() && thread.parent.is_none() => h_flex()
            .gap_1()
            .items_center()
            .text_xs()
            .text_color(p.faint_foreground)
            .child(Icon::new(IconName::SquarePen).xsmall())
            .child("Draft")
            .into_any_element(),
        _ => div()
            .text_xs()
            .text_color(p.faint_foreground)
            .child(relative_time(thread.updated_at_ms))
            .into_any_element(),
    };
    // An unsent draft names a new thread until it has a real title.
    let title = match thread.draft_text() {
        Some(draft) if !thread.is_started() && thread.title == "New thread" => {
            draft.lines().next().unwrap_or(draft).trim().to_owned()
        }
        _ => thread.title.clone(),
    };
    let indent = 12. + 14. * depth.min(4) as f32;
    // Threads under this one fold with a chevron in the margin (on hover,
    // or always while folded).
    let fold = under.map(|u| {
        let (store, id) = (store.clone(), thread.id.clone());
        div()
            .id(SharedString::from(format!("fold-{}", thread.id)))
            .absolute()
            .left(px(indent - 13.))
            .top_0()
            .bottom_0()
            .w(px(12.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .cursor_pointer()
            .text_color(p.faint_foreground)
            .hover(|d| d.text_color(p.foreground))
            .when(!u.folded, |d| {
                d.invisible().group_hover("thread-row", |d| d.visible())
            })
            .child(
                Icon::new(if u.folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .xsmall(),
            )
            .claims_press()
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let id = id.clone();
                store.update(cx, |s, cx| s.set_thread_folded(id, !u.folded, cx));
            })
    });
    // Folded: how many are tucked away, and whether any is at work.
    let tucked = under.filter(|u| u.folded).map(|u| {
        h_flex()
            .flex_none()
            .gap_1()
            .items_center()
            .px_1p5()
            .rounded(px(5.))
            .bg(p.muted)
            .text_xs()
            .text_color(p.muted_foreground)
            .when(u.busy, |d| d.child(Spinner::new().xsmall().color(p.accent)))
            .child(format!("+{}", u.count))
    });
    h_flex()
        .id(SharedString::from(format!("thread-{}", thread.id)))
        .relative()
        .gap_2()
        .items_center()
        .group("thread-row")
        .children(fold)
        .pl(px(indent))
        .pr_1p5()
        .py(px(6.))
        .rounded(px(7.))
        .cursor_pointer()
        .text_sm()
        .text_color(if active {
            p.foreground
        } else {
            p.foreground.opacity(0.82)
        })
        .when(active, |d| d.bg(p.selected))
        .when(!active, |d| d.hover(|d| d.bg(p.hover)))
        // A fork, or a thread another agent started (under its parent).
        .when_some(
            thread.parent.as_ref().filter(|p| !p.detached),
            |d, parent| {
                d.child(
                    Icon::new(if parent.kind == loft_protocol::ForkKind::Spawned {
                        IconName::CornerDownRight
                    } else {
                        IconName::GitFork
                    })
                    .xsmall()
                    .text_color(p.faint_foreground),
                )
            },
        )
        // Which agent runs it.
        .child(div().flex_none().when(!active, |d| d.opacity(0.85)).child(
            crate::brand::agent_logo(thread.harness.as_str(), px(14.), p),
        ))
        .child(match (renaming, hit) {
            // Renaming: the title is a field; Esc puts it back.
            (Some(input), _) => div()
                .id("rename-field")
                .flex_1()
                .min_w_0()
                .capture_key_down(cx.listener(|this, e: &gpui_kit::KeyDownEvent, _, cx| {
                    if e.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.finish_rename(false, cx);
                    }
                }))
                // Clicks in the field stay in the field; a click anywhere
                // else saves.
                .claims_press()
                .on_click(|_, _, cx| cx.stop_propagation())
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.finish_rename(true, cx)))
                .h(px(22.))
                .flex()
                .items_center()
                .px(px(1.))
                .ml(px(-6.))
                .rounded(px(5.))
                .border_1()
                .border_color(p.accent.opacity(0.8))
                .bg(p.background)
                .child(Input::new(&input).appearance(false).small().text_sm())
                .into_any_element(),
            (None, None) if subtitle.is_none() => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(title)
                .into_any_element(),
            // Title, then its project (Threads sidebar) and/or the message
            // that matched a search.
            (None, hit) => v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(div().truncate().child(title))
                .when_some(subtitle.as_ref(), |d, sub| {
                    d.child(subtitle_line(sub, branch.as_deref(), p))
                })
                .when_some(hit, |d, hit| d.child(search_snippet(hit, p)))
                .into_any_element(),
        })
        // A run of an automation.
        .when(thread.automation_id.is_some(), |d| {
            d.child(
                Icon::new(IconName::Clock)
                    .xsmall()
                    .text_color(p.faint_foreground),
            )
        })
        .when(
            thread.workspace.branch().is_some() && subtitle.is_none(),
            |d| {
                d.child(
                    Icon::new(IconName::GitBranch)
                        .xsmall()
                        .text_color(p.faint_foreground),
                )
            },
        )
        .children(tucked)
        // Under the pointer, the time (or status) makes way for Archive and
        // a ··· menu (the title gives up room for both).
        .child(
            div()
                .relative()
                .flex_none()
                .min_w(px(22.))
                .when(menu_open, |d| d.min_w(px(46.)))
                .group_hover("thread-row", |d| d.min_w(px(46.)))
                .h(px(20.))
                .flex()
                .items_center()
                .justify_end()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .when(menu_open, |d| d.invisible())
                        .when(!menu_open, |d| {
                            d.group_hover("thread-row", |d| d.invisible())
                        })
                        .child(trailing),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("row-menu-wrap-{}", thread.id)))
                        .absolute()
                        .right_0()
                        .top_0()
                        .when(!menu_open, |d| {
                            d.invisible().group_hover("thread-row", |d| d.visible())
                        })
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .claims_press()
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child({
                            use gpui_kit::component::button::{Button, ButtonVariants as _};
                            let (store, id) = (store.clone(), thread.id.clone());
                            Button::new(SharedString::from(format!("row-archive-{}", thread.id)))
                                .cursor_pointer()
                                .icon(IconName::Archive)
                                .ghost()
                                .xsmall()
                                .tooltip("Archive")
                                .claims_press()
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    let id = id.clone();
                                    store.update(cx, |s, cx| s.archive_thread(id, false, cx));
                                })
                        })
                        .child({
                            use gpui_kit::component::button::{Button, ButtonVariants as _};
                            use gpui_kit::component::menu::DropdownMenu as _;
                            let row = thread.id.clone();
                            let view = cx.entity().downgrade();
                            Button::new(SharedString::from(format!("row-menu-{}", thread.id)))
                                .cursor_pointer()
                                .icon(IconName::Ellipsis)
                                .ghost()
                                .xsmall()
                                .dropdown_menu_with_anchor(
                                    gpui_kit::Anchor::TopRight,
                                    thread_menu_items(thread, store, cx.entity().downgrade()),
                                )
                                .on_open_change(move |open, _, cx| {
                                    let row = row.clone();
                                    let open = *open;
                                    _ = view.update(cx, |this, cx| {
                                        if open {
                                            this.row_menu = Some(row);
                                        } else if this.row_menu.as_ref() == Some(&row) {
                                            this.row_menu = None;
                                        }
                                        cx.notify();
                                    });
                                })
                        }),
                ),
        )
        .on_click({
            let reveal = hit.map(|h| h.item_id.clone());
            cx.listener(move |this, event: &gpui_kit::ClickEvent, window, cx| {
                let id = id.clone();
                // Shift-click: every thread from the last one clicked to here.
                if event.modifiers().shift {
                    this.select_range(id, cx);
                    return;
                }
                this.multi.clear();
                this.anchor = Some(id.clone());
                // Double-click: rename in place.
                if event.click_count() == 2 {
                    this.start_rename(id, window, cx);
                    return;
                }
                // ⌘-click: beside the current one, in a pane of its own.
                if event.modifiers().platform {
                    this.store.update(cx, |s, cx| s.open_pane(id.clone(), cx));
                } else {
                    this.store
                        .update(cx, |s, cx| s.select_thread(id.clone(), cx));
                }
                // From a search: show the message that matched.
                if let Some(item) = reveal.clone() {
                    let thread = this.store.update(cx, |s, cx| s.ensure_thread(&id, cx));
                    thread.update(cx, |t, cx| t.reveal(item, cx));
                }
            })
        })
        .when(!editing, |d| {
            d.on_drag(
                DraggedThread {
                    id: thread.id.clone(),
                    title: thread.title.clone(),
                },
                |d, _, _, cx| cx.new(|_| d.clone()),
            )
        })
        .map(|d| match bulk {
            // Part of a multi-selection: one menu for all of them.
            Some(ids) => d
                .context_menu(bulk_menu_items(ids, store, cx.entity().downgrade()))
                .into_any_element(),
            None => d
                .context_menu(thread_menu_items(thread, store, cx.entity().downgrade()))
                .into_any_element(),
        })
        .into_any_element()
}

/// A line of a message that matched the sidebar search, the match marked.
fn search_snippet(hit: &loft_protocol::SearchHit, p: &Palette) -> AnyElement {
    let (mut text, mut start, mut end) = (hit.snippet.clone(), hit.start, hit.end);
    let ok = start <= end
        && end <= text.len()
        && text.is_char_boundary(start)
        && text.is_char_boundary(end);
    // The sidebar is narrow: keep only a few words before the match.
    const BEFORE: usize = 14;
    if ok && let Some((cut, _)) = text[..start].char_indices().rev().nth(BEFORE) {
        let kept = text[cut..].trim_start().to_owned();
        let shift = text.len() - kept.len();
        text = format!("…{kept}");
        let ellipsis = '…'.len_utf8();
        start = start - shift + ellipsis;
        end = end - shift + ellipsis;
    }
    let range = ok.then_some(start..end);
    let styled = gpui_kit::StyledText::new(text).with_highlights(range.map(|r| {
        (
            r,
            gpui_kit::HighlightStyle {
                color: Some(p.foreground),
                font_weight: Some(FontWeight::SEMIBOLD),
                background_color: Some(p.accent.opacity(0.22)),
                ..Default::default()
            },
        )
    }));
    h_flex()
        .gap_1()
        .items_center()
        .text_xs()
        .text_color(p.muted_foreground)
        .child(
            Icon::new(if hit.from_user {
                IconName::User
            } else {
                IconName::MessageSquare
            })
            .xsmall()
            .text_color(p.faint_foreground),
        )
        .child(div().flex_1().min_w_0().truncate().child(styled))
        .when(hit.count > 1, |d| {
            d.child(
                div()
                    .flex_none()
                    .text_color(p.faint_foreground)
                    .child(format!("+{}", hit.count - 1)),
            )
        })
        .into_any_element()
}

/// `claude-opus-5-5` → `Opus 5.5`.
pub fn harness_name(h: &HarnessId) -> &'static str {
    match h.as_str() {
        HarnessId::CLAUDE => "Claude",
        HarnessId::CODEX => "Codex",
        HarnessId::OPENCODE => "OpenCode",
        HarnessId::GEMINI => "Gemini",
        HarnessId::FAKE => "Echo",
        _ => "Agent",
    }
}

fn placeholder_for(h: &HarnessId) -> String {
    format!(
        "Ask {} to build, fix or explain something…",
        harness_name(h)
    )
}

/// `AGENTLOFT_DEV_TAB` (screenshots/tests): applied by the first window
/// only, so hooks that open windows don't cascade.
fn take_dev_tab() -> Option<String> {
    static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if TAKEN.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return None;
    }
    std::env::var("AGENTLOFT_DEV_TAB")
        .ok()
        .filter(|v| !v.is_empty())
}

/// Big enough to show as a chip rather than inline text.
fn is_large_paste(text: &str) -> bool {
    text.lines().count() > 20 || text.len() > 1500
}

fn human_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.1} KB", n as f64 / 1024.)
    }
}

/// How long the sidebars take to slide open or shut.
const SLIDE: std::time::Duration = std::time::Duration::from_millis(180);

/// A sidebar's slide: 0 = shut, 1 = open, eased in between.
#[derive(Default)]
struct Slide {
    open: bool,
    seen: bool,
    started: Option<web_time::Instant>,
}

impl Slide {
    /// Follow `open`, returning how far open to draw it (asks for frames
    /// while moving). The first call just takes the state, unanimated.
    fn update(&mut self, open: bool, window: &mut Window) -> f32 {
        if !self.seen {
            self.seen = true;
            self.open = open;
        } else if open != self.open {
            // Reversing mid-way starts from where it is.
            let now = self.linear();
            let from = if open { now } else { 1. - now };
            self.open = open;
            self.started = Some(web_time::Instant::now() - SLIDE.mul_f32(from));
        }
        let t = self.linear_progress();
        if t < 1. {
            window.request_animation_frame();
        } else {
            self.started = None;
        }
        let eased = 1. - (1. - t).powi(3);
        if self.open { eased } else { 1. - eased }
    }

    /// Progress of the current move, 0..1 (1 when still).
    fn linear_progress(&self) -> f32 {
        self.started
            .map(|t| (t.elapsed().as_secs_f32() / SLIDE.as_secs_f32()).min(1.))
            .unwrap_or(1.)
    }

    /// How open it is, before easing.
    fn linear(&self) -> f32 {
        let t = self.linear_progress();
        if self.open { t } else { 1. - t }
    }
}

/// Narrowest and shortest a pane may be: how many fit decides the grid.
const PANE_MIN_W: f32 = 340.;
const PANE_MIN_H: f32 = 320.;
/// Most panes a window shows.
const MAX_PANES: usize = 8;

/// The mini browser's picture size, and the whole card's (borders and
/// title bar included).
const PREVIEW_W: f32 = 340.;
const PREVIEW_H: f32 = 212.;
const PREVIEW_CARD_W: f32 = PREVIEW_W + 2.;
const PREVIEW_CARD_H: f32 = PREVIEW_H + 30. + 1. + 2.;

/// `at` (the mini browser's distance from the right and bottom edges) kept
/// so the card stays whole inside `room`. An unknown room keeps it as is.
fn keep_preview_in(
    at: gpui_kit::Point<gpui_kit::Pixels>,
    room: gpui_kit::Size<gpui_kit::Pixels>,
) -> gpui_kit::Point<gpui_kit::Pixels> {
    if room.width <= px(0.) || room.height <= px(0.) {
        return at;
    }
    let max_x = (room.width - px(PREVIEW_CARD_W)).max(px(0.));
    let max_y = (room.height - px(PREVIEW_CARD_H)).max(px(0.));
    gpui_kit::point(at.x.clamp(px(0.), max_x), at.y.clamp(px(0.), max_y))
}

/// The mini browser being dragged by its title bar (nothing is drawn under
/// the pointer: the card itself moves). `grab` is where in the bar it was
/// picked up.
#[derive(Clone, Default)]
pub(crate) struct DraggedPreview {
    grab: std::rc::Rc<std::cell::Cell<gpui_kit::Point<gpui_kit::Pixels>>>,
}

impl Render for DraggedPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui_kit::Empty
    }
}

/// A thread being dragged (from the sidebar, or a pane's title bar).
#[derive(Clone)]
pub(crate) struct DraggedThread {
    pub id: ThreadId,
    pub title: String,
}

impl Render for DraggedThread {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        h_flex()
            .gap_1p5()
            .items_center()
            .max_w(px(260.))
            .px_3()
            .py_1p5()
            .rounded(px(9.))
            .bg(p.elevated)
            .border_1()
            .border_color(p.accent.opacity(0.6))
            .shadow_lg()
            .text_sm()
            .child(Icon::new(IconName::Columns2).xsmall().text_color(p.accent))
            .child(div().truncate().child(self.title.clone()))
    }
}

/// A queued message being dragged to a new place in line.
#[derive(Clone)]
pub(crate) struct DraggedQueued {
    pub(crate) thread_id: ThreadId,
    pub(crate) id: u64,
    pub(crate) text: String,
}

impl Render for DraggedQueued {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        div()
            .max_w(px(320.))
            .px_3()
            .py_1p5()
            .rounded(px(12.))
            .bg(p.elevated)
            .border_1()
            .border_dashed()
            .border_color(p.accent.opacity(0.6))
            .shadow_lg()
            .text_sm()
            .text_color(p.muted_foreground)
            .child(div().truncate().child(self.text.clone()))
    }
}

/// The terminal dock's first height, and its shortest.
const DOCK_HEIGHT: f32 = 280.;
const DOCK_MIN_HEIGHT: f32 = 120.;

/// The side panel's narrowest width while there is room to spare.
const PANEL_MIN: f32 = 340.;

/// The narrowest the side panel gets when the window is tight: it gives
/// way before the conversation does.
const PANEL_FLOOR: f32 = 240.;

/// The conversation's narrowest width beside the side panel: room for the
/// new-thread choices on one line and the composer's row of controls. A
/// window with less than this and [`PANEL_FLOOR`] to share shows the panel
/// alone.
const CONVERSATION_MIN: f32 = 360.;

/// The side panel's first width: a share of the room beside the sidebar, so
/// the conversation keeps most of it. Dragging it wider is remembered.
fn default_panel_width(room: f32) -> f32 {
    (room * 0.4).clamp(PANEL_MIN, 640.)
}

/// The docked details card's width, and the conversation width (with it)
/// needed to dock rather than float.
const DOCK_WIDTH: f32 = 320.;
const DOCK_MIN: f32 = 900.;

/// The picture attached with page notes (replaced as notes are added).
const PAGE_NOTES_IMAGE: &str = "page-notes.png";

/// What a page note refers to, in words: "the button “Sign in” (`#login`)".
fn describe_note(note: &loft_browser::Annotation) -> String {
    if let Some(r) = note.rect {
        let mut out = format!(
            "the area at {}, {} ({} × {} px)",
            r.x as i64, r.y as i64, r.width as i64, r.height as i64
        );
        if !note.label.is_empty() {
            out.push_str(&format!(" showing “{}”", note.label));
        }
        if !note.selector.is_empty() {
            out.push_str(&format!(" (`{}`)", note.selector));
        }
        return out;
    }
    let kind = match note.tag.as_str() {
        "a" => "link",
        "button" => "button",
        "svg" => "icon",
        "img" | "picture" => "image",
        "video" => "video",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "heading",
        "p" => "paragraph",
        "input" | "textarea" | "select" => "field",
        "li" => "list item",
        "nav" => "navigation",
        "header" => "header",
        "footer" => "footer",
        "body" | "html" => "page",
        _ => "element",
    };
    if note.label.is_empty() {
        format!("the {kind} `{}`", note.selector)
    } else {
        format!("the {kind} “{}” (`{}`)", note.label, note.selector)
    }
}

/// What the pictures attached for a PDF's notes are called: this, the page
/// number, `.png`.
fn doc_notes_image_prefix(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    format!("notes-{file}-page-")
}

/// Notes on documents as part of the message, a block per file, numbered
/// like the pins on it. `pictured` names the images going with the message.
fn doc_notes_block(notes: &[(String, crate::panel::docs::DocNote)], pictured: &[&str]) -> String {
    let mut files: Vec<&str> = Vec::new();
    for (path, _) in notes {
        if !files.contains(&path.as_str()) {
            files.push(path);
        }
    }
    let mut blocks = Vec::new();
    for file in files {
        let prefix = doc_notes_image_prefix(file);
        let pages = pictured.iter().filter(|n| n.starts_with(&prefix)).count();
        let mut out = match pages {
            0 => format!("Notes on {file}:"),
            1 => format!(
                "Notes on {file} (the attached picture of the page shows them as numbered pins):"
            ),
            _ => format!(
                "Notes on {file} (the attached pictures of its pages show them as numbered pins):"
            ),
        };
        for (_, n) in notes.iter().filter(|(p, _)| p == file) {
            out.push_str(&format!("\n{}. On {}: {}", n.n, n.what, n.note));
        }
        blocks.push(out);
    }
    blocks.join("\n\n")
}

/// Page notes as part of the message, numbered like the pins on the page.
fn notes_block(notes: &[loft_browser::Annotation]) -> String {
    let mut pages: Vec<&str> = Vec::new();
    for n in notes {
        if !pages.contains(&n.url.as_str()) {
            pages.push(&n.url);
        }
    }
    let mut out = if pages.len() == 1 {
        format!(
            "Notes on {} (the attached screenshot shows them as numbered pins):",
            pages[0]
        )
    } else {
        "Notes on pages in the browser (the attached screenshot shows the numbered pins):"
            .to_owned()
    };
    for (i, n) in notes.iter().enumerate() {
        let num = if n.n > 0 { n.n as usize } else { i + 1 };
        out.push_str(&format!("\n{num}. On {}", describe_note(n)));
        if pages.len() > 1 {
            out.push_str(&format!(" at {}", n.url));
        }
        out.push_str(&format!(": {}", n.note));
    }
    out
}

/// The full pasted text, in a dialog.
fn show_pasted(text: String, window: &mut Window, cx: &mut gpui_kit::App) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::{DialogClose, DialogFooter};
    use gpui_kit::component::text::TextView;
    let lines = text.lines().count();
    let size = human_bytes(text.len());
    let body = crate::items::plain_markdown(&text);
    window.open_dialog(cx, move |dialog, _, cx| {
        let dialog = dialog.centered();
        let p = Palette::of(cx);
        let copy_text = text.clone();
        dialog
            .w(px(760.))
            .title(format!("Pasted text · {lines} lines · {size}"))
            .child(
                div()
                    .id("pasted-body")
                    .max_h(px(520.))
                    .overflow_y_scroll()
                    .p_3()
                    .rounded(px(10.))
                    .bg(p.muted)
                    .font_family(p.mono.clone())
                    .text_sm()
                    .child(TextView::markdown("pasted-text", body.clone()).selectable(true)),
            )
            .footer(
                DialogFooter::new().child(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("pasted-copy")
                                .cursor_pointer()
                                .label("Copy")
                                .icon(IconName::Copy)
                                .ghost()
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                        copy_text.clone(),
                                    ));
                                }),
                        )
                        .child(div().flex_1())
                        .child(
                            div().flex_none().child(
                                DialogClose::new().child(
                                    Button::new("pasted-close")
                                        .cursor_pointer()
                                        .label("Close")
                                        .outline(),
                                ),
                            ),
                        ),
                ),
            )
    });
}

/// [`tilde`] for a path string.
pub(crate) fn tilde_path_str(path: &str) -> String {
    tilde(path)
}

/// [`tilde`] for a path.
pub(crate) fn tilde_path(path: &std::path::Path) -> String {
    tilde(&path.to_string_lossy())
}

/// `/Users/me/x` → `~/x`. The home is that of the machine the path is on
/// (the daemon's, in a browser).
fn tilde(path: &str) -> String {
    let home = crate::host::user_home();
    // No real home known: nothing to shorten.
    if home.parent().is_none() {
        return path.to_owned();
    }
    match std::path::Path::new(path).strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_owned(),
    }
}

/// The header's thread menu: worktree actions and archive.
/// A thread's actions (its sidebar row's ··· button and right-click).
fn thread_menu_items(
    thread: &loft_protocol::Thread,
    store: &Entity<AppStore>,
    view: gpui_kit::WeakEntity<WorkspaceView>,
) -> impl Fn(
    gpui_kit::component::menu::PopupMenu,
    &mut Window,
    &mut Context<gpui_kit::component::menu::PopupMenu>,
) -> gpui_kit::component::menu::PopupMenu
+ 'static {
    use gpui_kit::component::menu::PopupMenuItem;
    let store = store.clone();
    let id = thread.id.clone();
    let harness = thread.harness.clone();
    let title = thread.title.clone();
    let worktree = match &thread.workspace {
        ThreadWorkspace::Worktree { path, branch, base } => {
            Some((path.clone(), branch.clone(), base.clone()))
        }
        _ => None,
    };
    let running = matches!(
        thread.status,
        ThreadStatus::Running | ThreadStatus::WaitingApproval
    );
    // Its work is all in the base: the worktree can go.
    let landed = thread.landed;
    // Cleaned up, its branch deleted: it can come back.
    let restorable = matches!(
        &thread.workspace,
        ThreadWorkspace::Released { tip: Some(_), .. }
    );
    move |mut menu, window, cx| {
        let (v1, i1) = (view.clone(), id.clone());
        menu = menu.item(
            PopupMenuItem::new("Rename")
                .icon(IconName::SquarePen)
                .on_click(move |_, window, cx| {
                    let id = i1.clone();
                    // After the menu closes, so the field keeps focus.
                    let v = v1.clone();
                    window.defer(cx, move |window, cx| {
                        let _ = v.update(cx, |v, cx| v.start_rename(id, window, cx));
                    });
                }),
        );
        // Pinned threads sit at the top of the sidebar.
        let pinned = store.read(cx).pinned_threads.contains(&id);
        let (s, t) = (store.clone(), id.clone());
        menu = menu.item(
            PopupMenuItem::new(if pinned { "Unpin" } else { "Pin" })
                .icon(if pinned {
                    IconName::PinOff
                } else {
                    IconName::Pin
                })
                .on_click(move |_, _, cx| {
                    let t = t.clone();
                    s.update(cx, |s, cx| s.set_threads_pinned(&[t], !pinned, cx));
                }),
        );
        let (s, t) = (store.clone(), id.clone());
        menu = menu
            .item(
                PopupMenuItem::new("Open in New Pane")
                    .icon(IconName::Columns2)
                    .on_click(move |_, _, cx| {
                        let id = t.clone();
                        s.update(cx, |s, cx| s.open_pane(id, cx));
                    }),
            )
            .item({
                // For pointing an agent at this thread.
                let raw = id.0.clone();
                PopupMenuItem::new("Copy Thread ID")
                    .icon(IconName::Hash)
                    .on_click(move |_, window, cx| {
                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(raw.clone()));
                        use gpui_kit::component::WindowExt as _;
                        use gpui_kit::component::notification::{Notification, NotificationType};
                        window.push_notification(
                            Notification::new()
                                .message("Thread ID copied")
                                .with_type(NotificationType::Success),
                            cx,
                        );
                    })
            })
            .separator();
        menu = fork_submenu(menu, &store, &id, &harness, window, cx);
        if let Some((path, branch, base)) = &worktree {
            let (s, t) = (store.clone(), id.clone());
            let (s2, t2) = (store.clone(), id.clone());
            let (path, branch) = (path.clone(), branch.clone());
            menu = menu
                .separator()
                .item(
                    PopupMenuItem::new(format!("Merge into {base}"))
                        .icon(IconName::GitMerge)
                        .disabled(running)
                        .on_click(move |_, _, cx| {
                            let t = t.clone();
                            s.update(cx, |s, cx| s.merge_thread(t, cx));
                        }),
                )
                .item(
                    PopupMenuItem::new("Create pull request")
                        .icon(IconName::GitPullRequest)
                        .disabled(running)
                        .on_click(move |_, _, cx| {
                            let t = t2.clone();
                            s2.update(cx, |s, cx| s.create_pull_request(t, cx));
                        }),
                )
                .item(
                    PopupMenuItem::new("Reveal worktree in Finder")
                        .icon(IconName::FolderOpen)
                        .on_click(move |_, _, cx| {
                            cx.reveal_path(std::path::Path::new(&path));
                        }),
                )
                .item(
                    PopupMenuItem::new("Copy branch name")
                        .icon(IconName::Copy)
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                branch.clone(),
                            ));
                        }),
                );
        }
        if landed && !running {
            let (s, t) = (store.clone(), id.clone());
            menu = menu.item(
                PopupMenuItem::new("Clean up worktree")
                    .icon(IconName::Trash)
                    .on_click(move |_, _, cx| {
                        let t = t.clone();
                        s.update(cx, |s, cx| s.clean_up_worktree(t, cx));
                    }),
            );
        }
        if restorable {
            let (s, t) = (store.clone(), id.clone());
            menu = menu.separator().item(
                PopupMenuItem::new("Restore branch")
                    .icon(IconName::GitBranch)
                    .on_click(move |_, _, cx| {
                        let t = t.clone();
                        s.update(cx, |s, cx| s.restore_branch(t, cx));
                    }),
            );
        }
        let (s, t) = (store.clone(), id.clone());
        let (s2, t2, title2) = (store.clone(), id.clone(), title.clone());
        menu.separator()
            .item(
                PopupMenuItem::new("Archive")
                    .icon(IconName::Archive)
                    .on_click(move |_, _, cx| {
                        let t = t.clone();
                        s.update(cx, |s, cx| s.archive_thread(t, false, cx));
                    }),
            )
            .item(
                PopupMenuItem::new("Delete…")
                    .icon(IconName::Trash)
                    .on_click(move |_, window, cx| {
                        confirm_delete(s2.clone(), t2.clone(), title2.clone(), window, cx)
                    }),
            )
    }
}

/// "Fork ▸" with one entry per agent: the same agent continues the real
/// conversation, another starts from a handoff summary.
fn fork_submenu(
    menu: gpui_kit::component::menu::PopupMenu,
    store: &Entity<AppStore>,
    thread_id: &ThreadId,
    current: &HarnessId,
    window: &mut Window,
    cx: &mut Context<gpui_kit::component::menu::PopupMenu>,
) -> gpui_kit::component::menu::PopupMenu {
    use gpui_kit::component::menu::PopupMenuItem;
    let agents = store.read(cx).available_harnesses();
    let (store, thread_id, current) = (store.clone(), thread_id.clone(), current.clone());
    menu.submenu_with_icon(
        Some(Icon::new(IconName::GitFork)),
        "Fork",
        window,
        cx,
        move |mut sub, _, _| {
            for &(h, name) in &agents {
                let (s, t) = (store.clone(), thread_id.clone());
                let label = if h == current.as_str() {
                    format!("{name} · same conversation")
                } else {
                    format!("{name} · from a summary")
                };
                sub = sub.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    let t = t.clone();
                    s.update(cx, |s, cx| s.fork_thread(t, HarnessId(h.into()), None, cx));
                }));
            }
            sub
        },
    )
}

/// Confirm, then delete a thread for good.
/// The menu for several sidebar threads picked together.
fn bulk_menu_items(
    ids: Vec<ThreadId>,
    store: &Entity<AppStore>,
    view: gpui_kit::WeakEntity<WorkspaceView>,
) -> impl Fn(
    gpui_kit::component::menu::PopupMenu,
    &mut Window,
    &mut Context<gpui_kit::component::menu::PopupMenu>,
) -> gpui_kit::component::menu::PopupMenu
+ 'static {
    use gpui_kit::component::menu::PopupMenuItem;
    let store = store.clone();
    move |menu, _, cx| {
        let n = ids.len();
        let clear = {
            let view = view.clone();
            move |cx: &mut gpui_kit::App| {
                let _ = view.update(cx, |v, cx| {
                    v.multi.clear();
                    cx.notify();
                });
            }
        };
        let (s1, i1) = (store.clone(), ids.clone());
        let (s2, i2, c2) = (store.clone(), ids.clone(), clear.clone());
        let c3 = clear.clone();
        let (s4, i4) = (store.clone(), ids.clone());
        // Pin them all, unless all are pinned already.
        let pin = {
            let pinned = &store.read(cx).pinned_threads;
            !ids.iter().all(|t| pinned.contains(t))
        };
        let (s5, i5, c5) = (store.clone(), ids.clone(), clear.clone());
        menu.item(
            PopupMenuItem::new(if pin {
                format!("Pin ({n})")
            } else {
                format!("Unpin ({n})")
            })
            .icon(if pin { IconName::Pin } else { IconName::PinOff })
            .on_click(move |_, _, cx| {
                s5.update(cx, |s, cx| s.set_threads_pinned(&i5, pin, cx));
                c5(cx);
            }),
        )
        .item(
            PopupMenuItem::new(format!("Open in Panes ({n})"))
                .icon(IconName::Columns2)
                .on_click(move |_, _, cx| {
                    for id in &i1 {
                        let id = id.clone();
                        s1.update(cx, |s, cx| s.open_pane(id, cx));
                    }
                }),
        )
        .item(
            PopupMenuItem::new(format!("Archive ({n})"))
                .icon(IconName::Archive)
                .on_click(move |_, _, cx| {
                    for id in &i2 {
                        let id = id.clone();
                        s2.update(cx, |s, cx| s.archive_thread(id, false, cx));
                    }
                    c2(cx);
                }),
        )
        .item(
            PopupMenuItem::new("Clear Selection")
                .icon(IconName::X)
                .on_click(move |_, _, cx| c3(cx)),
        )
        .separator()
        .item(
            PopupMenuItem::new(format!("Delete ({n})…"))
                .icon(IconName::Trash)
                .on_click(move |_, window, cx| {
                    confirm_delete_many(s4.clone(), i4.clone(), clear.clone(), window, cx)
                }),
        )
    }
}

/// "Delete 4 threads?" for a multi-selection.
fn confirm_delete_many(
    store: Entity<AppStore>,
    ids: Vec<ThreadId>,
    done: impl Fn(&mut gpui_kit::App) + 'static,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
    let done = std::rc::Rc::new(done);
    window.open_dialog(cx, move |dialog, _, _| {
        let dialog = dialog.centered();
        let (store, ids, done) = (store.clone(), ids.clone(), done.clone());
        dialog
            .title(format!("Delete {} threads?", ids.len()))
            .child(
                "Their conversations, turn history and checkpoints are erased and their agents \
                 and terminals stopped. Worktrees are removed; their branches are kept. This \
                 can't be undone.",
            )
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("cancel").cursor_pointer().label("Cancel")),
                    )
                    .child(
                        DialogAction::new()
                            .child(Button::new("ok").cursor_pointer().danger().label("Delete")),
                    ),
            )
            .on_ok(move |_, _, cx| {
                for id in &ids {
                    let id = id.clone();
                    store.update(cx, |s, cx| s.delete_thread(id, cx));
                }
                done(cx);
                true
            })
    });
}

/// "Remove “name”?": its threads are deleted, its files stay.
fn confirm_remove_project(
    store: Entity<AppStore>,
    project_id: ProjectId,
    name: String,
    threads: usize,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
    window.open_dialog(cx, move |dialog, _, _| {
        let dialog = dialog.centered();
        let (store, project_id) = (store.clone(), project_id.clone());
        let what = match threads {
            0 => "It has no threads.".to_owned(),
            1 => "Its thread is deleted, with its worktree and checkpoints.".to_owned(),
            n => format!("Its {n} threads are deleted, with their worktrees and checkpoints."),
        };
        dialog
            .title(format!("Remove “{name}” from Agentloft?"))
            .child(format!(
                "{what} The folder and its files on disk are not touched."
            ))
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("cancel").cursor_pointer().label("Cancel")),
                    )
                    .child(
                        DialogAction::new()
                            .child(Button::new("ok").cursor_pointer().danger().label("Remove")),
                    ),
            )
            .on_ok(move |_, _, cx| {
                let id = project_id.clone();
                store.update(cx, |s, cx| s.remove_project(id, cx));
                true
            })
    });
}

fn confirm_delete(
    store: Entity<AppStore>,
    thread_id: ThreadId,
    title: String,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
    let branch = store
        .read(cx)
        .workspace
        .threads
        .iter()
        .find(|t| t.id == thread_id)
        .and_then(|t| t.workspace.branch().map(str::to_owned));
    window.open_dialog(cx, move |dialog, _, _| {
        let dialog = dialog.centered();
        let (store, thread_id) = (store.clone(), thread_id.clone());
        let mut text = "Its conversation, turn history and checkpoints are erased and its \
                        agent and terminals are stopped. This can't be undone."
            .to_owned();
        if let Some(branch) = &branch {
            text.push_str(&format!(
                " The worktree is removed; the branch {branch} is kept."
            ));
        }
        dialog
            .title(format!("Delete “{title}”?"))
            .child(text)
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("cancel").cursor_pointer().label("Cancel")),
                    )
                    .child(
                        DialogAction::new()
                            .child(Button::new("ok").cursor_pointer().danger().label("Delete")),
                    ),
            )
            .on_ok(move |_, _, cx| {
                let t = thread_id.clone();
                store.update(cx, |s, cx| s.delete_thread(t, cx));
                true
            })
    });
}

fn confirm_archive(
    thread_id: ThreadId,
    reason: String,
    store: Entity<AppStore>,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
    window.open_dialog(cx, move |dialog, _, _| {
        let dialog = dialog.centered();
        let (store, thread_id) = (store.clone(), thread_id.clone());
        dialog
            .title("Archive thread?")
            .child(format!(
                "{reason}. Archiving removes the worktree and discards them; the branch is kept."
            ))
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("cancel").cursor_pointer().label("Cancel")),
                    )
                    .child(
                        DialogAction::new().child(
                            Button::new("ok")
                                .cursor_pointer()
                                .danger()
                                .label("Archive anyway"),
                        ),
                    ),
            )
            .on_ok(move |_, _, cx| {
                let t = thread_id.clone();
                store.update(cx, |s, cx| s.archive_thread(t, true, cx));
                true
            })
    });
}

/// `origin/main` or `main`.
fn base_label(base: &loft_protocol::WorktreeBase) -> String {
    match &base.remote {
        Some(r) => format!("{r}/{}", base.branch),
        None => base.branch.clone(),
    }
}

/// Local checkout vs. a new worktree, for a thread that hasn't started.
fn workspace_picker(
    thread_id: &ThreadId,
    current: &ThreadWorkspace,
    store: &Entity<AppStore>,
    p: &Palette,
) -> AnyElement {
    let wants = current.is_new_worktree();
    let mut row = h_flex()
        .gap_1()
        .text_xs()
        .max_w_full()
        .flex_wrap()
        .justify_center();
    for (worktree, label, icon) in [
        (false, "Local", IconName::Laptop),
        (true, "New worktree", IconName::GitBranch),
    ] {
        let active = wants == worktree;
        let store = store.clone();
        let thread_id = thread_id.clone();
        row = row.child(
            h_flex()
                .id(SharedString::from(format!("workspace-{worktree}")))
                .gap_1()
                .items_center()
                .px_2()
                .py_1()
                .rounded(px(6.))
                .cursor_pointer()
                .border_1()
                .when(active, |d| {
                    d.border_color(p.border)
                        .bg(p.elevated)
                        .text_color(p.foreground)
                })
                .when(!active, |d| {
                    d.border_color(gpui_kit::transparent_black())
                        .text_color(p.muted_foreground)
                        .hover(|d| d.text_color(p.foreground))
                })
                .child(Icon::new(icon).xsmall())
                .child(label)
                .on_click(move |_, _, cx| {
                    if !active {
                        let t = thread_id.clone();
                        store.update(cx, |s, cx| s.set_thread_workspace(t, worktree, None, cx));
                    }
                }),
        );
    }
    row.into_any_element()
}

/// Claude Code / Codex segmented picker for a thread that hasn't started.
fn harness_picker(
    thread_id: &ThreadId,
    current: &HarnessId,
    choices: Vec<(&'static str, &'static str)>,
    store: &Entity<AppStore>,
    p: &Palette,
) -> AnyElement {
    let mut row = h_flex()
        .mt_3()
        .p(px(3.))
        .gap(px(3.))
        .rounded(px(10.))
        .bg(p.muted)
        // More agents than fit on a line go onto a second one.
        .max_w_full()
        .flex_wrap()
        .justify_center();
    for (id, label) in choices {
        let active = current.as_str() == id;
        let store = store.clone();
        let thread_id = thread_id.clone();
        row = row.child(
            h_flex()
                .id(SharedString::from(format!("harness-{id}")))
                .gap_1p5()
                .items_center()
                .px_3()
                .py_1p5()
                .rounded(px(8.))
                .text_sm()
                .cursor_pointer()
                .when(active, |d| {
                    d.bg(p.background)
                        .text_color(p.foreground)
                        .font_weight(FontWeight::MEDIUM)
                        .shadow_sm()
                })
                .when(!active, |d| {
                    d.text_color(p.muted_foreground)
                        .hover(|d| d.text_color(p.foreground))
                })
                .child(crate::brand::agent_logo(id, px(15.), p))
                .child(label)
                .on_click(move |_, _, cx| {
                    if !active {
                        let (t, h) = (thread_id.clone(), HarnessId(id.into()));
                        store.update(cx, |s, cx| s.set_thread_harness(t, h, cx));
                    }
                }),
        );
    }
    row.into_any_element()
}

pub(crate) fn pretty_model(id: &str) -> String {
    // Provider-qualified ids (ACP agents): `opencode/iceman-free` → `iceman-free`.
    if let Some((_, name)) = id.rsplit_once('/') {
        return name.to_owned();
    }
    if let Some(rest) = id.strip_prefix("gpt-") {
        // gpt-5.1-codex → GPT-5.1 Codex
        let mut parts = rest.split('-');
        let version = parts.next().unwrap_or_default();
        let tail: Vec<String> = parts
            .map(|w| {
                let mut c = w.chars();
                c.next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default()
            })
            .collect();
        return format!("GPT-{version} {}", tail.join(" "))
            .trim()
            .to_owned();
    }
    let rest = id.strip_prefix("claude-").unwrap_or(id);
    let mut parts = rest.split('-');
    let Some(family) = parts.next() else {
        return id.to_owned();
    };
    let version: Vec<&str> = parts.take_while(|s| s.len() <= 2).collect();
    let mut name = family[..1].to_uppercase() + &family[1..];
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

const SETTINGS_TEMPLATE: &str = r##"# Agentloft settings — changes apply live.

[ui]
# theme = "system"          # system | light | dark
# accent = "#6e5bff"        # any #rrggbb
# font_family = "Inter"     # default: system UI font
# mono_font_family = "JetBrains Mono"
# font_size = 14
# mono_font_size = 12.5
# density = "comfortable"   # comfortable | compact
# content_width = 780       # conversation column width (px)

[harness.claude]
# path = "auto"
# default_permission_mode = "harness-default"
"##;

/// A thread's numbers over all its turns: how full the context is now,
/// the tokens it has used, and what it has cost (when the agent says).
#[derive(Default)]
struct ThreadStats {
    context: Option<loft_protocol::ContextUse>,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    cost: Option<f64>,
    /// Some of the cost is an estimate at API prices.
    estimated: bool,
}

impl ThreadStats {
    fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }

    fn fraction(&self) -> Option<f32> {
        let c = self.context?;
        Some(c.tokens as f32 / c.window?.max(1) as f32)
    }
}

fn thread_stats(
    store: &Entity<AppStore>,
    id: &ThreadId,
    cx: &gpui_kit::App,
) -> Option<ThreadStats> {
    let prices = &store.read(cx).pricing;
    let catalog = store.read(cx).price_catalog.clone();
    let thread = store.read(cx).threads.get(id)?.read(cx);
    let model = thread.model.as_ref()?;
    let turns = &model.turns;
    let mut stats = ThreadStats {
        context: turns.iter().rev().find_map(|t| t.context),
        ..Default::default()
    };
    // A window said once (Claude says it at a turn's end) still holds.
    if let Some(c) = stats.context.as_mut()
        && c.window.is_none()
    {
        c.window = turns.iter().rev().find_map(|t| t.context?.window);
    }
    for turn in turns {
        let Some(u) = &turn.usage else { continue };
        stats.input += u.input_tokens;
        stats.cache_read += u.cache_read_tokens;
        stats.cache_write += u.cache_write_tokens;
        stats.output += u.output_tokens;
        // The agent's own figure, else the tokens at the model's API price.
        let estimate = || {
            let name = turn.model.as_deref().or(model.thread.model.as_deref())?;
            let context = turn.context.map(|c| c.tokens);
            let p = loft_config::pricing::price(name, context, prices, catalog.as_deref())?;
            stats.estimated = true;
            Some(
                (u.input_tokens as f64 * p.input
                    + u.cache_read_tokens as f64 * p.cache_read
                    + u.cache_write_tokens as f64 * p.cache_write
                    + u.output_tokens as f64 * p.output)
                    / 1e6,
            )
        };
        if let Some(c) = u.cost_usd.or_else(estimate) {
            *stats.cost.get_or_insert(0.) += c;
        }
    }
    (stats.context.is_some() || stats.total() > 0 || stats.cost.is_some()).then_some(stats)
}

/// "$0.42", "$12.30", "<$0.01".
fn format_cost(usd: f64) -> String {
    if usd > 0. && usd < 0.01 {
        "<$0.01".into()
    } else {
        format!("${usd:.2}")
    }
}

/// The accent, amber past 75%, red past 90% (as when the agent is about
/// to compact by itself).
fn context_color(fraction: Option<f32>, p: &Palette) -> gpui_kit::Hsla {
    match fraction {
        Some(f) if f >= 0.9 => p.danger,
        Some(f) if f >= 0.75 => p.warning,
        _ => p.accent,
    }
}

/// "230K", "1.2M".
/// What a turn-map tick's card says: the first line of the message at
/// `message`, and the start of its turn's reply as plain text.
fn turn_peek(items: &[loft_protocol::Item], message: usize) -> (String, String) {
    let item = &items[message];
    let loft_protocol::ItemBody::UserMessage { text, attachments } = &item.body else {
        return Default::default();
    };
    let title = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(90).collect::<String>())
        .unwrap_or_else(|| match attachments.len() {
            0 => "Message".into(),
            1 => "Image".into(),
            n => format!("{n} images"),
        });
    let reply = items
        .iter()
        .rev()
        .filter(|it| it.parent_id.is_none() && it.turn_id == item.turn_id)
        .find_map(|it| match &it.body {
            loft_protocol::ItemBody::AssistantMessage { text } => Some(text.as_str()),
            _ => None,
        })
        .map(|t| {
            // Plain text: no Markdown emphasis, code ticks or headings.
            // Only as much of the reply as can fill the card is looked at.
            let mut flat = String::new();
            for word in t
                .split_whitespace()
                .map(|w| w.replace(['*', '`', '#'], ""))
                .filter(|w| !w.is_empty())
            {
                if !flat.is_empty() {
                    flat.push(' ');
                }
                flat.push_str(&word);
                if flat.chars().count() >= 220 {
                    break;
                }
            }
            flat.chars().take(220).collect::<String>()
        })
        .unwrap_or_default();
    (title, reply)
}

fn short_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{}K", n / 1_000),
        n if n >= 1_000 => format!("{:.1}K", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// A 14 px ring: the track, and an arc clockwise from the top to `fraction`.
fn context_ring(fraction: f32, color: gpui_kit::Hsla, track: gpui_kit::Hsla) -> AnyElement {
    use gpui_kit::{PathBuilder, point};
    let fraction = fraction.clamp(0., 1.);
    gpui_kit::canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let center = bounds.center();
            let r = (bounds.size.width.min(bounds.size.height) / 2.) - px(1.5);
            let at = |turn: f32| {
                let a = turn * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                point(center.x + r * a.cos(), center.y + r * a.sin())
            };
            let arc = |from: f32, to: f32| {
                let steps = ((to - from) * 64.).ceil().max(1.) as usize;
                let mut b = PathBuilder::stroke(px(2.));
                b.move_to(at(from));
                for i in 1..=steps {
                    b.line_to(at(from + (to - from) * i as f32 / steps as f32));
                }
                b.build().ok()
            };
            if let Some(path) = arc(0., 1.) {
                window.paint_path(path, track);
            }
            if fraction > 0.
                && let Some(path) = arc(0., fraction.max(0.02))
            {
                window.paint_path(path, color);
            }
        },
    )
    .size(px(14.))
    .into_any_element()
}

/// Dev hook: one click at `x,y[,after_ms]` (logical pixels from the
/// window's top left) in whichever window is active by then, through the
/// window's own event handling: opens popovers and menus for a screenshot.
fn dev_click(spec: String, window: &mut Window, cx: &mut Context<WorkspaceView>) {
    let n: Vec<f32> = spec
        .trim_start()
        .trim_start_matches("move")
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    let at = |i: usize, or: f32| n.get(i).copied().unwrap_or(or);
    let position = gpui_kit::point(px(at(0, 10.)), px(at(1, 10.)));
    let after = std::time::Duration::from_millis(at(2, 2500.) as u64);
    // `x,y,after_ms,x2,y2`: a drag, pressed at the first place and let go
    // at the second.
    let released = gpui_kit::point(px(at(3, at(0, 10.))), px(at(4, at(1, 10.))));
    // `move x,y,after_ms`: the pointer only passes over, no button held.
    let only_moves = spec.trim_start().starts_with("move");
    cx.spawn_in(window, async move |_, cx| {
        use gpui_kit::{
            Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
        };
        cx.background_executor().timer(after).await;
        // The front window (Settings, once it has opened), else this one:
        // an app that isn't in front has no active window.
        let Ok(target) =
            cx.update(|window, cx| cx.active_window().unwrap_or_else(|| window.window_handle()))
        else {
            return;
        };
        let (button, modifiers) = (MouseButton::Left, Modifiers::default());
        for step in [
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers,
            }),
            PlatformInput::MouseDown(MouseDownEvent {
                button,
                position,
                modifiers,
                click_count: 1,
                first_mouse: false,
            }),
            PlatformInput::MouseMove(MouseMoveEvent {
                position: released,
                pressed_button: Some(button),
                modifiers,
            }),
            PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: released,
                modifiers,
                click_count: 1,
            }),
        ]
        .into_iter()
        .enumerate()
        // (The move with the button down is a drag's only.)
        .filter_map(|(ix, step)| (ix != 2 || released != position).then_some(step))
        .take(if only_moves { 1 } else { 4 })
        {
            let _ = cx.update(|window, cx| {
                // This window is already in hand; another one is asked for.
                if target == window.window_handle() {
                    window.dispatch_event(step, cx);
                } else {
                    let _ = target.update(cx, |_, window, cx| {
                        window.dispatch_event(step, cx);
                    });
                }
            });
            cx.background_executor()
                .timer(std::time::Duration::from_millis(40))
                .await;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::pretty_model;

    #[test]
    fn doc_notes_are_written_a_block_per_file() {
        use super::{doc_notes_block, doc_notes_image_prefix};
        use crate::panel::docs::{DocNote, Place};
        let note = |n: u32, place: Place, what: &str, text: &str| DocNote {
            n,
            place,
            chip: String::new(),
            what: what.to_owned(),
            note: text.to_owned(),
        };
        let notes = vec![
            (
                "decks/pitch.pptx".to_owned(),
                note(
                    1,
                    Place::Shape { slide: 2, shape: 4 },
                    "slide 2, the shape “Card” (“Hello”)",
                    "Make it blue",
                ),
            ),
            (
                "report.pdf".to_owned(),
                note(
                    1,
                    Place::Page {
                        page: 2,
                        x: 0.5,
                        y: 0.25,
                    },
                    "page 3, 50% across and 25% down",
                    "Wrong total",
                ),
            ),
            (
                "decks/pitch.pptx".to_owned(),
                note(2, Place::Text, "slide 5", "Drop this slide"),
            ),
        ];
        let prefix = doc_notes_image_prefix("report.pdf");
        assert_eq!(prefix, "notes-report.pdf-page-");
        let picture = format!("{prefix}3.png");
        assert_eq!(
            doc_notes_block(&notes, &["screenshot.png", &picture]),
            "Notes on decks/pitch.pptx:\n\
             1. On slide 2, the shape “Card” (“Hello”): Make it blue\n\
             2. On slide 5: Drop this slide\n\n\
             Notes on report.pdf (the attached picture of the page shows them as numbered pins):\n\
             1. On page 3, 50% across and 25% down: Wrong total"
        );
    }

    #[test]
    fn mini_browser_stays_inside_the_conversation() {
        use super::{PREVIEW_CARD_H, PREVIEW_CARD_W, keep_preview_in};
        use gpui_kit::{point, px, size};
        let room = size(px(800.), px(600.));
        // Inside: left alone.
        assert_eq!(
            keep_preview_in(point(px(20.), px(150.)), room),
            point(px(20.), px(150.))
        );
        // Past the right/bottom edges, or past the left/top ones.
        assert_eq!(
            keep_preview_in(point(px(-50.), px(-9.)), room),
            point(px(0.), px(0.))
        );
        assert_eq!(
            keep_preview_in(point(px(5000.), px(5000.)), room),
            point(px(800. - PREVIEW_CARD_W), px(600. - PREVIEW_CARD_H))
        );
        // Not laid out yet: as it was.
        let unknown = size(px(0.), px(0.));
        assert_eq!(
            keep_preview_in(point(px(5000.), px(1.)), unknown),
            point(px(5000.), px(1.))
        );
    }

    #[test]
    fn model_names() {
        assert_eq!(pretty_model("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(pretty_model("claude-sonnet-5-5-20261001"), "Sonnet 5.5");
        assert_eq!(pretty_model("gpt-5"), "GPT-5");
        assert_eq!(pretty_model("gpt-5.1-codex-max"), "GPT-5.1 Codex Max");
    }
}
