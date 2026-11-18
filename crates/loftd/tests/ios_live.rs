//! A real iOS simulator through the daemon's backend and its helper
//! (`agentloftd simulator`): boot, pictures, a swipe, Home, turning, a
//! capture, shutdown. Needs Xcode with an iOS runtime; takes about a minute.
//!
//! `cargo test -p loftd --test ios_live -- --ignored --nocapture`
//!
//! It makes its own iPhone in a device set of its own (not the user's
//! simulators, which may be in use) and deletes it afterwards.

#![cfg(target_os = "macos")]

use loft_core::devices::ios::{Helper, IosBackend};
use loft_core::devices::{Backend, Picture, Screen};
use loft_protocol::{DeviceApp, DeviceButton, DeviceInput, DeviceState, TouchPhase};
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
    fn new() -> Self {
        let set = PathBuf::from(format!("/private/tmp/loft-ios-live-{}", std::process::id()));
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
    let sandbox = Sandbox::new();
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
