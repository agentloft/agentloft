//! The Agentloft MCP server's device tools (ADR-0020): agents use the iOS
//! simulators and Android emulators on the daemon's computer, the same ones
//! the user watches in the Device panel. Points are pixels of the latest
//! `device_screenshot`, which is scaled down for the model; the daemon takes
//! them as fractions of the screen.

use base64::Engine as _;
use loft_client::Client;
use loft_protocol::{
    DeviceApp, DeviceButton, DeviceId, DeviceInfo, DeviceInput, Request, Response, ThreadId,
    TouchPhase,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::mcp::{json_result, text_result, tool};

/// The longest side of a screenshot given to the model.
const MAX_SIDE: u32 = 1000;

/// What the server's instructions say about devices.
pub const INSTRUCTIONS: &str = "Devices: the `device_*` tools use the iOS simulators and \
Android emulators on this computer, which the user watches in Agentloft's Device panel. \
`device_list` shows them and `device_boot` starts one and waits until it runs. Work in a loop: \
inspect with `device_screenshot`, act with `device_tap`, `device_swipe`, `device_type`, \
`device_press` or `device_app`, then take a new screenshot to verify, since an action that \
returned proves nothing. Points are pixels of your latest screenshot of that device.";

/// The size of the last screenshot each device was given to the model in:
/// what the agent's points are in.
static PICTURE_SIZES: LazyLock<Mutex<HashMap<DeviceId, (u32, u32)>>> =
    LazyLock::new(Mutex::default);

fn sizes() -> std::sync::MutexGuard<'static, HashMap<DeviceId, (u32, u32)>> {
    PICTURE_SIZES.lock().unwrap_or_else(|e| e.into_inner())
}

fn id_property() -> Value {
    json!({ "type": "string", "description": "The device's id from device_list, e.g. \"ios:<UDID>\" or \"android:<AVD name>\"." })
}

/// `properties` with the device's `id` added.
fn with_id(mut properties: Value) -> Value {
    if let Some(map) = properties.as_object_mut() {
        map.insert("id".into(), id_property());
    }
    properties
}

pub fn tools() -> Vec<Value> {
    let point = |what: &str| json!({ "type": "number", "description": what });
    vec![
        tool(
            "device_list",
            "List the iOS simulators and Android emulators on this computer: id, name, OS, state (stopped, booting, running), screen size, and whether an agent is using it. When a platform can't run here, says what is missing.",
            json!({}),
            &[],
        ),
        tool(
            "device_boot",
            "Start a device (headless; the user sees it in Agentloft's Device panel) and wait until it runs, up to 3 minutes. Does nothing to one that already runs. Then take a device_screenshot.",
            with_id(json!({})),
            &["id"],
        ),
        tool(
            "device_screenshot",
            "The device's screen as an image, scaled so its longer side is at most 1000 pixels. The answer gives the picture's size: device_tap and device_swipe take points in its pixels. Take a new one after acting, to see what happened.",
            with_id(json!({})),
            &["id"],
        ),
        tool(
            "device_tap",
            "Tap the screen at x, y (pixels of your latest device_screenshot of this device, 0,0 top left). hold_ms holds the finger down: a long press is about 800.",
            with_id(json!({
                "x": point("From the left, in screenshot pixels."),
                "y": point("From the top, in screenshot pixels."),
                "hold_ms": { "type": "integer", "minimum": 0, "maximum": 10000, "description": "How long to hold before lifting (default: a quick tap)." }
            })),
            &["id", "x", "y"],
        ),
        tool(
            "device_swipe",
            "Drag a finger from one point to another over duration_ms (screenshot pixels). Swipe up to scroll down, swipe from an edge for system gestures (from the bottom edge up: home on iOS and gesture-navigation Android).",
            with_id(json!({
                "from_x": point("Start, from the left."),
                "from_y": point("Start, from the top."),
                "to_x": point("End, from the left."),
                "to_y": point("End, from the top."),
                "duration_ms": { "type": "integer", "minimum": 50, "maximum": 10000, "description": "Default 300; slower swipes scroll less far." }
            })),
            &["id", "from_x", "from_y", "to_x", "to_y"],
        ),
        tool(
            "device_type",
            "Type text into whatever has the device's focus (tap a field first). submit: press Enter afterwards.",
            with_id(json!({
                "text": { "type": "string" },
                "submit": { "type": "boolean", "default": false }
            })),
            &["id", "text"],
        ),
        tool(
            "device_press",
            "Press a key or one of the device's buttons (exactly one of them). count repeats it, e.g. Backspace to clear a field.",
            with_id(json!({
                "key": { "type": "string", "description": "A key without text: Enter, Backspace, Tab, Escape, ArrowLeft, ArrowRight, ArrowUp, ArrowDown." },
                "button": { "type": "string", "enum": ["home", "back", "recents", "power", "volume_up", "volume_down", "rotate_left", "rotate_right"], "description": "back is Android only; recents is the app switcher. Rotating changes the screenshot's size." },
                "count": { "type": "integer", "minimum": 1, "maximum": 50, "description": "How many times (default 1)." }
            })),
            &["id"],
        ),
        tool(
            "device_app",
            "Manage apps on a running device. install: a built app at an absolute path on this computer (an .app folder for an iOS simulator, an .apk for Android). launch, terminate, uninstall: by bundle id (iOS) or package name (Android). open_url: a web address or an app's own link.",
            with_id(json!({
                "action": { "type": "string", "enum": ["install", "launch", "terminate", "uninstall", "open_url"] },
                "path": { "type": "string", "description": "For install." },
                "app": { "type": "string", "description": "For launch, terminate and uninstall: e.g. com.example.App." },
                "url": { "type": "string", "description": "For open_url." }
            })),
            &["id", "action"],
        ),
        tool(
            "device_shutdown",
            "Shut a device down. Only for devices you booted and no longer need.",
            with_id(json!({})),
            &["id"],
        ),
    ]
}

