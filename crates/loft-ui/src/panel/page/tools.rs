//! A browser tab's toolbar beyond going somewhere: a screenshot for the
//! composer, notes for the agent, other screens, zoom and the ⋮ menu. All of
//! it acts on the daemon's page, so whoever else shows the same tab (the
//! web client, another window) sees it too.

use super::{PageEvent, PageView};
use crate::store::request_in;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Bounds, Context, IntoElement, ParentElement as _, Pixels, Styled as _, canvas, div,
};
use loft_browser::menu::{self, Entry};
use loft_protocol::{PageDevice, PageNav, Request, Response};
use std::cell::Cell;
use std::rc::Rc;

/// The icon for leaving notes on things for the agent.
pub use loft_browser::ANNOTATE_ICON;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Desktop,
    Tablet,
    Phone,
}

/// A screen to show the page as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Device {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub kind: DeviceKind,
}

/// Current screens (CSS viewport sizes), checked November 2026.
pub const DEVICES: &[Device] = &[
    Device {
        name: "Desktop",
        width: 1440,
        height: 900,
        kind: DeviceKind::Desktop,
    },
    Device {
        name: "Laptop",
        width: 1280,
        height: 800,
        kind: DeviceKind::Desktop,
    },
    Device {
        name: "iPad Pro 13″",
        width: 1032,
        height: 1376,
        kind: DeviceKind::Tablet,
    },
    Device {
        name: "iPad Pro 11″",
        width: 834,
        height: 1210,
        kind: DeviceKind::Tablet,
    },
    Device {
        name: "iPad Air",
        width: 820,
        height: 1180,
        kind: DeviceKind::Tablet,
    },
    Device {
        name: "iPad mini",
        width: 744,
        height: 1133,
        kind: DeviceKind::Tablet,
    },
    Device {
        name: "iPhone 18 Pro Max",
        width: 440,
        height: 956,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "iPhone 18 Pro",
        width: 402,
        height: 874,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "iPhone Air",
        width: 420,
        height: 912,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "iPhone 17e",
        width: 390,
        height: 844,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "Pixel 10 Pro",
        width: 410,
        height: 914,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "Android compact",
        width: 360,
        height: 800,
        kind: DeviceKind::Phone,
    },
];

// Safari on iOS 26 keeps reporting 18_6 as the OS; Chrome reduces Android's.
const UA_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1";
const UA_IPAD: &str = "Mozilla/5.0 (iPad; CPU OS 18_6 like Mac OS X) AppleWebKit/605.1.15 \
    (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1";
const UA_ANDROID: &str = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/142.0.0.0 Mobile Safari/537.36";

