use super::*;
use std::time::Instant;

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
    let launcher = backend
        .shell(
            &running,
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
    while backend
        .shell(&running, &["pidof", &launcher])
        .await
        .is_err()
        && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
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
    assert_eq!(
        first.width * h,
        first.height * w,
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