/// Run a device tool; `None` if `name` isn't one.
pub async fn call(client: &Client, thread: &ThreadId, name: &str, args: &Value) -> Option<Value> {
    if !name.starts_with("device_") {
        return None;
    }
    if name == "device_list" {
        return Some(list(client, thread).await);
    }
    let Some(id) = args
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    else {
        return Some(text_result(
            "id is required: a device's id from device_list.",
            true,
        ));
    };
    let id = DeviceId(id.trim().to_owned());
    let result = match name {
        "device_boot" => boot(client, thread, &id).await,
        "device_screenshot" => return Some(screenshot(client, thread, &id).await),
        "device_tap" => tap(client, thread, &id, args).await,
        "device_swipe" => swipe(client, thread, &id, args).await,
        "device_type" => type_text(client, thread, &id, args).await,
        "device_press" => press(client, thread, &id, args).await,
        "device_app" => app(client, thread, &id, args).await,
        "device_shutdown" => shutdown(client, &id).await,
        other => Err(format!("unknown tool: {other}")),
    };
    Some(match result {
        Ok(v) => v,
        Err(e) => text_result(e, true),
    })
}

/// A device as the agent is told it.
fn describe(device: &DeviceInfo, me: &ThreadId) -> Value {
    let mut v = json!({
        "id": device.id,
        "name": device.name,
        "os": device.os,
        "state": device.state,
    });
    if device.width > 0 && device.height > 0 {
        v["screen"] = json!(format!(
            "{}x{} pixels at {}x",
            device.width, device.height, device.scale
        ));
    }
    if let Some(error) = &device.error {
        v["error"] = json!(error);
    }
    if let Some(agent) = &device.agent {
        v["used_by"] = json!(if agent == me {
            "this thread"
        } else {
            "another thread"
        });
    }
    v
}

async fn list(client: &Client, me: &ThreadId) -> Value {
    match client.request(Request::DeviceList).await {
        Ok(Response::Devices { devices, support }) => {
            let mut v = json!({
                "devices": devices.iter().map(|d| describe(d, me)).collect::<Vec<_>>(),
            });
            let mut missing = serde_json::Map::new();
            if !support.ios {
                missing.insert(
                    "ios".into(),
                    json!(
                        support
                            .ios_problem
                            .as_deref()
                            .unwrap_or("iOS simulators aren't available here.")
                    ),
                );
            }
            if !support.android {
                missing.insert(
                    "android".into(),
                    json!(
                        support
                            .android_problem
                            .as_deref()
                            .unwrap_or("Android emulators aren't available here.")
                    ),
                );
            }
            if !missing.is_empty() {
                v["unavailable"] = Value::Object(missing);
            }
            json_result(&v)
        }
        Ok(other) => text_result(format!("unexpected response: {other:?}"), true),
        Err(err) => text_result(err.message, true),
    }
}

