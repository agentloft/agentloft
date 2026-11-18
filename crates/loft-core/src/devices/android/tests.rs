use super::*;
use loft_protocol::{DeviceOrientation, DevicePosture, DeviceTextSize};
use std::time::Instant;

/// What an agent types on an emulator without a keyboard goes to adb on its
/// stdin: every account on this computer can read a process's arguments.
#[cfg(unix)]
#[tokio::test]
async fn typed_text_is_on_no_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let adb = Adb {
        path: Some(crate::devices::recording_adb(dir.path())),
        env: Arc::new(HashMap::new()),
    };
    type_by_adb(&adb, "emulator-5554", "it's secret\nword")
        .await
        .unwrap();
    let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
    assert!(!argv.contains("secret") && !argv.contains("word"), "{argv}");
    assert!(
        argv.contains("input keyevent 66"),
        "keys stay as they were: {argv}"
    );
    let stdin = std::fs::read_to_string(dir.path().join("stdin")).unwrap();
    assert_eq!(
        stdin,
        "input text 'it'\\''s%ssecret'\nexit\ninput text word\nexit\n"
    );
}

/// An SDK folder with what `files` says, and an AVD folder.
fn scratch(with_tools: bool, with_avd: bool) -> (tempfile::TempDir, HashMap<String, String>) {
    let dir = tempfile::tempdir().unwrap();
    let sdk = dir.path().join("sdk");
    std::fs::create_dir_all(&sdk).unwrap();
    if with_tools {
        for (sub, name) in [("emulator", "emulator"), ("platform-tools", "adb")] {
            std::fs::create_dir_all(sdk.join(sub)).unwrap();
            std::fs::write(sdk.join(sub).join(sdk::exe_name(name)), "").unwrap();
        }
    }
    let avd_home = dir.path().join("avd");
    std::fs::create_dir_all(&avd_home).unwrap();
    if with_avd {
        std::fs::create_dir_all(avd_home.join("Phone.avd")).unwrap();
        std::fs::write(avd_home.join("Phone.ini"), "target=android-36\n").unwrap();
        std::fs::write(
            avd_home.join("Phone.avd/config.ini"),
            "hw.lcd.width=1080\nhw.lcd.height=2424\nhw.lcd.density=420\n",
        )
        .unwrap();
    }
    let env = HashMap::from([
        ("ANDROID_HOME".to_owned(), sdk.display().to_string()),
        (
            "ANDROID_AVD_HOME".to_owned(),
            avd_home.display().to_string(),
        ),
        // No other SDK on PATH.
        (
            "PATH".to_owned(),
            dir.path().join("bin").display().to_string(),
        ),
        ("HOME".to_owned(), dir.path().display().to_string()),
        ("USERPROFILE".to_owned(), dir.path().display().to_string()),
        ("LOCALAPPDATA".to_owned(), dir.path().display().to_string()),
    ]);
    (dir, env)
}

#[tokio::test]
async fn says_what_to_install() {
    let (_dir, env) = scratch(false, false);
    let why = AndroidBackend::new(&env)
        .support()
        .await
        .unwrap()
        .unwrap_err();
    assert!(why.contains("Android emulator and adb"), "{why}");

    let (_dir, env) = scratch(true, false);
    let why = AndroidBackend::new(&env)
        .support()
        .await
        .unwrap()
        .unwrap_err();
    assert!(why.contains("Device Manager"), "{why}");

    let (_dir, env) = scratch(true, true);
    AndroidBackend::new(&env).support().await.unwrap().unwrap();
}

#[tokio::test]
async fn lists_stopped_avds() {
    let (_dir, env) = scratch(true, true);
    let devices = AndroidBackend::new(&env).list().await.unwrap();
    assert_eq!(devices.len(), 1);
    let phone = &devices[0];
    assert_eq!(phone.id.as_str(), "android:Phone");
    assert_eq!(phone.name, "Phone");
    assert_eq!(phone.os, "Android 16");
    assert_eq!(phone.state, DeviceState::Stopped);
    assert_eq!((phone.width, phone.height), (1080, 2424));
    assert!((phone.scale - 2.625).abs() < 1e-6);
}

