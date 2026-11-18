//! A real iOS simulator through the daemon's backend and its helper
//! (`agentloftd simulator`): boot, pictures, a swipe, Home, turning, a
//! capture, shutdown; and settings read back, permissions, a push, the app
//! in front, an orientation, accessibility frames, a pinch in Maps and three
//! fingers on a web page. Needs Xcode with an iOS
//! runtime; takes a minute or two.
//!
//! `cargo test -p loftd --test ios_live -- --ignored --nocapture`
//!
//! It makes its own iPhone in a device set of its own (not the user's
//! simulators, which may be in use) and deletes it afterwards.

#![cfg(target_os = "macos")]

use loft_core::devices::ios::{Helper, IosBackend};
use loft_core::devices::{Backend, Picture, Screen};
use loft_protocol::{
    DeviceApp, DeviceButton, DeviceInput, DeviceOrientation, DeviceSetting, DeviceState,
    DeviceTextSize, TouchPhase, TouchPoint,
};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A device set in a folder of its own, with one new iPhone; shut down and
/// deleted when dropped.
struct Sandbox {
    set: PathBuf,
    udid: String,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let set = PathBuf::from(format!(
            "/private/tmp/loft-ios-live-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&set).unwrap();
        let (device_type, runtime) = newest_iphone();
        let out = Command::new("xcrun")
            .args(["simctl", "--set"])
            .arg(&set)
            .args(["create", "Loft Live iPhone", &device_type, &runtime])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "simctl create: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let udid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Self { set, udid }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for args in [["shutdown", "all"], ["delete", "all"]] {
            let _ = Command::new("xcrun")
                .args(["simctl", "--set"])
                .arg(&self.set)
                .args(args)
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.set);
    }
}

