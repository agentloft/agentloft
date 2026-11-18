//! A running emulator's settings: set through adb (`settings`, `cmd`,
//! `svc`, `pm`) or its control service, and read back from the device in
//! one adb call, so the panel shows what is really set.

use super::input::Turn;
use super::{AndroidBackend, fold, grpc, lock, sensor_turn, serial};
use anyhow::{anyhow, bail};
use loft_protocol::{
    DeviceOrientation, DevicePosture, DeviceSetting, DeviceSettings, DeviceTextSize,
};
use std::collections::HashMap;
use std::time::Duration;

const TALKBACK: &str = "com.google.android.marvin.talkback";
const TALKBACK_SERVICE: &str =
    "com.google.android.marvin.talkback/com.google.android.marvin.talkback.TalkBackService";
/// `IBinder.SYSPROPS_TRANSACTION`: asks every app to read system
/// properties again, so `debug.layout` shows at once. (The call's reply is
/// an error parcel; the apps still redraw.)
const SYSPROPS_TRANSACTION: &str = "1599295570";
const ANIMATION_SCALES: [&str; 3] = [
    "animator_duration_scale",
    "transition_animation_scale",
    "window_animation_scale",
];

/// One device shell line reading everything [`parse`] needs: `key=value`
/// lines, then the user's packages (`package:<name>`).
const READ: &str = "echo \"night=$(cmd uimode night)\"; \
     echo \"font=$(settings get system font_scale)\"; \
     echo \"contrast=$(settings get secure high_text_contrast_enabled)\"; \
     echo \"anim=$(settings get global animator_duration_scale) \
        $(settings get global transition_animation_scale) \
        $(settings get global window_animation_scale)\"; \
     echo \"a11y=$(settings get secure accessibility_enabled)\"; \
     echo \"services=$(settings get secure enabled_accessibility_services)\"; \
     echo \"talkback=$(pm path com.google.android.marvin.talkback 2>/dev/null | head -n 1)\"; \
     echo \"layout=$(getprop debug.layout)\"; \
     echo \"wifi=$(settings get global wifi_on)\"; \
     echo \"data=$(settings get global mobile_data)\"; \
     echo \"top=$(dumpsys activity activities 2>/dev/null | grep -m 1 -E 'topResumedActivity|mResumedActivity')\"; \
     pm list packages -3";

/// The font scale for a text size: Android's own steps (0.85, 1, 1.15,
/// 1.3 in every version; 14 added 1.5 to 2), with one below for the
/// smallest.
pub fn font_scale(size: DeviceTextSize) -> &'static str {
    match size {
        DeviceTextSize::ExtraSmall => "0.8",
        DeviceTextSize::Small => "0.85",
        DeviceTextSize::Default => "1.0",
        DeviceTextSize::Large => "1.15",
        DeviceTextSize::ExtraLarge => "1.3",
        DeviceTextSize::Largest => "2.0",
    }
}

/// The text size nearest a font scale, however it was set.
pub fn text_size(scale: f32) -> DeviceTextSize {
    match scale {
        s if s < 0.825 => DeviceTextSize::ExtraSmall,
        s if s < 0.925 => DeviceTextSize::Small,
        s if s < 1.075 => DeviceTextSize::Default,
        s if s < 1.225 => DeviceTextSize::Large,
        s if s < 1.65 => DeviceTextSize::ExtraLarge,
        _ => DeviceTextSize::Largest,
    }
}

/// How the rotation sensor turns the device, as the interface's direction.
/// Landscape left is turned a quarter counter-clockwise (its top on the
/// left), as iOS names it.
pub fn orientation(turn: Turn) -> DeviceOrientation {
    match turn {
        Turn::Portrait => DeviceOrientation::Portrait,
        Turn::Left => DeviceOrientation::LandscapeLeft,
        Turn::UpsideDown => DeviceOrientation::PortraitUpsideDown,
        Turn::Right => DeviceOrientation::LandscapeRight,
    }
}

