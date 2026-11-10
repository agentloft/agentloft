//! Settings ▸ Browser: what the browser panel searches with, what a new
//! tab opens, and which browser shows pages in the web client.

use super::{Field, SettingsView, row, text_input, web::heading};
use crate::theme::Palette;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Sizable as _, v_flex};
use gpui_kit::{AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _};
use loft_config::{SEARCH_ENGINES, SettingValue};

#[derive(Default)]
pub(super) struct BrowserPage {
    /// "Custom" was picked and no address has been saved for it yet.
    custom: bool,
}

impl SettingsView {
    /// Search with the engine named `id`, or (`None`) with an address of
    /// the user's own, typed in the field that then shows.
    fn pick_search_engine(&mut self, id: Option<&'static str>, cx: &mut Context<Self>) {
        match id {
            Some(id) => {
                self.browser.custom = false;
                // The first engine is what "nothing set" means.
                let value = if id == SEARCH_ENGINES[0].id {
                    SettingValue::Unset
                } else {
                    SettingValue::Str(id.to_owned())
                };
                self.write(&["browser", "search"], value, false, cx);
            }
            None => {
                self.browser.custom = true;
                cx.notify();
            }
        }
    }

    pub(super) fn browser_page(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let engine = self.config.browser.search_engine();
        // An address of one's own is in use, or was just asked for.
        let custom = self.browser.custom || engine.is_none();
        let label = match engine {
            Some(engine) if !custom => engine.name,
            _ => "Custom",
        };
        let view = cx.entity();
        let picker = Button::new("browser-search-engine")
            .cursor_pointer()
            .outline()
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                for option in SEARCH_ENGINES {
                    let view = view.clone();
                    let checked = !custom && engine.is_some_and(|e| e.id == option.id);
                    menu = menu.item(PopupMenuItem::new(option.name).checked(checked).on_click(
                        move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.pick_search_engine(Some(option.id), cx)
                            });
                        },
                    ));
                }
                let view = view.clone();
                menu.separator()
                    .item(PopupMenuItem::new("Custom…").checked(custom).on_click(
                        move |_, _, cx| {
                            view.update(cx, |this, cx| this.pick_search_engine(None, cx));
                        },
                    ))
            })
            .into_any_element();

        let mut page = v_flex().child(row(
            "Search engine",
            "Used when you type words in the address bar instead of an address.",
            picker,
            p,
        ));
        if custom {
            page = page.child(row(
                "Search address",
                "The address to search with. Put %s where your words go.",
                text_input(&self.input(Field::BrowserSearch)),
                p,
            ));
        }
        page = page.child(row(
            "Home page",
            "Opens in each new browser tab. Leave it empty for a blank tab.",
            text_input(&self.input(Field::BrowserHome)),
            p,
        ));
        // Which program runs on the daemon's computer is for that computer
        // to decide, not for a browser signed in to it.
        if !crate::host::IS_WEB {
            page = page
                .child(heading(
                    "In the web client",
                    "There the browser tab shows pages loaded by a browser on this computer.",
                    p,
                ))
                .child(row(
                    "Browser app",
                    "The path of the browser to run. Leave it empty to use Chrome, Chromium, Edge or Brave, whichever is installed.",
                    text_input(&self.input(Field::BrowserApp)),
                    p,
                ));
        }
        page.into_any_element()
    }
}
