//! The Agentloft MCP server's device tools (ADR-0020): agents use the iOS
//! simulators and Android emulators on the daemon's computer, the same ones
//! the user watches in the Device panel. Points are pixels of the latest
//! screenshot, which is scaled down for the model; the daemon takes them as
//! fractions of the screen.
//!
//! The same operations back `agent-device`, the command line agents have in
//! their shells (`device_cli.rs`): [`run`] does a tool's work and answers an
//! [`Outcome`] that each of them words its own way. The size of the last
//! screenshot of each device is kept per thread in a file ([`Pictures`]), so
//! points mean the same whichever of the two took it.

use base64::Engine as _;
use loft_client::Client;
use loft_config::Paths;
use loft_protocol::{
    DeviceApp, DeviceButton, DeviceId, DeviceInfo, DeviceInput, DeviceOrientation, DevicePosture,
    DeviceSetting, DeviceTextSize, Request, Response, ThreadId, TouchPhase, TouchPoint,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::mcp::{json_result, text_result, tool};

/// The longest side of a screenshot given to the model.
const MAX_SIDE: u32 = 1000;

/// What the server's instructions say about devices.
pub const INSTRUCTIONS: &str = "Devices: the `device_*` tools use the iOS simulators and \
Android emulators on this computer, which the user watches in Agentloft's Device panel. \
`device_list` shows them and `device_boot` starts one and waits until it runs. Work in a loop: \
inspect with `device_screenshot`, act with `device_tap`, `device_swipe`, `device_pinch`, \
`device_gesture` (several fingers), `device_type`, `device_press` or `device_app`, then take a \
new screenshot to verify, since an action that returned proves nothing. Points are pixels of \
your latest screenshot of that device. `device_settings` reads a device's settings and \
`device_set` changes them (appearance, text size, contrast, reduce motion, screen reader, \
accessibility frames, network, orientation, location); `device_fold` folds a foldable; \
`device_permission` grants or revokes an app's permission and `device_push` sends it a test \
notification. The same things are available in your shell as `agent-device` (`agent-device \
help`).";

/// The size of the last screenshot of each device given to one thread's
/// agent, through the tools or the command line: what its points are in.
/// A file in the daemon's run folder, since each `agent-device` is a
/// process of its own.
#[derive(Debug, Clone)]
pub struct Pictures {
    file: PathBuf,
}

impl Pictures {
    /// `thread`: the agent's; none for a command line run outside one.
    pub fn new(paths: &Paths, thread: Option<&ThreadId>) -> Self {
        let name: String = thread
            .map_or("shell", |t| t.as_str())
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Self {
            file: paths
                .run_dir()
                .join("device-pictures")
                .join(format!("{name}.json")),
        }
    }

    fn load(&self) -> BTreeMap<String, (u32, u32)> {
        std::fs::read(&self.file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, sizes: &BTreeMap<String, (u32, u32)>) {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            // Whole or not at all: another call may be reading it.
            let tmp = self
                .file
                .with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&tmp, serde_json::to_vec(sizes)?)?;
            std::fs::rename(&tmp, &self.file)
        };
        if let Err(err) = write() {
            tracing::warn!(%err, "couldn't keep the screenshot's size");
        }
    }

    fn get(&self, id: &DeviceId) -> Option<(u32, u32)> {
        self.load().get(id.as_str()).copied()
    }

    fn set(&self, id: &DeviceId, size: (u32, u32)) {
        let mut sizes = self.load();
        sizes.insert(id.0.clone(), size);
        self.save(&sizes);
    }

    /// The screen changed shape: points wait for a new screenshot (or the
    /// screen's own size).
    fn forget(&self, id: &DeviceId) {
        let mut sizes = self.load();
        if sizes.remove(id.as_str()).is_some() {
            self.save(&sizes);
        }
    }
}

/// What a device operation runs with.
pub struct Ctx<'a> {
    pub client: &'a Client,
    /// The thread whose agent acts: its windows show the device it uses.
    pub thread: Option<&'a ThreadId>,
    pub pictures: &'a Pictures,
}

impl Ctx<'_> {
    async fn request(&self, request: Request) -> Result<Response, String> {
        self.client.request(request).await.map_err(|e| e.message)
    }
}

/// What an operation answers, for the MCP server or the command line to
/// word.
pub enum Outcome {
    /// Done: what to tell the agent.
    Done(String),
    /// Something read back.
    Data(Value),
    Shot(Shot),
}

/// A screenshot as the agent gets it.
pub struct Shot {
    pub id: DeviceId,
    /// Scaled to fit [`MAX_SIDE`]: the agent's points are in its pixels.
    pub image: image::RgbImage,
    /// The screen's size.
    pub screen: (u32, u32),
}

