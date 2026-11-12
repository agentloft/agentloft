//! Project actions: the commands a project names in its
//! `.agentloft/config.toml` (`[[actions]]`: Dev, Test, Lint…). A thread
//! shows them as one control in its header and as "Run: …" entries in the
//! palette; running one types its command into a terminal tab named after
//! it, under the conversation, and shows its `preview_url` in the browser
//! panel.
//!
//! The daemon reads the file (`Request::ProjectActions`). What is kept here
//! is only what the header and the palette draw, asked for again when a
//! thread is picked, when one of its turns ends (the agent may have edited
//! the file) and when the window comes back to the front (the user may
//! have); running an action asks once more and runs what the file says
//! then.

use crate::store::{AppStore, StoreEvent};
use crate::theme::Palette;
use crate::workspace::WorkspaceView;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, AssetSource as _, Bounds, Context, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Pixels, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    canvas, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{ErrorBody, ProjectAction, ProjectId, Request, Response, ThreadId, TurnId};
use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;

/// An action as the UI draws it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Action {
    pub name: String,
    pub command: String,
    pub preview_url: Option<String>,
    pub icon: IconName,
}

impl From<ProjectAction> for Action {
    fn from(action: ProjectAction) -> Self {
        Self {
            icon: icon_for(&action.name, action.icon.as_deref()),
            name: action.name,
            command: action.command,
            preview_url: action.preview_url,
        }
    }
}

/// The icon an action shows: the Lucide one its project named, if the app
/// has it (only the icons listed in `assets.rs` are in the binary), else
/// one guessed from what it is called, else a play triangle.
fn icon_for(name: &str, icon: Option<&str>) -> IconName {
    icon.and_then(bundled_icon)
        .unwrap_or_else(|| guessed_icon(name))
}

/// `flask-conical`, `flask_conical` or `FlaskConical` as the icon, if it
/// is one the app can draw.
fn bundled_icon(name: &str) -> Option<IconName> {
    let plain = |s: &str| -> String {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let wanted = plain(name);
    if wanted.is_empty() {
        return None;
    }
    IconName::ALL.iter().copied().find(|icon| {
        let path = icon.path();
        let stem = path
            .rsplit('/')
            .next()
            .and_then(|file| file.strip_suffix(".svg"));
        stem.is_some_and(|stem| plain(stem) == wanted)
            && crate::assets::Assets
                .load(&path)
                .is_ok_and(|svg| svg.is_some())
    })
}

/// An icon for an action that names none, from the words of its name.
fn guessed_icon(name: &str) -> IconName {
    let name = name.to_lowercase();
    let says = |words: &[&str]| {
        name.split(|c: char| !c.is_alphanumeric())
            .any(|word| words.contains(&word))
    };
    if says(&["test", "tests", "spec", "specs", "e2e", "check"]) {
        IconName::FlaskConical
    } else if says(&["build", "compile", "bundle", "make"]) {
        IconName::Hammer
    } else if says(&["lint", "format", "fmt", "fix", "tidy"]) {
        IconName::WandSparkles
    } else if says(&["deploy", "release", "publish", "ship"]) {
        IconName::Rocket
    } else if says(&["debug"]) {
        IconName::Bug
    } else if says(&["install", "setup", "deps", "bootstrap"]) {
        IconName::Package
    } else if says(&["db", "database", "migrate", "migrations", "seed"]) {
        IconName::Database
    } else if says(&["clean", "reset"]) {
        IconName::Eraser
    } else {
        IconName::Play
    }
}

/// Each project's actions as last read, for the header and the palette.
#[derive(Default)]
pub(crate) struct ActionCache {
    by_project: HashMap<ProjectId, Rc<[Action]>>,
}

/// Ask the daemon for a project's actions, as its config file has them now.
fn fetch(
    store: &AppStore,
    project_id: ProjectId,
) -> impl Future<Output = Result<Vec<Action>, ErrorBody>> + 'static {
    let client = store.client();
    async move {
        match client {
            Some(client) => match client
                .request(Request::ProjectActions { project_id })
                .await?
            {
                Response::Actions { actions } => Ok(actions.into_iter().map(Into::into).collect()),
                other => Err(ErrorBody::new(
                    loft_protocol::ErrorCode::Internal,
                    format!("unexpected response: {other:?}"),
                )),
            },
            None => Err(ErrorBody::new(
                loft_protocol::ErrorCode::Internal,
                "not connected to agentloftd",
            )),
        }
    }
}

