//! The main window: an integrated sidebar (search, projects → threads,
//! status), a breadcrumb header, the conversation timeline and a floating
//! composer.

use crate::items::render_row;
use crate::panel::{Tab, WorkbenchPanel};
use crate::store::{AppStore, ConnState, StoreEvent, ThreadState};
use crate::theme::{self, Palette, relative_time};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
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
        NewTab
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
        KeyBinding::new("cmd-k", FocusSearch, Some(CONTEXT)),
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
            MenuItem::action("New Tab", NewTab),
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
            MenuItem::action("Search Threads", FocusSearch),
            MenuItem::separator(),
            MenuItem::action("Toggle Light/Dark", ToggleTheme),
        ]),
        Menu::new("Agent").items([MenuItem::action("Stop Turn", Interrupt)]),
    ]
}

pub struct WorkspaceView {
    store: Entity<AppStore>,
    composers: HashMap<ThreadId, Entity<TextareaState>>,
    /// Images waiting in each thread's composer.
    attachments: HashMap<ThreadId, Vec<crate::attachments::PendingImage>>,
    composer_harness: HashMap<ThreadId, HarnessId>,
    observed: HashMap<ThreadId, Subscription>,
    search: Entity<InputState>,
    query: String,
    sidebar_open: bool,
    pub panel: Entity<WorkbenchPanel>,
    panel_open: bool,
    /// Dev/test hook: `AGENTLOFT_DEV_TAB=files|changes|agent|file:<path>`
    /// opens a panel tab once the first thread has loaded.
    dev_tab: Option<String>,
    /// Last window title set (the native tab label).
    window_title: String,
    /// This window is one of several native tabs (we draw the tab strip).
    tabs_shown: bool,
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
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&search, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query = state.read(cx).value().to_lowercase();
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
            composer_harness: HashMap::new(),
            observed: HashMap::new(),
            search,
            query: String::new(),
            sidebar_open: true,
            panel,
            panel_open: true,
            window_title: String::new(),
            tabs_shown: false,
            dev_tab: take_dev_tab(),
            _subs: subs,
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
            StoreEvent::BrowserCall { call_id, command } => {
                self.run_browser_call(*call_id, command.clone(), window, cx);
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
            let agent = model.items.iter().find(|i| {
                matches!(&i.body, loft_protocol::ItemBody::ToolCall { tool_kind, .. }
                    if *tool_kind == loft_protocol::ToolKind::Subagent)
            });
            (model.thread.id.clone(), agent.map(|i| i.id.clone()))
        };
        self.panel_open = true;
        match dev.split_once(':') {
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
                self.store.update(cx, |s, cx| s.fork_thread(t, h, cx));
            }
            Some(("file", path)) => {
                let path = path.to_owned();
                self.panel
                    .update(cx, |p, cx| p.open_file(&thread_id, path, window, cx));
            }
            _ => match dev {
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
                    let tab = crate::windows::Placement::TabOf(window.window_handle());
                    crate::windows::open(other, tab, cx);
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
    fn run_browser_call(
        &mut self,
        call_id: u64,
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
        // Let the user watch: bring the browser tab forward.
        self.panel_open = true;
        self.panel
            .update(cx, |p, cx| p.show(Tab::Browser, window, cx));
        let browser = self.panel.update(cx, |p, cx| p.browser(window, cx));
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

    fn show_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
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

    fn submit(
        &mut self,
        thread_id: &ThreadId,
        composer: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = composer.read(cx).value().trim().to_owned();
        let has_images = self
            .attachments
            .get(thread_id)
            .is_some_and(|a| !a.is_empty());
        if text.is_empty() && !has_images {
            return;
        }
        composer.update(cx, |c, cx| c.set_value("", window, cx));
        let attachments: Vec<loft_protocol::Attachment> = self
            .attachments
            .remove(thread_id)
            .unwrap_or_default()
            .iter()
            .map(|a| a.to_attachment())
            .collect();
        let id = thread_id.clone();
        if self.is_running(thread_id, cx) {
            // Sent automatically when the current turn ends.
            let queued = crate::store::Queued { text, attachments };
            self.store
                .update(cx, |store, cx| store.enqueue(id, queued, cx));
        } else {
            self.store.update(cx, |store, cx| {
                store.send_message(id, text, attachments, cx)
            });
        }
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

    fn add_project(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        let store = self.store.clone();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                store.update(cx, |store, cx| {
                    store.add_project(path.to_string_lossy().into_owned(), cx)
                });
            }
        })
        .detach();
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

    fn new_thread(&mut self, project: Option<ProjectId>, cx: &mut Context<Self>) {
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
            None => self.add_project(cx),
        }
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.store.read(cx).selected.clone()
            && self.is_running(&id, cx)
        {
            self.store.update(cx, |s, cx| s.interrupt(id, cx));
        }
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        let path = self.store.read(cx).paths().config_file();
        if !path.exists() {
            let _ = std::fs::write(&path, SETTINGS_TEMPLATE);
        }
        crate::settings::open(self.store.clone(), None, cx);
    }