impl Shot {
    pub fn size(&self) -> (u32, u32) {
        self.image.dimensions()
    }
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
    let on_off = |what: &str| json!({ "type": "boolean", "description": what });
    vec![
        tool(
            "device_list",
            "List the iOS simulators and Android emulators on this computer (and Android phones plugged in): id, name, OS, state (stopped, booting, running), screen size, how a foldable is folded, and whether an agent is using it. When a platform can't run here, says what is missing.",
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
            "The device's screen as an image, scaled so its longer side is at most 1000 pixels. The answer gives the picture's size: device_tap, device_swipe, device_pinch and device_gesture take points in its pixels. Take a new one after acting, to see what happened.",
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
            "device_pinch",
            "Pinch with two fingers around x, y (screenshot pixels), side by side: a scale above 1 spreads them apart (zoom in), below 1 brings them together (zoom out). 2 doubles the distance between them, 0.5 halves it.",
            with_id(json!({
                "x": point("The middle between the fingers, from the left."),
                "y": point("The middle between the fingers, from the top."),
                "scale": { "type": "number", "minimum": 0.1, "maximum": 10, "description": "How much farther apart the fingers end than they start." },
                "duration_ms": { "type": "integer", "minimum": 50, "maximum": 10000, "description": "Default 400." }
            })),
            &["id", "x", "y", "scale"],
        ),
        tool(
            "device_gesture",
            "Move one to five fingers at once along paths (screenshot pixels), for what the other tools don't do: a two-finger turn, a three-finger swipe, a drag along a curve. Each finger is a list of [x, y] points it passes through, evenly over duration_ms: all go down together at their first point and lift together at their last. A finger with one point holds still.",
            with_id(json!({
                "fingers": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 5,
                    "items": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 }
                    },
                    "description": "Per finger, its [x, y] points, e.g. [[[100, 300], [200, 300]], [[300, 300], [200, 300]]]."
                },
                "duration_ms": { "type": "integer", "minimum": 50, "maximum": 10000, "description": "Default 400." }
            })),
            &["id", "fingers"],
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
            "device_fold",
            "Fold or unfold a foldable (device_list shows a `fold` for one): a posture, or the hinge's angle. Folded shut, the screen is the cover screen, of another size: take a new device_screenshot before tapping.",
            with_id(json!({
                "posture": { "type": "string", "enum": ["closed", "half_opened", "opened", "flipped", "tent"], "description": "closed: shut; half_opened: a laptop on a table or an open book; opened: flat. flipped and tent are for fold-outs." },
                "hinge": { "type": "number", "minimum": 0, "maximum": 360, "description": "Instead of a posture: degrees, 0 shut and 180 flat." }
            })),
            &["id"],
        ),
        tool(
            "device_settings",
            "A running device's settings as they are now: appearance (dark), text size, contrast, reduce motion, screen reader, accessibility frames, network, orientation, the app in front and the apps installed, which settings it takes (supported), and a foldable's fold.",
            with_id(json!({})),
            &["id"],
        ),
        tool(
            "device_set",
            "Change one or more of a running device's settings (give only those), then read them back.",
            with_id(json!({
                "appearance": { "type": "string", "enum": ["light", "dark"] },
                "text_size": { "type": "string", "enum": ["extra_small", "small", "default", "large", "extra_large", "largest"], "description": "largest: the biggest accessibility size." },
                "high_contrast": on_off("Increase Contrast (iOS), high-contrast text (Android)."),
                "reduce_motion": on_off("Reduce Motion (iOS), remove animations (Android)."),
                "screen_reader": on_off("VoiceOver (iOS), TalkBack (Android)."),
                "accessibility_frames": on_off("Outline every element accessibility sees, over the screen (Android's layout bounds)."),
                "network": on_off("The network: off to try the app offline (Android: Wi-Fi and mobile data)."),
                "orientation": { "type": "string", "enum": ["portrait", "landscape_left", "landscape_right", "portrait_upside_down"], "description": "Turns the screenshot too." },
                "location": {
                    "type": "object",
                    "properties": { "latitude": { "type": "number" }, "longitude": { "type": "number" } },
                    "required": ["latitude", "longitude"],
                    "description": "A simulated location, in degrees."
                }
            })),
            &["id"],
        ),
        tool(
            "device_permission",
            "Grant or revoke one of an app's permissions, without its prompt. permission is the platform's own name: on iOS photos, camera, location, contacts, microphone, calendar, reminders, all…; on Android android.permission.CAMERA (or just CAMERA).",
            with_id(json!({
                "app": { "type": "string", "description": "Bundle id (iOS) or package name (Android)." },
                "permission": { "type": "string" },
                "grant": { "type": "boolean", "description": "true grants, false revokes." }
            })),
            &["id", "app", "permission", "grant"],
        ),
        tool(
            "device_push",
            "Send an app a test push notification: a title and body, or a whole payload (an APNs payload on iOS; on Android its aps.alert title and body are posted as a notification).",
            with_id(json!({
                "app": { "type": "string", "description": "Bundle id (iOS) or package name (Android)." },
                "title": { "type": "string" },
                "body": { "type": "string" },
                "payload": { "type": "object", "description": "Instead of title and body, e.g. {\"aps\": {\"alert\": {\"title\": \"Hi\", \"body\": \"…\"}, \"badge\": 1}}." }
            })),
            &["id", "app"],
        ),
        tool(
            "device_shutdown",
            "Shut a device down. Only for devices you booted and no longer need.",
            with_id(json!({})),
            &["id"],
        ),
    ]
}