/// The newest iOS runtime and an iPhone it supports.
fn newest_iphone() -> (String, String) {
    let out = Command::new("xcrun")
        .args(["simctl", "list", "-j", "runtimes"])
        .output()
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let runtime = list["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["platform"] == "iOS" && r["isAvailable"] == true)
        .max_by(|a, b| {
            let v = |r: &serde_json::Value| {
                r["version"]
                    .as_str()
                    .unwrap_or("0")
                    .split('.')
                    .map(|n| n.parse::<u32>().unwrap_or(0))
                    .collect::<Vec<_>>()
            };
            v(a).cmp(&v(b))
        })
        .expect("an iOS runtime");
    let iphone = runtime["supportedDeviceTypes"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|t| {
            t["productFamily"] == "iPhone" && t["name"].as_str().is_some_and(|n| n.contains("Pro"))
        })
        .or_else(|| {
            runtime["supportedDeviceTypes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["productFamily"] == "iPhone")
        })
        .expect("an iPhone");
    (
        iphone["identifier"].as_str().unwrap().to_string(),
        runtime["identifier"].as_str().unwrap().to_string(),
    )
}

fn simulator_app_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "Simulator"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn decode(picture: &Picture) -> image::RgbImage {
    image::load_from_memory(&picture.jpeg).unwrap().to_rgb8()
}

/// How different two pictures are: the mean difference of their pixels, 0
/// to 255 (pictures of different sizes are as different as can be).
fn difference(a: &image::RgbImage, b: &image::RgbImage) -> f64 {
    if a.dimensions() != b.dimensions() {
        return 255.0;
    }
    let total: u64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(x, y)| x.abs_diff(*y) as u64)
        .sum();
    total as f64 / a.as_raw().len() as f64
}

/// The newest picture.
fn latest(screen: &Arc<dyn Screen>) -> Arc<Picture> {
    screen.pictures().borrow().clone().expect("a picture")
}

/// Wait until the screen stops changing (no new picture for `quiet`).
async fn settle(screen: &Arc<dyn Screen>, quiet: Duration, at_most: Duration) {
    let mut pictures = screen.pictures();
    let deadline = Instant::now() + at_most;
    while Instant::now() < deadline {
        if tokio::time::timeout(quiet, pictures.changed())
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Wait for a picture that differs from `before` by more than `by`;
/// returns when it came.
async fn changed_from(
    screen: &Arc<dyn Screen>,
    before: &image::RgbImage,
    by: f64,
    at_most: Duration,
) -> Option<Instant> {
    let mut pictures = screen.pictures();
    let deadline = Instant::now() + at_most;
    loop {
        let picture = pictures.borrow_and_update().clone();
        if let Some(p) = picture
            && difference(&decode(&p), before) > by
        {
            return Some(Instant::now());
        }
        let left = deadline.checked_duration_since(Instant::now())?;
        tokio::time::timeout(left, pictures.changed())
            .await
            .ok()?
            .ok()?;
    }
}

async fn touch(screen: &Arc<dyn Screen>, phase: TouchPhase, x: f32, y: f32) {
    screen
        .input(DeviceInput::Touch { phase, x, y })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real iOS simulator (needs Xcode)"]
async fn live_an_iphone_simulator() {
    let simulator_before = simulator_app_running();
    let sandbox = Sandbox::new("basics");
    let helper = Helper::agentloftd(PathBuf::from(env!("CARGO_BIN_EXE_agentloftd")));
    let backend =
        IosBackend::new(helper, loft_config::process_env()).with_device_set(sandbox.set.clone());

    backend
        .support()
        .await
        .unwrap()
        .expect("iOS simulators are supported");
    let find = |list: &[loft_protocol::DeviceInfo]| {
        list.iter()
            .find(|d| d.id.native() == sandbox.udid)
            .cloned()
            .expect("the new iPhone is listed")
    };
    let listed = find(&backend.list().await.unwrap());
    assert_eq!(listed.state, DeviceState::Stopped);
    assert!(listed.os.starts_with("iOS "), "{}", listed.os);
    assert!(listed.width > 0 && listed.scale >= 2.0, "{listed:?}");

    let started = Instant::now();
    backend.boot(&sandbox.udid).await.unwrap();
    eprintln!("booted in {:.1}s", started.elapsed().as_secs_f64());
    assert_eq!(
        find(&backend.list().await.unwrap()).state,
        DeviceState::Running
    );
    assert!(
        !simulator_app_running() || simulator_before,
        "booting opened Simulator.app"
    );

    let started = Instant::now();
    let screen = backend.attach(&sandbox.udid).await.unwrap();
    eprintln!(
        "attached in {:.0} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    let (width, height, scale) = screen.size().expect("the screen's size");
    assert_eq!(
        (width, height, scale),
        (listed.width, listed.height, listed.scale)
    );
    screen.watching(true);
    let mut pictures = screen.pictures();
    tokio::time::timeout(Duration::from_secs(10), pictures.wait_for(|p| p.is_some()))
        .await
        .expect("a first picture")
        .unwrap();
    let first = latest(&screen);
    assert!(first.width <= width && first.height <= height && first.height > first.width);
    // A freshly booted phone animates its home screen for a while.
    settle(
        &screen,
        Duration::from_millis(1500),
        Duration::from_secs(60),
    )
    .await;
    let home = decode(&latest(&screen));

    // Swipe left across the home screen (a real drag): the App Library
    // slides in.
    let mut frames = screen.pictures();
    let sent = Instant::now();
    touch(&screen, TouchPhase::Down, 0.85, 0.45).await;
    for step in 1..=12 {
        tokio::time::sleep(Duration::from_millis(16)).await;
        touch(&screen, TouchPhase::Move, 0.85 - 0.06 * step as f32, 0.45).await;
    }
    touch(&screen, TouchPhase::Up, 0.13, 0.45).await;
    let moved = changed_from(&screen, &home, 8.0, Duration::from_secs(5))
        .await
        .expect("the swipe changed the screen");
    eprintln!(
        "swipe: the screen changed {:.0} ms after the touch began",
        (moved - sent).as_secs_f64() * 1000.0
    );
    // Frames while it animates.
    let counted = Instant::now();
    let mut count = 0;
    while counted.elapsed() < Duration::from_millis(600) {
        if tokio::time::timeout(Duration::from_millis(600), frames.changed())
            .await
            .is_err()
        {
            break;
        }
        count += 1;
    }
    eprintln!(
        "{count} pictures in the {:.0} ms after the swipe",
        counted.elapsed().as_secs_f64() * 1000.0
    );
    settle(&screen, Duration::from_millis(800), Duration::from_secs(10)).await;
    let swiped = decode(&latest(&screen));
    assert!(
        difference(&swiped, &home) > 8.0,
        "the swipe left the home screen as it was"
    );

    // Home goes back to the first page.
    screen
        .input(DeviceInput::Button {
            button: DeviceButton::Home,
        })
        .await
        .unwrap();
    changed_from(&screen, &swiped, 8.0, Duration::from_secs(5))
        .await
        .expect("Home changed the screen");
    settle(&screen, Duration::from_millis(800), Duration::from_secs(10)).await;
    let back = decode(&latest(&screen));
    assert!(
        difference(&back, &home) < difference(&swiped, &home),
        "Home didn't go back towards the home screen"
    );

    // A capture is the whole screen, as a PNG.
    let png = screen.capture().await.unwrap();
    let captured = image::load_from_memory(&png).unwrap();
    assert_eq!((captured.width(), captured.height()), (width, height));

    // Turning: Safari turns with the device; the pictures and the size do.
    backend
        .app(
            &sandbox.udid,
            DeviceApp::Launch {
                app: "com.apple.mobilesafari".into(),
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    screen
        .input(DeviceInput::Button {
            button: DeviceButton::RotateLeft,
        })
        .await
        .unwrap();
    let turned = Instant::now();
    while screen.size().is_some_and(|(w, h, _)| w < h) && turned.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        screen.size().map(|(w, h, _)| (w, h)),
        Some((height, width)),
        "Safari didn't turn"
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        pictures.wait_for(|p| p.as_ref().is_some_and(|p| p.width > p.height)),
    )
    .await
    .expect("a turned picture")
    .unwrap();
    screen
        .input(DeviceInput::Button {
            button: DeviceButton::RotateRight,
        })
        .await
        .unwrap();

    // Shutting down ends the screen.
    let mut ended = screen.ended();
    backend.shutdown(&sandbox.udid).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), ended.wait_for(|e| e.is_some()))
        .await
        .expect("the screen ended")
        .unwrap();
    eprintln!("ended: {:?}", screen.ended().borrow().clone());
    assert_eq!(
        find(&backend.list().await.unwrap()).state,
        DeviceState::Stopped
    );
    drop(screen);
    assert!(
        !simulator_app_running() || simulator_before,
        "Simulator.app was opened"
    );
}

/// What `privacy` left in the device's TCC database for `service` and
/// `app`: 2 allowed, 0 denied, `None` no answer yet.
fn tcc(sandbox: &Sandbox, service: &str, app: &str) -> Option<i64> {
    let db = sandbox
        .set
        .join(&sandbox.udid)
        .join("data/Library/TCC/TCC.db");
    let out = Command::new("/usr/bin/sqlite3")
        .arg(&db)
        .arg(format!(
            "select auth_value from access where service = '{service}' and client = '{app}'"
        ))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// A page that turns green when three fingers touch it at once.
const THREE_FINGERS: &str = r#"<!doctype html><meta charset="utf-8">
<meta name="viewport" content="width=device-width,user-scalable=no">
<body style="height:100vh;margin:0;touch-action:none;background:#fff">
<script>
for (const kind of ["touchstart", "touchmove"]) {
  document.addEventListener(kind, (e) => {
    e.preventDefault();
    if (e.touches.length === 3) document.body.style.background = "rgb(0,200,0)";
  }, { passive: false });
}
</script>"#;

/// Serve `html` on this computer (the simulator shares its network) and
/// return its address. Each connection is answered on a thread of its own.
fn serve(html: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}/", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                    html.len()
                );
            });
        }
    });
    address
}