    // -- sidebar ------------------------------------------------------------

    fn render_tab_strip(
        &mut self,
        tabs: Vec<gpui_kit::SystemWindowTab>,
        window: &mut Window,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = window.window_handle().window_id();
        let mut strip = h_flex()
            .h(px(38.))
            .flex_shrink_0()
            .pl(px(TRAFFIC_LIGHTS_INSET))
            .pr_2()
            .gap_1()
            .items_center()
            .bg(p.sidebar)
            .border_b_1()
            .border_color(p.border);
        for (ix, tab) in tabs.into_iter().enumerate() {
            let active = tab.id == current;
            let handle = tab.handle;
            let title = tab
                .title
                .split(" — ")
                .next()
                .unwrap_or("Agentloft")
                .to_owned();
            strip = strip.child(
                h_flex()
                    .id(SharedString::from(format!("win-tab-{ix}")))
                    .group("win-tab")
                    .flex_1()
                    .min_w_0()
                    .max_w(px(260.))
                    .h(px(28.))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .rounded(px(7.))
                    .text_xs()
                    .cursor_pointer()
                    .when(active, |d| {
                        d.bg(p.background)
                            .text_color(p.foreground)
                            .border_1()
                            .border_color(p.border)
                    })
                    .when(!active, |d| {
                        d.text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    })
                    .child(div().flex_1().min_w_0().truncate().child(title))
                    .child(
                        div()
                            .id(SharedString::from(format!("win-tab-x-{ix}")))
                            .p(px(2.))
                            .rounded(px(4.))
                            .invisible()
                            .when(active, |d| d.visible())
                            .group_hover("win-tab", |d| d.visible())
                            .hover(|d| d.bg(p.border))
                            .child(Icon::new(IconName::X).xsmall())
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                let _ = handle.update(cx, |_, window, _| window.remove_window());
                            }),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = handle.update(cx, |_, window, _| window.activate_window());
                    }),
            );
        }
        strip
            .child(icon_button(
                "win-tab-new",
                IconName::Plus,
                p,
                cx.listener(|_, _, window, cx| {
                    let tab = crate::windows::Placement::TabOf(window.window_handle());
                    crate::windows::open(None, tab, cx)
                }),
            ))
            .into_any_element()
    }

    fn render_sidebar(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (projects, all_threads, selected, conn, error, unread) = {
            let store = self.store.read(cx);
            (
                store.workspace.projects.clone(),
                store.workspace.threads.clone(),
                store.selected.clone(),
                store.conn.clone(),
                store.last_error.clone(),
                store.unread.clone(),
            )
        };
        let query = self.query.clone();
        let mut list = v_flex().gap_4().px_2().pb_4();

        for project in &projects {
            let project_id = project.id.clone();
            let threads: Vec<_> = all_threads
                .iter()
                .filter(|t| t.project_id == project.id && t.status != ThreadStatus::Archived)
                .filter(|t| query.is_empty() || t.title.to_lowercase().contains(&query))
                .collect();
            if !query.is_empty() && threads.is_empty() {
                continue;
            }
            let header = h_flex()
                .gap_2()
                .items_center()
                .px_2()
                .text_color(p.muted_foreground)
                .child(Icon::new(IconName::Folder).xsmall())
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(project.name.clone()),
                )
                .child(icon_button(
                    format!("new-{}", project.id),
                    IconName::Plus,
                    p,
                    cx.listener(move |this, _, _, cx| {
                        this.new_thread(Some(project_id.clone()), cx)
                    }),
                ));
            let mut rows = v_flex().gap_px();
            for (thread, depth) in nest_forks(threads) {
                rows = rows.child(thread_row(
                    thread,
                    depth,
                    selected.as_ref() == Some(&thread.id),
                    unread.contains(&thread.id),
                    p,
                    cx,
                ));
            }
            list = list.child(v_flex().gap_1().child(header).child(rows));
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
                    .h(px(HEADER_HEIGHT))
                    .pl(px(if self.tabs_shown {
                        8.
                    } else {
                        TRAFFIC_LIGHTS_INSET
                    }))
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
                        cx.listener(|this, _, _, cx| this.add_project(cx)),
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
        let active_tab = self.panel.read(cx).active_tab(cx);
        h_flex()
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .px_4()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .when(!self.sidebar_open, |d| {
                d.when(!self.tabs_shown, |d| d.pl(px(TRAFFIC_LIGHTS_INSET)))
                    .child(icon_button(
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
                    ThreadWorkspace::NewWorktree => Some(chip_label(
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
                .child(chip_label(IconName::Sparkles, chip, p))
                .children(branch_chip)
                .child(thread_menu(&thread, &self.store))
            })
            .when(has_thread, |d| {
                let active = if self.panel_open { active_tab } else { None };
                let mut tools = h_flex().gap_0p5().pl_2();
                for (tab, icon, id) in [
                    (Tab::Files, IconName::FolderTree, "hdr-files"),
                    (Tab::Changes, IconName::GitCompare, "hdr-changes"),
                    (Tab::Terminal, IconName::SquareTerminal, "hdr-terminal"),
                    (Tab::Browser, IconName::Globe, "hdr-browser"),
                ] {
                    let on = active.as_ref() == Some(&tab);
                    tools = tools.child(toggle_button(
                        id,
                        icon,
                        on,
                        p,
                        cx.listener(move |this, _, window, cx| {
                            this.show_tab(tab.clone(), window, cx)
                        }),
                    ));
                }
                d.child(tools).child(toggle_button(
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
        let fork_parent = thread
            .read(cx)
            .model
            .as_ref()
            .and_then(|m| m.thread.parent.clone());
        let fork_bar = fork_parent.map(|parent| self.fork_bar(parent, p, cx));
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
        let waiting = model.open_requests().next().is_some();
        let empty = model.items.is_empty();
        let model_name = model
            .thread
            .model
            .clone()
            .map(|m| pretty_model(&m))
            .unwrap_or_else(|| harness_name(&model.thread.harness).into());
        let mode = mode_label(model.thread.permission_mode).to_owned();
        let current_mode = model.thread.permission_mode;
        let current_model = model.thread.model.clone();
        let harness = model.thread.harness.clone();
        let scroller_state = state.scroller.clone();
        let queued: Vec<crate::store::Queued> = self
            .store
            .read(cx)
            .queued
            .get(&thread_id)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default();
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
                        &self.store,
                        p,
                    ))
                    .child(workspace_picker(
                        &model.thread.id,
                        &model.thread.workspace,
                        &self.store,
                        p,
                    ))
                })
                .child(
                    div()
                        .max_w(px(520.))
                        .truncate()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(match model.thread.workspace {
                            ThreadWorkspace::NewWorktree => {
                                "Your first message creates an isolated worktree on a new branch"
                                    .to_owned()
                            }
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
            .flex_1()
            .min_h_0()
            .child(div().flex_1().min_h_0().flex().flex_col().child(timeline))
            .child(
                div().px_6().pb_5().pt_1().flex().justify_center().child(
                    v_flex()
                        .w_full()
                        .max_w(p.content_width)
                        .gap_1()
                        .children(queued_list(&thread_id, &queued, &self.store, p))
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
                                                .child(model_menu(
                                                    &thread_id,
                                                    &harness,
                                                    current_model.as_deref(),
                                                    model_name,
                                                    &self.store,
                                                ))
                                                .child(mode_menu(
                                                    &thread_id,
                                                    current_mode,
                                                    mode,
                                                    &self.store,
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
            .into_any_element()
    }

    fn render_welcome(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let has_projects = !self.store.read(cx).workspace.projects.is_empty();
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
                        cx.listener(|this, _, _, cx| this.add_project(cx)),
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
            .into_any_element()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
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
            Some(thread) => self.render_thread(thread, &p, window, cx),
            None => self.render_welcome(&p, cx),
        };
        // The window title doubles as the native tab label.
        let title = header_title
            .as_ref()
            .map(|(project, t)| format!("{} — {project}", t.title))
            .unwrap_or_else(|| "Agentloft".into());
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        let show_panel = self.panel_open && header_title.is_some();
        let header = self.render_header(header_title, &p, cx);
        // Native tab group (⌘T): GPUI doesn't draw macOS's tab bar under our
        // transparent title bar, so we draw the tabs ourselves.
        let window_tabs = window.tabbed_windows().filter(|tabs| tabs.len() > 1);
        self.tabs_shown = window_tabs.is_some();
        let tab_strip = window_tabs.map(|tabs| self.render_tab_strip(tabs, window, &p, cx));
        let sidebar = self.sidebar_open.then(|| self.render_sidebar(&p, cx));

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
            .on_action(cx.listener(|this, _: &AddProject, _, cx| this.add_project(cx)))
            .on_action(cx.listener(|_, _: &About, window, cx| show_about(window, cx)))
            .on_action(cx.listener(|_, _: &NewWindow, _, cx| {
                crate::windows::open(None, crate::windows::Placement::Window, cx)
            }))
            .on_action(cx.listener(|_, _: &NewTab, window, cx| {
                let tab = crate::windows::Placement::TabOf(window.window_handle());
                crate::windows::open(None, tab, cx)
            }))
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
            .children(tab_strip)
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
                        .child(
                            h_resizable("workbench")
                                .child(resizable_panel().child(body))
                                .child(
                                    resizable_panel()
                                        .size(px(560.))
                                        .size_range(px(340.)..px(1600.))
                                        .visible(show_panel)
                                        .child(
                                            div()
                                                .size_full()
                                                .border_l_1()
                                                .border_color(p.border)
                                                .child(self.panel.clone()),
                                        ),
                                ),
                        ),
                ),
            )
    }
}

fn show_about(window: &mut Window, cx: &mut gpui_kit::App) {
    use gpui_kit::component::WindowExt as _;
    let p = Palette::of(cx);
    window.open_dialog(cx, move |dialog, _, _| {
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
fn queued_list(
    thread_id: &ThreadId,
    queued: &[crate::store::Queued],
    store: &Entity<AppStore>,
    p: &Palette,
) -> Option<AnyElement> {
    if queued.is_empty() {
        return None;
    }
    let mut list = v_flex().gap_1().px_1();
    for (ix, q) in queued.iter().enumerate() {
        let text = match (q.text.is_empty(), q.attachments.len()) {
            (_, 0) => q.text.clone(),
            (true, n) => format!("{n} image{}", if n == 1 { "" } else { "s" }),
            (false, n) => format!("{} · {n} image{}", q.text, if n == 1 { "" } else { "s" }),
        };
        let (store, thread_id) = (store.clone(), thread_id.clone());
        list = list.child(
            h_flex()
                .gap_2()
                .items_center()
                .px_3()
                .py_1p5()
                .rounded(px(10.))
                .bg(p.muted)
                .text_sm()
                .child(
                    Icon::new(IconName::Clock)
                        .xsmall()
                        .text_color(p.muted_foreground),
                )
                .child(div().flex_1().min_w_0().truncate().child(text.clone()))
                .child(
                    div()
                        .id(SharedString::from(format!("unqueue-{ix}")))
                        .p(px(2.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .text_color(p.muted_foreground)
                        .hover(|d| d.bg(p.hover).text_color(p.foreground))
                        .child(Icon::new(IconName::X).xsmall())
                        .on_click(move |_, _, cx| {
                            store.update(cx, |s, cx| s.dequeue(&thread_id, ix, cx));
                        }),
                ),
        );
    }
    Some(
        v_flex()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child("Queued · sends when the current turn finishes"),
            )
            .child(list)
            .into_any_element(),
    )
}

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

/// Permission-mode dropdown in the composer footer (PLAN §2.7).
fn mode_menu(
    thread_id: &ThreadId,
    current: Option<loft_protocol::PermissionMode>,
    label: String,
    store: &Entity<AppStore>,
) -> AnyElement {
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
    use loft_protocol::PermissionMode::*;
    let store = store.clone();
    let thread_id = thread_id.clone();
    Button::new("mode-menu")
        .label(label)
        .icon(IconName::ShieldCheck)
        .ghost()
        .xsmall()
        .dropdown_menu(move |mut menu, _, _| {
            for mode in [
                None,
                Some(ReadOnly),
                Some(AskAll),
                Some(AcceptEdits),
                Some(Auto),
                Some(FullAccess),
            ] {
                let (store, thread_id) = (store.clone(), thread_id.clone());
                menu = menu.item(
                    PopupMenuItem::new(mode_label(mode))
                        .checked(mode == current)
                        .on_click(move |_, _, cx| {
                            let t = thread_id.clone();
                            store.update(cx, |s, cx| s.set_permission_mode(t, mode, cx));
                        }),
                );
                if mode.is_none() {
                    menu = menu.separator();
                }
            }
            menu
        })
        .into_any_element()
}

/// Model dropdown: harness default plus well-known models.
fn model_menu(
    thread_id: &ThreadId,
    harness: &HarnessId,
    current: Option<&str>,
    label: String,
    store: &Entity<AppStore>,
) -> AnyElement {
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
    let options: &[(&str, &str)] = match harness.as_str() {
        HarnessId::CLAUDE => &[("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")],
        HarnessId::CODEX => &[
            ("gpt-6.1-sol", "GPT-6.1 Sol"),
            ("gpt-6-astra", "GPT-6 Astra"),
            ("gpt-6.1-terra", "GPT-6.1 Terra"),
            ("gpt-6-luna", "GPT-6 Luna"),
        ],
        _ => &[],
    };
    let current = current.map(str::to_owned);
    let store = store.clone();
    let thread_id = thread_id.clone();
    Button::new("model-menu")
        .label(label)
        .icon(IconName::Bot)
        .ghost()
        .xsmall()
        .dropdown_menu(move |mut menu, _, _| {
            let (s, t) = (store.clone(), thread_id.clone());
            menu = menu
                .item(
                    PopupMenuItem::new("Harness default").on_click(move |_, _, cx| {
                        let t = t.clone();
                        s.update(cx, |s, cx| s.set_model(t, None, cx));
                    }),
                )
                .separator();
            for (id, name) in options {
                let (s, t, id) = (store.clone(), thread_id.clone(), (*id).to_owned());
                let checked = current.as_deref().is_some_and(|c| {
                    c == id
                        || c.contains(&format!("-{id}-"))
                        || c.starts_with(&format!("claude-{id}"))
                });
                menu = menu.item(PopupMenuItem::new(*name).checked(checked).on_click(
                    move |_, _, cx| {
                        let (t, m) = (t.clone(), Some(id.clone()));
                        s.update(cx, |s, cx| s.set_model(t, m, cx));
                    },
                ));
            }
            menu
        })
        .into_any_element()
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

fn thread_row(
    thread: &loft_protocol::Thread,
    depth: usize,
    active: bool,
    unread: bool,
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
        _ => div()
            .text_xs()
            .text_color(p.faint_foreground)
            .child(relative_time(thread.updated_at_ms))
            .into_any_element(),
    };
    h_flex()
        .id(SharedString::from(format!("thread-{}", thread.id)))
        .gap_2()
        .items_center()
        .pl(px(26. + 14. * depth.min(4) as f32))
        .pr_2()
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
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(thread.title.clone()),
        )
        .when(thread.workspace.branch().is_some(), |d| {
            d.child(
                Icon::new(IconName::GitBranch)
                    .xsmall()
                    .text_color(p.faint_foreground),
            )
        })
        .child(trailing)
        .on_click(cx.listener(move |this, _, _, cx| {
            let id = id.clone();
            this.store.update(cx, |s, cx| s.select_thread(id, cx));
        }))
        .into_any_element()
}

/// `claude-opus-5-5` → `Opus 5.5`.
pub fn harness_name(h: &HarnessId) -> &'static str {
    match h.as_str() {
        HarnessId::CLAUDE => "Claude",
        HarnessId::CODEX => "Codex",
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
fn thread_menu(thread: &loft_protocol::Thread, store: &Entity<AppStore>) -> AnyElement {
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
    let store = store.clone();
    let id = thread.id.clone();
    let harness = thread.harness.clone();
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
    Button::new("thread-menu")
        .icon(IconName::Ellipsis)
        .ghost()
        .xsmall()
        .dropdown_menu(move |mut menu, _, _| {
            if let Some((path, branch, base)) = &worktree {
                let (s, t) = (store.clone(), id.clone());
                menu = menu
                    .item(
                        PopupMenuItem::new(format!("Merge into {base}"))
                            .icon(IconName::GitMerge)
                            .disabled(running)
                            .on_click(move |_, _, cx| {
                                let t = t.clone();
                                s.update(cx, |s, cx| s.merge_thread(t, cx));
                            }),
                    )
                    .item({
                        let (s, t) = (store.clone(), id.clone());
                        PopupMenuItem::new("Create pull request")
                            .icon(IconName::GitPullRequest)
                            .disabled(running)
                            .on_click(move |_, _, cx| {
                                let t = t.clone();
                                s.update(cx, |s, cx| s.create_pull_request(t, cx));
                            })
                    })
                    .item({
                        let path = path.clone();
                        PopupMenuItem::new("Reveal worktree in Finder")
                            .icon(IconName::FolderOpen)
                            .on_click(move |_, _, cx| {
                                cx.reveal_path(std::path::Path::new(&path));
                            })
                    })
                    .item({
                        let branch = branch.clone();
                        PopupMenuItem::new("Copy branch name")
                            .icon(IconName::Copy)
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                    branch.clone(),
                                ));
                            })
                    })
                    .separator();
            }
            let t = id.clone();
            menu = menu
                .item(
                    PopupMenuItem::new("Open in New Tab")
                        .icon(IconName::SquarePen)
                        .on_click(move |_, window, cx| {
                            let tab = crate::windows::Placement::TabOf(window.window_handle());
                            crate::windows::open(Some(t.clone()), tab, cx)
                        }),
                )
                .separator();
            for (h, name) in [
                (HarnessId::CLAUDE, "Claude Code"),
                (HarnessId::CODEX, "Codex"),
            ] {
                let (s, t) = (store.clone(), id.clone());
                let label = if h == harness.as_str() {
                    format!("Fork with {name} (full context)")
                } else {
                    format!("Fork with {name} (handoff)")
                };
                menu = menu.item(PopupMenuItem::new(label).icon(IconName::GitFork).on_click(
                    move |_, _, cx| {
                        let t = t.clone();
                        s.update(cx, |s, cx| s.fork_thread(t, HarnessId(h.into()), cx));
                    },
                ));
            }
            menu = menu.separator();
            let (s, t) = (store.clone(), id.clone());
            menu.item(
                PopupMenuItem::new("Archive thread")
                    .icon(IconName::Archive)
                    .on_click(move |_, _, cx| {
                        let t = t.clone();
                        s.update(cx, |s, cx| s.archive_thread(t, false, cx));
                    }),
            )
        })
        .into_any_element()
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
        let (store, thread_id) = (store.clone(), thread_id.clone());
        dialog
            .title("Archive thread?")
            .child(format!(
                "{reason}. Archiving removes the worktree and discards them; the branch is kept."
            ))
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().child(Button::new("cancel").label("Cancel")))
                    .child(
                        DialogAction::new()
                            .child(Button::new("ok").danger().label("Archive anyway")),
                    ),
            )
            .on_ok(move |_, _, cx| {
                let t = thread_id.clone();
                store.update(cx, |s, cx| s.archive_thread(t, true, cx));
                true
            })
    });
}

/// Local checkout vs. a new worktree, for a thread that hasn't started.
fn workspace_picker(
    thread_id: &ThreadId,
    current: &ThreadWorkspace,
    store: &Entity<AppStore>,
    p: &Palette,
) -> AnyElement {
    let wants = matches!(current, ThreadWorkspace::NewWorktree);
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
                        store.update(cx, |s, cx| s.set_thread_workspace(t, worktree, cx));
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
    store: &Entity<AppStore>,
    p: &Palette,
) -> AnyElement {
    let mut row = h_flex()
        .mt_3()
        .p(px(3.))
        .gap(px(3.))
        .rounded(px(10.))
        .bg(p.muted);
    for (id, label, icon) in [
        (HarnessId::CLAUDE, "Claude Code", IconName::Sparkles),
        (HarnessId::CODEX, "Codex", IconName::SquareTerminal),
    ] {
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
                .child(Icon::new(icon).small())
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