/// Run a device tool for the MCP server; `None` if `name` isn't one.
pub async fn call(ctx: &Ctx<'_>, name: &str, args: &Value) -> Option<Value> {
    if !name.starts_with("device_") {
        return None;
    }
    Some(match run(ctx, name, args).await {
        Ok(Outcome::Done(text)) => text_result(text, false),
        Ok(Outcome::Data(v)) => json_result(&v),
        Ok(Outcome::Shot(shot)) => shot_result(shot).await,
        Err(e) => text_result(e, true),
    })
}

/// Do what the tool `name` does with its arguments `args`.
pub async fn run(ctx: &Ctx<'_>, name: &str, args: &Value) -> Result<Outcome, String> {
    if name == "device_list" {
        return list(ctx).await;
    }
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("id is required: a device's id, as the list of devices shows it.")?;
    let id = DeviceId(id.to_owned());
    match name {
        "device_boot" => boot(ctx, &id).await,
        "device_screenshot" => screenshot(ctx, &id).await.map(Outcome::Shot),
        "device_tap" => tap(ctx, &id, args).await,
        "device_swipe" => swipe(ctx, &id, args).await,
        "device_pinch" => pinch(ctx, &id, args).await,
        "device_gesture" => gesture(ctx, &id, args).await,
        "device_type" => type_text(ctx, &id, args).await,
        "device_press" => press(ctx, &id, args).await,
        "device_app" => app(ctx, &id, args).await,
        "device_fold" => fold(ctx, &id, args).await,
        "device_settings" => settings(ctx, &id).await,
        "device_set" => set(ctx, &id, args).await,
        "device_permission" => permission(ctx, &id, args).await,
        "device_push" => push(ctx, &id, args).await,
        "device_shutdown" => shutdown(ctx, &id).await,
        other => Err(format!("unknown tool: {other}")),
    }
}

fn unexpected(response: Response) -> String {
    format!("unexpected response: {response:?}")
}

/// A device as the agent is told it.
pub fn describe(device: &DeviceInfo, me: Option<&ThreadId>) -> Value {
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
    if device.physical {
        v["physical"] = json!(true);
    }
    if let Some(fold) = &device.fold {
        v["fold"] = json!(fold);
    }
    if let Some(error) = &device.error {
        v["error"] = json!(error);
    }
    if let Some(agent) = &device.agent {
        v["used_by"] = json!(if Some(agent) == me {
            "this thread"
        } else {
            "another thread"
        });
    }
    v
}

async fn list(ctx: &Ctx<'_>) -> Result<Outcome, String> {
    let (devices, support) = match ctx.request(Request::DeviceList).await? {
        Response::Devices { devices, support } => (devices, support),
        other => return Err(unexpected(other)),
    };
    let mut v = json!({
        "devices": devices.iter().map(|d| describe(d, ctx.thread)).collect::<Vec<_>>(),
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
    Ok(Outcome::Data(v))
}

/// One device as the daemon lists it.
async fn device_info(ctx: &Ctx<'_>, id: &DeviceId) -> Result<DeviceInfo, String> {
    match ctx.request(Request::DeviceList).await? {
        Response::Devices { devices, .. } => devices
            .into_iter()
            .find(|d| d.id == *id)
            .ok_or_else(|| format!("No device {id}: the list of devices shows their ids.")),
        other => Err(unexpected(other)),
    }
}

async fn boot(ctx: &Ctx<'_>, id: &DeviceId) -> Result<Outcome, String> {
    let request = Request::DeviceBoot {
        id: id.clone(),
        thread_id: ctx.thread.cloned(),
        wait: true,
    };
    match ctx.request(request).await? {
        Response::Device { device } => {
            let mut v = describe(&device, ctx.thread);
            v["next"] = json!("It is running. Take a screenshot to see its screen.");
            Ok(Outcome::Data(v))
        }
        other => Err(unexpected(other)),
    }
}

async fn shutdown(ctx: &Ctx<'_>, id: &DeviceId) -> Result<Outcome, String> {
    ctx.request(Request::DeviceShutdown { id: id.clone() })
        .await?;
    ctx.pictures.forget(id);
    Ok(Outcome::Done(format!("{id} is shut down.")))
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

/// A PNG of the screen, scaled to [`fit`]; and the screen's size.
fn scale_down(png: &[u8]) -> Result<(image::RgbImage, (u32, u32)), String> {
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
    Ok((img, screen))
}

/// A screenshot as a JPEG (for the model) or a PNG (a file).
pub fn encode(image: &image::RgbImage, format: image::ImageFormat) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let written = if format == image::ImageFormat::Jpeg {
        image.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut out, 85,
        ))
    } else {
        image.write_to(&mut std::io::Cursor::new(&mut out), format)
    };
    written.map_err(|e| format!("The screenshot couldn't be encoded: {e}"))?;
    Ok(out)
}