pub fn turn(orientation: DeviceOrientation) -> Turn {
    match orientation {
        DeviceOrientation::Portrait => Turn::Portrait,
        DeviceOrientation::LandscapeLeft => Turn::Left,
        DeviceOrientation::PortraitUpsideDown => Turn::UpsideDown,
        DeviceOrientation::LandscapeRight => Turn::Right,
    }
}

/// `CAMERA` is `android.permission.CAMERA`; a dotted name is used as is.
pub fn permission_name(permission: &str) -> String {
    let p = permission.trim();
    if p.contains('.') {
        p.to_owned()
    } else {
        format!("android.permission.{}", p.to_ascii_uppercase())
    }
}

/// The title and body of an APNs payload's `aps.alert` (a string is the
/// body), else of a plain `{title, body}`.
pub fn push_text(payload: &serde_json::Value) -> (String, String) {
    let alert = payload.pointer("/aps/alert").unwrap_or(payload);
    let field = |k: &str| {
        alert
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    match alert.as_str() {
        Some(body) => (String::new(), body.to_owned()),
        None => (field("title"), field("body")),
    }
}

/// The package of `topResumedActivity=ActivityRecord{57eb82b u0
/// com.example/.Main t7}`.
pub fn foreground_package(line: &str) -> Option<String> {
    line.split_whitespace()
        .find_map(|word| word.split_once('/').map(|(pkg, _)| pkg))
        .filter(|pkg| !pkg.is_empty() && pkg.contains('.'))
        .map(str::to_owned)
}

/// The accessibility services list with TalkBack added or taken out.
pub fn with_talkback(services: &str, on: bool) -> String {
    let mut list: Vec<&str> = services
        .split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "null")
        .filter(|s| !s.starts_with(&format!("{TALKBACK}/")))
        .collect();
    if on {
        list.push(TALKBACK_SERVICE);
    }
    list.join(":")
}

/// What [`READ`] printed, as settings. `talkback` and `orientation` come
/// from elsewhere.
pub fn parse(out: &str) -> (DeviceSettings, bool) {
    let mut values = HashMap::new();
    let mut apps = Vec::new();
    for line in out.lines().map(str::trim) {
        if let Some(pkg) = line.strip_prefix("package:") {
            apps.push(pkg.trim().to_owned());
        } else if let Some((k, v)) = line.split_once('=') {
            values
                .entry(k.to_owned())
                .or_insert_with(|| v.trim().to_owned());
        }
    }
    apps.sort();
    let get = |k: &str| {
        values
            .get(k)
            .map(String::as_str)
            .filter(|v| !v.is_empty() && *v != "null")
    };
    let flag = |k: &str| get(k).map(|v| v != "0");
    let dark = get("night").and_then(|v| {
        // "Night mode: yes" (or "no", "auto", "custom").
        match v.rsplit(':').next().map(str::trim) {
            Some("yes") => Some(true),
            Some("no") => Some(false),
            _ => None,
        }
    });
    let scale = get("font")
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(1.0);
    let reduce_motion = values.get("anim").map(|v| {
        let scales: Vec<f32> = v
            .split_whitespace()
            .map(|s| s.parse().unwrap_or(1.0))
            .collect();
        !scales.is_empty() && scales.iter().all(|s| *s == 0.0)
    });
    let talkback = get("talkback").is_some_and(|v| v.starts_with("package:"));
    let screen_reader = talkback.then(|| {
        flag("a11y") == Some(true)
            && get("services").is_some_and(|s| {
                s.split(':')
                    .any(|s| s.trim().starts_with(&format!("{TALKBACK}/")))
            })
    });
    let wifi = flag("wifi");
    let data = flag("data");
    let network = match (wifi, data) {
        (None, None) => None,
        (w, d) => Some(w == Some(true) || d == Some(true)),
    };
    let settings = DeviceSettings {
        supported: Vec::new(),
        dark,
        text_size: Some(text_size(scale)),
        contrast: Some(flag("contrast") == Some(true)),
        reduce_motion,
        screen_reader,
        accessibility_frames: Some(get("layout") == Some("true")),
        network,
        orientation: None,
        foreground_app: get("top").and_then(foreground_package),
        apps,
    };
    (settings, talkback)
}

