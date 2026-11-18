//! The panel's Device tabs (ADR-0020): one per device, each a
//! [`DeviceView`]. Choosing a device another tab already shows switches to
//! that tab; a tab can be renamed (double-click, or its menu); and the
//! thread's tabs are kept in the window's session ([`DeviceTab`]), so a
//! restart shows them again.

use super::device::{DeviceEvent, DeviceView};
use super::{Open, PanelEvent, Tab, WorkbenchPanel};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::{AppContext as _, Context, Entity, EntityId, Window};
use loft_protocol::{DeviceId, ThreadId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One Device tab as the window's session keeps it: the device it shows
/// (none: the list) and the name it was given, if renamed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceTab {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Each thread's Device tabs from a saved session: `tabs` as saved now, and
/// `single` the one device per thread that sessions saved before there
/// could be several tabs (each becomes a tab of its own, unless the thread
/// has tabs saved the new way).
pub fn from_session(
    single: Vec<(ThreadId, DeviceId)>,
    tabs: Vec<(ThreadId, Vec<DeviceTab>)>,
) -> HashMap<ThreadId, Vec<DeviceTab>> {
    let mut out: HashMap<ThreadId, Vec<DeviceTab>> = HashMap::new();
    for (thread, list) in tabs {
        if !list.is_empty() {
            out.entry(thread).or_default().extend(list);
        }
    }
    for (thread, device) in single {
        out.entry(thread).or_insert_with(|| {
            vec![DeviceTab {
                device: Some(device),
                name: None,
            }]
        });
    }
    out
}

/// A tab name as typed: trimmed, and none if nothing is left.
fn clean_name(text: &str) -> Option<String> {
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

impl WorkbenchPanel {
    /// A new Device tab for `thread_id` showing `tab` (shown if
    /// `activate`).
    pub(super) fn new_device_tab(
        &mut self,
        thread_id: &ThreadId,
        tab: DeviceTab,
        activate: bool,
        cx: &mut Context<Self>,
    ) -> Option<Entity<DeviceView>> {
        let store = self.store.clone();
        let view = cx.new(|cx| DeviceView::new(store, tab, cx));
        let id = thread_id.clone();
        let weak = view.downgrade();
        let subs = [
            // Its device's name and state show on the tab.
            cx.observe(&view, |_, _, cx| cx.notify()),
            cx.subscribe(&view, move |this, _, event: &DeviceEvent, cx| match event {
                DeviceEvent::Screenshot(png) => {
                    cx.emit(PanelEvent::Screenshot(id.clone(), png.clone()))
                }
                DeviceEvent::Float => cx.emit(PanelEvent::FloatDevice(id.clone())),
                DeviceEvent::Changed => this.save_device_tabs(&id, cx),
                DeviceEvent::Pick { device, boot } => {
                    if let Some(view) = weak.upgrade() {
                        this.pick_device(&id, &view, device.clone(), *boot, cx);
                    }
                }
            }),
        ];
        let panel = self.threads.get_mut(thread_id)?;
        panel._subs.extend(subs);
        panel.tabs.push(Open::Device { view: view.clone() });
        if activate || panel.tabs.len() == 1 {
            panel.active = panel.tabs.len() - 1;
        }
        self.save_device_tabs(thread_id, cx);
        cx.notify();
        Some(view)
    }

    /// The thread's Device tabs as last saved (a restart): open them again,
    /// behind the tab showing.
    pub(super) fn restore_device_tabs(&mut self, thread_id: &ThreadId, cx: &mut Context<Self>) {
        let saved = self
            .store
            .read(cx)
            .device_tabs
            .get(thread_id)
            .cloned()
            .unwrap_or_default();
        for tab in saved {
            self.new_device_tab(thread_id, tab, false, cx);
        }
    }

    /// Keep the thread's Device tabs in the store (and so in the session).
    pub(super) fn save_device_tabs(&mut self, thread_id: &ThreadId, cx: &mut Context<Self>) {
        let Some(panel) = self.threads.get(thread_id) else {
            return;
        };
        let tabs: Vec<DeviceTab> = panel
            .tabs
            .iter()
            .filter_map(|t| match t {
                Open::Device { view } => Some(view.read(cx).saved()),
                _ => None,
            })
            .collect();
        let thread = thread_id.clone();
        self.store.update(cx, |s, _| {
            if tabs.is_empty() {
                s.device_tabs.remove(&thread);
            } else {
                s.device_tabs.insert(thread, tabs);
            }
        });
    }

    /// The index of the thread's Device tab on `device`.
    fn device_tab_on(
        &self,
        thread_id: &ThreadId,
        device: &DeviceId,
        cx: &gpui_kit::App,
    ) -> Option<usize> {
        self.threads.get(thread_id)?.tabs.iter().position(
            |t| matches!(t, Open::Device { view } if view.read(cx).chosen() == Some(device)),
        )
    }

    fn device_at(&self, thread_id: &ThreadId, ix: usize) -> Option<Entity<DeviceView>> {
        match self.threads.get(thread_id)?.tabs.get(ix)? {
            Open::Device { view } => Some(view.clone()),
            _ => None,
        }
    }

    /// Someone picked `device` in `view` (its list or its picker): if
    /// another tab shows it, that tab comes forward (and a tab that only
    /// showed the list goes away); else `view` shows it.
    fn pick_device(
        &mut self,
        thread_id: &ThreadId,
        view: &Entity<DeviceView>,
        device: DeviceId,
        boot: bool,
        cx: &mut Context<Self>,
    ) {
        let other = self.threads.get(thread_id).and_then(|panel| {
            panel.tabs.iter().position(|t| {
                matches!(t, Open::Device { view: v } if v != view && v.read(cx).chosen() == Some(&device))
            })
        });
        let Some(ix) = other else {
            view.update(cx, |v, cx| v.choose(Some(device), boot, cx));
            return;
        };
        let shown = self.device_at(thread_id, ix);
        if let Some(shown) = &shown {
            shown.update(cx, |v, cx| v.choose(Some(device), boot, cx));
        }
        let Some(panel) = self.threads.get_mut(thread_id) else {
            return;
        };
        panel.active = ix;
        // A tab that showed only the list was opened to pick one: done.
        if view.read(cx).chosen().is_none()
            && let Some(at) = panel
                .tabs
                .iter()
                .position(|t| matches!(t, Open::Device { view: v } if v == view))
        {
            self.close(thread_id, at, cx);
            if let Some(panel) = self.threads.get_mut(thread_id)
                && let Some(shown) = &shown
                && let Some(ix) = panel
                    .tabs
                    .iter()
                    .position(|t| matches!(t, Open::Device { view: v } if v == shown))
            {
                panel.active = ix;
            }
        }
        cx.notify();
    }

    /// Show a Device tab for `thread_id`: the one on device `id` if a tab
    /// shows it, else one showing the list (made if none does) turned to
    /// it. With no `id`: the Device tab showing, or the last one, or a new
    /// one. `boot` starts it if it is stopped.
    pub fn show_device(
        &mut self,
        thread_id: &ThreadId,
        id: Option<DeviceId>,
        boot: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.store.read(cx).pages_allowed() {
            return;
        }
        let Some(panel) = self.ensure_id(thread_id, window, cx) else {
            return;
        };
        let any = panel.find(&Tab::Device);
        let ix = match &id {
            Some(device) => self.device_tab_on(thread_id, device, cx).or_else(|| {
                // A tab showing the list takes it.
                self.threads.get(thread_id)?.tabs.iter().position(
                    |t| matches!(t, Open::Device { view } if view.read(cx).chosen().is_none()),
                )
            }),
            None => any,
        };
        let view = match ix.and_then(|ix| self.device_at(thread_id, ix)) {
            Some(view) => {
                if let Some(ix) = ix {
                    self.activate(thread_id, ix, window, cx);
                }
                view
            }
            None => match self.new_device_tab(thread_id, DeviceTab::default(), true, cx) {
                Some(view) => view,
                None => return,
            },
        };
        if let Some(id) = id {
            view.update(cx, |v, cx| v.choose(Some(id), boot, cx));
        }
    }

    /// A new Device tab showing the list, as the + menu opens one.
    pub(super) fn add_device_tab(&mut self, thread_id: &ThreadId, cx: &mut Context<Self>) {
        if self.store.read(cx).pages_allowed() {
            self.new_device_tab(thread_id, DeviceTab::default(), true, cx);
        }
    }

    /// `thread_id`'s device for the card floating over the chat: the tab on
    /// `id` (one made if none shows it), else the Device tab showing, else
    /// the one floated last, else the last. It is no longer the tab the
    /// panel shows. `None` with nothing to show; else whether the panel
    /// still has other tabs to show.
    pub fn float_device(
        &mut self,
        thread_id: &ThreadId,
        id: Option<DeviceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(Entity<DeviceView>, bool)> {
        if !self.store.read(cx).pages_allowed() {
            return None;
        }
        let panel = self.ensure_id(thread_id, window, cx)?;
        let active_device =
            matches!(panel.active_tab(), Some(Open::Device { .. })).then_some(panel.active);
        let floated = panel.floated;
        let ix = match &id {
            Some(device) => self.device_tab_on(thread_id, device, cx),
            None => active_device
                .or_else(|| {
                    let panel = self.threads.get(thread_id)?;
                    panel.tabs.iter().position(
                        |t| matches!(t, Open::Device { view } if Some(view.entity_id()) == floated),
                    )
                })
                .or_else(|| self.threads.get(thread_id)?.find(&Tab::Device)),
        };
        let view = match ix.and_then(|ix| self.device_at(thread_id, ix)) {
            Some(view) => view,
            None => {
                let tab = DeviceTab {
                    device: id.clone(),
                    name: None,
                };
                self.new_device_tab(thread_id, tab, false, cx)?
            }
        };
        if let Some(id) = id
            && view.read(cx).chosen() != Some(&id)
        {
            view.update(cx, |v, cx| v.choose(Some(id), false, cx));
        }
        let panel = self.threads.get_mut(thread_id)?;
        panel.floated = Some(view.entity_id());
        // Shown in the panel too: show another tab there.
        let here = panel
            .tabs
            .iter()
            .position(|t| matches!(t, Open::Device { view: v } if *v == view));
        if here == Some(panel.active)
            && let Some(other) = (0..panel.tabs.len()).rev().find(|&ix| Some(ix) != here)
        {
            panel.active = other;
        }
        let others = panel.tabs.len() > 1;
        cx.notify();
        Some((view, others))
    }

    /// Show the floating device's tab in the panel (its card's "open in the
    /// side panel"), else any Device tab.
    pub fn show_floated_device(
        &mut self,
        thread_id: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let floated = self.threads.get(thread_id).and_then(|p| p.floated);
        let ix = self.threads.get(thread_id).and_then(|panel| {
            panel.tabs.iter().position(
                |t| matches!(t, Open::Device { view } if Some(view.entity_id()) == floated),
            )
        });
        match ix {
            Some(ix) => self.activate(thread_id, ix, window, cx),
            None => self.show_device(thread_id, None, false, window, cx),
        }
    }

    /// The device floating over `thread_id`'s chat, if its tab is still
    /// open.
    pub fn floating_device(&self, thread_id: &ThreadId) -> Option<Entity<DeviceView>> {
        let panel = self.threads.get(thread_id)?;
        let floated = panel.floated?;
        panel.tabs.iter().find_map(|t| match t {
            Open::Device { view } if view.entity_id() == floated => Some(view.clone()),
            _ => None,
        })
    }

    /// Whether the panel shows `view` now.
    pub fn shows_device(&self, thread_id: &ThreadId, view: EntityId) -> bool {
        self.threads
            .get(thread_id)
            .and_then(|p| p.active_tab())
            .is_some_and(|t| matches!(t, Open::Device { view: v } if v.entity_id() == view))
    }

    /// `thread_id`'s Device tab the panel shows, else its last.
    pub fn existing_device(&self, thread_id: &ThreadId) -> Option<Entity<DeviceView>> {
        let panel = self.threads.get(thread_id)?;
        self.device_at(thread_id, panel.find(&Tab::Device)?)
    }

    /// The Device tab that was showing stops streaming at once (only the
    /// visible tab streams).
    pub(super) fn rest_device(&self, thread_id: &ThreadId, ix: usize, cx: &mut Context<Self>) {
        let floated = self.threads.get(thread_id).and_then(|p| p.floated);
        if let Some(view) = self.device_at(thread_id, ix)
            && Some(view.entity_id()) != floated
        {
            view.update(cx, |v, cx| v.close(cx));
        }
    }

    // -- renaming ----------------------------------------------------------------

    /// Rename tab `ix` in place: its label becomes a field (Enter or
    /// clicking away saves, Esc cancels). Only Device tabs have names.
    pub(super) fn start_rename(
        &mut self,
        thread_id: &ThreadId,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.device_at(thread_id, ix) else {
            return;
        };
        let title = view.read(cx).title(cx);
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
        self.renaming = Some((thread_id.clone(), view.entity_id(), input.clone(), sub));
        // A menu that just closed hands focus back to where it opened
        // from; take it again once that's done.
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(60))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.renaming.as_ref().is_some_and(|(.., i, _)| *i == input)
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

    /// End an in-place rename, keeping the new name or not. An empty name
    /// goes back to the device's own.
    pub(super) fn finish_rename(&mut self, save: bool, cx: &mut Context<Self>) {
        let Some((thread_id, view, input, _sub)) = self.renaming.take() else {
            return;
        };
        if save {
            let name = clean_name(&input.read(cx).value());
            let target = self.threads.get(&thread_id).and_then(|p| {
                p.tabs.iter().find_map(|t| match t {
                    Open::Device { view: v } if v.entity_id() == view => Some(v.clone()),
                    _ => None,
                })
            });
            if let Some(target) = target {
                target.update(cx, |v, cx| v.set_name(name, cx));
            }
        }
        cx.notify();
    }

    /// The field renaming tab `view` of `thread_id`, if it is being renamed.
    pub(super) fn rename_field(
        &self,
        thread_id: &ThreadId,
        view: EntityId,
    ) -> Option<Entity<InputState>> {
        self.renaming
            .as_ref()
            .filter(|(t, v, ..)| t == thread_id && *v == view)
            .map(|(.., input, _)| input.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(device: &str, name: Option<&str>) -> DeviceTab {
        DeviceTab {
            device: Some(DeviceId(device.into())),
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn a_session_from_before_tabs_gets_one_tab_per_thread() {
        let (a, b) = (ThreadId("a".into()), ThreadId("b".into()));
        let old = vec![
            (a.clone(), DeviceId("ios:ONE".into())),
            (b.clone(), DeviceId("android:Two".into())),
        ];
        let tabs = from_session(old, Vec::new());
        assert_eq!(tabs[&a], vec![tab("ios:ONE", None)]);
        assert_eq!(tabs[&b], vec![tab("android:Two", None)]);
    }

    #[test]
    fn tabs_saved_the_new_way_win_over_the_old_single_device() {
        let a = ThreadId("a".into());
        let new = vec![(
            a.clone(),
            vec![
                tab("ios:ONE", Some("Mine")),
                DeviceTab::default(),
                tab("android:Two", None),
            ],
        )];
        let old = vec![(a.clone(), DeviceId("ios:OLD".into()))];
        let tabs = from_session(old, new);
        assert_eq!(tabs[&a].len(), 3);
        assert_eq!(tabs[&a][0], tab("ios:ONE", Some("Mine")));
        assert_eq!(tabs[&a][1], DeviceTab::default());
        // Saved and read back: the same.
        let json = serde_json::to_string(&tabs[&a]).unwrap();
        let back: Vec<DeviceTab> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tabs[&a]);
    }

    #[test]
    fn names_are_trimmed_and_blank_means_the_device_name() {
        assert_eq!(clean_name("  Test phone "), Some("Test phone".into()));
        assert_eq!(clean_name("   "), None);
    }
}