impl Device {
    /// The device as the daemon is told it, held upright or `rotated`.
    pub fn page_device(&self, rotated: bool) -> PageDevice {
        let (width, height) = if rotated {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        let android = self.name.starts_with("Pixel") || self.name.starts_with("Android");
        let user_agent = match self.kind {
            DeviceKind::Desktop => None,
            DeviceKind::Phone if android => Some(UA_ANDROID),
            DeviceKind::Phone => Some(UA_IPHONE),
            DeviceKind::Tablet => Some(UA_IPAD),
        };
        PageDevice {
            name: self.name.to_owned(),
            width,
            height,
            mobile: self.kind != DeviceKind::Desktop,
            user_agent: user_agent.map(str::to_owned),
        }
    }

    pub(super) fn icon(&self) -> IconName {
        match self.kind {
            DeviceKind::Desktop => IconName::Monitor,
            DeviceKind::Tablet => IconName::Tablet,
            DeviceKind::Phone => IconName::Smartphone,
        }
    }
}

const ZOOM_STEPS: [f32; 11] = [0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// The zoom one step from `zoom`: `1` in, `-1` out, `0` back to 100%.
pub fn zoom_step(zoom: f32, dir: i32) -> f32 {
    let at = ZOOM_STEPS
        .iter()
        .position(|z| (z - zoom).abs() < 0.01)
        .unwrap_or(5);
    match dir {
        0 => 1.0,
        d if d > 0 => ZOOM_STEPS[(at + 1).min(ZOOM_STEPS.len() - 1)],
        _ => ZOOM_STEPS[at.saturating_sub(1)],
    }
}

/// Where a toolbar button is, for the menu it opens.
pub(super) type Anchor = Rc<Cell<Option<Bounds<Pixels>>>>;

fn anchored(button: Button, at: Anchor) -> AnyElement {
    div()
        .relative()
        .child(button)
        .child(
            canvas(move |bounds, _, _| at.set(Some(bounds)), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
        .into_any_element()
}

impl PageView {
    pub(super) fn page_request(&self, body: Request, cx: &mut Context<Self>) {
        request_in(&self.store, body, cx, |this, result, cx| {
            if let Err(err) = result {
                this.store.update(cx, |_, cx| {
                    cx.emit(crate::store::StoreEvent::Notify {
                        message: err.message,
                        error: true,
                    })
                });
            }
        });
    }

    pub fn is_annotating(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.annotating)
    }

    /// Turn annotation mode on or off (for everyone showing the tab).
    pub fn set_annotating(&mut self, on: bool, cx: &mut Context<Self>) {
        let Some(info) = &mut self.info else {
            return;
        };
        if info.annotating == on {
            return;
        }
        info.annotating = on;
        let id = info.id.clone();
        self.page_request(Request::PageAnnotate { id, on }, cx);
        if !on {
            cx.emit(PageEvent::AnnotatingStopped);
        }
        cx.notify();
    }

    /// Take note `n`'s pin off the page.
    pub fn remove_annotation(&mut self, n: u32, cx: &mut Context<Self>) {
        if let Some(info) = &self.info {
            let id = info.id.clone();
            self.page_request(Request::PageUnpin { id, n: Some(n) }, cx);
        }
    }

    /// Take every pin off the page (the notes were sent).
    pub fn clear_annotations(&mut self, cx: &mut Context<Self>) {
        if let Some(info) = &self.info {
            let id = info.id.clone();
            self.page_request(Request::PageUnpin { id, n: None }, cx);
        }
    }

    /// A PNG of the page as it shows (pins included), for attaching.
    pub fn capture(&self, cx: &mut Context<Self>) -> Option<gpui_kit::Task<Option<Vec<u8>>>> {
        let id = self.info.as_ref()?.id.clone();
        let client = self.store.read(cx).client()?;
        let asked = client.request(Request::PageCapture { id });
        Some(cx.background_executor().spawn(async move {
            match asked.await {
                Ok(Response::Picture { png }) => Some(png.0),
                _ => None,
            }
        }))
    }

    fn take_screenshot(&mut self, cx: &mut Context<Self>) {
        let Some(task) = self.capture(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Some(png) = task.await {
                let _ = this.update(cx, |_, cx| cx.emit(PageEvent::Screenshot(png)));
            }
        })
        .detach();
    }

    /// Show the page as the device `ix` of [`DEVICES`], or at this view's
    /// size (`None`).
    pub fn set_device(&mut self, ix: Option<usize>, rotated: bool, cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        let device = ix.map(|ix| DEVICES[ix].page_device(rotated));
        let id = info.id.clone();
        self.page_request(Request::PageEmulate { id, device }, cx);
    }

    pub fn zoom_by(&mut self, dir: i32, cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        let zoom = zoom_step(info.zoom, dir);
        let id = info.id.clone();
        self.page_request(Request::PageZoom { id, zoom }, cx);
    }

    fn clear_data(&mut self, cookies: bool, cache: bool, cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        let id = info.id.clone();
        self.page_request(
            Request::PageClearData {
                id: id.clone(),
                cookies,
                cache,
            },
            cx,
        );
        if cookies {
            self.page_request(
                Request::PageNavigate {
                    id,
                    nav: PageNav::Reload,
                },
                cx,
            );
        }
    }

    /// The toolbar's buttons after the address.
    pub(super) fn tool_buttons(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let ready = self.info.is_some();
        let annotating = self.is_annotating();
        vec![
            Button::new("page-shot")
                .cursor_pointer()
                .ghost()
                .small()
                .icon(IconName::Camera)
                .tooltip("Screenshot to the composer")
                .disabled(!ready)
                .on_click(cx.listener(|this, _, _, cx| this.take_screenshot(cx)))
                .into_any_element(),
            Button::new("page-responsive")
                .cursor_pointer()
                .small()
                .icon(IconName::MonitorSmartphone)
                .when(self.is_responsive(), |b| b.primary())
                .when(!self.is_responsive(), |b| b.ghost())
                .tooltip(if self.is_responsive() {
                    "Fill the panel again"
                } else {
                    "Responsive: give the page a size of its own, or a phone's or a tablet's"
                })
                .disabled(!ready)
                .on_click(cx.listener(|this, _, _, cx| {
                    let on = !this.is_responsive();
                    this.set_responsive(on, cx);
                }))
                .into_any_element(),
            Button::new("page-annotate")
                .cursor_pointer()
                .small()
                .icon(ANNOTATE_ICON)
                .when(annotating, |b| b.primary())
                .when(!annotating, |b| b.ghost())
                .tooltip(if annotating {
                    "Stop annotating"
                } else {
                    "Annotate: click an element or drag over an area to leave notes for the agent"
                })
                .disabled(!ready)
                .on_click(cx.listener(|this, _, _, cx| {
                    let on = !this.is_annotating();
                    this.set_annotating(on, cx);
                }))
                .into_any_element(),
            self.more_menu(cx),
        ]
    }

    /// The strip under the toolbar while notes are being left.
    pub(super) fn annotate_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.is_annotating() {
            return None;
        }
        Some(
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(p.border)
                .bg(p.accent.opacity(0.12))
                .text_xs()
                .child(Icon::new(ANNOTATE_ICON).xsmall().text_color(p.accent))
                .child(div().flex_1().min_w_0().child(
                    "Click an element or drag over an area to leave a note. Notes wait in the composer until you send.",
                ))
                .child(
                    Button::new("page-annotate-done")
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .label("Done")
                        .on_click(cx.listener(|this, _, _, cx| this.set_annotating(false, cx))),
                )
                .into_any_element(),
        )
    }

    /// The ⋮ menu: reloads, zoom, site data.
    fn more_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.weak_entity();
        let zoom = self.info.as_ref().map_or(1.0, |i| i.zoom);
        let anchor = self.more_anchor.clone();
        let button = Button::new("page-more")
            .cursor_pointer()
            .small()
            .ghost()
            .icon(IconName::EllipsisVertical)
            .tooltip("More")
            .disabled(self.info.is_none())
            .on_click(move |_, window, cx| {
                let act = |label: String,
                           icon: IconName,
                           f: fn(&mut PageView, &mut Context<PageView>)| {
                    let v = view.clone();
                    Entry::item(label, icon, move |cx| {
                        _ = v.update(cx, f);
                    })
                };
                let entries = vec![
                    act("Hard Reload".into(), IconName::RotateCw, |this, cx| {
                        this.navigate(PageNav::HardReload, cx)
                    }),
                    Entry::Separator,
                    act("Zoom In".into(), IconName::ZoomIn, |this, cx| {
                        this.zoom_by(1, cx)
                    }),
                    act("Zoom Out".into(), IconName::ZoomOut, |this, cx| {
                        this.zoom_by(-1, cx)
                    }),
                    act(
                        format!("Actual Size · {:.0}%", zoom * 100.),
                        IconName::Expand,
                        |this, cx| this.zoom_by(0, cx),
                    )
                    .disabled((zoom - 1.0).abs() < 0.01),
                    Entry::Separator,
                    act(
                        "Clear Cookies & Site Data".into(),
                        IconName::Cookie,
                        |this, cx| this.clear_data(true, false, cx),
                    ),
                    act("Clear Cache".into(), IconName::Eraser, |this, cx| {
                        this.clear_data(false, true, cx)
                    }),
                ];
                if let Some(at) = anchor.get() {
                    menu::open(window, at, entries, cx);
                }
            });
        anchored(button, self.more_anchor.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps() {
        assert_eq!(zoom_step(1.0, 1), 1.1);
        assert_eq!(zoom_step(1.0, -1), 0.9);
        assert_eq!(zoom_step(2.0, 1), 2.0);
        assert_eq!(zoom_step(0.5, -1), 0.5);
        assert_eq!(zoom_step(1.5, 0), 1.0);
    }

    #[test]
    fn devices_say_what_they_are() {
        let phone = DEVICES.iter().find(|d| d.name == "iPhone 18 Pro").unwrap();
        let d = phone.page_device(false);
        assert!(d.mobile && d.user_agent.as_deref().unwrap().contains("iPhone"));
        assert_eq!((d.width, d.height), (402, 874));
        let turned = phone.page_device(true);
        assert_eq!((turned.width, turned.height), (874, 402));
        let pixel = DEVICES.iter().find(|d| d.name == "Pixel 10 Pro").unwrap();
        assert!(
            pixel
                .page_device(false)
                .user_agent
                .unwrap()
                .contains("Android")
        );
        let laptop = DEVICES.iter().find(|d| d.name == "Laptop").unwrap();
        let d = laptop.page_device(false);
        assert!(!d.mobile && d.user_agent.is_none());
    }
}
