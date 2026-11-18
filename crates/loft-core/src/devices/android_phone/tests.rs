use super::h264::{self, Splitter, Unit};
use super::touch::{self, Fingers, Rotation, ScrollDrag, Step};
use super::*;

#[test]
fn reads_adb_devices() {
    let out = "* daemon not running; starting now at tcp:5037\n\
               * daemon started successfully\n\
               List of devices attached\n\
               R5CT1234ABC            device usb:1-1 product:a54xnseea model:SM_A546B device:a54x transport_id:3\n\
               emulator-5554          device product:sdk_gphone64_arm64 model:sdk_gphone64_arm64 device:emu64a transport_id:1\n\
               0A281FDD4000QX         unauthorized usb:2-1 transport_id:4\n\
               192.168.1.20:5555      offline transport_id:5\n\
               adb-0A281FDD-x1y2._adb-tls-connect._tcp device product:panther model:Pixel_7 device:panther transport_id:6\n\
               9B061FFAZ001           no permissions (missing udev rules? user is in the plugdev group); see [http://developer.android.com/tools/device.html] usb:1-2 transport_id:7\n\n";
    let listed = adb::parse_devices(out);
    let pick = |s: &str| listed.iter().find(|d| d.serial == s).unwrap();
    assert_eq!(listed.len(), 6);
    let samsung = pick("R5CT1234ABC");
    assert!(samsung.ready() && !samsung.is_emulator() && !samsung.wireless());
    assert_eq!(samsung.model.as_deref(), Some("SM_A546B"));
    assert!(pick("emulator-5554").is_emulator());
    assert_eq!(pick("0A281FDD4000QX").state, "unauthorized");
    assert_eq!(pick("0A281FDD4000QX").model, None);
    assert!(pick("192.168.1.20:5555").wireless());
    let pixel = pick("adb-0A281FDD-x1y2._adb-tls-connect._tcp");
    assert!(pixel.wireless() && pixel.ready());
    assert_eq!(pixel.model.as_deref(), Some("Pixel_7"));
    let linux = pick("9B061FFAZ001");
    assert_eq!(linux.state, "no permissions");
    assert!(problem(linux).contains("udev"));
    assert!(problem(pick("0A281FDD4000QX")).contains("accept the prompt"));
    assert!(adb::parse_devices("List of devices attached\n\n").is_empty());
}

#[test]
fn reads_what_a_phone_says_about_itself() {
    let about = adb::parse_about(
        "market=\nvmarket=\nmodel=Pixel 8\nrelease=15\nabi=arm64-v8a\n\
         Physical size: 1080x2400\nPhysical density: 420\n",
    );
    assert_eq!(
        about,
        About {
            name: "Pixel 8".into(),
            release: "15".into(),
            width: 1080,
            height: 2400,
            density: 420,
            abi: "arm64-v8a".into(),
        }
    );
    // A market name wins; an override is what the screen shows.
    let about = adb::parse_about(
        "market=Redmi Note 13\nvmarket=\nmodel=23129RAA4G\nrelease=14\n\
         Physical size: 1080x2400\nOverride size: 720x1600\n\
         Physical density: 440\nOverride density: 320\n",
    );
    assert_eq!(about.name, "Redmi Note 13");
    assert_eq!((about.width, about.height, about.density), (720, 1600, 320));
    assert_eq!(adb::parse_size(" 1440x3120 "), Some((1440, 3120)));
    assert_eq!(adb::parse_size("x"), None);

    let listed = Listed {
        serial: "R5CT".into(),
        state: "device".into(),
        model: Some("SM_A546B".into()),
    };
    let info = PhoneBackend::info(&listed, None);
    assert_eq!(info.id.as_str(), "android:@R5CT");
    assert_eq!(info.id.phone_serial(), Some("R5CT"));
    assert_eq!(info.name, "SM A546B");
    assert!(info.physical);
    assert_eq!(info.state, DeviceState::Running);
    let about = adb::parse_about(
        "model=Galaxy A54\nrelease=14\nPhysical size: 1080x2340\nPhysical density: 450\n",
    );
    let info = PhoneBackend::info(&listed, Some(&about));
    assert_eq!(
        (info.name.as_str(), info.os.as_str()),
        ("Galaxy A54", "Android 14")
    );
    assert_eq!((info.width, info.height), (1080, 2340));
    assert!((info.scale - 2.8125).abs() < 1e-6);
}