async fn screenshot(ctx: &Ctx<'_>, id: &DeviceId) -> Result<Shot, String> {
    let request = Request::DeviceCapture {
        id: id.clone(),
        thread_id: ctx.thread.cloned(),
    };
    let png = match ctx.request(request).await? {
        Response::Picture { png } => png.0,
        other => return Err(unexpected(other)),
    };
    let (image, screen) = tokio::task::spawn_blocking(move || scale_down(&png))
        .await
        .map_err(|e| e.to_string())??;
    ctx.pictures.set(id, image.dimensions());
    Ok(Shot {
        id: id.clone(),
        image,
        screen,
    })
}

/// A screenshot as the MCP server answers it: the picture, then its size.
async fn shot_result(shot: Shot) -> Value {
    let (id, (w, h), (sw, sh)) = (shot.id.clone(), shot.size(), shot.screen);
    let jpeg =
        match tokio::task::spawn_blocking(move || encode(&shot.image, image::ImageFormat::Jpeg))
            .await
        {
            Ok(Ok(jpeg)) => jpeg,
            Ok(Err(e)) => return text_result(e, true),
            Err(e) => return text_result(e.to_string(), true),
        };
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

const SCREENSHOT_FIRST: &str = "Take a screenshot first: points are in its pixels.";

/// The size of the picture the agent's points are in: its last screenshot
/// of `id`, or else the device's screen as a screenshot would show it.
async fn picture_size(ctx: &Ctx<'_>, id: &DeviceId) -> Result<(u32, u32), String> {
    if let Some(size) = ctx.pictures.get(id) {
        return Ok(size);
    }
    let device = device_info(ctx, id).await.map_err(|e| {
        if e.starts_with("No device") {
            e
        } else {
            SCREENSHOT_FIRST.into()
        }
    })?;
    if device.width > 0 && device.height > 0 {
        Ok(fit(device.width, device.height))
    } else {
        Err(SCREENSHOT_FIRST.into())
    }
}

/// `v` pixels of a side `max` long, as a fraction of it.
fn to_fraction(v: f64, max: u32, key: &str, (w, h): (u32, u32)) -> Result<f32, String> {
    if !(0.0..=max as f64).contains(&v) || max == 0 {
        return Err(format!(
            "{key} = {v} is outside the screenshot, which is {w}x{h} pixels."
        ));
    }
    Ok((v / max as f64) as f32)
}

/// A point of the screenshot, as fractions of the screen.
fn fraction(args: &Value, kx: &str, ky: &str, size: (u32, u32)) -> Result<(f32, f32), String> {
    let coord = |key: &str, max: u32| -> Result<f32, String> {
        let v = args
            .get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("{key} is required (a number of screenshot pixels)."))?;
        to_fraction(v, max, key, size)
    };
    Ok((coord(kx, size.0)?, coord(ky, size.1)?))
}

async fn send(ctx: &Ctx<'_>, id: &DeviceId, input: DeviceInput) -> Result<(), String> {
    ctx.request(Request::DeviceInput {
        id: id.clone(),
        input,
        thread_id: ctx.thread.cloned(),
    })
    .await
    .map(|_| ())
}

fn touch(phase: TouchPhase, (x, y): (f32, f32)) -> DeviceInput {
    DeviceInput::Touch { phase, x, y }
}

/// Fingers at `points`: one is a touch, several are touches.
fn fingers_at(phase: TouchPhase, points: &[(f32, f32)]) -> DeviceInput {
    match points {
        [one] => touch(phase, *one),
        _ => DeviceInput::Touches {
            phase,
            touches: points.iter().map(|&(x, y)| TouchPoint { x, y }).collect(),
        },
    }
}

/// Where a finger following `path` is at `t` (0 to 1): its points spread
/// evenly over the time.
fn along(path: &[(f32, f32)], t: f32) -> (f32, f32) {
    let segments = path.len().saturating_sub(1);
    if segments == 0 {
        return path.first().copied().unwrap_or_default();
    }
    let s = t.clamp(0., 1.) * segments as f32;
    let i = (s.floor() as usize).min(segments - 1);
    let f = s - i as f32;
    let (a, b) = (path[i], path[i + 1]);
    (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f)
}

/// Fingers go down at their paths' starts, move along them over
/// `duration_ms` and lift at their ends; a finger that went down is lifted
/// (cancelled) even when a move failed.
async fn stroke(
    ctx: &Ctx<'_>,
    id: &DeviceId,
    paths: &[Vec<(f32, f32)>],
    duration_ms: u64,
) -> Result<(), String> {
    let at = |t: f32| -> Vec<(f32, f32)> { paths.iter().map(|p| along(p, t)).collect() };
    // About one move a frame, within reason.
    let steps = (duration_ms / 16).clamp(4, 60) as u32;
    let pause = Duration::from_millis(duration_ms / steps as u64);
    send(ctx, id, fingers_at(TouchPhase::Down, &at(0.))).await?;
    let mut moved = Ok(());
    for i in 1..=steps {
        tokio::time::sleep(pause).await;
        moved = send(
            ctx,
            id,
            fingers_at(TouchPhase::Move, &at(i as f32 / steps as f32)),
        )
        .await;
        if moved.is_err() {
            break;
        }
    }
    let end = at(1.);
    match moved {
        Ok(()) => send(ctx, id, fingers_at(TouchPhase::Up, &end)).await,
        Err(e) => {
            let _ = send(ctx, id, fingers_at(TouchPhase::Cancel, &end)).await;
            Err(e)
        }
    }
}

