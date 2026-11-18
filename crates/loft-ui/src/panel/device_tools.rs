//! The Device panel's tools drawer (ADR-0020): the app in front, and the
//! device's settings it takes (`DeviceSettings::supported`): appearance,
//! text size, contrast, reduce motion, the screen reader, accessibility
//! frames, the network, orientation, a fake location, an app's permissions
//! and a test push. Every control shows what the device says after each
//! change (`Request::DeviceSettings` again after `Request::DeviceSet`), not
//! what was clicked.

use crate::press::ClaimsPress as _;
use crate::spinner::Spinner;
use crate::store::{AppStore, request_in};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Task, Window, canvas, div, px,
};
use loft_protocol::{
    DeviceId, DeviceOrientation, DevicePlatform, DeviceSetting, DeviceSettings, DeviceTextSize,
    Request, Response,
};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// How wide the drawer is.
pub const DRAWER_WIDTH: f32 = 288.;
/// While the drawer is open its settings are read again this often (the
/// app in front changes as the device is used).
const READ_EVERY: Duration = Duration::from_secs(4);

/// Text sizes, smallest first.
const TEXT_SIZES: [DeviceTextSize; 6] = [
    DeviceTextSize::ExtraSmall,
    DeviceTextSize::Small,
    DeviceTextSize::Default,
    DeviceTextSize::Large,
    DeviceTextSize::ExtraLarge,
    DeviceTextSize::Largest,
];

/// Places to pretend to be.
const PLACES: [(&str, f64, f64); 6] = [
    ("Cupertino", 37.3349, -122.009),
    ("London", 51.5072, -0.1276),
    ("Tokyo", 35.6764, 139.65),
    ("New York", 40.7128, -74.006),
    ("Sydney", -33.8688, 151.2093),
    ("São Paulo", -23.5558, -46.6396),
];

/// Permissions an app can be granted: iOS `simctl privacy` services, and
/// Android's runtime permissions (sent as `android.permission.<NAME>`).
const IOS_PERMISSIONS: [&str; 12] = [
    "photos",
    "photos-add",
    "camera",
    "location",
    "location-always",
    "contacts",
    "calendar",
    "reminders",
    "microphone",
    "motion",
    "media-library",
    "siri",
];
const ANDROID_PERMISSIONS: [&str; 10] = [
    "CAMERA",
    "ACCESS_FINE_LOCATION",
    "ACCESS_COARSE_LOCATION",
    "READ_CONTACTS",
    "RECORD_AUDIO",
    "POST_NOTIFICATIONS",
    "READ_CALENDAR",
    "READ_MEDIA_IMAGES",
    "BLUETOOTH_CONNECT",
    "ACTIVITY_RECOGNITION",
];

/// The permissions offered for a platform.
pub fn permissions(platform: DevicePlatform) -> &'static [&'static str] {
    match platform {
        DevicePlatform::Ios => &IOS_PERMISSIONS,
        DevicePlatform::Android => &ANDROID_PERMISSIONS,
    }
}

/// A permission's name as the device takes it.
fn permission_name(platform: DevicePlatform, short: &str) -> String {
    match platform {
        DevicePlatform::Android if !short.contains('.') => format!("android.permission.{short}"),
        _ => short.to_owned(),
    }
}

/// A test push for an app: an APNs payload with an alert.
pub fn push_payload(title: &str, body: &str) -> serde_json::Value {
    serde_json::json!({ "aps": { "alert": { "title": title, "body": body } } })
}

/// A latitude and longitude as typed, in degrees, or why not.
pub fn parse_location(latitude: &str, longitude: &str) -> Result<(f64, f64), &'static str> {
    let lat: f64 = latitude
        .trim()
        .parse()
        .map_err(|_| "The latitude isn't a number.")?;
    let lon: f64 = longitude
        .trim()
        .parse()
        .map_err(|_| "The longitude isn't a number.")?;
    if !(-90. ..=90.).contains(&lat) {
        return Err("The latitude is between -90 and 90.");
    }
    if !(-180. ..=180.).contains(&lon) {
        return Err("The longitude is between -180 and 180.");
    }
    Ok((lat, lon))
}

/// The text size `step`s from `size` (none known: from the default),
/// staying within the sizes there are.
pub fn step_text_size(size: Option<DeviceTextSize>, step: i32) -> DeviceTextSize {
    let at = TEXT_SIZES
        .iter()
        .position(|s| Some(*s) == size)
        .unwrap_or(2) as i32;
    TEXT_SIZES[(at + step).clamp(0, TEXT_SIZES.len() as i32 - 1) as usize]
}

