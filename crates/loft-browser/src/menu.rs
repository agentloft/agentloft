//! Menus drawn in their own small window. The web page is a native view
//! that covers anything GPUI draws in the main window, so an in-window
//! menu would open underneath it; a floating window sits above it and the
//! page keeps running.

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions, div, point, prelude::FluentBuilder as _, px, size,
};
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

thread_local! {
    /// When a menu last closed because the user clicked away: a click on
    /// the button that opened it closes it, rather than opening it again.
    static CLOSED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
}

const ROW: f32 = 30.;
const SEPARATOR: f32 = 9.;
const PAD: f32 = 5.;
const WIDTH: f32 = 236.;

type Action = Rc<dyn Fn(&mut App)>;

/// One line of a [`PopupMenu`].
pub enum Entry {
    Item {
        label: SharedString,
        icon: IconName,
        checked: bool,
        disabled: bool,
        /// A letter that picks the item while the menu is open.
        shortcut: Option<char>,
        action: Action,
    },
    Separator,
}

impl Entry {
    pub fn item(
        label: impl Into<SharedString>,
        icon: IconName,
        action: impl Fn(&mut App) + 'static,
    ) -> Self {
        Entry::Item {
            label: label.into(),
            icon,
            checked: false,
            disabled: false,
            shortcut: None,
            action: Rc::new(action),
        }
    }

    pub fn checked(mut self, on: bool) -> Self {
        if let Entry::Item { checked, .. } = &mut self {
            *checked = on;
        }
        self
    }

    pub fn shortcut(mut self, key: char) -> Self {
        if let Entry::Item { shortcut, .. } = &mut self {
            *shortcut = Some(key.to_ascii_lowercase());
        }
        self
    }

    pub fn disabled(mut self, on: bool) -> Self {
        if let Entry::Item { disabled, .. } = &mut self {
            *disabled = on;
        }
        self
    }
}

/// Open `entries` under `anchor` (bounds in `window`'s coordinates), right
/// edges aligned. The menu closes on a pick, Escape, or clicking elsewhere.
pub fn open(window: &Window, anchor: Bounds<Pixels>, entries: Vec<Entry>, cx: &mut App) {
    if CLOSED_AT
        .get()
        .is_some_and(|t| t.elapsed() < Duration::from_millis(250))
    {
        return;
    }
    let height = PAD * 2.
        + entries
            .iter()
            .map(|e| match e {
                Entry::Item { .. } => ROW,
                Entry::Separator => SEPARATOR,
            })
            .sum::<f32>();
    let win = window.bounds();
    let x = f32::from(win.origin.x + anchor.origin.x + anchor.size.width) - WIDTH;
    let y = f32::from(win.origin.y + anchor.origin.y + anchor.size.height) + 4.;
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(x.max(0.)), px(y)),
            size(px(WIDTH), px(height)),
        ))),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id: window.display(cx).map(|d| d.id()),
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    let _ = cx.open_window(options, move |window, cx| {
        cx.new(|cx| PopupMenu::new(entries, window, cx))
    });
}

struct PopupMenu {
    entries: Vec<Entry>,
    focus: FocusHandle,
    hovered: Option<usize>,
}

impl PopupMenu {
    fn new(entries: Vec<Entry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        // Clicking anywhere else (or switching apps) closes the menu.
        cx.observe_window_activation(window, |_, window, _| {
            if !window.is_window_active() {
                CLOSED_AT.set(Some(Instant::now()));
                window.remove_window();
            }
        })
        .detach();
        Self {
            entries,
            focus,
            hovered: None,
        }
    }
}

impl Focusable for PopupMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PopupMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, border, fg, muted, hover, accent) = (
            theme.popover,
            theme.border,
            theme.popover_foreground,
            theme.muted_foreground,
            theme.accent,
            theme.primary,
        );
        let mut list = v_flex().p(px(PAD));
        for (ix, entry) in self.entries.iter().enumerate() {
            match entry {
                Entry::Separator => {
                    list = list.child(
                        div()
                            .h(px(SEPARATOR))
                            .flex()
                            .items_center()
                            .child(div().w_full().h_px().bg(border)),
                    );
                }
                Entry::Item {
                    label,
                    icon,
                    checked,
                    disabled,
                    shortcut,
                    action,
                } => {
                    let action = action.clone();
                    let (disabled, checked) = (*disabled, *checked);
                    list = list.child(
                        h_flex()
                            .id(SharedString::from(format!("menu-row-{ix}")))
                            .h(px(ROW))
                            .px_2()
                            .gap_2()
                            .items_center()
                            .rounded(px(6.))
                            .text_sm()
                            .when(disabled, |d| d.text_color(muted.opacity(0.6)))
                            .when(!disabled, |d| {
                                d.text_color(fg)
                                    .cursor_pointer()
                                    .when(self.hovered == Some(ix), |d| d.bg(hover))
                            })
                            .on_hover(cx.listener(move |this, on: &bool, _, cx| {
                                this.hovered = if *on { Some(ix) } else { None };
                                cx.notify();
                            }))
                            .child(Icon::new(*icon).small().text_color(if checked {
                                accent
                            } else {
                                muted
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .when(checked, |d| d.font_weight(FontWeight::MEDIUM))
                                    .child(label.clone()),
                            )
                            .when(checked, |d| {
                                d.child(Icon::new(IconName::Check).small().text_color(accent))
                            })
                            .when_some(*shortcut, |d, key| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(key.to_ascii_uppercase().to_string()),
                                )
                            })
                            .when(!disabled, |d| {
                                d.on_click(move |_, window, cx| {
                                    window.remove_window();
                                    action(cx);
                                })
                            }),
                    );
                }
            }
        }
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(
                cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    if key == "escape" {
                        window.remove_window();
                        return;
                    }
                    let picked = this.entries.iter().find_map(|e| match e {
                        Entry::Item {
                            shortcut: Some(k),
                            disabled: false,
                            action,
                            ..
                        } if key.len() == 1 && key.starts_with(*k) => Some(action.clone()),
                        _ => None,
                    });
                    if let Some(action) = picked {
                        window.remove_window();
                        action(cx);
                    }
                }),
            )
            .child(
                list.size_full()
                    .bg(bg)
                    .border_1()
                    .border_color(border)
                    .rounded(px(10.))
                    .shadow_lg(),
            )
    }
}