async fn boot(client: &Client, thread: &ThreadId, id: &DeviceId) -> Result<Value, String> {
    match client
        .request(Request::DeviceBoot {
            id: id.clone(),
            thread_id: Some(thread.clone()),
            wait: true,
        })
        .await
    {
        Ok(Response::Device { device }) => {
            let mut v = describe(&device, thread);
            v["next"] = json!("It is running. Take a device_screenshot to see its screen.");
            Ok(json_result(&v))
        }
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(err) => Err(err.message),
    }
}

async fn shutdown(client: &Client, id: &DeviceId) -> Result<Value, String> {
    client
        .request(Request::DeviceShutdown { id: id.clone() })
        .await
        .map_err(|e| e.message)?;
    sizes().remove(id);
    Ok(text_result(format!("{id} is shut down."), false))
}

/// The size a picture of `width` x `height` is given to the model in.
pub(crate) fn fit(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= MAX_SIDE || long == 0 {
        return (width, height);
    }
    let scale = MAX_SIDE as f64 / long as f64;
    let side = |n: u32| ((n as f64 * scale).round() as u32).max(1);
    (side(width), side(height))
}

/// A screenshot as the model gets it.
struct Scaled {
    jpeg: Vec<u8>,
    /// The picture's size: what the agent's points are in.
    size: (u32, u32),
    /// The screen's.
    screen: (u32, u32),
}

/// A PNG of the screen, scaled to [`fit`], as a JPEG.
fn scale_down(png: &[u8]) -> Result<Scaled, String> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| format!("The device's screenshot couldn't be read: {e}"))?;
    let screen = (img.width(), img.height());
    let size = fit(screen.0, screen.1);
    let img = if size == screen {
        img.to_rgb8()
    } else {
        image::imageops::resize(
            &img.to_rgb8(),
            size.0,
            size.1,
            image::imageops::FilterType::Triangle,
        )
    };
    let mut jpeg = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85);
    img.write_with_encoder(encoder)
        .map_err(|e| format!("The screenshot couldn't be encoded: {e}"))?;
    Ok(Scaled { jpeg, size, screen })
}

async fn screenshot(client: &Client, thread: &ThreadId, id: &DeviceId) -> Value {
    let png = match client
        .request(Request::DeviceCapture {
            id: id.clone(),
            thread_id: Some(thread.clone()),
        })
        .await
    {
        Ok(Response::Picture { png }) => png.0,
        Ok(other) => return text_result(format!("unexpected response: {other:?}"), true),
        Err(err) => return text_result(err.message, true),
    };
    let scaled = tokio::task::spawn_blocking(move || scale_down(&png)).await;
    let Scaled {
        jpeg,
        size: (w, h),
        screen: (sw, sh),
    } = match scaled {
        Ok(Ok(scaled)) => scaled,
        Ok(Err(e)) => return text_result(e, true),
        Err(e) => return text_result(e.to_string(), true),
    };
    sizes().insert(id.clone(), (w, h));
    let about = if (w, h) == (sw, sh) {
        format!("{id}: {w}x{h} pixels. device_tap and device_swipe take points in these pixels.")
    } else {
        format!(
            "{id}: {w}x{h} pixels (the screen is {sw}x{sh}, scaled down). device_tap and \
             device_swipe take points in this picture's pixels."
        )
    };
    json!({
        "content": [
            {
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(&jpeg),
                "mimeType": "image/jpeg",
            },
            { "type": "text", "text": about },
        ],
        "isError": false,
    })
}

/// The size of the picture the agent's points are in: its last screenshot
/// of `id`, or else the device's screen as a screenshot would show it.
async fn picture_size(client: &Client, id: &DeviceId) -> Result<(u32, u32), String> {
    if let Some(size) = sizes().get(id) {
        return Ok(*size);
    }
    let Ok(Response::Devices { devices, .. }) = client.request(Request::DeviceList).await else {
        return Err("Take a device_screenshot first: points are in its pixels.".into());
    };
    match devices.iter().find(|d| d.id == *id) {
        None => Err(format!("No device {id}: device_list shows the ids.")),
        Some(d) if d.width > 0 && d.height > 0 => Ok(fit(d.width, d.height)),
        Some(_) => Err("Take a device_screenshot first: points are in its pixels.".into()),
    }
}