/// The share of a picture's pixels in the outlines' pink.
fn pinkness(picture: &image::RgbImage) -> f64 {
    let pink = picture
        .pixels()
        .filter(|p| {
            let [r, g, b] = p.0;
            r > 220 && g < 110 && (90..200).contains(&b)
        })
        .count();
    pink as f64 / (picture.width() * picture.height()) as f64
}

/// The share of a picture's pixels that are near white.
fn whiteness(picture: &image::RgbImage) -> f64 {
    let white = picture
        .pixels()
        .filter(|p| p.0.iter().all(|&c| c > 235))
        .count();
    white as f64 / (picture.width() * picture.height()) as f64
}

async fn tap(screen: &Arc<dyn Screen>, x: f32, y: f32) {
    touch(screen, TouchPhase::Down, x, y).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    touch(screen, TouchPhase::Up, x, y).await;
}

async fn fingers(screen: &Arc<dyn Screen>, phase: TouchPhase, points: [(f32, f32); 2]) {
    screen
        .input(DeviceInput::Touches {
            phase,
            touches: points.iter().map(|&(x, y)| TouchPoint { x, y }).collect(),
        })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real iOS simulator (needs Xcode)"]
async fn live_settings_and_fingers() {
    let sandbox = Sandbox::new("settings");
    let helper = Helper::agentloftd(PathBuf::from(env!("CARGO_BIN_EXE_agentloftd")));
    let backend =
        IosBackend::new(helper, loft_config::process_env()).with_device_set(sandbox.set.clone());
    let udid = sandbox.udid.as_str();
    backend.boot(udid).await.unwrap();
    let screen = backend.attach(udid).await.unwrap();
    screen.watching(true);
    let mut pictures = screen.pictures();
    tokio::time::timeout(Duration::from_secs(10), pictures.wait_for(|p| p.is_some()))
        .await
        .expect("a first picture")
        .unwrap();

    let read = backend.settings(udid).await.unwrap();
    eprintln!("settings at first: {read:?}");
    for name in [
        "appearance",
        "text_size",
        "contrast",
        "reduce_motion",
        "orientation",
    ] {
        assert!(
            read.supported.iter().any(|s| s == name),
            "{name} unsupported"
        );
    }
    for name in ["screen_reader", "network", "posture"] {
        assert!(
            !read.supported.iter().any(|s| s == name),
            "{name} supported"
        );
    }
    assert_eq!(read.dark, Some(false));
    assert_eq!(read.text_size, Some(DeviceTextSize::Default));
    assert_eq!(read.contrast, Some(false));
    assert_eq!(read.reduce_motion, Some(false));
    assert_eq!(read.orientation, Some(DeviceOrientation::Portrait));
    assert!(
        read.apps.is_empty(),
        "a new iPhone has no apps of the user's"
    );

    // Each setting, set and read back from the device.
    let set = |setting| backend.set(udid, setting);
    set(DeviceSetting::Appearance { dark: true }).await.unwrap();
    set(DeviceSetting::TextSize {
        size: DeviceTextSize::Largest,
    })
    .await
    .unwrap();
    set(DeviceSetting::Contrast { high: true }).await.unwrap();
    set(DeviceSetting::ReduceMotion { on: true }).await.unwrap();
    let read = backend.settings(udid).await.unwrap();
    assert_eq!(read.dark, Some(true));
    assert_eq!(read.text_size, Some(DeviceTextSize::Largest));
    assert_eq!(read.contrast, Some(true));
    assert_eq!(read.reduce_motion, Some(true));
    set(DeviceSetting::Appearance { dark: false })
        .await
        .unwrap();
    set(DeviceSetting::TextSize {
        size: DeviceTextSize::Small,
    })
    .await
    .unwrap();
    set(DeviceSetting::Contrast { high: false }).await.unwrap();
    set(DeviceSetting::ReduceMotion { on: false })
        .await
        .unwrap();
    let read = backend.settings(udid).await.unwrap();
    assert_eq!(read.dark, Some(false));
    assert_eq!(read.text_size, Some(DeviceTextSize::Small));
    assert_eq!(read.contrast, Some(false));
    assert_eq!(read.reduce_motion, Some(false));
    let unsupported = set(DeviceSetting::Network { on: false }).await.unwrap_err();
    eprintln!("network: {unsupported:#}");

    // A permission of a built-in app, granted and taken back.
    let permission = |grant| DeviceSetting::Permission {
        app: "com.apple.mobilesafari".into(),
        permission: "photos".into(),
        grant,
    };
    set(permission(true)).await.unwrap();
    assert_eq!(
        tcc(&sandbox, "kTCCServicePhotos", "com.apple.mobilesafari"),
        Some(2)
    );
    set(permission(false)).await.unwrap();
    assert_eq!(
        tcc(&sandbox, "kTCCServicePhotos", "com.apple.mobilesafari"),
        Some(0)
    );

    // A push to a built-in app.
    set(DeviceSetting::Push {
        app: "com.apple.Preferences".into(),
        payload: serde_json::json!({"aps": {"alert": {"title": "Agentloft", "body": "A test push"}}}),
    })
    .await
    .unwrap();

    // The app in front: the home screen, then Safari.
    let read = backend.settings(udid).await.unwrap();
    assert_eq!(
        read.foreground_app.as_deref(),
        Some("com.apple.springboard")
    );
    backend
        .app(
            udid,
            DeviceApp::Launch {
                app: "com.apple.mobilesafari".into(),
            },
        )
        .await
        .unwrap();
    let launched = Instant::now();
    let front = loop {
        let front = backend.settings(udid).await.unwrap().foreground_app;
        if front.as_deref() == Some("com.apple.mobilesafari")
            || launched.elapsed() > Duration::from_secs(10)
        {
            break front;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert_eq!(front.as_deref(), Some("com.apple.mobilesafari"));

    // Landscape, set directly: Safari turns, and it reads back.
    let (width, height, _) = screen.size().unwrap();
    set(DeviceSetting::Orientation {
        orientation: DeviceOrientation::LandscapeLeft,
    })
    .await
    .unwrap();
    let turned = Instant::now();
    while screen.size().is_some_and(|(w, h, _)| w < h) && turned.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(screen.size().map(|(w, h, _)| (w, h)), Some((height, width)));
    assert_eq!(
        backend.settings(udid).await.unwrap().orientation,
        Some(DeviceOrientation::LandscapeLeft)
    );
    set(DeviceSetting::Orientation {
        orientation: DeviceOrientation::Portrait,
    })
    .await
    .unwrap();
    let turned = Instant::now();
    while screen.size().is_some_and(|(w, h, _)| w > h) && turned.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(screen.size().map(|(w, h, _)| (w, h)), Some((width, height)));

    // Accessibility frames: Safari's elements outlined in the pictures.
    settle(
        &screen,
        Duration::from_millis(1000),
        Duration::from_secs(10),
    )
    .await;
    let plain = decode(&latest(&screen));
    assert!(pinkness(&plain) < 0.002, "outlined before asked");
    set(DeviceSetting::AccessibilityFrames { on: true })
        .await
        .unwrap();
    let mut pictures = screen.pictures();
    tokio::time::timeout(
        Duration::from_secs(10),
        pictures.wait_for(|p| p.as_ref().is_some_and(|p| pinkness(&decode(p)) > 0.004)),
    )
    .await
    .expect("outlines in the pictures")
    .unwrap();
    assert_eq!(
        backend.settings(udid).await.unwrap().accessibility_frames,
        Some(true)
    );
    set(DeviceSetting::AccessibilityFrames { on: false })
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        pictures.wait_for(|p| p.as_ref().is_some_and(|p| pinkness(&decode(p)) < 0.002)),
    )
    .await
    .expect("the outlines went")
    .unwrap();
    assert_eq!(
        backend.settings(udid).await.unwrap().accessibility_frames,
        Some(false)
    );

    // A pinch in Maps zooms the map. Somewhere with streets, and Maps' two
    // first-run screens answered where they show (its location prompt's
    // "Allow While Using App", then the Ads notice's Continue).
    set(DeviceSetting::Location {
        latitude: 51.5007,
        longitude: -0.1246,
    })
    .await
    .unwrap();
    backend
        .app(
            udid,
            DeviceApp::Launch {
                app: "com.apple.Maps".into(),
            },
        )
        .await
        .unwrap();
    settle(
        &screen,
        Duration::from_millis(2000),
        Duration::from_secs(20),
    )
    .await;
    tap(&screen, 0.5, 0.613).await;
    settle(
        &screen,
        Duration::from_millis(2000),
        Duration::from_secs(20),
    )
    .await;
    // Then white pages ("Get Notified…", the Ads notice) whose button is at
    // the same place: tapped until the map shows.
    for _ in 0..6 {
        if whiteness(&decode(&latest(&screen))) < 0.5 {
            break;
        }
        tap(&screen, 0.5, 0.925).await;
        settle(
            &screen,
            Duration::from_millis(2000),
            Duration::from_secs(20),
        )
        .await;
    }
    assert_eq!(
        backend
            .settings(udid)
            .await
            .unwrap()
            .foreground_app
            .as_deref(),
        Some("com.apple.Maps")
    );
    let before = decode(&latest(&screen));
    let (a, b) = ((0.4f32, 0.3f32), (0.6f32, 0.4f32));
    fingers(&screen, TouchPhase::Down, [a, b]).await;
    let mut at = [a, b];
    for step in 1..=15 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let t = step as f32;
        at = [
            (a.0 - 0.015 * t, a.1 - 0.01 * t),
            (b.0 + 0.015 * t, b.1 + 0.01 * t),
        ];
        fingers(&screen, TouchPhase::Move, at).await;
    }
    fingers(&screen, TouchPhase::Up, at).await;
    settle(
        &screen,
        Duration::from_millis(1500),
        Duration::from_secs(10),
    )
    .await;
    let after = decode(&latest(&screen));
    let changed = difference(&before, &after);
    eprintln!("the pinch changed the picture by {changed:.1}");
    if changed <= 8.0 {
        let dir = std::env::temp_dir();
        before
            .save(dir.join("loft-ios-live-pinch-before.png"))
            .unwrap();
        after
            .save(dir.join("loft-ios-live-pinch-after.png"))
            .unwrap();
        panic!(
            "the pinch didn't zoom the map (pictures in {})",
            dir.display()
        );
    }

    // Three fingers at once reach a web page as three touches: it turns
    // green.
    let page = serve(THREE_FINGERS);
    backend
        .app(udid, DeviceApp::OpenUrl { url: page })
        .await
        .unwrap();
    settle(
        &screen,
        Duration::from_millis(1500),
        Duration::from_secs(20),
    )
    .await;
    let three = [(0.25, 0.3), (0.5, 0.35), (0.75, 0.3)];
    let touches = |dx: f32| {
        three
            .iter()
            .map(|&(x, y)| TouchPoint { x: x + dx, y })
            .collect::<Vec<_>>()
    };
    for (phase, dx) in [
        (TouchPhase::Down, 0.0),
        (TouchPhase::Move, 0.02),
        (TouchPhase::Up, 0.02),
    ] {
        screen
            .input(DeviceInput::Touches {
                phase,
                touches: touches(dx),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut pictures = screen.pictures();
    tokio::time::timeout(
        Duration::from_secs(5),
        pictures.wait_for(|p| {
            p.as_ref().is_some_and(|p| {
                let picture = decode(p);
                let [r, g, b] = picture
                    .get_pixel(picture.width() / 2, picture.height() / 4)
                    .0;
                g > 150 && r < 80 && b < 80
            })
        }),
    )
    .await
    .expect("the page saw three touches at once")
    .unwrap();

    backend.shutdown(udid).await.unwrap();
    drop(screen);
}