#[test]
fn emulators_count_as_phones_only_when_asked() {
    assert_eq!(Emulators::from_setting(None), Emulators::None);
    assert_eq!(Emulators::from_setting(Some("0")), Emulators::None);
    assert!(Emulators::from_setting(Some("1")).includes("emulator-5554"));
    let some = Emulators::from_setting(Some("emulator-5600, emulator-5602"));
    assert!(some.includes("emulator-5602") && !some.includes("emulator-5554"));
    assert_eq!(serial_of("@R5CT").unwrap(), "R5CT");
    assert!(serial_of("Pixel_8").is_err());
    assert!(serial_of("@").is_err());
}

#[test]
fn cuts_h264_into_pictures() {
    let sps = [0x67, 0x42, 0xc0, 0x1f, 0xda];
    let pps = [0x68, 0xce, 0x3c, 0x80];
    let idr = [0x65, 0x88, 0x84, 0x00, 0x21];
    let p1 = [0x41, 0x9a, 0x02, 0x03];
    // A second slice of the same picture (first_mb_in_slice != 0).
    let p1b = [0x41, 0x40, 0x07];
    let p2 = [0x41, 0x9b, 0x05];
    let mut stream = Vec::new();
    for nal in [&sps[..], &pps, &idr, &p1, &p1b, &p2] {
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(nal);
    }
    // Byte by byte: start codes split across reads are found.
    let mut splitter = Splitter::default();
    let mut units = Vec::new();
    for b in &stream {
        units.extend(splitter.push(std::slice::from_ref(b)));
    }
    assert!(!splitter.pending_key());
    units.extend(splitter.flush());
    assert_eq!(
        units,
        vec![
            Unit::Params {
                sps: sps.to_vec(),
                pps: pps.to_vec()
            },
            Unit::Frame {
                nals: vec![idr.to_vec()],
                key: true
            },
            Unit::Frame {
                nals: vec![p1.to_vec(), p1b.to_vec()],
                key: false
            },
            Unit::Frame {
                nals: vec![p2.to_vec()],
                key: false
            },
        ]
    );
    // In one go: a picture is complete once the next one starts, or the
    // stream pauses.
    let mut splitter = Splitter::default();
    let units = splitter.push(&stream);
    assert_eq!(units.len(), 2);
    assert_eq!(
        splitter.flush(),
        vec![
            Unit::Frame {
                nals: vec![p1.to_vec(), p1b.to_vec()],
                key: false
            },
            Unit::Frame {
                nals: vec![p2.to_vec()],
                key: false
            }
        ]
    );
    // The same parameter sets again (a new recording's) aren't repeated;
    // new ones are.
    let mut again = vec![0, 0, 1];
    again.extend_from_slice(&sps);
    again.extend_from_slice(&[0, 0, 1]);
    again.extend_from_slice(&pps);
    again.extend_from_slice(&[0, 0, 1, 0x65, 0x88]);
    let units = splitter.push(&again);
    assert!(units.is_empty(), "{units:?}");
    let units = splitter.flush();
    assert!(matches!(&units[..], [Unit::Frame { key: true, .. }]));
    assert!(!splitter.take_broken());

    // A picture in two parts with a pause between: given out at the pause,
    // then found broken when the rest comes.
    splitter.push(&[0, 0, 0, 1, 0x41, 0x9a, 0x10]);
    assert_eq!(splitter.flush().len(), 1);
    splitter.push(&[0x22, 0x33]);
    assert!(splitter.take_broken());
    assert!(!splitter.take_broken());
    // The next picture after a pause is not.
    splitter.push(&[0, 0, 1, 0x41, 0x9a]);
    splitter.flush();
    splitter.push(&[0]);
    splitter.push(&[0, 0, 1, 0x41, 0x9a]);
    assert!(!splitter.take_broken());
    assert_eq!(h264::continues_unit(&[0, 0]), None);
    assert_eq!(h264::continues_unit(&[0, 0, 1, 0x41]), Some(false));
    assert_eq!(h264::continues_unit(&[0, 1, 0x41]), Some(true));
    assert_eq!(h264::continues_unit(&[0x41]), Some(true));
}