fn text_size_label(size: DeviceTextSize) -> &'static str {
    match size {
        DeviceTextSize::ExtraSmall => "Extra small",
        DeviceTextSize::Small => "Small",
        DeviceTextSize::Default => "Default",
        DeviceTextSize::Large => "Large",
        DeviceTextSize::ExtraLarge => "Extra large",
        DeviceTextSize::Largest => "Largest",
    }
}

const ORIENTATIONS: [(DeviceOrientation, &str); 4] = [
    (DeviceOrientation::Portrait, "Portrait"),
    (DeviceOrientation::LandscapeLeft, "Left"),
    (DeviceOrientation::LandscapeRight, "Right"),
    (DeviceOrientation::PortraitUpsideDown, "Upside down"),
];

/// A switch in the drawer: the setting's name in `supported`, its label,
/// its value read back, and the setting it makes.
type Toggle = (
    &'static str,
    &'static str,
    Option<bool>,
    fn(bool) -> DeviceSetting,
);

/// The drawer for one device.
pub struct DeviceTools {
    store: Entity<AppStore>,
    id: DeviceId,
    platform: DevicePlatform,
    /// The device's settings as it last said.
    settings: Option<DeviceSettings>,
    /// Reading them now; a change waits for its read back.
    reading: bool,
    changing: bool,
    /// What went wrong last (a setting the device refused).
    error: Option<String>,
    /// What the last change that can't be read back did ("Location set").
    done: Option<String>,
    running: bool,
    latitude: Entity<InputState>,
    longitude: Entity<InputState>,
    push_title: Entity<InputState>,
    push_body: Entity<InputState>,
    /// The app and permission picked for the permission tool, and the app
    /// for a push (none: the app in front).
    perm_app: Option<String>,
    permission: &'static str,
    push_app: Option<String>,
    /// Where the menu buttons are (their menus open under them).
    perm_app_at: Rc<Cell<Option<Bounds<Pixels>>>>,
    permission_at: Rc<Cell<Option<Bounds<Pixels>>>>,
    push_app_at: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Reads the settings again now and then while the drawer is open.
    poll: Option<Task<()>>,
}

impl DeviceTools {
    pub fn new(
        store: Entity<AppStore>,
        id: DeviceId,
        platform: DevicePlatform,
        running: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let mut this = Self {
            store,
            id,
            platform,
            settings: None,
            reading: false,
            changing: false,
            error: None,
            done: None,
            running,
            latitude: input("Latitude", window, cx),
            longitude: input("Longitude", window, cx),
            push_title: input("Title", window, cx),
            push_body: input("Message", window, cx),
            perm_app: None,
            permission: permissions(platform)[0],
            push_app: None,
            perm_app_at: Rc::default(),
            permission_at: Rc::default(),
            push_app_at: Rc::default(),
            poll: None,
        };
        if running {
            this.refresh(cx);
        }
        this
    }

    pub fn device(&self) -> &DeviceId {
        &self.id
    }

    /// The device's settings as it last said, once it has.
    pub fn settings(&self) -> Option<&DeviceSettings> {
        self.settings.as_ref()
    }

    pub fn supports(&self, setting: &str) -> bool {
        self.settings
            .as_ref()
            .is_some_and(|s| s.supported.iter().any(|n| n == setting))
    }

    /// Whether the device runs (its settings can be read); read them once
    /// it starts.
    pub fn set_running(&mut self, running: bool, cx: &mut Context<Self>) {
        if running && !self.running {
            self.running = true;
            self.refresh(cx);
        }
        self.running = running;
    }