impl AndroidBackend {
    /// Change one of a running emulator's settings (not location, not the
    /// fold: `set` does those).
    pub(super) async fn set_setting(
        &self,
        native: &str,
        running: &super::Running,
        setting: DeviceSetting,
    ) -> anyhow::Result<()> {
        let serial = serial(running)?;
        let sh = |words: Vec<String>| {
            let serial = serial.clone();
            async move {
                let words: Vec<&str> = words.iter().map(String::as_str).collect();
                self.adb.shell(&serial, &words).await
            }
        };
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match setting {
            DeviceSetting::Appearance { dark } => {
                sh(words(&[
                    "cmd",
                    "uimode",
                    "night",
                    if dark { "yes" } else { "no" },
                ]))
                .await?;
            }
            DeviceSetting::TextSize { size } => {
                sh(words(&[
                    "settings",
                    "put",
                    "system",
                    "font_scale",
                    font_scale(size),
                ]))
                .await?;
            }
            DeviceSetting::Contrast { high } => {
                sh(words(&[
                    "settings",
                    "put",
                    "secure",
                    "high_text_contrast_enabled",
                    if high { "1" } else { "0" },
                ]))
                .await?;
            }
            DeviceSetting::ReduceMotion { on } => {
                // Android's "Remove animations": every animation scale at 0.
                for scale in ANIMATION_SCALES {
                    sh(words(&[
                        "settings",
                        "put",
                        "global",
                        scale,
                        if on { "0" } else { "1" },
                    ]))
                    .await?;
                }
            }
            DeviceSetting::ScreenReader { on } => {
                let has = sh(words(&["pm", "path", TALKBACK])).await;
                if !has.is_ok_and(|out| out.contains("package:")) {
                    bail!("This device's system image has no TalkBack.");
                }
                let services = sh(words(&[
                    "settings",
                    "get",
                    "secure",
                    "enabled_accessibility_services",
                ]))
                .await?;
                let list = with_talkback(services.trim(), on);
                if list.is_empty() {
                    sh(words(&[
                        "settings",
                        "delete",
                        "secure",
                        "enabled_accessibility_services",
                    ]))
                    .await?;
                } else {
                    sh(words(&[
                        "settings",
                        "put",
                        "secure",
                        "enabled_accessibility_services",
                        &list,
                    ]))
                    .await?;
                }
                if on || list.is_empty() {
                    sh(words(&[
                        "settings",
                        "put",
                        "secure",
                        "accessibility_enabled",
                        if on { "1" } else { "0" },
                    ]))
                    .await?;
                }
            }
            DeviceSetting::AccessibilityFrames { on } => {
                sh(words(&[
                    "setprop",
                    "debug.layout",
                    if on { "true" } else { "false" },
                ]))
                .await?;
                // Its reply is an error parcel even when it worked.
                let _ = sh(words(&[
                    "service",
                    "call",
                    "activity",
                    SYSPROPS_TRANSACTION,
                ]))
                .await;
            }
            DeviceSetting::Permission {
                app,
                permission,
                grant,
            } => {
                let permission = permission_name(&permission);
                sh(words(&[
                    "pm",
                    if grant { "grant" } else { "revoke" },
                    &app,
                    &permission,
                ]))
                .await?;
            }
            DeviceSetting::Push { app, payload } => {
                let (title, body) = push_text(&payload);
                if title.is_empty() && body.is_empty() {
                    bail!("The payload has no aps.alert to show.");
                }
                let text = if body.is_empty() { &title } else { &body };
                let mut post = words(&["cmd", "notification", "post"]);
                if !title.is_empty() && !body.is_empty() {
                    post.extend(words(&["-t", &title]));
                }
                post.extend([app.clone(), text.clone()]);
                let out = sh(post).await?;
                if out.contains("Error") || out.contains("Exception") {
                    bail!("posting the notification: {}", out.trim());
                }
            }
            DeviceSetting::Network { on } => {
                let to = if on { "enable" } else { "disable" };
                sh(words(&["svc", "wifi", to])).await?;
                sh(words(&["svc", "data", to])).await?;
            }
            DeviceSetting::Orientation { orientation } => {
                let conn = self.grpc(running).await?;
                set_turn(&conn, turn(orientation)).await?;
                self.screens_of(native, |s| *lock(&s.turn) = turn(orientation));
            }
            DeviceSetting::Location { .. }
            | DeviceSetting::Posture { .. }
            | DeviceSetting::Hinge { .. } => unreachable!("set does these"),
        }
        Ok(())
    }

