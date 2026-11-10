//! The main window: an integrated sidebar (search, projects → threads,
//! status), a breadcrumb header, the conversation timeline and a floating
//! composer.

use crate::items::render_row;
use crate::panel::{Tab, WorkbenchPanel};
use crate::store::{AppStore, ConnState, StoreEvent, ThreadState};
use crate::theme::{self, Palette, relative_time};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, ElementId, Entity, FontWeight, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, actions, div,
    prelude::FluentBuilder as _, px,
};
use loft_protocol::{HarnessId, ProjectId, ThreadId, ThreadStatus, ThreadWorkspace};
use std::collections::HashMap;

actions!(
    agentloft,
    [
        NewThread,
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
        ShowBrowser,
        NewWindow,
        NewTab,
        OpenPalette
    ]
);

const CONTEXT: &str = "Workspace";
const SIDEBAR_WIDTH: f32 = 272.;
const HEADER_HEIGHT: f32 = 52.;
/// Leaves room for the macOS traffic lights in the transparent title bar.
const TRAFFIC_LIGHTS_INSET: f32 = 78.;

pub fn bind_keys(cx: &mut gpui_kit::App) {
    loft_browser::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("cmd-n", NewThread, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-n", NewWindow, None),
        KeyBinding::new("cmd-t", NewTab, None),
        KeyBinding::new("cmd-.", Interrupt, Some(CONTEXT)),
        KeyBinding::new("cmd-b", ToggleSidebar, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-l", ToggleTheme, Some(CONTEXT)),
        KeyBinding::new("cmd-,", OpenSettings, Some(CONTEXT)),
        KeyBinding::new("cmd-o", AddProject, Some(CONTEXT)),
        KeyBinding::new("cmd-k", OpenPalette, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-f", FocusSearch, Some(CONTEXT)),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-j", TogglePanel, Some(CONTEXT)),
        KeyBinding::new("cmd-enter", ApproveOnce, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-enter", ApproveSession, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-backspace", DenyRequest, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-e", ShowFiles, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-g", ShowChanges, Some(CONTEXT)),
        KeyBinding::new("ctrl-`", ShowTerminal, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-b", ShowBrowser, Some(CONTEXT)),
    ]);
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
            MenuItem::action("Terminal", ShowTerminal),
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
    composers: HashMap<ThreadId, Entity<TextareaState>>,
    /// Images waiting in each thread's composer.
    attachments: HashMap<ThreadId, Vec<crate::attachments::PendingImage>>,
    /// Large pastes held as chips until sent, per thread.
    pasted: HashMap<ThreadId, Vec<String>>,
    /// Notes left on the browser page, waiting to be sent.
    page_notes: HashMap<ThreadId, Vec<loft_browser::Annotation>>,
    composer_harness: HashMap<ThreadId, HarnessId>,
    observed: HashMap<ThreadId, Subscription>,
    search: Entity<InputState>,
    query: String,
    /// The sidebar lists archived threads too.
    show_archived: bool,
    /// The sidebar thread whose ··· menu is open.
    row_menu: Option<ThreadId>,
    /// Threads whose messages match the sidebar search, and the search
    /// those answer.
    search_hits: HashMap<ThreadId, loft_protocol::SearchHit>,
    search_seq: u64,
    /// The open `/` or `@` completion popover.
    completion: Option<crate::completion::Completion>,
    completion_seq: u64,
    /// Open the folder browser on the next render (asked for without a
    /// window at hand).
    pending_add_project: bool,
    /// The floating mini browser, shown when an agent drives the browser
    /// while the panel isn't showing it.
    browser_preview: bool,
    /// Whether we listen to the browser's events yet (annotations).
    /// The composer's model picker (for the selected thread) and whether
    /// it's open.
    model_picker: Option<Entity<crate::model_picker::ModelPicker>>,
    picker_open: bool,
    /// Pending debounced draft saves, per thread.
    draft_saves: HashMap<ThreadId, gpui_kit::Task<()>>,
    /// Threads whose saved draft has been put back in their composer.
    drafts_restored: std::collections::HashSet<ThreadId>,
    /// The new-worktree branch chooser and whether it's open.
    branch_picker: Option<Entity<crate::branch_picker::BranchPicker>>,
    branch_picker_open: bool,
    pub(crate) sidebar_open: bool,
    pub panel: Entity<WorkbenchPanel>,
    pub(crate) panel_open: bool,
    /// Where the details button is (its card opens under it).
    details_anchor: std::rc::Rc<std::cell::Cell<Option<gpui_kit::Bounds<gpui_kit::Pixels>>>>,
    sidebar_slide: Slide,
    /// How far open the sidebar is drawn this frame (0–1).
    sidebar_amount: f32,
    panel_slide: Slide,
    details_slide: Slide,
    /// The side panel's width as last laid out (it slides at this width).
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
    /// Dev/test hook: `AGENTLOFT_DEV_TAB=files|changes|agent|file:<path>`
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
        let subs = vec![
            cx.subscribe_in(&store, window, |this, _, event, window, cx| {
                this.on_store_event(event, window, cx)
            }),
            // Notes and screenshots from a thread's browser go to its composer.
            cx.subscribe_in(&panel, window, |this, _, event, window, cx| match event {
                crate::panel::PanelEvent::Annotation(thread_id, note) => {
                    this.add_annotation(thread_id, note, window, cx)
                }
                crate::panel::PanelEvent::Maximized(_) => cx.notify(),
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
            // Switching native tabs: refresh the tab strip.
            cx.observe_window_activation(window, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            composers: HashMap::new(),
            attachments: HashMap::new(),
            pasted: HashMap::new(),
            page_notes: HashMap::new(),
            composer_harness: HashMap::new(),
            observed: HashMap::new(),
            search,
            query: String::new(),
            show_archived: false,
            row_menu: None,
            search_hits: HashMap::new(),
            search_seq: 0,
            completion: None,
            completion_seq: 0,
            pending_add_project: false,
            browser_preview: false,
            model_picker: None,
            picker_open: false,
            branch_picker: None,
            branch_picker_open: false,
            draft_saves: HashMap::new(),
            drafts_restored: Default::default(),
            sidebar_open: true,
            panel,
            panel_open: false,
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
            panel_width: std::rc::Rc::new(std::cell::Cell::new(560.)),
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

    fn on_store_event(&mut self, event: &StoreEvent, window: &mut Window, cx: &mut Context<Self>) {
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
                    note.with_type(gpui_kit::component::notification::NotificationType::Error)
                } else {
                    note.with_type(gpui_kit::component::notification::NotificationType::Success)
                };
                window.push_notification(note, cx);
            }
            StoreEvent::Browse { url } => {
                self.panel_open = true;
                self.panel
                    .update(cx, |p, cx| p.show(Tab::Browser, window, cx));
                if let Some(browser) = self.panel.update(cx, |p, cx| p.browser(window, cx)) {
                    browser.update(cx, |b, cx| b.navigate(url, cx));
                }
            }
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
            Some(("addproject", text)) => {
                let text = text.to_owned();
                cx.defer_in(window, move |this, window, cx| {
                    let picker = this.add_project(window, cx);
                    picker.update(cx, |p, cx| p.type_text(&text, window, cx));
                });
            }
            Some(("settings", section)) => {
                let section = crate::settings::Section::parse(section);
                crate::settings::open(self.store.clone(), section, cx);
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
            Some(("queue", text)) => {
                self.panel_open = false;
                let queued = crate::store::Queued {
                    text: text.to_owned(),
                    images: Vec::new(),
                };
                let id = thread_id.clone();
                self.store.update(cx, |s, cx| s.enqueue(id, queued, cx));
            }
            Some(("file", path)) => {
                let path = path.to_owned();
                self.panel
                    .update(cx, |p, cx| p.open_file(&thread_id, path, window, cx));
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
                "rename" => {
                    let title = thread
                        .read(cx)
                        .model
                        .as_ref()
                        .map(|m| m.thread.title.clone())
                        .unwrap_or_default();
                    rename_dialog(self.store.clone(), thread_id.clone(), title, window, cx);
                }
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

    /// Chips for notes left on the page, each removable.
    fn note_chips(
        &self,
        thread_id: &ThreadId,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let notes = self.page_notes.get(thread_id).filter(|v| !v.is_empty())?;
        let mut row = h_flex().gap_1p5().flex_wrap().px_3().pt_2();
        for (ix, note) in notes.iter().enumerate() {
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
                h_flex()
                    .id(SharedString::from(format!("note-{ix}")))
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
                    .tooltip({
                        let text = describe_note(note);
                        move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(text.clone())
                                .build(window, cx)
                        }
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
                            .child(note.n.max(1).to_string()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.foreground)
                            .child(what),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(p.muted_foreground)
                            .child(note.note.clone()),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("note-remove-{ix}")))
                            .flex_none()
                            .cursor_pointer()
                            .p(px(2.))
                            .rounded(px(5.))
                            .text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.selected).text_color(p.foreground))
                            .child(Icon::new(IconName::X).xsmall())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_note(&tid, ix, cx);
                            })),
                    ),
            );
        }
        Some(row.into_any_element())
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
        let amount = self
            .details_slide
            .update(self.details_open && self.can_dock(), window);
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
                    .flex_none()
                    .w(px(DOCK_WIDTH * amount))
                    .h_full()
                    .overflow_hidden()
                    .child(div().w(px(DOCK_WIDTH)).pt_3().pr_3().child(card))
                    .into_any_element()
            }))
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
        let sub = cx.subscribe_in(&composer, window, move |this, state, event, window, cx| {
            match event {
                // ⌘⏎ / ⌘⇧⏎ answer an open approval even while typing.
                InputEvent::PressEnter {
                    secondary: true,
                    shift,
                } if this.open_request(cx).is_some() => {
                    let decision = if *shift {
                        loft_protocol::DecisionKind::AllowSession
                    } else {
                        loft_protocol::DecisionKind::AllowOnce
                    };
                    this.answer(decision, cx);
                }
                InputEvent::PressEnter { shift: false, .. } => {
                    this.submit(&thread_id, state, window, cx);
                }
                InputEvent::Change => {
                    this.refresh_completion(&thread_id, state, cx);
                    this.queue_draft_save(&thread_id, state, cx);
                }
                InputEvent::Blur if this.completion.take().is_some() => cx.notify(),
                _ => {}
            }
        });
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

    fn submit(
        &mut self,
        thread_id: &ThreadId,
        composer: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let typed = composer.read(cx).value().trim().to_owned();
        let has_images = self
            .attachments
            .get(thread_id)
            .is_some_and(|a| !a.is_empty());
        let pasted = self.pasted.get(thread_id).cloned().unwrap_or_default();
        let notes = self.page_notes.get(thread_id).cloned().unwrap_or_default();
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
        if harness == loft_protocol::HarnessId::CODEX && pasted.is_empty() && notes.is_empty() {
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
        if let Some(kind) = report.filter(|_| pasted.is_empty() && notes.is_empty())
            && let Some(thread) = self.store.read(cx).threads.get(thread_id).cloned()
        {
            composer.update(cx, |c, cx| c.set_value("", window, cx));
            let client = self.store.read(cx).client();
            thread.update(cx, |t, cx| t.add_card(kind, client, cx));
            return;
        }
        if typed.is_empty() && !has_images && pasted.is_empty() && notes.is_empty() {
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
        // Pasted blocks follow what was typed.
        let mut parts = Vec::new();
        if !typed.is_empty() {
            parts.push(typed);
        }
        parts.extend(pasted.into_iter().map(|p| p.trim_end().to_owned()));
        if !notes.is_empty() {
            parts.push(notes_block(&notes));
        }
        let text = parts.join("\n\n");
        composer.update(cx, |c, cx| c.set_value("", window, cx));
        let images = self.attachments.remove(thread_id).unwrap_or_default();
        let attachments: Vec<loft_protocol::Attachment> =
            images.iter().map(|a| a.to_attachment()).collect();
        let id = thread_id.clone();
        if self.is_running(thread_id, cx) {
            // Sent automatically when the current turn ends.
            let queued = crate::store::Queued { text, images };
            self.store
                .update(cx, |store, cx| store.enqueue(id, queued, cx));
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
        let mut marks: Vec<(usize, String, String)> = Vec::new();
        for (row_ix, row) in state.rows.iter().enumerate() {
            let crate::store::Row::Item(i) = row else {
                continue;
            };
            let loft_protocol::ItemBody::UserMessage { text, attachments } = &items[*i].body else {
                continue;
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
            let turn = &items[*i].turn_id;
            let reply = items
                .iter()
                .rev()
                .filter(|it| it.parent_id.is_none() && &it.turn_id == turn)
                .find_map(|it| match &it.body {
                    loft_protocol::ItemBody::AssistantMessage { text } => Some(text.as_str()),
                    _ => None,
                })
                .map(|t| {
                    // Plain text: no Markdown emphasis, code ticks or headings.
                    let plain: String = t
                        .chars()
                        .filter(|c| !matches!(c, '*' | '`' | '#'))
                        .collect();
                    let flat = plain.split_whitespace().collect::<Vec<_>>().join(" ");
                    flat.chars().take(220).collect::<String>()
                })
                .unwrap_or_default();
            marks.push((row_ix, title, reply));
        }
        if marks.len() < 2 {
            return None;
        }
        // Only in the margin beside the text, never over it.
        let width = self.timeline_widths.get(&tid).map_or(0., |w| w.get());
        let content = f32::from(p.content_width).min(width);
        let margin = (width - content) / 2. + 24.;
        if width <= 0. || margin < 40. {
            return None;
        }
        // Where the reader is: the last message at or above the top row.
        let top = state.scroller.read(cx).logical_scroll_top().item_ix;
        let current = marks.iter().rposition(|(row, ..)| *row <= top).unwrap_or(0);
        let spacing = match marks.len() {
            0..=40 => 14.,
            41..=80 => 8.,
            _ => 5.,
        };
        let scroller = state.scroller.clone();
        let mut rail = v_flex().w(px(34.)).items_start();
        for (ix, (row, title, reply)) in marks.into_iter().enumerate() {
            let hovered = self.peek.as_ref() == Some(&(tid.clone(), ix));
            let is_current = ix == current;
            let scroller = scroller.clone();
            let card = hovered.then(|| {
                v_flex()
                    .absolute()
                    .left(px(38.))
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
                    .pl_3()
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .h(px(2.))
                            .rounded(px(1.))
                            .w(px(if is_current {
                                20.
                            } else if hovered {
                                16.
                            } else {
                                10.
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

    /// Messages waiting for the current turn to end: dashed bubbles you can
    /// send now (stopping the turn), take back to edit, or drop.
    fn queued_list(
        &self,
        thread_id: &ThreadId,
        queued: &[crate::store::Queued],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use gpui_kit::StyledImage as _;
        if queued.is_empty() {
            return None;
        }
        let mut list = v_flex().gap_3().pb_2();
        for (ix, q) in queued.iter().enumerate() {
            let mut thumbs = h_flex().gap_1p5().flex_wrap().justify_end();
            for (n, image) in q.images.iter().enumerate() {
                thumbs = thumbs.child(
                    div()
                        .id(SharedString::from(format!("queued-{ix}-img-{n}")))
                        .size(px(44.))
                        .rounded(px(8.))
                        .overflow_hidden()
                        .border_1()
                        .border_color(p.border)
                        .child(
                            gpui_kit::img(image.preview.clone())
                                .size_full()
                                .object_fit(gpui_kit::ObjectFit::Cover),
                        ),
                );
            }
            let preview: String = q.text.chars().take(400).collect();
            let action = |id: &str, icon: Option<IconName>, label: Option<&'static str>| {
                h_flex()
                    .id(SharedString::from(format!("{id}-{ix}")))
                    .gap_1()
                    .items_center()
                    .px_1p5()
                    .py(px(3.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    .children(icon.map(|i| Icon::new(i).xsmall()))
                    .children(label)
            };
            let (t1, t2, t3) = (thread_id.clone(), thread_id.clone(), thread_id.clone());
            list = list.child(
                v_flex()
                    .items_end()
                    .gap_1()
                    .child(
                        v_flex()
                            .max_w(gpui_kit::relative(0.85))
                            .gap_2()
                            .px_4()
                            .py_2p5()
                            .rounded(px(16.))
                            .border_1()
                            .border_dashed()
                            .border_color(p.border)
                            .text_sm()
                            .text_color(p.muted_foreground)
                            .when(!q.images.is_empty(), |d| d.child(thumbs))
                            .when(!preview.is_empty(), |d| {
                                d.child(div().line_clamp(4).child(preview))
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .items_center()
                            .child(
                                action("queued-send", Some(IconName::ArrowUp), Some("Send now"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let id = t1.clone();
                                        this.store
                                            .update(cx, |s, cx| s.send_queued_now(id, ix, cx));
                                    })),
                            )
                            .child(
                                action("queued-edit", Some(IconName::SquarePen), None).on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        this.edit_queued(&t2, ix, window, cx)
                                    }),
                                ),
                            )
                            .child(action("queued-drop", Some(IconName::X), None).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    let id = t3.clone();
                                    this.store.update(cx, |s, cx| s.dequeue(&id, ix, cx));
                                }),
                            )),
                    ),
            );
        }
        Some(list.into_any_element())
    }

    /// Put a queued message back in the composer (text and images).
    fn edit_queued(
        &mut self,
        thread_id: &ThreadId,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(q) = self
            .store
            .update(cx, |s, cx| s.take_queued(thread_id, ix, cx))
        else {
            return;
        };
        let Some(thread) = self.store.read(cx).threads.get(thread_id).cloned() else {
            return;
        };
        let composer = self.composer_for(&thread, window, cx);
        composer.update(cx, |c, cx| {
            let current = c.value().to_string();
            let text = if current.trim().is_empty() {
                q.text.clone()
            } else {
                format!("{}\n\n{}", q.text, current.trim_end())
            };
            let end = text.len();
            c.set_value(text, window, cx);
            c.set_selected_range(end..end, cx);
            c.focus(window, cx);
        });
        self.add_images(thread_id, q.images, cx);
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

    fn answer(&mut self, decision: loft_protocol::DecisionKind, cx: &mut Context<Self>) {
        let Some(req) = self.open_request(cx) else {
            return;
        };
        if !req.options.contains(&decision) {
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
                    scroll,
                });
                cx.notify();
            }
            Kind::File => {
                let Some(client) = self.store.read(cx).client() else {
                    return;
                };
                let seq = self.completion_seq;
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
                        this.completion = (!items.is_empty()).then(|| Completion {
                            thread_id,
                            kind,
                            token,
                            selected: previous.min(items.len().saturating_sub(1)),
                            items,
                            info: Vec::new(),
                            scroll,
                        });
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
        if !path.exists() {
            let _ = std::fs::write(&path, SETTINGS_TEMPLATE);
        }
        crate::settings::open(self.store.clone(), None, cx);
    }

    // -- sidebar ------------------------------------------------------------

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

    fn render_sidebar(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (projects, all_threads, selected, conn, error, unread, collapsed) = {
            let store = self.store.read(cx);
            (
                store.workspace.projects.clone(),
                store.workspace.threads.clone(),
                store.selected.clone(),
                store.conn.clone(),
                store.last_error.clone(),
                store.unread.clone(),
                store.collapsed.clone(),
            )
        };
        let all_projects: Vec<ProjectId> = projects.iter().map(|p| p.id.clone()).collect();
        let query = self.query.clone();
        let mut list = v_flex().gap_4().px_2().pb_4();

        for project in &projects {
            let project_id = project.id.clone();
            let threads: Vec<_> = all_threads
                .iter()
                .filter(|t| t.project_id == project.id && t.status != ThreadStatus::Archived)
                // A new thread shows once it has a message or a draft (or
                // while it's open).
                .filter(|t| {
                    t.is_started()
                        || t.parent.is_some()
                        || t.draft_text().is_some()
                        || selected.as_ref() == Some(&t.id)
                })
                .filter(|t| self.search_matches(t, &project.name))
                .collect();
            if !query.is_empty() && threads.is_empty() {
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
                p,
                cx.listener(move |this, _, _, cx| this.new_thread(Some(project_id.clone()), cx)),
            );
            let mut rows = v_flex().gap_px();
            if !folded {
                for (thread, depth) in nest_forks(threads) {
                    let id = thread.id.clone();
                    rows = rows.child(thread_row(
                        &self.store,
                        thread,
                        depth,
                        selected.as_ref() == Some(&id),
                        unread.contains(&id),
                        self.row_menu.as_ref() == Some(&id),
                        // A message matched (not just the title): show where.
                        self.search_hits
                            .get(&id)
                            .filter(|_| !thread.title.to_lowercase().contains(&query)),
                        p,
                        cx,
                    ));
                }
            }
            list = list.child(v_flex().gap_1().child(header).child(rows));
        }

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
                h_flex()
                    .id("archived-toggle")
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .hover(|d| d.bg(p.hover))
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
            list = list.child(section);
        }

        let (dot, status): (_, SharedString) = match &conn {
            ConnState::Connecting => (p.warning, "Connecting…".into()),
            ConnState::Connected => (p.success, "Connected".into()),
            ConnState::Disconnected(_) => (p.danger, "Reconnecting…".into()),
        };
        let dark = p.dark;

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
                    .pl(px(TRAFFIC_LIGHTS_INSET))
                    .pr_2()
                    .items_center()
                    .justify_end()
                    .gap_1()
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
                        cx.listener(|this, _, _, cx| this.new_thread(None, cx)),
                    )),
            )
            .child(
                div().px_3().pb_3().child(
                    Input::new(&self.search)
                        .small()
                        .prefix(
                            Icon::new(IconName::Search)
                                .xsmall()
                                .text_color(p.muted_foreground),
                        )
                        .cleanable(true),
                ),
            )
            .child(
                h_flex()
                    .px_4()
                    .pb_2()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child("PROJECTS")
                    .child(icon_button(
                        "add-project",
                        IconName::FolderPlus,
                        p,
                        cx.listener(|this, _, window, cx| {
                            this.add_project(window, cx);
                        }),
                    )),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .when_some(error, |d, e| {
                d.child(
                    div()
                        .mx_3()
                        .mb_2()
                        .px_2()
                        .py_1p5()
                        .rounded(px(8.))
                        .bg(p.danger_soft)
                        .text_xs()
                        .text_color(p.danger)
                        .child(e),
                )
            })
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

    // -- main pane ----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn render_header(
        &mut self,
        title: Option<(String, loft_protocol::Thread)>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_thread = title.is_some();
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
                    ThreadWorkspace::Local => None,
                };
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
            })
            .when(has_thread, |d| {
                let anchor = self.details_anchor.clone();
                let measure = anchor.clone();
                let docked = self.details_open && self.can_dock();
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
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "another thread".into());
        let how = match parent.kind {
            loft_protocol::ForkKind::Native => "full context",
            loft_protocol::ForkKind::Handoff => "handoff",
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
                    .cursor_pointer()
                    .hover(|d| d.text_color(p.foreground).bg(p.hover))
                    .child(Icon::new(IconName::GitFork).xsmall())
                    .child(
                        div()
                            .max_w(px(420.))
                            .truncate()
                            .child(format!("Forked from “{title}”")),
                    )
                    .child(
                        div()
                            .text_color(p.faint_foreground)
                            .child(format!("· {how}")),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let id = id.clone();
                        this.store.update(cx, |s, cx| s.select_thread(id, cx));
                    })),
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
        let queued_list = {
            let id = thread.read(cx).id.clone();
            let queued: Vec<crate::store::Queued> = self
                .store
                .read(cx)
                .queued
                .get(&id)
                .map(|q| q.iter().cloned().collect())
                .unwrap_or_default();
            self.queued_list(&id, &queued, p, cx)
        };
        let turn_map = self.turn_map(&thread, p, cx);
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
        let waiting = pinned.is_empty() && model.open_requests().next().is_some();
        let empty = model.items.is_empty();
        let harness_choices = self.store.read(cx).available_harnesses();
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
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(p.muted_foreground)
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.foreground)
                        .child("What should we work on?"),
                )
                .when_some(model.thread.parent.clone(), |d, parent| {
                    let title = self
                        .store
                        .read(cx)
                        .workspace
                        .threads
                        .iter()
                        .find(|t| t.id == parent.thread_id)
                        .map(|t| t.title.clone())
                        .unwrap_or_else(|| "another thread".into());
                    let how = match parent.kind {
                        loft_protocol::ForkKind::Native => format!(
                            "{} continues with the full conversation.",
                            harness_name(&model.thread.harness)
                        ),
                        loft_protocol::ForkKind::Handoff => {
                            "It starts from a handoff summary. Review it below, then send.".into()
                        }
                    };
                    d.child(
                        h_flex()
                            .mt_2()
                            .gap_1p5()
                            .items_center()
                            .text_sm()
                            .child(Icon::new(IconName::GitFork).small())
                            .child(format!("Forked from “{title}”")),
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
                    .child(workspace_picker(
                        &model.thread.id,
                        &model.thread.workspace,
                        &self.store,
                        p,
                    ))
                    .children(base_row)
                })
                .child(
                    div()
                        .max_w(px(520.))
                        .truncate()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(match &model.thread.workspace {
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
                    .with_bottom_fade(p.background)
                    // Rows space themselves (`Palette::row_gap`); the
                    // scroller's own 32px gap under each row is too much.
                    .with_row_style(gpui_kit::StyleRefinement::default().pb(px(0.)))
                    .flex_1(),
                )
                .into_any_element()
        };

        let drop_accent = p.accent;
        let action = if running {
            let id = thread_id.clone();
            round_button(
                "stop",
                IconName::Square,
                p.foreground,
                p.background,
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
                    this.submit(&id, &composer_h, window, cx);
                }),
            )
        };

        v_flex()
            .relative()
            .flex_1()
            .min_h_0()
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
                        gpui_kit::canvas(
                            move |bounds, window, _| {
                                let w = f32::from(bounds.size.width);
                                if (width.get() - w).abs() > 0.5 {
                                    width.set(w);
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
                div().px_6().pb_5().pt_1().flex().justify_center().child(
                    v_flex()
                        .w_full()
                        .max_w(p.content_width)
                        .gap_1()
                        .children(queued_list)
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
                                .capture_action(cx.listener(
                                    |this, _: &gpui_kit::component::input::MoveUp, _, cx| {
                                        if let Some(c) = &mut this.completion {
                                            c.move_by(-1);
                                            cx.stop_propagation();
                                            cx.notify();
                                        }
                                    },
                                ))
                                .capture_action(cx.listener(
                                    |this, _: &gpui_kit::component::input::MoveDown, _, cx| {
                                        if let Some(c) = &mut this.completion {
                                            c.move_by(1);
                                            cx.stop_propagation();
                                            cx.notify();
                                        }
                                    },
                                ))
                                .capture_action(cx.listener(
                                    |this, _: &gpui_kit::component::input::Enter, window, cx| {
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
                                            let images = crate::attachments::from_clipboard(&item);
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
                                .children(strip)
                                .children(self.note_chips(&thread_id, p, cx))
                                .children(self.paste_chips(&thread_id, p, cx))
                                .child(
                                    div()
                                        .px_2()
                                        .pt_1()
                                        .child(Textarea::new(&composer).appearance(false)),
                                )
                                .child(
                                    h_flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .child({
                                                    let id = thread_id.clone();
                                                    icon_button(
                                                        "attach",
                                                        IconName::Paperclip,
                                                        p,
                                                        cx.listener(move |this, _, _, cx| {
                                                            this.pick_images(id.clone(), cx)
                                                        }),
                                                    )
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
                                                )),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .items_center()
                                                .when(running, |d| {
                                                    d.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(p.faint_foreground)
                                                            .child("⌘. to stop"),
                                                    )
                                                })
                                                .child(action),
                                        ),
                                ),
                        ),
                ),
            )
            // Last, so it floats over the conversation.
            .children(preview)
            .into_any_element()
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
        const W: f32 = 340.;
        const H: f32 = 212.;
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
        Some(
            v_flex()
                .absolute()
                .right(px(20.))
                .bottom(px(150.))
                .w(px(W + 2.))
                .rounded(px(12.))
                .overflow_hidden()
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .shadow_lg()
                .child(
                    h_flex()
                        .h(px(30.))
                        .px_2()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .border_b_1()
                        .border_color(p.border)
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
                        "⌘O",
                        p,
                        cx.listener(|this, _, window, cx| {
                            this.add_project(window, cx);
                        }),
                    ))
                    .when(has_projects, |d| {
                        d.child(pill_button(
                            "welcome-new",
                            IconName::SquarePen,
                            "New thread",
                            "⌘N",
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
        let p = Palette::of(cx);
        if std::mem::take(&mut self.pending_add_project) {
            cx.defer_in(window, |this, window, cx| {
                this.add_project(window, cx);
            });
        }
        let selected = self.store.read(cx).selected_thread();
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
            Some((project, model.thread.clone()))
        });

        let body = match selected {
            Some(thread) => self.render_panes(thread, &p, window, cx),
            None => self.render_welcome(&p, cx),
        };
        let body = self.with_details_dock(body, window, cx);
        let mut body_slot = Some(body);
        let show_panel = self.panel_open && header_title.is_some();
        let maximized = show_panel && self.panel.read(cx).maximized;
        let side = self.sidebar_slide.update(self.sidebar_open, window);
        self.sidebar_amount = side;
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
            .on_action(cx.listener(|this, _: &NewThread, _, cx| this.new_thread(None, cx)))
            .on_action(cx.listener(|this, _: &Interrupt, _, cx| this.interrupt(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .on_action(cx.listener(|_, _: &ToggleTheme, _, cx| theme::toggle_mode(cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.open_settings(cx)))
            .on_action(cx.listener(|this, _: &AddProject, window, cx| {
                this.add_project(window, cx);
            }))
            .on_action(cx.listener(|_, _: &About, window, cx| show_about(window, cx)))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                crate::palette::open(cx.entity(), &this.store, window, cx)
            }))
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
                                    .child(
                                        div()
                                            .flex_none()
                                            .h_full()
                                            .w(px(self.panel_width.get() * panel_amount))
                                            .overflow_hidden()
                                            .border_l_1()
                                            .border_color(p.border)
                                            .child(self.panel.clone()),
                                    ),
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
                                        .child(resizable_panel().children(body_slot.take()))
                                        .child(
                                            resizable_panel()
                                                .size(px(560.))
                                                .size_range(px(340.)..px(1600.))
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
    shortcut: &'static str,
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
fn nest_forks(threads: Vec<&loft_protocol::Thread>) -> Vec<(&loft_protocol::Thread, usize)> {
    let present: std::collections::HashSet<&ThreadId> = threads.iter().map(|t| &t.id).collect();
    let parent_of = |t: &loft_protocol::Thread| {
        t.parent
            .as_ref()
            .map(|p| &p.thread_id)
            .filter(|id| present.contains(id))
            .cloned()
    };
    fn push<'a>(
        t: &'a loft_protocol::Thread,
        depth: usize,
        all: &[&'a loft_protocol::Thread],
        parent_of: &dyn Fn(&loft_protocol::Thread) -> Option<ThreadId>,
        out: &mut Vec<(&'a loft_protocol::Thread, usize)>,
    ) {
        out.push((t, depth));
        for child in all.iter().filter(|c| parent_of(c).as_ref() == Some(&t.id)) {
            push(child, depth + 1, all, parent_of, out);
        }
    }
    let mut out = Vec::with_capacity(threads.len());
    for root in threads.iter().filter(|t| parent_of(t).is_none()) {
        push(root, 0, &threads, &parent_of, &mut out);
    }
    out
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
    p: &Palette,
    on_new: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> AnyElement {
    let id = project.id.clone();
    let group = SharedString::from(format!("project-{}", project.id));
    let waiting = threads
        .iter()
        .any(|t| t.status == ThreadStatus::WaitingApproval);
    let running = threads.iter().any(|t| t.status == ThreadStatus::Running);
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
        move |menu: gpui_kit::component::menu::PopupMenu,
              _: &mut Window,
              _: &mut Context<gpui_kit::component::menu::PopupMenu>| {
            use gpui_kit::component::menu::PopupMenuItem;
            let (s1, a1, s2, a2) = (store.clone(), all.clone(), store.clone(), all.clone());
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
                        .child(
                            Icon::new(if folded {
                                IconName::Folder
                            } else {
                                IconName::FolderOpen
                            })
                            .xsmall(),
                        ),
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

#[allow(clippy::too_many_arguments)]
fn thread_row(
    store: &Entity<AppStore>,
    thread: &loft_protocol::Thread,
    depth: usize,
    active: bool,
    unread: bool,
    menu_open: bool,
    hit: Option<&loft_protocol::SearchHit>,
    p: &Palette,
    cx: &mut Context<WorkspaceView>,
) -> AnyElement {
    let id = thread.id.clone();
    let trailing: AnyElement = match thread.status {
        ThreadStatus::Running => Spinner::new().xsmall().color(p.accent).into_any_element(),
        ThreadStatus::WaitingApproval => div()
            .px_1p5()
            .rounded(px(5.))
            .bg(p.warning_soft)
            .text_xs()
            .text_color(p.warning)
            .child("Approve")
            .into_any_element(),
        ThreadStatus::Error => Icon::new(IconName::CircleAlert)
            .xsmall()
            .text_color(p.danger)
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
    h_flex()
        .id(SharedString::from(format!("thread-{}", thread.id)))
        .gap_2()
        .items_center()
        .group("thread-row")
        .pl(px(12. + 14. * depth.min(4) as f32))
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
        .when(thread.parent.is_some(), |d| {
            d.child(
                Icon::new(IconName::GitFork)
                    .xsmall()
                    .text_color(p.faint_foreground),
            )
        })
        // Which agent runs it.
        .child(div().flex_none().when(!active, |d| d.opacity(0.85)).child(
            crate::brand::agent_logo(thread.harness.as_str(), px(14.), p),
        ))
        .child(match hit {
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(title)
                .into_any_element(),
            // Searching: the matching message under the title.
            Some(hit) => v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(1.))
                .child(div().truncate().child(title))
                .child(search_snippet(hit, p))
                .into_any_element(),
        })
        .when(thread.workspace.branch().is_some(), |d| {
            d.child(
                Icon::new(IconName::GitBranch)
                    .xsmall()
                    .text_color(p.faint_foreground),
            )
        })
        // Under the pointer, the time (or status) makes way for a ··· menu.
        .child(
            div()
                .relative()
                .flex_none()
                .min_w(px(22.))
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
                        .on_click(|_, _, cx| cx.stop_propagation())
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
                                    thread_menu_items(thread, store),
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
            cx.listener(move |this, event: &gpui_kit::ClickEvent, _, cx| {
                let id = id.clone();
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
        .on_drag(
            DraggedThread {
                id: thread.id.clone(),
                title: thread.title.clone(),
            },
            |d, _, _, cx| cx.new(|_| d.clone()),
        )
        .context_menu(thread_menu_items(thread, store))
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

fn rename_dialog(
    store: Entity<AppStore>,
    thread_id: ThreadId,
    title: String,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
    let input = cx.new(|cx| {
        let mut s = InputState::new(window, cx);
        s.set_value(title, window, cx);
        s
    });
    let focus = input.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let dialog = dialog.centered();
        let (store, thread_id, input) = (store.clone(), thread_id.clone(), input.clone());
        dialog
            .title("Rename thread")
            .w(px(420.))
            .child(Input::new(&input))
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("cancel").cursor_pointer().label("Cancel")),
                    )
                    .child(
                        DialogAction::new()
                            .child(Button::new("ok").cursor_pointer().primary().label("Rename")),
                    ),
            )
            .on_ok(move |_, _, cx| {
                let title = input.read(cx).value().trim().to_owned();
                if title.is_empty() {
                    return false;
                }
                let id = thread_id.clone();
                store.update(cx, |s, cx| s.rename_thread(id, title, cx));
                true
            })
    });
    focus.update(cx, |s, cx| s.focus(window, cx));
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
    started: Option<std::time::Instant>,
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
            self.started = Some(std::time::Instant::now() - SLIDE.mul_f32(from));
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

/// `/Users/me/x` → `~/x`.
fn tilde(path: &str) -> String {
    match std::env::home_dir() {
        Some(home) => match std::path::Path::new(path).strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.to_owned(),
        },
        None => path.to_owned(),
    }
}

/// The header's thread menu: worktree actions and archive.
/// A thread's actions (its sidebar row's ··· button and right-click).
fn thread_menu_items(
    thread: &loft_protocol::Thread,
    store: &Entity<AppStore>,
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
    move |mut menu, window, cx| {
        let (s1, i1, t1) = (store.clone(), id.clone(), title.clone());
        menu = menu.item(
            PopupMenuItem::new("Rename…")
                .icon(IconName::SquarePen)
                .on_click(move |_, window, cx| {
                    rename_dialog(s1.clone(), i1.clone(), t1.clone(), window, cx)
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
    let mut row = h_flex().gap_1().text_xs();
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
        .bg(p.muted);
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

fn pretty_model(id: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::pretty_model;

    #[test]
    fn model_names() {
        assert_eq!(pretty_model("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(pretty_model("claude-sonnet-5-5-20261001"), "Sonnet 5.5");
        assert_eq!(pretty_model("gpt-5"), "GPT-5");
        assert_eq!(pretty_model("gpt-5.1-codex-max"), "GPT-5.1 Codex Max");
    }
}