fn duration(args: &Value, default: u64) -> u64 {
    args.get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .clamp(50, 10_000)
}

const DONE: &str = "Done. Take a new screenshot to see the result.";

async fn tap(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let size = picture_size(ctx, id).await?;
    let at = fraction(args, "x", "y", size)?;
    let hold = args
        .get("hold_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(10_000);
    send(ctx, id, touch(TouchPhase::Down, at)).await?;
    // A quick tap still lasts a moment, as a finger's does.
    tokio::time::sleep(Duration::from_millis(hold.max(40))).await;
    send(ctx, id, touch(TouchPhase::Up, at)).await?;
    Ok(Outcome::Done(DONE.into()))
}

async fn swipe(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let size = picture_size(ctx, id).await?;
    let from = fraction(args, "from_x", "from_y", size)?;
    let to = fraction(args, "to_x", "to_y", size)?;
    stroke(ctx, id, &[vec![from, to]], duration(args, 300)).await?;
    Ok(Outcome::Done(DONE.into()))
}

/// Where a pinch's two fingers start and end, as fractions of the screen:
/// side by side around `at`, as far from it at their widest as a quarter
/// of the screen's shorter side, and `scale` times nearer at their closest.
fn pinch_paths(at: (f32, f32), scale: f64, (w, h): (u32, u32)) -> Vec<Vec<(f32, f32)>> {
    let short = w.min(h) as f64;
    let wide = 0.25 * short;
    let near = (wide / scale.max(1. / scale)).max(0.02 * short);
    let (from, to) = if scale > 1. {
        (near, wide)
    } else {
        (wide, near)
    };
    let finger = |side: f64, r: f64| {
        let x = (at.0 as f64 + side * r / w.max(1) as f64).clamp(0., 1.);
        (x as f32, at.1)
    };
    vec![
        vec![finger(-1., from), finger(-1., to)],
        vec![finger(1., from), finger(1., to)],
    ]
}

async fn pinch(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let size = picture_size(ctx, id).await?;
    let at = fraction(args, "x", "y", size)?;
    let scale = args.get("scale").and_then(Value::as_f64).ok_or(
        "scale is required: above 1 spreads the fingers (zoom in), below 1 brings them \
         together (zoom out).",
    )?;
    if !(0.1..=10.).contains(&scale) || (scale - 1.).abs() < 1e-3 {
        return Err("scale is between 0.1 and 10, and not 1.".into());
    }
    let paths = pinch_paths(at, scale, size);
    stroke(ctx, id, &paths, duration(args, 400)).await?;
    Ok(Outcome::Done(DONE.into()))
}

async fn gesture(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let size = picture_size(ctx, id).await?;
    let fingers = args
        .get("fingers")
        .and_then(Value::as_array)
        .filter(|f| (1..=5).contains(&f.len()))
        .ok_or("fingers is required: one to five lists of [x, y] points.")?;
    let mut paths = Vec::new();
    for (n, finger) in fingers.iter().enumerate() {
        let what = format!(
            "Finger {}: a list of [x, y] points (screenshot pixels).",
            n + 1
        );
        let points = finger
            .as_array()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| what.clone())?;
        let mut path = Vec::new();
        for point in points {
            let (x, y) = point
                .as_array()
                .filter(|xy| xy.len() == 2)
                .and_then(|xy| Some((xy[0].as_f64()?, xy[1].as_f64()?)))
                .ok_or_else(|| what.clone())?;
            path.push((
                to_fraction(x, size.0, "x", size)?,
                to_fraction(y, size.1, "y", size)?,
            ));
        }
        paths.push(path);
    }
    stroke(ctx, id, &paths, duration(args, 400)).await?;
    Ok(Outcome::Done(DONE.into()))
}

async fn type_text(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or("text is required.")?;
    if !text.is_empty() {
        send(
            ctx,
            id,
            DeviceInput::Text {
                text: text.to_owned(),
            },
        )
        .await?;
    }
    if args.get("submit").and_then(flag) == Some(true) {
        send(ctx, id, key("Enter")).await?;
    }
    Ok(Outcome::Done(DONE.into()))
}

fn key(name: &str) -> DeviceInput {
    DeviceInput::Key {
        key: name.to_owned(),
        modifiers: 0,
    }
}

/// One of a wire enum's names, however it is cased, spaced or hyphenated
/// (`volume-up`, `Half Opened`).
fn named<T: DeserializeOwned>(v: &Value) -> Option<T> {
    let s = v
        .as_str()?
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_");
    serde_json::from_value(json!(s)).ok()
}

/// `device_press`'s `button` names.
pub fn button(name: &str) -> Option<DeviceButton> {
    named(&json!(name))
}