#[test]
fn finds_jpegs_in_a_stream() {
    let a = [
        0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x05, 0x00, 0x02, 0x40, 0x03, 0xff, 0x00, 0x12,
        0xff, 0xd9,
    ];
    let mut buf = vec![0x00];
    buf.extend_from_slice(&a);
    buf.extend_from_slice(&a[..5]);
    let jpeg = stream::next_jpeg(&mut buf).unwrap();
    assert_eq!(jpeg, a);
    assert_eq!(jpeg_size(&jpeg), Some((576, 1280)));
    assert_eq!(stream::next_jpeg(&mut buf), None);
    assert_eq!(buf, a[..5]);
}

#[test]
fn sizes_for_the_encoder_and_pictures() {
    assert_eq!(fit(1080, 2400, 1280), (576, 1280));
    assert_eq!(fit(2400, 1080, 1280), (1280, 576));
    assert_eq!(fit(720, 1280, 1280), (720, 1280));
    assert_eq!(stream::record_size(1080, 2400), (576, 1280));
    // 1440x3120 fits as 590.8 wide: the encoder gets a multiple of 16.
    assert_eq!(stream::record_size(1440, 3120), (592, 1280));
    assert_eq!(stream::record_size(3120, 1440), (1280, 592));
}

#[test]
fn reads_screencaps_raw_pixels() {
    // 2x1, RGBA, with the newer 16-byte header (a colour space).
    let mut raw = Vec::new();
    for v in [2u32, 1, 1, 1] {
        raw.extend_from_slice(&v.to_le_bytes());
    }
    raw.extend_from_slice(&[255, 0, 0, 255, 0, 0, 255, 255]);
    let picture = stream::raw_picture(&raw).unwrap();
    assert_eq!((picture.width, picture.height), (2, 1));
    assert!(picture.jpeg.starts_with(&[0xff, 0xd8]));
    // The older 12-byte header, BGRA.
    let mut old = Vec::new();
    for v in [1u32, 1, 5] {
        old.extend_from_slice(&v.to_le_bytes());
    }
    old.extend_from_slice(&[0, 0, 255, 255]);
    assert_eq!(stream::raw_picture(&old).unwrap().width, 1);
    old[8] = 9;
    assert!(stream::raw_picture(&old).is_err(), "an unknown format");
    assert!(stream::raw_picture(&[0; 8]).is_err());
}

#[test]
fn picks_decoders_in_order() {
    let ffmpeg = Some(std::path::PathBuf::from("/opt/homebrew/bin/ffmpeg"));
    let all = stream::decoders(ffmpeg.clone(), None);
    let names: Vec<_> = all.iter().map(stream::Decode::name).collect();
    if cfg!(target_os = "macos") {
        assert_eq!(names, ["VideoToolbox", "ffmpeg", "screencap"]);
    } else {
        assert_eq!(names, ["ffmpeg", "screencap"]);
    }
    let forced = stream::decoders(ffmpeg, Some("ffmpeg"));
    assert_eq!(forced.len(), 2);
    assert_eq!(forced[0].name(), "ffmpeg");
    assert_eq!(stream::decoders(None, Some("ffmpeg"))[0].name(), {
        if cfg!(target_os = "macos") {
            "VideoToolbox"
        } else {
            "screencap"
        }
    });
}