/// A point of the screenshot, as fractions of the screen.
fn fraction(args: &Value, kx: &str, ky: &str, (w, h): (u32, u32)) -> Result<(f32, f32), String> {
    let coord = |key: &str, max: u32| -> Result<f32, String> {
        let v = args
            .get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("{key} is required (a number of screenshot pixels)."))?;
        if !(0.0..=max as f64).contains(&v) {
            return Err(format!(
                "{key} = {v} is outside the screenshot, which is {w}x{h} pixels."
            ));
        }
        Ok((v / max as f64) as f32)
    };
    Ok((coord(kx, w)?, coord(ky, h)?))
}

async fn send(
    client: &Client,
    thread: &ThreadId,
    id: &DeviceId,
    input: DeviceInput,
) -> Result<(), String> {
    client
        .request(Request::DeviceInput {
            id: id.clone(),
            input,
            thread_id: Some(thread.clone()),
        })
        .await
        .map(|_| ())
        .map_err(|e| e.message)
}

fn touch(phase: TouchPhase, (x, y): (f32, f32)) -> DeviceInput {
    DeviceInput::Touch { phase, x, y }
}

/// A finger that went down is lifted even when what followed failed.
async fn lift_after(
    client: &Client,
    thread: &ThreadId,
    id: &DeviceId,
    at: (f32, f32),
    moved: Result<(), String>,
) -> Result<(), String> {
    match moved {
        Ok(()) => send(client, thread, id, touch(TouchPhase::Up, at)).await,
        Err(e) => {
            let _ = send(client, thread, id, touch(TouchPhase::Cancel, at)).await;
            Err(e)
        }
    }
}

const DONE: &str = "Done. Take a device_screenshot to see the result.";

async fn tap(
    client: &Client,
    thread: &ThreadId,
    id: &DeviceId,
    args: &Value,
) -> Result<Value, String> {
    let size = picture_size(client, id).await?;
    let at = fraction(args, "x", "y", size)?;
    let hold = args
        .get("hold_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(10_000);
    send(client, thread, id, touch(TouchPhase::Down, at)).await?;
    // A quick tap still lasts a moment, as a finger's does.
    tokio::time::sleep(Duration::from_millis(hold.max(40))).await;
    send(client, thread, id, touch(TouchPhase::Up, at)).await?;
    Ok(text_result(DONE, false))
}

async fn swipe(
    client: &Client,
    thread: &ThreadId,
    id: &DeviceId,
    args: &Value,
) -> Result<Value, String> {
    let size = picture_size(client, id).await?;
    let from = fraction(args, "from_x", "from_y", size)?;
    let to = fraction(args, "to_x", "to_y", size)?;
    let duration = args
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(300)
        .clamp(50, 10_000);
    // About one move a frame, within reason.
    let steps = (duration / 16).clamp(4, 60) as u32;
    let pause = Duration::from_millis(duration / steps as u64);
    send(client, thread, id, touch(TouchPhase::Down, from)).await?;
    let mut moved = Ok(());
    for i in 1..=steps {
        tokio::time::sleep(pause).await;
        let t = i as f32 / steps as f32;
        let at = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
        moved = send(client, thread, id, touch(TouchPhase::Move, at)).await;
        if moved.is_err() {
            break;
        }
    }
    lift_after(client, thread, id, to, moved).await?;
    Ok(text_result(DONE, false))
}

async fn type_text(
    client: &Client,
    thread: &ThreadId,
    id: &DeviceId,
    args: &Value,
) -> Result<Value, String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or("text is required.")?;
    if !text.is_empty() {
        send(
            client,
            thread,
            id,
            DeviceInput::Text {
                text: text.to_owned(),
            },
        )
        .await?;
    }
    if args.get("submit").and_then(Value::as_bool) == Some(true) {
        send(client, thread, id, key("Enter")).await?;
    }
    Ok(text_result(DONE, false))
}

fn key(name: &str) -> DeviceInput {
    DeviceInput::Key {
        key: name.to_owned(),
        modifiers: 0,
    }
}

/// `device_press`'s `button` names.
fn button(name: &str) -> Option<DeviceButton> {
    serde_json::from_value(json!(name.trim().to_ascii_lowercase())).ok()
}