impl AppStore {
    /// The project's actions as last read. None for a browser session that
    /// may not open terminals: there is nowhere to run one.
    pub(crate) fn project_actions(&self, project_id: &ProjectId) -> Rc<[Action]> {
        self.actions
            .by_project
            .get(project_id)
            .filter(|_| self.terminals_allowed())
            .cloned()
            .unwrap_or_default()
    }

    /// Read the project's actions again; views redraw if they changed.
    pub(crate) fn refresh_actions(&mut self, project_id: ProjectId, cx: &mut Context<Self>) {
        let fetched = fetch(self, project_id.clone());
        cx.spawn(async move |this, cx| {
            if let Ok(actions) = fetched.await {
                let _ = this.update(cx, |this, cx| this.set_actions(project_id, actions, cx));
            }
        })
        .detach();
    }

    fn set_actions(&mut self, project_id: ProjectId, actions: Vec<Action>, cx: &mut Context<Self>) {
        let known = self.actions.by_project.get(&project_id);
        if known.map_or(&[][..], |a| &a[..]) != actions.as_slice() {
            self.actions.by_project.insert(project_id, actions.into());
            cx.notify();
        }
    }
}

/// The header control's own state.
#[derive(Default)]
pub(crate) struct ActionsBar {
    /// Where the control is (its menu opens under it).
    anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The thread, and its last finished turn, the actions were last read
    /// for: another of either and they are read again.
    read_for: Option<(ThreadId, Option<TurnId>)>,
}

impl ActionsBar {
    /// What is known may be out of date (the window was away): read again
    /// at the next draw.
    pub(crate) fn stale(&mut self) {
        self.read_for = None;
    }
}