    /// Read the settings from the device again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.reading {
            return;
        }
        self.reading = true;
        let id = self.id.clone();
        request_in(
            &self.store,
            Request::DeviceSettings { id },
            cx,
            |this, result, cx| {
                this.reading = false;
                this.changing = false;
                match result {
                    Ok(Response::DeviceSettings { settings }) => {
                        tracing::debug!(?settings, "the device's settings");
                        this.settings = Some(settings);
                    }
                    Ok(_) => {}
                    Err(err) => this.error = Some(err.message),
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    /// Change a setting, then read them all back.
    pub fn set(&mut self, setting: DeviceSetting, cx: &mut Context<Self>) {
        self.error = None;
        self.done = None;
        self.changing = true;
        let id = self.id.clone();
        let done = match &setting {
            DeviceSetting::Location {
                latitude,
                longitude,
            } => Some(format!("Location set to {latitude:.4}, {longitude:.4}.")),
            DeviceSetting::Permission {
                app,
                permission,
                grant,
            } => Some(format!(
                "{} {permission} for {app}.",
                if *grant { "Granted" } else { "Revoked" }
            )),
            DeviceSetting::Push { app, .. } => Some(format!("Sent a push to {app}.")),
            _ => None,
        };
        tracing::debug!(?setting, "change the device's setting");
        request_in(
            &self.store,
            Request::DeviceSet { id, setting },
            cx,
            move |this, result, cx| {
                match result {
                    Ok(_) => this.done = done,
                    Err(err) => this.error = Some(err.message),
                }
                // Whatever happened, show what the device says now.
                this.reading = false;
                this.refresh(cx);
            },
        );
        cx.notify();
    }

    /// The drawer opened or closed: while open, read the settings again
    /// now and then.
    pub fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if !open {
            self.poll = None;
            return;
        }
        if self.poll.is_some() {
            return;
        }
        if self.running {
            self.refresh(cx);
        }
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(READ_EVERY).await;
                let alive = this.update(cx, |this, cx| {
                    if this.running && !this.changing {
                        this.refresh(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    fn set_location(&mut self, cx: &mut Context<Self>) {
        let lat = self.latitude.read(cx).value().to_string();
        let lon = self.longitude.read(cx).value().to_string();
        match parse_location(&lat, &lon) {
            Ok((latitude, longitude)) => self.set(
                DeviceSetting::Location {
                    latitude,
                    longitude,
                },
                cx,
            ),
            Err(why) => {
                self.done = None;
                self.error = Some(why.to_owned());
                cx.notify();
            }
        }
    }

    fn go_to(
        &mut self,
        latitude: f64,
        longitude: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.latitude
            .update(cx, |s, cx| s.set_value(latitude.to_string(), window, cx));
        self.longitude
            .update(cx, |s, cx| s.set_value(longitude.to_string(), window, cx));
        self.set(
            DeviceSetting::Location {
                latitude,
                longitude,
            },
            cx,
        );
    }

    /// The app a tool acts on: the one picked, else the one in front, else
    /// the first there is.
    fn app_for(&self, picked: &Option<String>) -> Option<String> {
        let settings = self.settings.as_ref();
        picked
            .clone()
            .or_else(|| settings.and_then(|s| s.foreground_app.clone()))
            .filter(|a| settings.is_none_or(|s| s.apps.is_empty() || s.apps.contains(a)))
            .or_else(|| settings.and_then(|s| s.apps.first().cloned()))
    }

    fn grant(&mut self, grant: bool, cx: &mut Context<Self>) {
        let Some(app) = self.app_for(&self.perm_app) else {
            return;
        };
        let permission = permission_name(self.platform, self.permission);
        self.set(
            DeviceSetting::Permission {
                app,
                permission,
                grant,
            },
            cx,
        );
    }

    fn send_push(&mut self, cx: &mut Context<Self>) {
        let Some(app) = self.app_for(&self.push_app) else {
            return;
        };
        let title = self.push_title.read(cx).value().trim().to_owned();
        let body = self.push_body.read(cx).value().trim().to_owned();
        let title = if title.is_empty() {
            "Test notification".to_owned()
        } else {
            title
        };
        self.set(
            DeviceSetting::Push {
                app,
                payload: push_payload(&title, &body),
            },
            cx,
        );
    }

    /// A menu of the device's apps for one tool.
    fn app_menu(&self, push: bool, window: &mut Window, cx: &mut Context<Self>) {
        use loft_browser::menu::{self, Entry};
        let anchor = if push {
            &self.push_app_at
        } else {
            &self.perm_app_at
        };
        let Some(at) = anchor.get() else {
            return;
        };
        let current = self.app_for(if push { &self.push_app } else { &self.perm_app });
        let me = cx.weak_entity();
        let entries = self
            .settings
            .as_ref()
            .map(|s| s.apps.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|app| {
                let (me, pick) = (me.clone(), app.clone());
                Entry::item(app.clone(), IconName::Smartphone, move |cx| {
                    let pick = Some(pick.clone());
                    let _ = me.update(cx, |t, cx| {
                        if push {
                            t.push_app = pick;
                        } else {
                            t.perm_app = pick;
                        }
                        cx.notify();
                    });
                })
                .checked(current.as_ref() == Some(&app))
            })
            .collect();
        menu::open(window, at, entries, cx);
    }

    fn permission_menu(&self, window: &mut Window, cx: &mut Context<Self>) {
        use loft_browser::menu::{self, Entry};
        let Some(at) = self.permission_at.get() else {
            return;
        };
        let me = cx.weak_entity();
        let entries = permissions(self.platform)
            .iter()
            .map(|&name| {
                let me = me.clone();
                Entry::item(name, IconName::ShieldCheck, move |cx| {
                    let _ = me.update(cx, |t, cx| {
                        t.permission = name;
                        cx.notify();
                    });
                })
                .checked(self.permission == name)
            })
            .collect();
        menu::open(window, at, entries, cx);
    }
}

// -- drawing ------------------------------------------------------------------------

/// A titled group of controls.
fn section(title: &'static str, p: &Palette) -> gpui_kit::Div {
    v_flex().gap_1p5().child(
        div()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(p.muted_foreground)
            .child(title),
    )
}

/// A label with its control at the right.
fn row(label: &'static str, control: impl IntoElement, p: &Palette) -> gpui_kit::Div {
    h_flex()
        .min_h(px(28.))
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(p.foreground)
                .child(label),
        )
        .child(control)
}

/// A button that opens a menu under it (kept where it is drawn).
fn menu_button(
    id: &'static str,
    label: String,
    anchor: &Rc<Cell<Option<Bounds<Pixels>>>>,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let anchor = anchor.clone();
    h_flex()
        .id(id)
        .relative()
        .flex_1()
        .min_w_0()
        .h(px(28.))
        .px_2()
        .gap_1()
        .items_center()
        .rounded(px(6.))
        .border_1()
        .border_color(p.border)
        .text_sm()
        .text_color(p.foreground)
        .cursor_pointer()
        .hover(|d| d.bg(p.hover))
        .child(div().flex_1().min_w_0().truncate().child(label))
        .child(
            Icon::new(IconName::ChevronDown)
                .xsmall()
                .text_color(p.muted_foreground),
        )
        .child(
            canvas(
                move |bounds, _, _| anchor.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .claims_press()
}

impl Render for DeviceTools {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let body = self.body(&p, cx);
        let note = match (&self.error, &self.done) {
            (Some(e), _) => Some((e.clone(), p.danger)),
            (None, Some(d)) => Some((d.clone(), p.success)),
            _ => None,
        }
        .filter(|_| self.running && self.settings.is_some());
        v_flex()
            .id("device-tools")
            .w(px(DRAWER_WIDTH))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(p.border)
            .bg(p.background)
            .child(
                h_flex()
                    .h(px(36.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        Icon::new(IconName::SlidersHorizontal)
                            .small()
                            .text_color(p.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.foreground)
                            .child("Device tools"),
                    )
                    .when(self.reading || self.changing, |d| {
                        d.child(Spinner::new().xsmall().color(p.muted_foreground))
                    })
                    .child(
                        Button::new("device-tools-refresh")
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .tooltip("Read the settings again")
                            .disabled(!self.running)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            // What the last change did, or why it didn't: kept in sight
            // whatever the drawer is scrolled to.
            .when_some(note, |d, (text, color)| {
                d.child(
                    div()
                        .id("device-tools-note")
                        .flex_none()
                        .mx_3()
                        .mt_2()
                        .px_2()
                        .py_1p5()
                        .rounded(px(6.))
                        .bg(p.muted)
                        .text_xs()
                        .text_color(color)
                        .child(text),
                )
            })
            .child(
                div()
                    .id("device-tools-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

impl DeviceTools {
    fn body(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let muted = |text: &str| {
            div()
                .p_3()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(text.to_owned())
                .into_any_element()
        };
        if !self.running {
            return muted("Start the device to see and change its settings.");
        }
        let Some(settings) = self.settings.clone() else {
            return match &self.error {
                Some(error) => muted(error),
                None => h_flex()
                    .p_3()
                    .gap_2()
                    .items_center()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(Spinner::new().xsmall().color(p.muted_foreground))
                    .child("Reading the device's settings…")
                    .into_any_element(),
            };
        };
        let has = |name: &str| settings.supported.iter().any(|s| s == name);
        let busy = self.changing;
        let mut body = v_flex().p_3().gap_4();

        // The app in front.
        body = body.child(
            section("App in front", p).child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_sm()
                    .text_color(p.foreground)
                    .child(
                        Icon::new(IconName::Smartphone)
                            .small()
                            .text_color(p.muted_foreground),
                    )
                    .child(
                        div().min_w_0().truncate().child(
                            settings
                                .foreground_app
                                .clone()
                                .unwrap_or_else(|| "Home screen".into()),
                        ),
                    ),
            ),
        );

        // Display.
        let mut display = section("Display", p);
        let mut any_display = false;
        if has("appearance") {
            any_display = true;
            let dark = settings.dark;
            let choice = |id: &'static str, label: &'static str, on: bool| {
                Button::new(id)
                    .cursor_pointer()
                    .small()
                    .label(label)
                    .selected(dark == Some(on))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set(DeviceSetting::Appearance { dark: on }, cx)
                    }))
            };
            display = display.child(row(
                "Appearance",
                h_flex()
                    .gap_1()
                    .child(choice("device-tools-light", "Light", false))
                    .child(choice("device-tools-dark", "Dark", true)),
                p,
            ));
        }
        if has("text_size") {
            any_display = true;
            let size = settings.text_size;
            let step = |id: &'static str, icon: IconName, by: i32, tip: &'static str| {
                Button::new(id)
                    .cursor_pointer()
                    .ghost()
                    .xsmall()
                    .icon(icon)
                    .tooltip(tip)
                    // At the smallest or largest already.
                    .disabled(busy || size.is_some_and(|s| step_text_size(Some(s), by) == s))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let size = step_text_size(this.settings().and_then(|s| s.text_size), by);
                        this.set(DeviceSetting::TextSize { size }, cx)
                    }))
            };
            display = display.child(row(
                "Text size",
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(step(
                        "device-tools-text-smaller",
                        IconName::Minus,
                        -1,
                        "Smaller",
                    ))
                    .child(
                        div()
                            .w(px(76.))
                            .text_center()
                            .text_xs()
                            .text_color(p.foreground)
                            .child(size.map_or("Unknown", text_size_label)),
                    )
                    .child(step(
                        "device-tools-text-larger",
                        IconName::Plus,
                        1,
                        "Larger",
                    )),
                p,
            ));
        }
        if has("orientation") {
            any_display = true;
            let now = settings.orientation;
            let mut buttons = h_flex().gap_1().flex_wrap();
            for (orientation, label) in ORIENTATIONS {
                buttons = buttons.child(
                    Button::new(SharedString::from(format!("device-tools-turn-{label}")))
                        .cursor_pointer()
                        .xsmall()
                        .label(label)
                        .selected(now == Some(orientation))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set(DeviceSetting::Orientation { orientation }, cx)
                        })),
                );
            }
            display = display
                .child(
                    div()
                        .text_sm()
                        .text_color(p.foreground)
                        .child("Orientation"),
                )
                .child(buttons);
        }
        if any_display {
            body = body.child(display);
        }

        // Accessibility.
        let switches: [Toggle; 4] = [
            ("contrast", "Increase contrast", settings.contrast, |on| {
                DeviceSetting::Contrast { high: on }
            }),
            (
                "reduce_motion",
                "Reduce motion",
                settings.reduce_motion,
                |on| DeviceSetting::ReduceMotion { on },
            ),
            (
                "screen_reader",
                match self.platform {
                    DevicePlatform::Ios => "VoiceOver",
                    DevicePlatform::Android => "TalkBack",
                },
                settings.screen_reader,
                |on| DeviceSetting::ScreenReader { on },
            ),
            (
                "accessibility_frames",
                "Show accessibility frames",
                settings.accessibility_frames,
                |on| DeviceSetting::AccessibilityFrames { on },
            ),
        ];
        let mut access = section("Accessibility", p);
        let mut any_access = false;
        for (name, label, value, make) in switches {
            if !has(name) {
                continue;
            }
            any_access = true;
            access = access.child(row(
                label,
                Switch::new(SharedString::from(format!("device-tools-{name}")))
                    .checked(value == Some(true))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, on: &bool, _, cx| this.set(make(*on), cx))),
                p,
            ));
        }
        if any_access {
            body = body.child(access);
        }

        if has("network") {
            body = body.child(
                section("Network", p).child(row(
                    "Wi-Fi and mobile data",
                    Switch::new("device-tools-network")
                        .checked(settings.network == Some(true))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, on: &bool, _, cx| {
                            this.set(DeviceSetting::Network { on: *on }, cx)
                        })),
                    p,
                )),
            );
        }

        if has("location") {
            let mut places = h_flex().gap_1().flex_wrap();
            for (name, lat, lon) in PLACES {
                places =
                    places.child(
                        Button::new(SharedString::from(format!("device-tools-place-{name}")))
                            .cursor_pointer()
                            .xsmall()
                            .label(name)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.go_to(lat, lon, window, cx)
                            })),
                    );
            }
            body = body.child(
                section("Location", p)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(div().flex_1().child(Input::new(&self.latitude).small()))
                            .child(div().flex_1().child(Input::new(&self.longitude).small()))
                            .child(
                                Button::new("device-tools-locate")
                                    .cursor_pointer()
                                    .small()
                                    .icon(IconName::MapPin)
                                    .tooltip("Set the location")
                                    .on_click(cx.listener(|this, _, _, cx| this.set_location(cx))),
                            ),
                    )
                    .child(places),
            );
        }

        let no_apps = settings.apps.is_empty();
        if has("permission") {
            let app = self
                .app_for(&self.perm_app)
                .unwrap_or_else(|| "No apps".into());
            let grant = |id: &'static str, label: &'static str, on: bool| {
                Button::new(id)
                    .cursor_pointer()
                    .small()
                    .label(label)
                    .disabled(busy || no_apps)
                    .on_click(cx.listener(move |this, _, _, cx| this.grant(on, cx)))
            };
            body = body.child(
                section("Permissions", p)
                    .child(
                        menu_button("device-tools-perm-app", app, &self.perm_app_at, p).on_click(
                            cx.listener(|this, _, window, cx| this.app_menu(false, window, cx)),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                menu_button(
                                    "device-tools-permission",
                                    self.permission.to_owned(),
                                    &self.permission_at,
                                    p,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.permission_menu(window, cx),
                                )),
                            )
                            .child(grant("device-tools-grant", "Grant", true))
                            .child(grant("device-tools-revoke", "Revoke", false)),
                    ),
            );
        }

        if has("push") {
            let app = self
                .app_for(&self.push_app)
                .unwrap_or_else(|| "No apps".into());
            body = body.child(
                section("Test notification", p)
                    .child(
                        menu_button("device-tools-push-app", app, &self.push_app_at, p).on_click(
                            cx.listener(|this, _, window, cx| this.app_menu(true, window, cx)),
                        ),
                    )
                    .child(Input::new(&self.push_title).small())
                    .child(Input::new(&self.push_body).small())
                    .child(
                        h_flex().justify_end().child(
                            Button::new("device-tools-push")
                                .cursor_pointer()
                                .small()
                                .icon(IconName::Bell)
                                .label("Send")
                                .disabled(busy || no_apps)
                                .on_click(cx.listener(|this, _, _, cx| this.send_push(cx))),
                        ),
                    ),
            );
        }

        if settings.supported.is_empty() {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child("This device's settings can't be changed from here yet."),
            );
        }
        body.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_push_is_an_apns_alert() {
        assert_eq!(
            push_payload("Hi", "There"),
            serde_json::json!({ "aps": { "alert": { "title": "Hi", "body": "There" } } })
        );
    }

    #[test]
    fn locations_are_degrees_in_range() {
        assert_eq!(parse_location(" 51.5 ", "-0.12"), Ok((51.5, -0.12)));
        assert!(parse_location("91", "0").is_err());
        assert!(parse_location("0", "-181").is_err());
        assert!(parse_location("north", "0").is_err());
        assert!(parse_location("", "0").is_err());
    }

    #[test]
    fn text_size_steps_stay_within_the_sizes() {
        use DeviceTextSize::*;
        assert_eq!(step_text_size(Some(Default), 1), Large);
        assert_eq!(step_text_size(Some(Default), -1), Small);
        assert_eq!(step_text_size(Some(Largest), 1), Largest);
        assert_eq!(step_text_size(Some(ExtraSmall), -1), ExtraSmall);
        // Not known yet: from the default.
        assert_eq!(step_text_size(None, 1), Large);
    }

    #[test]
    fn android_permissions_get_their_full_names() {
        assert_eq!(
            permission_name(DevicePlatform::Android, "CAMERA"),
            "android.permission.CAMERA"
        );
        assert_eq!(
            permission_name(DevicePlatform::Android, "com.example.MINE"),
            "com.example.MINE"
        );
        assert_eq!(permission_name(DevicePlatform::Ios, "photos"), "photos");
        assert!(permissions(DevicePlatform::Ios).contains(&"camera"));
        assert!(permissions(DevicePlatform::Android).contains(&"POST_NOTIFICATIONS"));
    }
}