#[tokio::test]
async fn lists_a_stopped_foldable_open_flat() {
    let (dir, env) = scratch(true, false);
    let avd_home = dir.path().join("avd");
    std::fs::create_dir_all(avd_home.join("Fold.avd")).unwrap();
    std::fs::write(avd_home.join("Fold.ini"), "target=android-35\n").unwrap();
    std::fs::write(
        avd_home.join("Fold.avd/config.ini"),
        "hw.lcd.width=2076\nhw.lcd.height=2152\nhw.lcd.density=390\n\
         hw.displayRegion.0.1.width=1080\nhw.displayRegion.0.1.height=2424\n\
         hw.sensor.hinge=yes\nhw.sensor.hinge.count=1\nhw.sensor.hinge.type=1\n\
         hw.sensor.hinge.ranges=0-180\nhw.sensor.hinge.defaults=180\n\
         hw.sensor.posture_list=1, 2, 3\n\
         hw.sensor.hinge_angles_posture_definitions=0-30, 30-150, 150-180\n\
         hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture=1\n",
    )
    .unwrap();
    let devices = AndroidBackend::new(&env).list().await.unwrap();
    let fold = devices[0].fold.clone().expect("it folds");
    assert_eq!((devices[0].width, devices[0].height), (2076, 2152));
    assert_eq!(fold.axis, loft_protocol::FoldAxis::Vertical);
    assert!(fold.inward && fold.cover_screen && !fold.on_cover);
    assert_eq!(fold.posture, DevicePosture::Opened);
    assert_eq!(
        fold.postures,
        vec![
            DevicePosture::Closed,
            DevicePosture::HalfOpened,
            DevicePosture::Opened
        ]
    );
    assert_eq!(
        (fold.hinge, fold.hinge_min, fold.hinge_max),
        (180.0, 0.0, 180.0)
    );
}

#[test]
fn picks_the_errors_from_a_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("emu.log");
    std::fs::write(
        &log,
        "INFO | starting\nERROR | Running multiple emulators with the same AVD\nINFO | bye\n",
    )
    .unwrap();
    assert_eq!(
        log_tail(&log).as_deref(),
        Some("ERROR | Running multiple emulators with the same AVD")
    );
    std::fs::write(&log, "\nINFO | a\nINFO | b\n").unwrap();
    assert_eq!(log_tail(&log).as_deref(), Some("INFO | a\nINFO | b"));
}