impl WorkspaceView {
    /// The thread's actions in the header: a button that runs the first,
    /// and, when there are more, a chevron that lists them all. Nothing
    /// for a project that names none.
    pub(crate) fn actions_control(
        &mut self,
        thread: &loft_protocol::Thread,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let finished = self
            .store
            .read(cx)
            .threads
            .get(&thread.id)
            .and_then(|t| t.read(cx).model.as_ref()?.last_turn())
            .filter(|turn| turn.status.is_terminal())
            .map(|turn| turn.id.clone());
        let read_for = Some((thread.id.clone(), finished));
        if self.actions_bar.read_for != read_for {
            self.actions_bar.read_for = read_for;
            let project = thread.project_id.clone();
            self.store
                .update(cx, |s, cx| s.refresh_actions(project, cx));
        }

        let actions = self.store.read(cx).project_actions(&thread.project_id);
        let first = actions.first()?.clone();
        let more = actions.len() > 1;
        // Each part rounds its own outer corners (inside the border's).
        let part = |id: &'static str, left: bool, right: bool| {
            h_flex()
                .id(id)
                .h_full()
                .items_center()
                .cursor_pointer()
                .when(left, |d| d.rounded_l(px(6.)))
                .when(right, |d| d.rounded_r(px(6.)))
                .hover(|d| d.bg(p.hover).text_color(p.foreground))
        };
        let (thread_id, name, tip) = (
            thread.id.clone(),
            first.name.clone(),
            SharedString::from(format!("Run `{}`", first.command)),
        );
        let run = part("hdr-action-run", true, !more)
            .pl(px(7.))
            .pr(px(if more { 6. } else { 8. }))
            .gap(px(5.))
            .child(Icon::new(first.icon).xsmall())
            .child(div().max_w(px(120.)).truncate().child(first.name.clone()))
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.run_action(thread_id.clone(), name.clone(), window, cx)
            }));
        let thread_id = thread.id.clone();
        let list = part("hdr-action-more", false, true)
            .w(px(20.))
            .justify_center()
            .border_l_1()
            .border_color(p.border)
            .child(Icon::new(IconName::ChevronDown).xsmall())
            .on_click(
                cx.listener(move |this, _, window, cx| this.actions_menu(&thread_id, window, cx)),
            );
        let anchor = self.actions_bar.anchor.clone();
        Some(
            h_flex()
                .relative()
                .flex_none()
                .h(px(26.))
                .items_center()
                .rounded(px(7.))
                .border_1()
                .border_color(p.border)
                .text_xs()
                .text_color(p.muted_foreground)
                .child(run)
                .when(more, |d| d.child(list))
                .child(
                    canvas(
                        move |bounds, _, _| anchor.set(Some(bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .into_any_element(),
        )
    }

    /// Every action of the thread's project, under the header control.
    fn actions_menu(&mut self, thread_id: &ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        use loft_browser::menu::{self, Entry};
        let Some(at) = self.actions_bar.anchor.get() else {
            return;
        };
        let store = self.store.read(cx);
        let Some(project_id) = store
            .workspace
            .threads
            .iter()
            .find(|t| &t.id == thread_id)
            .map(|t| t.project_id.clone())
        else {
            return;
        };
        let (view, handle) = (cx.weak_entity(), window.window_handle());
        let entries = store
            .project_actions(&project_id)
            .iter()
            .map(|action| {
                let (view, id, name) = (view.clone(), thread_id.clone(), action.name.clone());
                Entry::item(action.name.clone(), action.icon, move |cx| {
                    let (view, id, name) = (view.clone(), id.clone(), name.clone());
                    let _ = handle.update(cx, move |_, window, cx| {
                        _ = view.update(cx, |v, cx| v.run_action(id, name, window, cx));
                    });
                })
            })
            .collect();
        menu::open(window, at, entries, cx);
    }

    /// Run the action called `name` in `thread_id`: its command as the
    /// project's config has it now, not as it was when the button was
    /// drawn.
    pub(crate) fn run_action(
        &mut self,
        thread_id: ThreadId,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let Some(project_id) = store
            .workspace
            .threads
            .iter()
            .find(|t| t.id == thread_id)
            .map(|t| t.project_id.clone())
        else {
            return;
        };
        let fetched = fetch(store, project_id.clone());
        cx.spawn_in(window, async move |this, cx| {
            let fetched = fetched.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let found = fetched.map(|actions| {
                    let action = actions.iter().find(|a| a.name == name).cloned();
                    this.store
                        .update(cx, |s, cx| s.set_actions(project_id, actions, cx));
                    action
                });
                match found {
                    Ok(Some(action)) => this.start_action(&thread_id, &action, window, cx),
                    Ok(None) => this.store.update(cx, |s, cx| {
                        s.fail(
                            format!("“{name}” is no longer in .agentloft/config.toml"),
                            cx,
                        )
                    }),
                    Err(err) => this.store.update(cx, |s, cx| s.fail(err.message, cx)),
                }
            });
        })
        .detach();
    }

    fn start_action(
        &mut self,
        thread_id: &ThreadId,
        action: &Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The thread was left while the daemon answered: its terminal and
        // browser aren't the ones showing.
        if self.store.read(cx).selected.as_ref() != Some(thread_id) {
            return;
        }
        // The page first, so the terminal ends up with the keyboard (to
        // stop the command with Ctrl-C).
        if let Some(url) = &action.preview_url {
            self.show_preview(thread_id, url, window, cx);
        }
        let (name, command) = (action.name.clone(), action.command.clone());
        self.panel.update(cx, |panel, cx| {
            panel.run_action(thread_id, &name, command, window, cx)
        });
        cx.notify();
    }

    /// Show an action's page in the browser panel. The command has only
    /// just been typed, so nothing is listening yet: the page is asked for
    /// again until it loads (a server that takes longer than
    /// [`PREVIEW_PATIENCE`] is a reload away).
    fn show_preview(
        &mut self,
        thread_id: &ThreadId,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let browse = StoreEvent::Browse {
            url: url.to_owned(),
        };
        self.on_store_event(&browse, window, cx);
        // The streamed page of the web client keeps the address it was
        // sent to, and says why it didn't load.
        if crate::host::IS_WEB {
            return;
        }
        let (thread_id, url) = (thread_id.clone(), url.to_owned());
        cx.spawn(async move |this, cx| {
            let started = web_time::Instant::now();
            while started.elapsed() < PREVIEW_PATIENCE {
                cx.background_executor().timer(PREVIEW_RETRY).await;
                let waiting = this.update(cx, |this, cx| {
                    // Its tab was closed: the user is done with it.
                    let Some(browser) = this.panel.read(cx).existing_browser(&thread_id, cx) else {
                        return false;
                    };
                    let (loaded, loading) = {
                        let browser = browser.read(cx);
                        (browser.url().is_some(), browser.is_loading())
                    };
                    if !loaded && !loading {
                        browser.update(cx, |b, cx| b.navigate(&url, cx));
                    }
                    !loaded
                });
                if !matches!(waiting, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }
}

/// How often, and for how long, a preview page that hasn't loaded is asked
/// for again while its server starts.
const PREVIEW_RETRY: std::time::Duration = std::time::Duration::from_millis(700);
const PREVIEW_PATIENCE: std::time::Duration = std::time::Duration::from_secs(45);

#[cfg(test)]
mod tests {
    use super::*;

    fn action(name: &str, icon: Option<&str>) -> Action {
        ProjectAction {
            name: name.into(),
            command: "true".into(),
            icon: icon.map(str::to_owned),
            preview_url: None,
        }
        .into()
    }

    #[test]
    fn a_named_icon_is_used_if_the_app_has_it() {
        assert_eq!(action("Dev", Some("play")).icon, IconName::Play);
        assert_eq!(action("Dev", Some("rocket")).icon, IconName::Rocket);
        // However it is written.
        for name in ["flask-conical", "flask_conical", "FlaskConical"] {
            assert_eq!(action("Dev", Some(name)).icon, IconName::FlaskConical);
        }
        // Not Lucide's, or Lucide's but not in the binary: the guess.
        assert_eq!(action("Dev", Some("no-such-icon")).icon, IconName::Play);
        assert_eq!(
            action("Test", Some("accessibility")).icon,
            IconName::FlaskConical
        );
    }

    #[test]
    fn an_icon_is_guessed_from_the_name() {
        for (name, icon) in [
            ("Dev", IconName::Play),
            ("Test", IconName::FlaskConical),
            ("Unit tests", IconName::FlaskConical),
            ("Build", IconName::Hammer),
            ("Lint", IconName::WandSparkles),
            ("Deploy (staging)", IconName::Rocket),
            ("db:migrate", IconName::Database),
            // A word of its own, not part of one: "Latest" isn't a test.
            ("Latest", IconName::Play),
        ] {
            assert_eq!(action(name, None).icon, icon, "{name}");
        }
    }

    /// Every icon an action can be given without naming one is in the
    /// binary (an icon that isn't draws as a blank box).
    #[test]
    fn guessed_icons_are_bundled() {
        for name in [
            "dev", "test", "build", "lint", "deploy", "debug", "install", "db", "clean",
        ] {
            let icon = guessed_icon(name);
            let found = crate::assets::Assets.load(&icon.path()).ok().flatten();
            assert!(found.is_some(), "{} isn't bundled", icon.path());
        }
    }
}
