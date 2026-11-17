use super::fake::{FakeBackend, FakeEvent};
use super::*;
use loft_protocol::{DeviceButton, DevicePlatform, TouchPhase};

fn devices() -> (Arc<Devices>, Arc<FakeBackend>, broadcast::Receiver<BusMsg>) {
    let (bus, rx) = broadcast::channel(256);
    let fake = Arc::new(FakeBackend::new(
        DevicePlatform::Ios,
        Duration::from_millis(50),
    ));
    let devices = Arc::new(Devices::new(vec![fake.clone()], bus));
    (devices, fake, rx)
}

#[tokio::test]
async fn boots_streams_and_takes_input() {
    let (devices, fake, mut bus) = devices();
    let id = fake.id();
    let (list, support) = devices.list().await;
    assert!(support.ios);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].state, DeviceState::Stopped);

    // Watching a stopped device attaches nothing yet.
    devices.watching(&id, true).await;
    assert!(devices.frame_after(&id, 0).is_none());

    let info = devices
        .boot_and_wait(&id, None, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(info.state, DeviceState::Running);

    // The watched device got its screen when it came up.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let seq = loop {
        if let Some(frame) = devices.frame_after(&id, 0) {
            break frame.seq;
        }
        assert!(tokio::time::Instant::now() < deadline, "no picture");
        let _ = tokio::time::timeout(Duration::from_millis(100), bus.recv()).await;
    };

    let thread = ThreadId::new();
    devices
        .input(
            &id,
            DeviceInput::Touch {
                phase: TouchPhase::Down,
                x: 0.5,
                y: 0.5,
            },
            Some(&thread),
        )
        .await
        .unwrap();
    assert!(fake.log().contains(&FakeEvent::Input(DeviceInput::Touch {
        phase: TouchPhase::Down,
        x: 0.5,
        y: 0.5,
    })));
    assert_eq!(devices.info(&id).unwrap().agent, Some(thread));

    // A new picture follows the touch.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while devices.frame_after(&id, seq).is_none() {
        assert!(tokio::time::Instant::now() < deadline, "no new picture");
        let _ = tokio::time::timeout(Duration::from_millis(100), bus.recv()).await;
    }

    // Turning it changes the size viewers are told.
    devices
        .input(
            &id,
            DeviceInput::Button {
                button: DeviceButton::RotateLeft,
            },
            None,
        )
        .await
        .unwrap();
    let info = devices.info(&id).unwrap();
    assert!(info.width > info.height);

    devices.shutdown(&id).await.unwrap();
    assert_eq!(devices.info(&id).unwrap().state, DeviceState::Stopped);
    assert!(devices.frame_after(&id, 0).is_none());
}

#[tokio::test]
async fn input_to_a_stopped_device_says_to_boot_it() {
    let (devices, fake, _bus) = devices();
    devices.list().await;
    let err = devices
        .input(&fake.id(), DeviceInput::Text { text: "hi".into() }, None)
        .await
        .unwrap_err();
    assert!(err.message.contains("boot"), "{}", err.message);
}

#[tokio::test]
async fn unknown_platform_is_not_found() {
    let (devices, _fake, _bus) = devices();
    let err = devices
        .input(
            &DeviceId::android("Nope"),
            DeviceInput::Text { text: "x".into() },
            None,
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("aren't available"), "{}", err.message);
}