    /// Fold a running foldable to `posture`, or its hinge to `degrees`.
    pub(super) async fn fold_to(
        &self,
        native: &str,
        running: &super::Running,
        posture: Option<DevicePosture>,
        degrees: Option<f32>,
    ) -> anyhow::Result<()> {
        let config = self
            .sdk
            .avd(native)
            .and_then(|a| a.fold)
            .ok_or_else(|| anyhow!("This device doesn't fold."))?;
        let conn = self.grpc(running).await?;
        let posture = match (posture, degrees) {
            (Some(posture), _) => {
                if !config.takes(posture) {
                    bail!("This device can't be put in the {posture:?} posture.");
                }
                fold::set_posture(&conn, posture).await?;
                posture
            }
            (None, Some(degrees)) => {
                let degrees = degrees.clamp(config.hinge_min, config.hinge_max);
                fold::set_hinge(&conn, degrees).await?;
                config
                    .posture_at(degrees)
                    .ok_or_else(|| anyhow!("This device has no postures."))?
            }
            (None, None) => return Ok(()),
        };
        fold::settle(&conn, posture).await;
        self.screens_of(native, |s| {
            *lock(&s.screen) = config.screen(posture, (s.width, s.height));
        });
        Ok(())
    }

    pub(super) async fn read_settings(&self, native: &str) -> anyhow::Result<DeviceSettings> {
        let running = self.running_or_err(native).await?;
        let serial = serial(&running)?;
        let out = self
            .adb
            .run(Some(&serial), &["shell", READ], Duration::from_secs(30))
            .await?;
        let (mut settings, talkback) = parse(&out);
        let conn = self.grpc(&running).await.ok();
        if let Some(conn) = &conn {
            settings.orientation = sensor_turn(conn).await.ok().map(orientation);
        }
        let foldable = self.sdk.avd(native).is_some_and(|a| a.fold.is_some());
        let mut supported = vec![
            "appearance",
            "location",
            "text_size",
            "contrast",
            "reduce_motion",
        ];
        if talkback {
            supported.push("screen_reader");
        }
        supported.extend(["accessibility_frames", "permission", "push", "network"]);
        if conn.is_some() {
            supported.push("orientation");
            if foldable {
                supported.extend(["posture", "hinge"]);
            }
        }
        settings.supported = supported.into_iter().map(str::to_owned).collect();
        Ok(settings)
    }
}