/// Yes or no: a JSON boolean, or a command line's word for one.
fn flag(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_u64().filter(|n| *n <= 1).map(|n| n == 1),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "on" | "yes" | "true" | "1" | "high" | "grant" => Some(true),
            "off" | "no" | "false" | "0" | "normal" | "revoke" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A non-empty string argument.
fn string(args: &Value, k: &str) -> Option<String> {
    args.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn need(args: &Value, k: &str) -> Result<String, String> {
    string(args, k).ok_or_else(|| format!("{k} is required."))
}

async fn press(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let input = match (string(args, "key"), string(args, "button")) {
        (Some(k), None) => key(&k),
        (None, Some(b)) => DeviceInput::Button {
            button: button(&b).ok_or_else(|| {
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
        send(ctx, id, input.clone()).await?;
    }
    let turned = matches!(
        input,
        DeviceInput::Button {
            button: DeviceButton::RotateLeft | DeviceButton::RotateRight
        }
    );
    if turned {
        // The next screenshot has another shape.
        ctx.pictures.forget(id);
        return Ok(Outcome::Done(
            "Done. The screen turned: take a new screenshot before tapping again.".into(),
        ));
    }
    Ok(Outcome::Done(DONE.into()))
}

async fn app(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let need = |k: &str| need(args, k).map_err(|_| format!("{k} is required for this action."));
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
    ctx.request(Request::DeviceApp {
        id: id.clone(),
        app,
        thread_id: ctx.thread.cloned(),
    })
    .await?;
    Ok(Outcome::Done(done.into()))
}

async fn set_one(ctx: &Ctx<'_>, id: &DeviceId, setting: DeviceSetting) -> Result<(), String> {
    ctx.request(Request::DeviceSet {
        id: id.clone(),
        setting,
    })
    .await
    .map(|_| ())
}

async fn fold(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let posture = args.get("posture").filter(|v| !v.is_null());
    let hinge = args.get("hinge").filter(|v| !v.is_null());
    let setting = match (posture, hinge) {
        (Some(p), None) => DeviceSetting::Posture {
            posture: named::<DevicePosture>(p).ok_or_else(|| {
                format!("Unknown posture {p}: closed, half_opened, opened, flipped or tent.")
            })?,
        },
        (None, Some(h)) => DeviceSetting::Hinge {
            degrees: h
                .as_f64()
                .or_else(|| h.as_str()?.trim().parse().ok())
                .filter(|d| (0.0..=360.).contains(d))
                .ok_or("hinge is degrees, 0 (shut) to 180 (flat), or 360 for a fold-out.")?
                as f32,
        },
        _ => return Err("Give exactly one of posture and hinge.".into()),
    };
    set_one(ctx, id, setting).await?;
    // Folded or unfolded, the screen may be another one, of another size.
    ctx.pictures.forget(id);
    let device = device_info(ctx, id).await?;
    let mut v = json!({ "id": device.id, "fold": device.fold });
    if device.width > 0 && device.height > 0 {
        v["screen"] = json!(format!("{}x{} pixels", device.width, device.height));
    }
    v["next"] = json!("Take a new screenshot before tapping: the screen may have changed.");
    Ok(Outcome::Data(v))
}

/// The device's settings as it reads them back.
async fn read_settings(ctx: &Ctx<'_>, id: &DeviceId) -> Result<Value, String> {
    match ctx
        .request(Request::DeviceSettings { id: id.clone() })
        .await?
    {
        Response::DeviceSettings { settings } => {
            serde_json::to_value(settings).map_err(|e| e.to_string())
        }
        other => Err(unexpected(other)),
    }
}

async fn settings(ctx: &Ctx<'_>, id: &DeviceId) -> Result<Outcome, String> {
    let mut v = read_settings(ctx, id).await?;
    if let Ok(DeviceInfo {
        fold: Some(fold), ..
    }) = device_info(ctx, id).await
    {
        v["fold"] = json!(fold);
    }
    Ok(Outcome::Data(v))
}

/// `device_set`'s settings, in the order they are applied.
pub const SETTINGS: &[&str] = &[
    "appearance",
    "text_size",
    "high_contrast",
    "reduce_motion",
    "screen_reader",
    "accessibility_frames",
    "network",
    "orientation",
    "location",
];

/// A latitude and longitude: `{latitude, longitude}`, or "lat,lon".
fn location(v: &Value) -> Option<(f64, f64)> {
    let (lat, lon) = match v {
        Value::Object(m) => (m.get("latitude")?.as_f64()?, m.get("longitude")?.as_f64()?),
        Value::String(s) => {
            let (a, b) = s.split_once(',')?;
            (a.trim().parse().ok()?, b.trim().parse().ok()?)
        }
        _ => return None,
    };
    ((-90.0..=90.).contains(&lat) && (-180.0..=180.).contains(&lon)).then_some((lat, lon))
}

/// One of `device_set`'s settings with its value.
fn setting(name: &str, v: &Value) -> Result<DeviceSetting, String> {
    let on = || flag(v).ok_or_else(|| format!("{name} is on or off (true or false)."));
    Ok(match name {
        "appearance" => DeviceSetting::Appearance {
            dark: match v.as_str().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
                Some("dark") => true,
                Some("light") => false,
                _ => return Err("appearance is light or dark.".into()),
            },
        },
        "text_size" => DeviceSetting::TextSize {
            size: named::<DeviceTextSize>(v).ok_or(
                "text_size is extra_small, small, default, large, extra_large or largest.",
            )?,
        },
        "high_contrast" => DeviceSetting::Contrast { high: on()? },
        "reduce_motion" => DeviceSetting::ReduceMotion { on: on()? },
        "screen_reader" => DeviceSetting::ScreenReader { on: on()? },
        "accessibility_frames" => DeviceSetting::AccessibilityFrames { on: on()? },
        "network" => DeviceSetting::Network { on: on()? },
        "orientation" => DeviceSetting::Orientation {
            orientation: named::<DeviceOrientation>(v).ok_or(
                "orientation is portrait, landscape_left, landscape_right or \
                 portrait_upside_down.",
            )?,
        },
        "location" => {
            let (latitude, longitude) = location(v)
                .ok_or("location is {latitude, longitude} in degrees, e.g. 48.8584, 2.2945.")?;
            DeviceSetting::Location {
                latitude,
                longitude,
            }
        }
        other => {
            return Err(format!(
                "Unknown setting `{other}`: {}.",
                SETTINGS.join(", ")
            ));
        }
    })
}

async fn set(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let given = args.as_object().cloned().unwrap_or_default();
    // All of them understood before any is changed.
    let mut changes = Vec::new();
    if let Some(name) = given
        .keys()
        .find(|k| k.as_str() != "id" && !SETTINGS.contains(&k.as_str()))
    {
        return Err(format!(
            "Unknown setting `{name}`: {}.",
            SETTINGS.join(", ")
        ));
    }
    for name in SETTINGS {
        if let Some(v) = given.get(*name).filter(|v| !v.is_null()) {
            changes.push((*name, setting(name, v)?));
        }
    }
    if changes.is_empty() {
        return Err(format!(
            "Give at least one setting to change: {}.",
            SETTINGS.join(", ")
        ));
    }
    let mut done = Vec::new();
    for (name, change) in changes {
        if let Err(e) = set_one(ctx, id, change).await {
            return Err(if done.is_empty() {
                format!("{name}: {e}")
            } else {
                format!("Set {}, but {name} failed: {e}", done.join(", "))
            });
        }
        done.push(name);
    }
    let mut v = json!({ "set": done });
    if done.contains(&"orientation") {
        ctx.pictures.forget(id);
        v["next"] = json!("The screen turned: take a new screenshot before tapping again.");
    }
    match read_settings(ctx, id).await {
        Ok(settings) => v["settings"] = settings,
        Err(e) => v["settings_unread"] = json!(e),
    }
    Ok(Outcome::Data(v))
}

async fn permission(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let app = need(args, "app")?;
    let permission = need(args, "permission")?;
    let grant = args
        .get("grant")
        .and_then(flag)
        .ok_or("grant is required: true grants, false revokes.")?;
    let done = format!(
        "{} {permission} for {app}.",
        if grant { "Granted" } else { "Revoked" }
    );
    set_one(
        ctx,
        id,
        DeviceSetting::Permission {
            app,
            permission,
            grant,
        },
    )
    .await?;
    Ok(Outcome::Done(done))
}

/// A push's payload: the one given (an object, or JSON text of one), or
/// an alert of the title and body.
fn push_payload(args: &Value) -> Result<Value, String> {
    let (title, body) = (string(args, "title"), string(args, "body"));
    let payload = match args.get("payload").filter(|v| !v.is_null()) {
        Some(_) if title.is_some() || body.is_some() => {
            return Err("Give a title and body, or a payload: not both.".into());
        }
        Some(Value::String(s)) => {
            serde_json::from_str(s).map_err(|e| format!("The payload isn't JSON: {e}"))?
        }
        Some(v) => v.clone(),
        None if title.is_none() && body.is_none() => {
            return Err("Give a title and body, or a payload.".into());
        }
        None => {
            let mut alert = serde_json::Map::new();
            if let Some(title) = title {
                alert.insert("title".into(), json!(title));
            }
            if let Some(body) = body {
                alert.insert("body".into(), json!(body));
            }
            json!({ "aps": { "alert": alert } })
        }
    };
    if !payload.is_object() {
        return Err("The payload is a JSON object, e.g. {\"aps\": {\"alert\": \"Hi\"}}.".into());
    }
    Ok(payload)
}

async fn push(ctx: &Ctx<'_>, id: &DeviceId, args: &Value) -> Result<Outcome, String> {
    let app = need(args, "app")?;
    let payload = push_payload(args)?;
    set_one(ctx, id, DeviceSetting::Push { app, payload }).await?;
    Ok(Outcome::Done(
        "Sent. Take a screenshot to see the notification.".into(),
    ))
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
        let (scaled, screen) = scale_down(&png).unwrap();
        assert_eq!(scaled.dimensions(), (461, 1000));
        assert_eq!(screen, (1179, 2556));
        let jpeg = encode(&scaled, image::ImageFormat::Jpeg).unwrap();
        let jpeg = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((jpeg.width(), jpeg.height()), (461, 1000));
        assert!(scale_down(b"not a png").is_err());
    }

    #[test]
    fn fingers_follow_their_paths_evenly() {
        let path = [(0., 0.), (1., 0.), (1., 1.)];
        assert_eq!(along(&path, 0.), (0., 0.));
        assert_eq!(along(&path, 0.25), (0.5, 0.));
        assert_eq!(along(&path, 0.75), (1., 0.5));
        assert_eq!(along(&path, 1.), (1., 1.));
        assert_eq!(along(&[(0.3, 0.4)], 0.5), (0.3, 0.4));
        assert!(matches!(
            fingers_at(TouchPhase::Down, &[(0.1, 0.2), (0.3, 0.4)]),
            DeviceInput::Touches { touches, .. } if touches.len() == 2
        ));
        assert!(matches!(
            fingers_at(TouchPhase::Down, &[(0.1, 0.2)]),
            DeviceInput::Touch { .. }
        ));
    }

    #[test]
    fn a_pinch_spreads_or_closes_two_fingers_around_the_point() {
        // 400 wide: a quarter of the shorter side is 100 pixels, 0.25.
        let open = pinch_paths((0.5, 0.5), 2., (400, 800));
        let [left, right] = [&open[0], &open[1]];
        assert!((left[0].0 - 0.375).abs() < 1e-6 && (left[1].0 - 0.25).abs() < 1e-6);
        assert!((right[0].0 - 0.625).abs() < 1e-6 && (right[1].0 - 0.75).abs() < 1e-6);
        assert!(open.iter().flatten().all(|p| p.1 == 0.5));
        let close = pinch_paths((0.5, 0.5), 0.5, (400, 800));
        assert_eq!(close[0], vec![left[1], left[0]]);
        // At the edge, the fingers stay on the screen.
        let edge = pinch_paths((0.0, 0.5), 4., (400, 800));
        assert!(edge.iter().flatten().all(|p| (0.0..=1.0).contains(&p.0)));
    }

    #[test]
    fn settings_take_the_command_lines_words_too() {
        assert_eq!(
            setting("appearance", &json!("Dark")).unwrap(),
            DeviceSetting::Appearance { dark: true }
        );
        assert_eq!(
            setting("text_size", &json!("extra-large")).unwrap(),
            DeviceSetting::TextSize {
                size: DeviceTextSize::ExtraLarge
            }
        );
        assert_eq!(
            setting("network", &json!("off")).unwrap(),
            DeviceSetting::Network { on: false }
        );
        assert_eq!(
            setting("high_contrast", &json!(true)).unwrap(),
            DeviceSetting::Contrast { high: true }
        );
        assert_eq!(
            setting("location", &json!("48.85, 2.29")).unwrap(),
            DeviceSetting::Location {
                latitude: 48.85,
                longitude: 2.29
            }
        );
        assert_eq!(
            setting("orientation", &json!("landscape left")).unwrap(),
            DeviceSetting::Orientation {
                orientation: DeviceOrientation::LandscapeLeft
            }
        );
        assert!(setting("location", &json!({"latitude": 91, "longitude": 0})).is_err());
        assert!(setting("network", &json!("maybe")).is_err());
        assert!(setting("volume", &json!(1)).is_err());
        assert_eq!(
            named::<DevicePosture>(&json!("Half Opened")),
            Some(DevicePosture::HalfOpened)
        );
    }

    #[test]
    fn a_push_is_a_title_and_body_or_a_payload() {
        assert_eq!(
            push_payload(&json!({"title": "Hi", "body": "There"})).unwrap(),
            json!({"aps": {"alert": {"title": "Hi", "body": "There"}}})
        );
        assert_eq!(
            push_payload(&json!({"payload": "{\"aps\": {\"badge\": 1}}"})).unwrap(),
            json!({"aps": {"badge": 1}})
        );
        assert!(push_payload(&json!({})).is_err());
        assert!(push_payload(&json!({"title": "Hi", "payload": {}})).is_err());
        assert!(push_payload(&json!({"payload": [1]})).is_err());
    }

    #[test]
    fn screenshot_sizes_are_kept_per_thread() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let thread = ThreadId("t/1".into());
        let id = DeviceId::ios("X");
        let mine = Pictures::new(&paths, Some(&thread));
        mine.set(&id, (300, 600));
        assert_eq!(
            Pictures::new(&paths, Some(&thread)).get(&id),
            Some((300, 600))
        );
        assert_eq!(Pictures::new(&paths, None).get(&id), None);
        mine.forget(&id);
        assert_eq!(mine.get(&id), None);
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
            "device_pinch",
            "device_gesture",
            "device_fold",
            "device_settings",
            "device_set",
            "device_permission",
            "device_push",
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
