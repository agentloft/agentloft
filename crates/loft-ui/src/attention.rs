//! Getting the user's attention while they work elsewhere: the Dock badge
//! counts threads waiting for approval, and the Dock icon bounces when a
//! turn finishes or needs approval while Agentloft is in the background.
//! (Notification banners need a signed bundle; they come with packaging.)

/// Show `count` on the Dock icon (`0` clears it).
pub fn set_badge(count: usize) {
    platform::set_badge(count);
}

/// Bounce the Dock icon once if the app isn't frontmost.
pub fn request(cx: &gpui_kit::App) {
    if cx.active_window().is_none() {
        platform::bounce();
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
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn set_badge(_: usize) {}
    pub fn bounce() {}
}
