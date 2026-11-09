//! Getting the user's attention while they work elsewhere: the Dock badge
//! counts threads waiting for approval; when a turn finishes or needs
//! approval while Agentloft is in the background, the Dock icon bounces
//! and — in the app bundle — a notification banner appears.

/// Show `count` on the Dock icon (`0` clears it).
pub fn set_badge(count: usize) {
    platform::set_badge(count);
}

/// Bounce the Dock icon and post a banner, if the app isn't frontmost.
pub fn request(cx: &gpui_kit::App, title: &str, body: &str, id: &str) {
    if cx.active_window().is_none() {
        platform::bounce();
        platform::notify(title, body, id);
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSRequestUserAttentionType};
    use objc2_foundation::NSString;

    pub fn set_badge(count: usize) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
        NSApplication::sharedApplication(mtm)
            .dockTile()
            .setBadgeLabel(label.as_deref());
    }

    pub fn bounce() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        NSApplication::sharedApplication(mtm)
            .requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
    }

    /// A Notification Center banner. Needs a bundle identifier
    /// (`cargo xtask bundle`); an unbundled dev binary just skips it, since
    /// UNUserNotificationCenter aborts without one.
    pub fn notify(title: &str, body: &str, id: &str) {
        use objc2_foundation::NSBundle;
        use objc2_user_notifications::{
            UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
            UNUserNotificationCenter,
        };
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            return;
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        static ASKED: std::sync::Once = std::sync::Once::new();
        ASKED.call_once(|| {
            let done = block2::RcBlock::new(
                |_granted: objc2::runtime::Bool, _err: *mut objc2_foundation::NSError| {},
            );
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &done,
            );
        });
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        // One banner per thread: a newer one replaces the older.
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(id),
            &content,
            None,
        );
        center.addNotificationRequest_withCompletionHandler(&request, None);
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn set_badge(_: usize) {}
    pub fn bounce() {}
    pub fn notify(_: &str, _: &str, _: &str) {}
}