async fn press(
    client: &Client,
    thread: &ThreadId,
    id: &DeviceId,
    args: &Value,
) -> Result<Value, String> {
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let input = match (str_arg("key"), str_arg("button")) {
        (Some(k), None) => key(k),
        (None, Some(b)) => DeviceInput::Button {
            button: button(b).ok_or_else(|| {
                format!(
                    "Unknown button `{b}`: home, back, recents, power, volume_up, volume_down, \
                     rotate_left or rotate_right."
                )
            })?,
        },
        _ => return Err("Give exactly one of key and button.".into()),
    };
    let count = args
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 50);
    for _ in 0..count {
        send(client, thread, id, input.clone()).await?;
    }
    let turned = matches!(
        input,
        DeviceInput::Button {
            button: DeviceButton::RotateLeft | DeviceButton::RotateRight
        }
    );
    if turned {
        // The next screenshot has another shape.
        sizes().remove(id);
        return Ok(text_result(
            "Done. The screen turned: take a device_screenshot before tapping again.",
            false,
        ));
    }
    Ok(text_result(DONE, false))
}

async fn app(
    client: &Client,
    thread: &ThreadId,
    id: &DeviceId,
    args: &Value,
) -> Result<Value, String> {
    let need = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("{k} is required for this action."))
    };
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (app, done) = match action {
        "install" => (
            DeviceApp::Install {
                path: need("path")?,
            },
            "Installed.",
        ),
        "launch" => (DeviceApp::Launch { app: need("app")? }, "Launched."),
        "terminate" => (DeviceApp::Terminate { app: need("app")? }, "Stopped."),
        "uninstall" => (DeviceApp::Uninstall { app: need("app")? }, "Uninstalled."),
        "open_url" => (DeviceApp::OpenUrl { url: need("url")? }, "Opened."),
        other => {
            return Err(format!(
                "Unknown action `{other}`: install, launch, terminate, uninstall or open_url."
            ));
        }
    };
    client
        .request(Request::DeviceApp {
            id: id.clone(),
            app,
            thread_id: Some(thread.clone()),
        })
        .await
        .map_err(|e| e.message)?;
    Ok(text_result(done, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_fit_in_a_thousand_pixels() {
        assert_eq!(fit(1179, 2556), (461, 1000));
        assert_eq!(fit(2556, 1179), (1000, 461));
        assert_eq!(fit(390, 844), (390, 844));
        assert_eq!(fit(0, 0), (0, 0));
    }

    #[test]
    fn large_screenshots_are_scaled_down() {
        let img = image::RgbImage::from_pixel(1179, 2556, image::Rgb([10, 20, 30]));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let scaled = scale_down(&png).unwrap();
        assert_eq!(scaled.size, (461, 1000));
        assert_eq!(scaled.screen, (1179, 2556));
        let jpeg = image::load_from_memory(&scaled.jpeg).unwrap();
        assert_eq!((jpeg.width(), jpeg.height()), (461, 1000));
        assert!(scale_down(b"not a png").is_err());
    }

    #[test]
    fn points_become_fractions_of_the_screen() {
        let size = (400, 1000);
        let (x, y) = fraction(&json!({"x": 100, "y": 500}), "x", "y", size).unwrap();
        assert!((x - 0.25).abs() < 1e-6 && (y - 0.5).abs() < 1e-6);
        let err = fraction(&json!({"x": 401, "y": 0}), "x", "y", size).unwrap_err();
        assert!(err.contains("400x1000"), "{err}");
        assert!(fraction(&json!({"x": 1}), "x", "y", size).is_err());
    }

    #[test]
    fn buttons_have_their_wire_names() {
        assert_eq!(button("home"), Some(DeviceButton::Home));
        assert_eq!(button("Volume_Up"), Some(DeviceButton::VolumeUp));
        assert_eq!(button("rotate_left"), Some(DeviceButton::RotateLeft));
        assert_eq!(button("jump"), None);
    }

    #[test]
    fn device_tools_are_listed_without_a_tab() {
        let tools = crate::mcp::tools();
        for name in [
            "device_list",
            "device_boot",
            "device_screenshot",
            "device_tap",
            "device_swipe",
            "device_type",
            "device_press",
            "device_app",
            "device_shutdown",
        ] {
            let t = tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("{name} isn't listed"));
            assert!(t["inputSchema"]["properties"].get("tab").is_none());
            if name != "device_list" {
                assert!(
                    t["inputSchema"]["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("id"))
                );
            }
        }
    }
}
