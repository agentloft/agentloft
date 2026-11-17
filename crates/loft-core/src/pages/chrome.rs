//! Finding a Chromium-family browser on this machine. Agentloft doesn't
//! ship one: it runs the one that is installed, headless, with a profile of
//! its own.

use std::path::{Path, PathBuf};

/// What the user is told when there is none.
pub const MISSING: &str = "The browser needs Google Chrome, Chromium, Microsoft Edge or Brave installed on this computer.";

/// The browser to run: the configured one, else the first installed.
pub fn find(configured: Option<&str>) -> Result<PathBuf, String> {
    if let Some(path) = configured.map(str::trim).filter(|p| !p.is_empty()) {
        let path = loft_config::expand_tilde(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!("No browser at {}.", path.display()))
        };
    }
    candidates()
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| MISSING.to_owned())
}

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        let apps = [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
            "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        ];
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(home) = std::env::home_dir() {
            roots.push(home.join("Applications"));
        }
        for root in &roots {
            out.extend(apps.iter().map(|app| root.join(app)));
        }
    } else if cfg!(windows) {
        let apps = [
            r"Google\Chrome\Application\chrome.exe",
            r"Microsoft\Edge\Application\msedge.exe",
            r"BraveSoftware\Brave-Browser\Application\brave.exe",
            r"Chromium\Application\chrome.exe",
        ];
        let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .iter()
            .filter_map(|var| std::env::var_os(var).map(PathBuf::from))
            .collect();
        for app in apps {
            out.extend(roots.iter().map(|root| root.join(app)));
        }
    } else {
        let names = [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ];
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            out.extend(names.iter().map(|name| dir.join(name)));
        }
    }
    out
}

/// The command line for a headless browser that speaks the DevTools
/// protocol on file descriptors 3 and 4 and nowhere else: no debugging
/// port that another program on the machine could connect to.
///
/// `sound`: tabs may play sound, each muted or not by Agentloft's sound
/// extension (`sound.rs`), which the pipe may then load. Without it the
/// whole browser is muted: sound would come out of a computer nobody may
/// be sitting at.
pub fn args(profile: &Path, sound: bool) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--headless=new".into(),
        "--remote-debugging-pipe".into(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        // Its own cookie store key, so macOS never asks for the keychain.
        "--use-mock-keychain".into(),
        "--password-store=basic".into(),
        "--disable-features=Translate,MediaRouter,OptimizationHints".into(),
        "--disable-background-networking".into(),
        "--disable-sync".into(),
        "--disable-component-update".into(),
        "--disable-default-apps".into(),
        "--disable-search-engine-choice-screen".into(),
        // Nothing of the browser's own chrome over or beside the page: no
        // "restore pages?" after it was stopped, no notice bars.
        "--hide-crash-restore-bubble".into(),
        "--disable-infobars".into(),
        "--disable-prompt-on-repost".into(),
        // Nobody sees its window, but its pages are being looked at: they
        // keep their timers and animations.
        "--disable-renderer-backgrounding".into(),
        "--disable-backgrounding-occluded-windows".into(),
        "--disable-background-timer-throttling".into(),
        "--window-size=1280,800".into(),
        // Drawn at a dense screen's density. A page's own scale
        // (`Emulation.setDeviceMetricsOverride`) sets what it lays out and
        // screenshots at, but a headless screencast is drawn at the
        // browser's: without this, one pixel per CSS pixel, which on a
        // Retina display is a blurred page while it moves.
        "--force-device-scale-factor=2".into(),
        // A person uses these pages: they don't say a program drives them
        // (`navigator.webdriver`), as a pipe-driven browser otherwise does.
        // YouTube stops a video after about 40 seconds for one that says so.
        "--disable-blink-features=AutomationControlled".into(),
    ];
    args.push(if sound {
        "--enable-unsafe-extension-debugging".into()
    } else {
        "--mute-audio".into()
    });
    args.push("about:blank".into());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_browser_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("chrome");
        assert!(find(Some(&exe.to_string_lossy())).is_err());
        std::fs::write(&exe, "").unwrap();
        assert_eq!(find(Some(&exe.to_string_lossy())).unwrap(), exe);
        // Blank means "find one".
        assert_eq!(find(Some("  ")).is_ok(), find(None).is_ok());
    }

    #[test]
    fn the_browser_listens_on_a_pipe_only() {
        for sound in [true, false] {
            let args = args(Path::new("/tmp/profile"), sound);
            assert!(args.iter().any(|a| a == "--remote-debugging-pipe"));
            assert!(!args.iter().any(|a| a.contains("remote-debugging-port")));
            assert!(args.iter().any(|a| a == "--user-data-dir=/tmp/profile"));
            // Muted whole, unless tabs are muted one by one.
            assert_eq!(args.iter().any(|a| a == "--mute-audio"), !sound);
            // Pages don't say a program drives them.
            assert!(
                args.iter()
                    .any(|a| a == "--disable-blink-features=AutomationControlled")
            );
        }
    }
}