/// Boots `Pixel_8_Play` (an AVD on the maintainer's Mac; another with
/// `AGENTLOFT_TEST_AVD`), streams its screen, swipes, presses Home, turns
/// it, scrolls, then stops it again if it wasn't running.
/// `cargo test -p loft-core devices::android -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real Android emulator"]
async fn live_boots_streams_and_takes_input() {
    let env: HashMap<String, String> = std::env::vars().collect();
    let backend = AndroidBackend::new(&env);
    backend.support().await.unwrap().unwrap();
    let avd = std::env::var("AGENTLOFT_TEST_AVD").unwrap_or_else(|_| "Pixel_8_Play".into());
    let state = |devices: &[DeviceInfo]| {
        devices
            .iter()
            .find(|d| d.id.native() == avd)
            .unwrap_or_else(|| panic!("no AVD {avd}: {devices:?}"))
            .state
    };
    let was = state(&backend.list().await.unwrap());
    let started = Instant::now();
    backend.boot(&avd).await.unwrap();
    eprintln!("booted in {:?} (was {was:?})", started.elapsed());
    assert_eq!(state(&backend.list().await.unwrap()), DeviceState::Running);

    let screen = backend.attach(&avd).await.unwrap();
    let (w, h, scale) = screen.size().unwrap();
    eprintln!("screen {w}x{h} at {scale}");
    assert!(w > 0 && h > 0 && scale > 0.0);
    let mut pictures = screen.pictures();
    screen.watching(true);
    // Wait for the launcher (a cold boot shows "starting…" for a while).
    let running = backend.running(&avd).await.unwrap();
    wait_for_launcher(&backend, &running).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let home = DeviceInput::Button {
        button: DeviceButton::Home,
    };
    screen.input(home.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let first = next_picture(&mut pictures).await;
    eprintln!(
        "picture {}x{}, {} KB",
        first.width,
        first.height,
        first.jpeg.len() / 1024
    );
    assert!(first.width.max(first.height) <= PICTURE_MAX);
    // The shape, give or take a pixel of rounding.
    assert!(
        (first.width as f32 * h as f32 / w as f32 - first.height as f32).abs() <= 1.0,
        "keeps the screen's shape"
    );
    let still = count_frames(&mut pictures, Duration::from_secs(2)).await;
    let at_home = pictures.borrow().clone().unwrap();

    // A swipe up from the middle opens the app drawer.
    let swiped = Instant::now();
    for (phase, y) in [
        (TouchPhase::Down, 0.8),
        (TouchPhase::Move, 0.65),
        (TouchPhase::Move, 0.5),
        (TouchPhase::Move, 0.35),
        (TouchPhase::Up, 0.35),
    ] {
        screen
            .input(DeviceInput::Touch { phase, x: 0.5, y })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    pictures.mark_unchanged();
    next_picture(&mut pictures).await;
    eprintln!("first picture after the swipe: {:?}", swiped.elapsed());
    let moving = count_frames(&mut pictures, Duration::from_secs(1)).await;
    eprintln!("{still} pictures in 2 s while still, {moving} in the 1 s after a swipe");
    let drawer = pictures.borrow().clone().unwrap();
    let swipe_diff = difference(&at_home, &drawer);
    eprintln!("home vs drawer: {swipe_diff:.1}");
    assert!(swipe_diff > 12.0, "the swipe didn't open the drawer");

    // Home closes it.
    screen.input(home.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let back_home = pictures.borrow().clone().unwrap();
    let home_diff = difference(&drawer, &back_home);
    eprintln!("drawer vs home again: {home_diff:.1}");
    assert!(home_diff > 12.0, "Home didn't close the drawer");

    let png = screen.capture().await.unwrap();
    let full = image::load_from_memory(&png).unwrap();
    assert_eq!((full.width(), full.height()), (w, h));

    // Turned left and back.
    screen
        .input(DeviceInput::Button {
            button: DeviceButton::RotateLeft,
        })
        .await
        .unwrap();
    assert_eq!(screen.size().unwrap().0, h);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let turned = pictures.borrow().clone().unwrap();
    assert!(turned.width > turned.height, "the picture turned");
    screen
        .input(DeviceInput::Button {
            button: DeviceButton::RotateRight,
        })
        .await
        .unwrap();
    assert_eq!(screen.size().unwrap().0, w);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let upright = pictures.borrow().clone().unwrap();
    assert!(upright.width < upright.height, "the picture turned back");

    // Settings scrolls under a scroll.
    let running = backend.running(&avd).await.unwrap();
    backend
        .shell(
            &running,
            &["am", "start", "-W", "-a", "android.settings.SETTINGS"],
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let top = pictures.borrow().clone().unwrap();
    for _ in 0..10 {
        screen
            .input(DeviceInput::Scroll {
                x: 0.5,
                y: 0.6,
                dx: 0.0,
                dy: 0.04,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let scrolled = pictures.borrow().clone().unwrap();
    let scroll_diff = difference(&top, &scrolled);
    eprintln!("settings before and after scrolling: {scroll_diff:.1}");
    assert!(scroll_diff > 5.0, "the scroll didn't move Settings");
    screen.input(home).await.unwrap();

    screen.watching(false);
    drop(screen);
    if was == DeviceState::Stopped {
        backend.shutdown(&avd).await.unwrap();
        assert_eq!(state(&backend.list().await.unwrap()), DeviceState::Stopped);
    }
}

/// Wait until the home screen's app runs.
async fn wait_for_launcher(backend: &AndroidBackend, running: &Running) {
    let launcher = backend
        .shell(
            running,
            &[
                "cmd",
                "package",
                "resolve-activity",
                "--brief",
                "-a",
                "android.intent.action.MAIN",
                "-c",
                "android.intent.category.HOME",
            ],
        )
        .await
        .unwrap();
    let launcher = launcher
        .lines()
        .last()
        .and_then(|l| l.split('/').next())
        .unwrap()
        .trim()
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(90);
    while backend.shell(running, &["pidof", &launcher]).await.is_err() && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// How different two pictures look: the mean absolute difference of their
/// grey levels (0–255).
fn difference(a: &Picture, b: &Picture) -> f64 {
    let a = image::load_from_memory(&a.jpeg).unwrap().to_luma8();
    let b = image::load_from_memory(&b.jpeg).unwrap().to_luma8();
    assert_eq!(a.dimensions(), b.dimensions());
    let sum: u64 = a
        .pixels()
        .zip(b.pixels())
        .map(|(p, q)| p.0[0].abs_diff(q.0[0]) as u64)
        .sum();
    sum as f64 / (a.width() * a.height()) as f64
}

async fn next_picture(pictures: &mut watch::Receiver<Option<Arc<Picture>>>) -> Arc<Picture> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            pictures.changed().await.unwrap();
            if let Some(p) = pictures.borrow_and_update().clone() {
                return p;
            }
        }
    })
    .await
    .expect("no picture in 20 s")
}

async fn count_frames(
    pictures: &mut watch::Receiver<Option<Arc<Picture>>>,
    within: Duration,
) -> usize {
    let mut n = 0;
    let _ = tokio::time::timeout(within, async {
        while pictures.changed().await.is_ok() {
            pictures.borrow_and_update();
            n += 1;
        }
    })
    .await;
    n
}

#[tokio::test]
async fn knows_whether_a_process_is_alive() {
    assert!(process_alive(std::process::id()).await);
    assert!(!process_alive(u32::MAX - 7).await);
}

/// Makes a foldable AVD (a Pixel 9 Pro Fold) in an AVD folder of its own,
/// boots it, folds it shut (pictures switch to the cover screen), half
/// open and open, pinches a picture in Photos, sets and reads back its
/// settings, then stops it; the AVD goes with its folder. Needs the SDK's
/// command-line tools and the system image `AGENTLOFT_TEST_FOLD_IMAGE`
/// (default `system-images;android-35;google_apis_playstore;arm64-v8a`).
/// `cargo test -p loft-core devices::android::tests::live_a_foldable -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "makes and boots a real Android emulator"]
async fn live_a_foldable() {
    let avd_home = tempfile::Builder::new()
        .prefix("loft-fold-avd")
        .tempdir_in("/private/tmp")
        .or_else(|_| tempfile::tempdir())
        .unwrap();
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.insert(
        "ANDROID_AVD_HOME".into(),
        avd_home.path().display().to_string(),
    );
    let sdk = Sdk::find(&env);
    let avdmanager = sdk
        .root
        .as_ref()
        .map(|r| r.join("cmdline-tools/latest/bin/avdmanager"))
        .filter(|p| p.is_file())
        .expect("the SDK's command-line tools (avdmanager)");
    let image = std::env::var("AGENTLOFT_TEST_FOLD_IMAGE")
        .unwrap_or_else(|_| "system-images;android-35;google_apis_playstore;arm64-v8a".into());
    let avd = "Test_Fold";
    let mut made = std::process::Command::new(&avdmanager)
        .envs(&env)
        .args([
            "create",
            "avd",
            "-n",
            avd,
            "-k",
            &image,
            "-d",
            "pixel_9_pro_fold",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // "Do you wish to create a custom hardware profile?"
    use std::io::Write as _;
    let _ = made.stdin.take().unwrap().write_all(b"no\n");
    assert!(
        made.wait().unwrap().success(),
        "avdmanager create avd failed"
    );

    let backend = AndroidBackend::new(&env);
    let listed = backend.list().await.unwrap();
    let info = listed.iter().find(|d| d.id.native() == avd).unwrap();
    let fold = info.fold.clone().expect("a foldable");
    eprintln!("{} {}x{}: {fold:?}", info.name, info.width, info.height);
    assert_eq!(fold.posture, DevicePosture::Opened);
    assert!(fold.cover_screen && fold.inward);

    let started = Instant::now();
    backend.boot(avd).await.unwrap();
    eprintln!("booted in {:?}", started.elapsed());
    // Whatever happens next, stop it again.
    struct Stop<'a>(&'a AndroidBackend, &'a str);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            if let Some(running) = self.0.sdk.running().into_iter().find(|r| r.avd == self.1)
                && let Some(serial) = running.serial()
                && let Some(adb) = &self.0.sdk.adb
            {
                let _ = std::process::Command::new(adb)
                    .args(["-s", &serial, "emu", "kill"])
                    .status();
                // Gone before its folder is: else it writes there again.
                let pid = running.pid.to_string();
                for _ in 0..100 {
                    let alive = std::process::Command::new("ps")
                        .args(["-p", &pid])
                        .stdout(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success());
                    if !alive {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
    }
    let stop = Stop(&backend, avd);
    let running = backend.running(avd).await.unwrap();
    wait_for_launcher(&backend, &running).await;

    let screen = backend.attach(avd).await.unwrap();
    let (w, h, _) = screen.size().unwrap();
    eprintln!("open: {w}x{h}");
    let mut pictures = screen.pictures();
    screen.watching(true);
    next_picture(&mut pictures).await;

    let fold_to = |posture| DeviceSetting::Posture { posture };
    let folded_at = |posture| {
        let backend = &backend;
        async move {
            let listed = backend.list().await.unwrap();
            let info = listed.into_iter().find(|d| d.id.native() == avd).unwrap();
            let fold = info.fold.clone().unwrap();
            assert_eq!(fold.posture, posture, "{fold:?}");
            (info, fold)
        }
    };

    // Shut: the pictures show the cover screen.
    let t = Instant::now();
    backend
        .set(avd, fold_to(DevicePosture::Closed))
        .await
        .unwrap();
    let (info, fold) = folded_at(DevicePosture::Closed).await;
    eprintln!("closed in {:?}: {fold:?}", t.elapsed());
    assert!(fold.on_cover);
    assert!(fold.hinge < 30.0);
    assert_eq!((info.width, info.height), (1080, 2424));
    let (cw, ch, _) = screen.size().unwrap();
    assert_eq!((cw, ch), (1080, 2424));
    let cover = picture_shaped(&mut pictures, 1080.0 / 2424.0).await;
    eprintln!(
        "cover picture {}x{} {:?} after folding",
        cover.width,
        cover.height,
        t.elapsed()
    );

    // Half open (a book): the main screen again.
    backend
        .set(avd, fold_to(DevicePosture::HalfOpened))
        .await
        .unwrap();
    let (info, fold) = folded_at(DevicePosture::HalfOpened).await;
    eprintln!("half open: {fold:?}");
    assert!(!fold.on_cover && fold.hinge > 30.0 && fold.hinge < 150.0);
    assert_eq!((info.width, info.height), (w, h));
    assert_eq!(screen.size().unwrap().0, w);
    picture_shaped(&mut pictures, w as f32 / h as f32).await;

    // The hinge itself: 10 degrees is shut, 180 flat.
    backend
        .set(avd, DeviceSetting::Hinge { degrees: 10.0 })
        .await
        .unwrap();
    let (_, fold) = folded_at(DevicePosture::Closed).await;
    assert!((fold.hinge - 10.0).abs() < 0.5);
    backend
        .set(avd, fold_to(DevicePosture::Opened))
        .await
        .unwrap();
    let (_, fold) = folded_at(DevicePosture::Opened).await;
    assert_eq!(fold.hinge, 180.0);
    assert_eq!(screen.size().unwrap().0, w);
    assert!(
        backend
            .set(avd, fold_to(DevicePosture::Tent))
            .await
            .is_err(),
        "a fold-in has no tent posture"
    );

    // Two fingers spread apart zoom into a picture in Photos.
    let grid = avd_home.path().join("grid.png");
    image::RgbImage::from_fn(800, 800, |x, y| {
        if (x / 50 + y / 50) % 2 == 0 {
            image::Rgb([255, 200, 0])
        } else {
            image::Rgb([0, 0, 0])
        }
    })
    .save(&grid)
    .unwrap();
    let serial = running.serial().unwrap();
    backend
        .adb(
            Some(&serial),
            &[
                "push",
                &grid.display().to_string(),
                "/sdcard/Pictures/grid.png",
            ],
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    backend
        .shell(
            &running,
            &[
                "content",
                "call",
                "--method",
                "scan_volume",
                "--uri",
                "content://media",
                "--arg",
                "external_primary",
            ],
        )
        .await
        .unwrap();
    let rows = backend
        .shell(
            &running,
            &[
                "content",
                "query",
                "--uri",
                "content://media/external/images/media",
                "--projection",
                "_id:_display_name",
            ],
        )
        .await
        .unwrap();
    let id = rows
        .lines()
        .find(|l| l.contains("grid.png"))
        .and_then(|l| l.split("_id=").nth(1))
        .and_then(|r| r.split(',').next())
        .expect("the picture in the media store")
        .to_owned();
    backend
        .app(
            avd,
            DeviceApp::OpenUrl {
                url: format!("content://media/external/images/media/{id}"),
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let before = pictures.borrow().clone().unwrap();
    let pinch = |phase, spread: f32| DeviceInput::Touches {
        phase,
        touches: vec![
            TouchPoint {
                x: 0.5 - spread,
                y: 0.5,
            },
            TouchPoint {
                x: 0.5 + spread,
                y: 0.5,
            },
        ],
    };
    screen.input(pinch(TouchPhase::Down, 0.08)).await.unwrap();
    for i in 1..=12 {
        tokio::time::sleep(Duration::from_millis(16)).await;
        screen
            .input(pinch(TouchPhase::Move, 0.08 + 0.025 * i as f32))
            .await
            .unwrap();
    }
    // Both fingers are down, as the system sees them.
    let fingers = backend
        .shell(&running, &["dumpsys", "input"])
        .await
        .unwrap();
    let two_down = fingers.contains("pointerCount=2") || fingers.contains("PointerCount: 2");
    screen.input(pinch(TouchPhase::Up, 0.38)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = pictures.borrow().clone().unwrap();
    let (squares_before, squares_after) = (squares(&before), squares(&after));
    eprintln!(
        "pinch: two fingers seen {two_down}; squares across {squares_before} -> {squares_after}"
    );
    assert!(
        squares_after * 3 <= squares_before * 2,
        "the pinch didn't zoom in"
    );

    // Settings, set and read back from the device.
    let was = backend.settings(avd).await.unwrap();
    eprintln!("settings: {was:?}");
    for name in [
        "appearance",
        "text_size",
        "contrast",
        "reduce_motion",
        "accessibility_frames",
        "network",
        "orientation",
        "posture",
        "hinge",
    ] {
        assert!(
            was.supported.iter().any(|s| s == name),
            "{name} unsupported"
        );
    }
    assert_eq!(
        was.foreground_app.as_deref(),
        Some("com.google.android.apps.photos")
    );
    let changes = [
        DeviceSetting::Appearance { dark: true },
        DeviceSetting::TextSize {
            size: DeviceTextSize::Large,
        },
        DeviceSetting::Contrast { high: true },
        DeviceSetting::ReduceMotion { on: true },
        DeviceSetting::AccessibilityFrames { on: true },
        DeviceSetting::Network { on: false },
        DeviceSetting::Orientation {
            orientation: DeviceOrientation::LandscapeLeft,
        },
    ];
    let t = Instant::now();
    for change in changes {
        backend.set(avd, change).await.unwrap();
    }
    let now = backend.settings(avd).await.unwrap();
    eprintln!("set and read back in {:?}: {now:?}", t.elapsed());
    assert_eq!(now.dark, Some(true));
    assert_eq!(now.text_size, Some(DeviceTextSize::Large));
    assert_eq!(now.contrast, Some(true));
    assert_eq!(now.reduce_motion, Some(true));
    assert_eq!(now.accessibility_frames, Some(true));
    assert_eq!(now.network, Some(false));
    assert_eq!(now.orientation, Some(DeviceOrientation::LandscapeLeft));
    if now.supported.iter().any(|s| s == "screen_reader") {
        backend
            .set(avd, DeviceSetting::ScreenReader { on: true })
            .await
            .unwrap();
        assert_eq!(
            backend.settings(avd).await.unwrap().screen_reader,
            Some(true)
        );
        backend
            .set(avd, DeviceSetting::ScreenReader { on: false })
            .await
            .unwrap();
        assert_eq!(
            backend.settings(avd).await.unwrap().screen_reader,
            Some(false)
        );
    }
    backend
        .set(
            avd,
            DeviceSetting::Permission {
                app: "com.android.chrome".into(),
                permission: "CAMERA".into(),
                grant: true,
            },
        )
        .await
        .unwrap();
    let granted = backend
        .shell(&running, &["dumpsys", "package", "com.android.chrome"])
        .await
        .unwrap();
    assert!(granted.contains("android.permission.CAMERA: granted=true"));
    backend
        .set(
            avd,
            DeviceSetting::Push {
                app: "com.example.app".into(),
                payload: serde_json::json!({"aps": {"alert": {"title": "Hello", "body": "From Agentloft"}}}),
            },
        )
        .await
        .unwrap();
    // The system posts it a moment later.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let posted = backend
            .shell(&running, &["cmd", "notification", "list"])
            .await
            .unwrap();
        if posted.contains("|com.example.app|") {
            break;
        }
        assert!(Instant::now() < deadline, "not posted: {posted}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Back as it was.
    for change in [
        DeviceSetting::Appearance { dark: false },
        DeviceSetting::TextSize {
            size: DeviceTextSize::Default,
        },
        DeviceSetting::Contrast { high: false },
        DeviceSetting::ReduceMotion { on: false },
        DeviceSetting::AccessibilityFrames { on: false },
        DeviceSetting::Network { on: true },
        DeviceSetting::Orientation {
            orientation: DeviceOrientation::Portrait,
        },
    ] {
        backend.set(avd, change).await.unwrap();
    }
    let back = backend.settings(avd).await.unwrap();
    assert_eq!(back.dark, Some(false));
    assert_eq!(back.text_size, Some(DeviceTextSize::Default));
    assert_eq!(back.reduce_motion, Some(false));
    assert_eq!(back.network, Some(true));
    assert_eq!(back.orientation, Some(DeviceOrientation::Portrait));

    screen.watching(false);
    drop(screen);
    backend.shutdown(avd).await.unwrap();
    drop(stop);
    assert!(backend.running(avd).await.is_none());
}

/// The first picture whose width over height is `shape`.
async fn picture_shaped(
    pictures: &mut watch::Receiver<Option<Arc<Picture>>>,
    shape: f32,
) -> Arc<Picture> {
    let like = |p: &Picture| (p.width as f32 / p.height as f32 - shape).abs() < 0.02;
    if let Some(p) = pictures.borrow().clone()
        && like(&p)
    {
        return p;
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let p = next_picture(pictures).await;
            if like(&p) {
                return p;
            }
        }
    })
    .await
    .expect("no picture of that shape in 20 s")
}

/// How many squares of the test grid a row through the picture's middle
/// crosses.
fn squares(picture: &Picture) -> usize {
    let grey = image::load_from_memory(&picture.jpeg).unwrap().to_luma8();
    let y = grey.height() / 2;
    let mut last = None;
    let mut changes = 0;
    for x in 0..grey.width() {
        let bright = grey.get_pixel(x, y).0[0] > 100;
        if last.is_some_and(|l| l != bright) {
            changes += 1;
        }
        last = Some(bright);
    }
    changes + 1
}