#[test]
fn touches_become_multitouch_events() {
    let register: serde_json::Value =
        serde_json::from_str(&touch::register(1080, 2400, "virtual")).unwrap();
    assert_eq!(register["command"], "register");
    assert_eq!(register["bus"], "virtual");
    assert_eq!(register["name"], touch::DEVICE_NAME);
    let x_max = register["abs_info"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["code"] == 0x35)
        .unwrap()["info"]["maximum"]
        .clone();
    assert_eq!(x_max, 1079);

    let events = |line: Option<String>| -> Vec<i64> {
        let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        assert_eq!(v["command"], "inject");
        v["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_i64().unwrap())
            .collect()
    };
    let mut fingers = Fingers::new(1080, 2400);
    // One finger down: slot, a new tracking id, size, pressure, position,
    // then BTN_TOUCH, BTN_TOOL_FINGER and SYN_REPORT.
    assert_eq!(
        events(fingers.frame(&[(0, Some((0.5, 0.25)))])),
        vec![
            3, 0x2f, 0, 3, 0x39, 1, 3, 0x30, 8, 3, 0x3a, 128, 3, 0x35, 540, 3, 0x36, 600, 1, 0x14a,
            1, 1, 0x145, 1, 0, 0, 0
        ]
    );
    // A move: just where.
    assert_eq!(
        events(fingers.frame(&[(0, Some((1.0, 1.0)))])),
        vec![3, 0x2f, 0, 3, 0x35, 1079, 3, 0x36, 2399, 0, 0, 0]
    );
    // A second finger: no BTN_TOUCH again.
    let second = events(fingers.frame(&[(1, Some((0.1, 0.1)))]));
    assert_eq!(&second[..6], &[3, 0x2f, 1, 3, 0x39, 2]);
    assert!(!second.chunks(3).any(|e| e[1] == 0x14a));
    // The first lifts; the second is still down.
    let up = events(fingers.frame(&[(0, None)]));
    assert_eq!(up, vec![3, 0x2f, 0, 3, 0x39, -1, 0, 0, 0]);
    assert!(fingers.any_down());
    // The last lifts: BTN_TOUCH goes up.
    let up = events(fingers.frame(&[(1, None)]));
    assert_eq!(
        up,
        vec![3, 0x2f, 1, 3, 0x39, -1, 1, 0x14a, 0, 1, 0x145, 0, 0, 0, 0]
    );
    assert!(fingers.frame(&[(0, None)]).is_none());
}

#[test]
fn points_follow_the_rotation() {
    assert_eq!(Rotation(0).to_natural(0.25, 0.75), (0.25, 0.75));
    // Turned a quarter: the picture's top left is the display's top right.
    assert_eq!(Rotation(1).to_natural(0.0, 0.0), (1.0, 0.0));
    assert_eq!(Rotation(1).to_natural(1.0, 1.0), (0.0, 1.0));
    assert_eq!(Rotation(2).to_natural(0.0, 0.0), (1.0, 1.0));
    assert_eq!(Rotation(3).to_natural(0.0, 0.0), (0.0, 1.0));
    assert_eq!(Rotation(3).to_natural(1.0, 0.0), (0.0, 0.0));
    assert_eq!(Rotation(1).size(1080, 2400), (2400, 1080));
    assert_eq!(Rotation(2).size(1080, 2400), (1080, 2400));
}

#[test]
fn scrolls_drag_a_finger_the_other_way() {
    let mut drag = ScrollDrag::default();
    assert_eq!(
        drag.scroll(0.5, 0.6, 0.0, 0.1),
        vec![Step::Down(0.5, 0.6), Step::Move(0.5, 0.5)]
    );
    assert_eq!(
        drag.scroll(0.5, 0.6, 0.05, 0.1),
        vec![Step::Move(0.45, 0.4)]
    );
    let steps = drag.scroll(0.5, 0.6, 0.0, 0.5);
    assert_eq!(steps[0], Step::Up);
    assert_eq!(steps[1], Step::Down(0.5, 0.6));
    assert!(drag.lift());
    assert!(!drag.lift());
}

#[test]
fn keys_and_text_for_input() {
    assert_eq!(keycode("Enter"), Some(66));
    assert_eq!(keycode("a"), Some(29));
    assert_eq!(keycode("Z"), Some(54));
    assert_eq!(keycode("5"), Some(12));
    assert_eq!(keycode("F12"), Some(142));
    assert_eq!(keycode("Esc"), Some(111));
    assert_eq!(keycode("!"), None);
    assert_eq!(keycode("Hyper"), None);
    assert_eq!(button_keycode(DeviceButton::Recents), Some(187));
    assert_eq!(button_keycode(DeviceButton::RotateLeft), None);
    assert_eq!(
        modifier_keycodes(page_modifiers::SHIFT | page_modifiers::CONTROL),
        vec![113, 59]
    );
    assert_eq!(
        text_commands("it's a\ttest\nok").unwrap(),
        vec![
            r"input text 'it'\''s%sa'",
            "input keyevent 61",
            "input text test",
            "input keyevent 66",
            "input text ok",
        ]
    );
    assert_eq!(text_commands("").unwrap(), Vec::<String>::new());
    assert!(text_commands("café").is_none());
}

/// What an agent types goes to adb on its stdin: every account on this
/// computer can read a process's arguments.
#[cfg(unix)]
#[tokio::test]
async fn typed_text_is_on_no_command_line() {
    let dir = tempfile::tempdir().unwrap();
    crate::devices::recording_adb(dir.path());
    let env = HashMap::from([
        ("PATH".to_owned(), dir.path().display().to_string()),
        ("HOME".to_owned(), dir.path().display().to_string()),
    ]);
    let adb = Adb::find(&env);
    type_text(&adb, "R5CT", "my secret\tpass\nword")
        .await
        .unwrap();
    let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
    assert!(
        !["secret", "pass", "word"].iter().any(|w| argv.contains(w)),
        "{argv}"
    );
    assert!(argv.starts_with("-s\nR5CT\nshell\n"), "{argv}");
    let stdin = std::fs::read_to_string(dir.path().join("stdin")).unwrap();
    assert_eq!(
        stdin,
        "input text my%ssecret\ninput keyevent 61\ninput text pass\n\
         input keyevent 66\ninput text word\nexit\n"
    );
}

#[test]
fn reads_settings_back() {
    let text = "night=Night mode: yes\nfont=1.15\ncontrast=0\nanim=0.0\n\
                a11y=com.google.android.marvin.talkback/com.google.android.marvin.talkback.TalkBackService\n\
                talkback=package:/product/app/talkback/talkback.apk\nlayout=\nwifi=1\ndata=0\n\
                rot=mRotation=1\n\
                top=topResumedActivity=ActivityRecord{9f1 u0 com.android.settings/.Settings t12}\n\
                package:com.example.b\npackage:com.example.a\n";
    let s = parse_settings(text);
    assert_eq!(s.dark, Some(true));
    assert_eq!(s.text_size, Some(DeviceTextSize::Large));
    assert_eq!(s.contrast, Some(false));
    assert_eq!(s.reduce_motion, Some(true));
    assert_eq!(s.screen_reader, Some(true));
    assert!(s.supported.iter().any(|x| x == "screen_reader"));
    assert_eq!(s.accessibility_frames, Some(false));
    assert_eq!(s.network, Some(true));
    assert_eq!(s.orientation, Some(DeviceOrientation::LandscapeLeft));
    assert_eq!(s.foreground_app.as_deref(), Some("com.android.settings"));
    assert_eq!(s.apps, vec!["com.example.a", "com.example.b"]);
    // No TalkBack, nothing known.
    let s = parse_settings("night=\nfont=null\nwifi=0\ndata=null\n");
    assert_eq!(s.dark, None);
    assert_eq!(s.text_size, Some(DeviceTextSize::Default));
    assert_eq!(s.network, Some(false));
    assert!(!s.supported.iter().any(|x| x == "screen_reader"));
    assert_eq!(text_size(0.8), DeviceTextSize::ExtraSmall);
    assert_eq!(text_size(1.9), DeviceTextSize::Largest);
    for size in [DeviceTextSize::Small, DeviceTextSize::ExtraLarge] {
        assert_eq!(text_size(font_scale(size).parse().unwrap()), size);
    }
}

#[tokio::test]
async fn says_what_to_install() {
    let dir = tempfile::tempdir().unwrap();
    let env = HashMap::from([
        (
            "PATH".to_owned(),
            dir.path().join("bin").display().to_string(),
        ),
        ("HOME".to_owned(), dir.path().display().to_string()),
        ("USERPROFILE".to_owned(), dir.path().display().to_string()),
        ("LOCALAPPDATA".to_owned(), dir.path().display().to_string()),
    ]);
    let backend = PhoneBackend::new(&env);
    let why = backend.support().await.unwrap().unwrap_err();
    assert!(why.contains("Platform-Tools"), "{why}");
    let err = backend.shutdown("@R5CT").await.unwrap_err();
    assert!(format!("{err:#}").contains("can't be shut down"));
}

/// A stand-in adb that lists one phone of each kind.
#[cfg(unix)]
#[tokio::test]
async fn lists_phones_through_adb() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let adb = bin.join("adb");
    std::fs::write(
        &adb,
        "#!/bin/sh\n\
         if [ \"$1\" = devices ]; then\n\
           printf 'List of devices attached\\nR5CT\\tdevice usb:1 model:SM_A546B transport_id:1\\n\
         emulator-5554\\tdevice transport_id:2\\nX1\\tunauthorized usb:2 transport_id:3\\n\\n'\n\
           exit 0\n\
         fi\n\
         if [ \"$3\" = shell ]; then\n\
           printf 'market=\\nmodel=Galaxy A54\\nrelease=14\\nabi=arm64-v8a\\nPhysical size: 1080x2340\\nPhysical density: 450\\n'\n\
           exit 0\n\
         fi\n\
         exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = HashMap::from([
        ("PATH".to_owned(), bin.display().to_string()),
        ("HOME".to_owned(), dir.path().display().to_string()),
    ]);
    let mut backend = PhoneBackend::new(&env);
    backend.emulators = Emulators::None;
    backend.support().await.unwrap().unwrap();
    let devices = backend.list().await.unwrap();
    let ids: Vec<_> = devices.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["android:@R5CT", "android:@X1"]);
    assert_eq!(devices[0].name, "Galaxy A54");
    assert_eq!(devices[0].os, "Android 14");
    assert_eq!(devices[0].state, DeviceState::Running);
    assert_eq!(devices[1].state, DeviceState::Stopped);
    assert!(
        devices[1]
            .error
            .as_deref()
            .unwrap()
            .contains("Allow USB debugging")
    );
    backend.boot("@R5CT").await.unwrap();
    let err = backend.boot("@X1").await.unwrap_err();
    assert!(format!("{err:#}").contains("accept the prompt"), "{err:#}");
    let err = backend.boot("@GONE").await.unwrap_err();
    assert!(format!("{err:#}").contains("isn't connected"), "{err:#}");
    // Asked for, emulators are phones too.
    backend.emulators = Emulators::All;
    lock(&backend.devices).take();
    assert_eq!(backend.list().await.unwrap().len(), 3);
}