/// Turn the device with its rotation sensor, as the rotate buttons do.
pub async fn set_turn(conn: &super::Grpc, turn: Turn) -> anyhow::Result<()> {
    conn.unary::<_, grpc::Empty>(
        "setPhysicalModel",
        &grpc::PhysicalModelValue {
            target: grpc::physical::ROTATION,
            status: 0,
            value: Some(grpc::ParameterValue {
                data: vec![0.0, 0.0, turn.degrees()],
            }),
            interpolation: 1,
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_text_sizes_to_font_scales_and_back() {
        for size in [
            DeviceTextSize::ExtraSmall,
            DeviceTextSize::Small,
            DeviceTextSize::Default,
            DeviceTextSize::Large,
            DeviceTextSize::ExtraLarge,
            DeviceTextSize::Largest,
        ] {
            assert_eq!(text_size(font_scale(size).parse().unwrap()), size);
        }
        // Set elsewhere, to Android 14's in-between steps.
        assert_eq!(text_size(1.5), DeviceTextSize::ExtraLarge);
        assert_eq!(text_size(1.8), DeviceTextSize::Largest);
        assert_eq!(text_size(0.9), DeviceTextSize::Small);
    }

    #[test]
    fn reads_settings_back() {
        let out = "night=Night mode: yes\n\
                   font=1.15\n\
                   contrast=1\n\
                   anim=0 0.0 0\n\
                   a11y=1\n\
                   services=com.example/.Svc:com.google.android.marvin.talkback/.TalkBackService\n\
                   talkback=package:/product/app/talkback/talkback.apk\n\
                   layout=true\n\
                   wifi=0\n\
                   data=1\n\
                   top=      topResumedActivity=ActivityRecord{57eb82b u0 com.google.android.apps.maps/com.google.android.maps.MapsActivity t9}\n\
                   package:com.example.b\n\
                   package:com.example.a\n";
        let (s, talkback) = parse(out);
        assert!(talkback);
        assert_eq!(s.dark, Some(true));
        assert_eq!(s.text_size, Some(DeviceTextSize::Large));
        assert_eq!(s.contrast, Some(true));
        assert_eq!(s.reduce_motion, Some(true));
        assert_eq!(s.screen_reader, Some(true));
        assert_eq!(s.accessibility_frames, Some(true));
        assert_eq!(s.network, Some(true));
        assert_eq!(
            s.foreground_app.as_deref(),
            Some("com.google.android.apps.maps")
        );
        assert_eq!(s.apps, vec!["com.example.a", "com.example.b"]);

        // A fresh device: most settings were never written.
        let fresh = "night=Night mode: no\nfont=null\ncontrast=null\nanim=null 1.0 1.0\n\
                     a11y=0\nservices=null\ntalkback=\nlayout=\nwifi=0\ndata=0\n\
                     top=  mResumedActivity: ActivityRecord{1 u0 com.android.settings/.Settings t3}\n";
        let (s, talkback) = parse(fresh);
        assert!(!talkback);
        assert_eq!(s.dark, Some(false));
        assert_eq!(s.text_size, Some(DeviceTextSize::Default));
        assert_eq!(s.contrast, Some(false));
        assert_eq!(s.reduce_motion, Some(false));
        assert_eq!(s.screen_reader, None);
        assert_eq!(s.accessibility_frames, Some(false));
        assert_eq!(s.network, Some(false));
        assert_eq!(s.foreground_app.as_deref(), Some("com.android.settings"));
        assert!(s.apps.is_empty());
    }

    #[test]
    fn edits_the_accessibility_services() {
        assert_eq!(with_talkback("null", true), TALKBACK_SERVICE);
        assert_eq!(
            with_talkback("com.example/.Svc", true),
            format!("com.example/.Svc:{TALKBACK_SERVICE}")
        );
        assert_eq!(
            with_talkback(
                "com.example/.Svc:com.google.android.marvin.talkback/.TalkBackService",
                false
            ),
            "com.example/.Svc"
        );
        assert_eq!(with_talkback(TALKBACK_SERVICE, false), "");
    }

    #[test]
    fn names_permissions_and_push_text() {
        assert_eq!(permission_name("CAMERA"), "android.permission.CAMERA");
        assert_eq!(permission_name("camera"), "android.permission.CAMERA");
        assert_eq!(
            permission_name("android.permission.POST_NOTIFICATIONS"),
            "android.permission.POST_NOTIFICATIONS"
        );
        let apns = serde_json::json!({"aps": {"alert": {"title": "Hi", "body": "There"}}});
        assert_eq!(push_text(&apns), ("Hi".into(), "There".into()));
        let plain = serde_json::json!({"aps": {"alert": "Just text"}});
        assert_eq!(push_text(&plain), (String::new(), "Just text".into()));
        let bare = serde_json::json!({"title": "T", "body": "B"});
        assert_eq!(push_text(&bare), ("T".into(), "B".into()));
    }

    #[test]
    fn maps_orientations() {
        for o in [
            DeviceOrientation::Portrait,
            DeviceOrientation::LandscapeLeft,
            DeviceOrientation::LandscapeRight,
            DeviceOrientation::PortraitUpsideDown,
        ] {
            assert_eq!(orientation(turn(o)), o);
        }
        assert_eq!(orientation(Turn::Left), DeviceOrientation::LandscapeLeft);
    }
}