/// Against a real phone, or an emulator standing in for one:
/// `AGENTLOFT_TEST_PHONE=<adb serial> cargo test -p loft-core
/// devices::android_phone -- --ignored --nocapture`. It opens Settings,
/// scrolls, types into its search, turns the screen and goes Home.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a phone (or an emulator) on adb: AGENTLOFT_TEST_PHONE=<serial>"]
async fn live_a_phone_streams_and_takes_input() {
    let Ok(serial) = std::env::var("AGENTLOFT_TEST_PHONE") else {
        eprintln!("AGENTLOFT_TEST_PHONE isn't set: skipping");
        return;
    };
    let env: HashMap<String, String> = std::env::vars().collect();
    let make = |decoder: Option<&str>, limit: Option<u32>| {
        let mut b = PhoneBackend::new(&env).with_emulators(&[&serial]);
        if let Some(d) = decoder {
            b = b.with_decoder(d);
        }
        if let Some(l) = limit {
            b = b.with_record_limit(l);
        }
        b
    };
    let backend = make(None, Some(14));
    backend.support().await.unwrap().unwrap();
    let devices = backend.list().await.unwrap();
    let native = format!("@{serial}");
    let phone = devices
        .iter()
        .find(|d| d.id.phone_serial() == Some(serial.as_str()))
        .expect("the phone is listed");
    eprintln!("listed: {phone:?}");
    assert_eq!(phone.state, DeviceState::Running);
    assert!(phone.physical && phone.width > 0 && phone.os.starts_with("Android"));
    backend.boot(&native).await.unwrap();
    let adb = backend.adb.clone();
    let shell = |line: &'static str| {
        let (adb, serial) = (adb.clone(), serial.clone());
        async move { adb.script(&serial, line).await.unwrap() }
    };
    let before_rotation = shell("settings get system accelerometer_rotation").await;
    let before_user_rotation = shell("settings get system user_rotation").await;
    shell("input keyevent WAKEUP; wm dismiss-keyguard").await;
    backend
        .app(
            &native,
            DeviceApp::Launch {
                app: "com.android.settings".into(),
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    // The system's pointer trace: every touch changes the screen, so
    // pictures can be counted and timed.
    let before_pointer = shell("settings get system pointer_location").await;
    shell("settings put system pointer_location 1").await;
    let screen = backend.attach(&native).await.unwrap();
    screen.watching(true);
    let mut pictures = screen.pictures();
    let first = tokio::time::timeout(Duration::from_secs(10), pictures.wait_for(Option::is_some))
        .await
        .expect("a first picture")
        .unwrap()
        .clone()
        .unwrap();
    eprintln!("first picture {}x{}", first.width, first.height);
    assert!(first.height > first.width);

    // Pictures a second while a finger drags at 60 Hz.
    let fps = drag_and_count(&*screen, Duration::from_secs(4)).await;
    eprintln!(
        "{}: {fps:.1} pictures/s while dragging",
        backend.decoder_names()[0]
    );

    // Recordings last 14 s here, the next starting 8 s before: the longest
    // gap between pictures while dragging across the hand-over.
    let gap = longest_gap(&*screen, Duration::from_secs(12)).await;
    eprintln!("longest gap between pictures across recordings: {gap:?}");
    assert!(gap < Duration::from_millis(500));

    // Touch to picture, the screen still before.
    let mut latencies = Vec::new();
    for i in 0..8 {
        latencies.push(touch_latency(&*screen, 0.3 + 0.05 * i as f32).await);
    }
    latencies.sort();
    eprintln!("touch→picture: {latencies:?}");

    // Two fingers at once reach the system as one gesture.
    let pointers = two_fingers(&*screen, &adb, &serial).await;
    eprintln!("two fingers: the system saw {pointers} pointers at once");
    assert_eq!(pointers, 2);

    // Text: into Settings' search.
    shell("am start -a android.settings.APP_SEARCH_SETTINGS >/dev/null 2>&1 || am start -n com.google.android.settings.intelligence/com.android.settings.intelligence.search.SearchActivity >/dev/null 2>&1").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    screen
        .input(DeviceInput::Text {
            text: "bat tery".into(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let ui = shell("uiautomator dump /sdcard/agentloft-ui.xml >/dev/null; cat /sdcard/agentloft-ui.xml; rm /sdcard/agentloft-ui.xml").await;
    assert!(ui.contains("bat tery"), "the search field shows the text");
    assert!(
        screen
            .input(DeviceInput::Text {
                text: "café".into()
            })
            .await
            .is_err()
    );

    // Turned: pictures follow, landscape.
    backend
        .set(
            &native,
            DeviceSetting::Orientation {
                orientation: DeviceOrientation::LandscapeLeft,
            },
        )
        .await
        .unwrap();
    let turned = Instant::now();
    let landscape = tokio::time::timeout(
        Duration::from_secs(8),
        pictures.wait_for(|p| p.as_ref().is_some_and(|p| p.width > p.height)),
    )
    .await
    .is_ok();
    eprintln!("landscape pictures after {:?}", turned.elapsed());
    assert!(landscape, "pictures turned");
    assert!(screen.size().unwrap().0 > screen.size().unwrap().1);
    let settings = backend.settings(&native).await.unwrap();
    eprintln!("settings: {settings:?}");
    assert_eq!(settings.orientation, Some(DeviceOrientation::LandscapeLeft));
    screen
        .input(DeviceInput::Button {
            button: DeviceButton::RotateRight,
        })
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(8),
        pictures.wait_for(|p| p.as_ref().is_some_and(|p| p.height > p.width)),
    )
    .await
    .expect("portrait again")
    .map(|_| ())
    .unwrap();

    // Home.
    screen
        .input(DeviceInput::Button {
            button: DeviceButton::Home,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let top = backend.settings(&native).await.unwrap().foreground_app;
    eprintln!("after Home: {top:?}");
    assert!(top.is_some_and(|t| t.contains("launcher")));

    let png = screen.capture().await.unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    drop(pictures);
    drop(screen);

    // The other ways to make pictures, for comparison.
    for decoder in ["ffmpeg", "screencap"] {
        let backend = make(Some(decoder), None);
        if backend.decoder_names()[0] != decoder {
            continue;
        }
        backend
            .app(
                &native,
                DeviceApp::Launch {
                    app: "com.android.settings".into(),
                },
            )
            .await
            .unwrap();
        let screen = backend.attach(&native).await.unwrap();
        screen.watching(true);
        let mut pictures = screen.pictures();
        tokio::time::timeout(Duration::from_secs(10), pictures.wait_for(Option::is_some))
            .await
            .expect("a first picture")
            .unwrap();
        let fps = drag_and_count(&*screen, Duration::from_secs(4)).await;
        let mut latencies = Vec::new();
        for i in 0..4 {
            latencies.push(touch_latency(&*screen, 0.3 + 0.1 * i as f32).await);
        }
        latencies.sort();
        eprintln!("{decoder}: {fps:.1} pictures/s while dragging, touch→picture {latencies:?}");
    }

    shell("input keyevent HOME").await;
    adb.script(
        &serial,
        &format!(
            "settings put system user_rotation {}; settings put system accelerometer_rotation {}; \
             settings put system pointer_location {}",
            before_user_rotation.trim(),
            before_rotation.trim(),
            before_pointer.trim(),
        ),
    )
    .await
    .unwrap();
}

/// Drag the screen's content up and down at 60 Hz for `time`; returns
/// pictures a second.
async fn drag_and_count(screen: &dyn Screen, time: Duration) -> f64 {
    let mut pictures = screen.pictures();
    pictures.borrow_and_update();
    let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = count.clone();
    let counting = tokio::spawn(async move {
        while pictures.changed().await.is_ok() {
            pictures.borrow_and_update();
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    });
    let started = Instant::now();
    let finger = |phase, y| DeviceInput::Touch { phase, x: 0.5, y };
    screen.input(finger(TouchPhase::Down, 0.6)).await.unwrap();
    let mut i = 0u32;
    while started.elapsed() < time {
        i += 1;
        let t = (i % 60) as f32 / 60.0;
        let y = 0.6 - 0.25 * (t * std::f32::consts::PI).sin();
        screen.input(finger(TouchPhase::Move, y)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    screen.input(finger(TouchPhase::Up, 0.6)).await.unwrap();
    let elapsed = started.elapsed();
    counting.abort();
    count.load(std::sync::atomic::Ordering::Relaxed) as f64 / elapsed.as_secs_f64()
}

/// From a finger going down to the next picture, the screen still before
/// (with the pointer trace on, a touch shows at once).
async fn touch_latency(screen: &dyn Screen, x: f32) -> Duration {
    let mut pictures = screen.pictures();
    let waiting = Instant::now();
    while waiting.elapsed() < Duration::from_secs(10) {
        pictures.borrow_and_update();
        if tokio::time::timeout(Duration::from_millis(700), pictures.changed())
            .await
            .is_err()
        {
            break;
        }
    }
    let at = |phase| DeviceInput::Touch { phase, x, y: 0.75 };
    let sent = Instant::now();
    screen.input(at(TouchPhase::Down)).await.unwrap();
    let latency = tokio::time::timeout(Duration::from_secs(3), pictures.changed())
        .await
        .map(|_| sent.elapsed())
        .unwrap_or(Duration::from_secs(3));
    screen.input(at(TouchPhase::Up)).await.unwrap();
    latency
}

/// The most pointers the system's input dispatcher saw in one event while
/// two fingers went down, spread and lifted.
async fn two_fingers(screen: &dyn Screen, adb: &Adb, serial: &str) -> usize {
    let mut cmd = adb.command(Some(serial)).unwrap();
    cmd.args(["shell", "getevent -l"]);
    let watch = cmd.spawn().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let at = |phase, d: f32| DeviceInput::Touches {
        phase,
        touches: vec![
            loft_protocol::TouchPoint { x: 0.5 - d, y: 0.5 },
            loft_protocol::TouchPoint { x: 0.5 + d, y: 0.5 },
        ],
    };
    screen.input(at(TouchPhase::Down, 0.1)).await.unwrap();
    for i in 1..10 {
        screen
            .input(at(TouchPhase::Move, 0.1 + i as f32 * 0.02))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    screen.input(at(TouchPhase::Up, 0.3)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut watch = watch;
    let _ = watch.start_kill();
    let out = watch.wait_with_output().await.unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    // Tracking ids given out while both were down: count the most at once.
    let (mut down, mut most) = (0usize, 0usize);
    for line in text.lines() {
        if line.contains("ABS_MT_TRACKING_ID") {
            if line.trim_end().ends_with("ffffffff") {
                down = down.saturating_sub(1);
            } else {
                down += 1;
                most = most.max(down);
            }
        }
    }
    most
}

/// The longest wait between two pictures while a finger drags for `time`.
async fn longest_gap(screen: &dyn Screen, time: Duration) -> Duration {
    let mut pictures = screen.pictures();
    pictures.borrow_and_update();
    let longest = Arc::new(Mutex::new(Duration::ZERO));
    let measured = longest.clone();
    let gaps = tokio::spawn(async move {
        let mut last = Instant::now();
        while pictures.changed().await.is_ok() {
            pictures.borrow_and_update();
            let mut longest = lock(&measured);
            *longest = (*longest).max(last.elapsed());
            last = Instant::now();
        }
    });
    drag_and_count(screen, time).await;
    gaps.abort();
    *lock(&longest)
}
