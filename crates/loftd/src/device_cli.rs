//! `agent-device`: the device tools (`mcp_devices.rs`, ADR-0020) as a
//! command line for agents' shells. The daemon writes an `agent-device`
//! script that runs `agentloftd --home <home> device …` and puts it first on
//! every agent's `PATH`, with the agent's thread in `AGENTLOFT_THREAD_ID`
//! (`loft_core::McpBridge::install_cli`), so the user's window shows the
//! device the agent uses and points mean what they do for its MCP tools.
//!
//! Each command becomes the matching tool's arguments and runs through
//! [`mcp_devices::run`]; only the wording of the answer is this file's.

use crate::mcp_devices::{self, Ctx, Outcome, Pictures};
use clap::{Parser, Subcommand};
use loft_client::Client;
use loft_config::Paths;
use loft_protocol::{ClientKind, ThreadId};
use serde_json::{Value, json};
use std::path::PathBuf;

const ABOUT: &str = "Use the iOS simulators and Android emulators on this computer (and Android \
phones plugged in): the ones the user watches in Agentloft's Device panel.";

const AFTER: &str = "\
Work in a loop: take a screenshot, act, then take a new screenshot to check, since a command \
that returned proves nothing.

Ids: `agent-device list` shows them: ios:<UDID>, android:<AVD name>, android:@<serial> for a \
phone.

Coordinates: tap, swipe, pinch and gesture take pixels of your latest screenshot of that \
device (0,0 top left), whether taken with `agent-device screenshot` or the device_screenshot \
tool of the same thread. Screenshots are scaled so their longer side is at most 1000 pixels; \
with none taken yet, points are pixels of the screen scaled that way. After the screen turns \
or folds, take a new screenshot before tapping.

`agent-device help <command>` says more about each. Add --json for output a script can read: the answer, or {\"error\": …} (the exit status is \
then 1).";

#[derive(Parser)]
#[command(
    name = "agent-device",
    about = ABOUT,
    after_help = AFTER,
    disable_version_flag = true
)]
struct Cli {
    /// Print JSON, for scripts.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the devices: id, name, OS, state, screen size.
    List,
    /// Start a device (headless) and wait until it runs.
    Boot { id: String },
    /// Save the screen as a PNG and print its path and size: points are in its pixels.
    Screenshot {
        id: String,
        /// Where to save it (default: a new file in the temporary folder).
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Tap at X Y.
    Tap {
        id: String,
        x: f64,
        y: f64,
        /// Hold the finger down this long first (a long press is about 800).
        #[arg(long, value_name = "MS")]
        hold: Option<u64>,
    },
    /// Drag a finger from X1 Y1 to X2 Y2 (swipe up to scroll down).
    Swipe {
        id: String,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        /// How long it takes (default 300).
        #[arg(long, value_name = "MS")]
        ms: Option<u64>,
    },
    /// Two fingers around X Y: SCALE above 1 spreads them (zoom in), below 1 closes them.
    Pinch {
        id: String,
        x: f64,
        y: f64,
        scale: f64,
        /// How long it takes (default 400).
        #[arg(long, value_name = "MS")]
        ms: Option<u64>,
    },
    /// Move several fingers at once along paths.
    ///
    /// One argument per finger (up to five), its points as "x,y x,y …": all go down at
    /// their first point and lift at their last. A two-finger turn:
    /// agent-device gesture <id> "100,300 200,200" "300,300 200,400"
    Gesture {
        id: String,
        #[arg(required = true, num_args = 1..=5)]
        fingers: Vec<String>,
        /// How long it takes (default 400).
        #[arg(long, value_name = "MS")]
        ms: Option<u64>,
    },
    /// Type text into whatever has the focus (tap a field first).
    ///
    /// Its words are typed with a space between them. Text that starts with a hyphen goes
    /// after `--`: agent-device type <id> --submit -- -5
    Type {
        id: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// Press Enter afterwards.
        #[arg(long)]
        submit: bool,
    },
    /// Press a key or one of the device's buttons.
    ///
    /// Keys: Enter, Backspace, Tab, Escape, ArrowLeft, ArrowRight, ArrowUp, ArrowDown.
    /// Buttons: home, back (Android), recents, power, volume_up, volume_down, rotate_left,
    /// rotate_right.
    Press {
        id: String,
        name: String,
        /// How many times.
        #[arg(long)]
        count: Option<u64>,
        /// NAME is a key even where a button has that name.
        #[arg(long)]
        key: bool,
    },
    /// Install, launch, stop or remove an app, or open an address.
    ///
    /// ACTION TARGET: install <path to an .app (iOS) or .apk>, launch | terminate |
    /// uninstall <bundle id or package name>, open-url <web address or app link>.
    App {
        id: String,
        action: String,
        target: String,
    },
    /// Fold or unfold a foldable.
    ///
    /// A posture (closed, half-opened, opened; flipped and tent for a fold-out), or --hinge
    /// DEGREES (0 shut, 180 flat). Folded shut, the screen is the cover screen: take a new
    /// screenshot.
    Fold {
        id: String,
        #[arg(required_unless_present = "hinge")]
        posture: Option<String>,
        #[arg(long, value_name = "DEGREES", conflicts_with = "posture")]
        hinge: Option<f64>,
    },
    /// Print the device's settings, as read back from it.
    Settings { id: String },
    /// Change settings, as NAME VALUE pairs, then print them.
    ///
    /// appearance light|dark; text-size extra-small|small|default|large|extra-large|largest;
    /// contrast high|normal; reduce-motion, screen-reader, accessibility-frames, network
    /// on|off; orientation portrait|landscape-left|landscape-right|portrait-upside-down;
    /// location LAT,LON.
    ///
    /// Example: agent-device set ios:<UDID> appearance dark text-size largest
    Set {
        id: String,
        #[arg(
            required = true,
            num_args = 2..,
            allow_hyphen_values = true,
            value_name = "NAME VALUE"
        )]
        pairs: Vec<String>,
    },
    /// Grant or revoke one of an app's permissions.
    ///
    /// PERMISSION is the platform's name: iOS photos, camera, location, contacts,
    /// microphone, calendar, all…; Android android.permission.CAMERA (or CAMERA).
    Permission {
        id: String,
        app: String,
        permission: String,
        #[arg(value_parser = ["grant", "revoke"])]
        grant: String,
    },
    /// Send an app a test push notification (--title and --body, or --payload).
    Push {
        id: String,
        app: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        /// A whole payload instead, as JSON or @file.
        #[arg(long)]
        payload: Option<String>,
    },
    /// Shut a device down.
    Shutdown { id: String },
}

/// Run `agent-device` with `args` (without the program's name); the exit
/// status.
pub async fn run(paths: Paths, args: Vec<String>) -> i32 {
    // `--json` anywhere before a `--`: `set` takes words that start with
    // a hyphen (`location -33.9,151.2`), which would swallow it.
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let json_flag = args[..end].iter().any(|a| a == "--json");
    let args = args
        .into_iter()
        .enumerate()
        .filter(|(i, a)| *i >= end || a != "--json")
        .map(|(_, a)| a);
    let cli = match Cli::try_parse_from(std::iter::once("agent-device".to_owned()).chain(args)) {
        Ok(cli) => cli,
        Err(e) => {
            // Help goes to stdout with status 0, mistakes to stderr.
            let _ = e.print();
            return e.exit_code();
        }
    };
    let json = cli.json || json_flag;
    match go(&paths, cli.cmd, json).await {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) if json => {
            println!("{}", json!({ "error": e }));
            1
        }
        Err(e) => {
            eprintln!("agent-device: {e}");
            1
        }
    }
}

/// What the command prints.
async fn go(paths: &Paths, cmd: Cmd, json: bool) -> Result<String, String> {
    let (name, args, out) = tool_call(cmd)?;
    let thread = std::env::var(loft_core::THREAD_ENV)
        .ok()
        .filter(|t| !t.trim().is_empty())
        .map(ThreadId);
    let (client, _events) = Client::connect(&paths.socket_file(), ClientKind::Cli, crate::BUILD)
        .await
        .map_err(|e| format!("Agentloft isn't reachable (is it running?): {e:#}"))?;
    let pictures = Pictures::new(paths, thread.as_ref());
    let ctx = Ctx {
        client: &client,
        thread: thread.as_ref(),
        pictures: &pictures,
    };
    let outcome = mcp_devices::run(&ctx, name, &args).await?;
    match outcome {
        Outcome::Done(text) if json => Ok(format!("{}\n", json!({ "ok": true, "message": text }))),
        Outcome::Done(text) => Ok(format!("{text}\n")),
        Outcome::Data(v) if json => Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&v).unwrap_or_default()
        )),
        Outcome::Data(v) if name == "device_list" => Ok(list_text(&v)),
        Outcome::Data(v) => {
            let mut text = String::new();
            render(&v, 0, &mut text);
            Ok(text)
        }
        Outcome::Shot(shot) => save_shot(shot, out, json).await,
    }
}

/// The tool a command is, its arguments, and where a screenshot goes.
fn tool_call(cmd: Cmd) -> Result<(&'static str, Value, Option<PathBuf>), String> {
    let ms = |ms: Option<u64>| ms.map_or(Value::Null, |ms| json!(ms));
    Ok(match cmd {
        Cmd::List => ("device_list", json!({}), None),
        Cmd::Boot { id } => ("device_boot", json!({ "id": id }), None),
        Cmd::Screenshot { id, out } => ("device_screenshot", json!({ "id": id }), out),
        Cmd::Tap { id, x, y, hold } => (
            "device_tap",
            json!({ "id": id, "x": x, "y": y, "hold_ms": ms(hold) }),
            None,
        ),
        Cmd::Swipe {
            id,
            x1,
            y1,
            x2,
            y2,
            ms: duration,
        } => (
            "device_swipe",
            json!({ "id": id, "from_x": x1, "from_y": y1, "to_x": x2, "to_y": y2, "duration_ms": ms(duration) }),
            None,
        ),
        Cmd::Pinch {
            id,
            x,
            y,
            scale,
            ms: duration,
        } => (
            "device_pinch",
            json!({ "id": id, "x": x, "y": y, "scale": scale, "duration_ms": ms(duration) }),
            None,
        ),
        Cmd::Gesture {
            id,
            fingers,
            ms: duration,
        } => {
            let fingers = fingers
                .iter()
                .map(|f| finger_path(f))
                .collect::<Result<Vec<_>, _>>()?;
            (
                "device_gesture",
                json!({ "id": id, "fingers": fingers, "duration_ms": ms(duration) }),
                None,
            )
        }
        Cmd::Type { id, text, submit } => (
            "device_type",
            json!({ "id": id, "text": text.join(" "), "submit": submit }),
            None,
        ),
        Cmd::Press {
            id,
            name,
            count,
            key,
        } => {
            let which = if !key && mcp_devices::button(&name).is_some() {
                "button"
            } else {
                "key"
            };
            (
                "device_press",
                json!({ "id": id, which: name, "count": count }),
                None,
            )
        }
        Cmd::App { id, action, target } => {
            let action = action.trim().to_ascii_lowercase().replace('-', "_");
            let (action, key, target) = match action.as_str() {
                "install" => ("install", "path", absolute(&target)),
                "launch" | "terminate" | "uninstall" => (action.as_str(), "app", target),
                "open_url" | "open" => ("open_url", "url", target),
                other => {
                    return Err(format!(
                        "Unknown app action `{other}`: install, launch, terminate, uninstall \
                         or open-url."
                    ));
                }
            };
            (
                "device_app",
                json!({ "id": id, "action": action, key: target }),
                None,
            )
        }
        Cmd::Fold { id, posture, hinge } => (
            "device_fold",
            json!({ "id": id, "posture": posture, "hinge": hinge }),
            None,
        ),
        Cmd::Settings { id } => ("device_settings", json!({ "id": id }), None),
        Cmd::Set { id, pairs } => {
            if pairs.len() % 2 != 0 {
                return Err(format!(
                    "Settings come as NAME VALUE pairs: `{}` has no value.",
                    pairs.last().map(String::as_str).unwrap_or_default()
                ));
            }
            let mut args = json!({ "id": id });
            for pair in pairs.chunks(2) {
                let name = match pair[0].trim().to_ascii_lowercase().replace('-', "_") {
                    n if n == "contrast" => "high_contrast".to_owned(),
                    n => n,
                };
                args[name] = json!(pair[1]);
            }
            ("device_set", args, None)
        }
        Cmd::Permission {
            id,
            app,
            permission,
            grant,
        } => (
            "device_permission",
            json!({ "id": id, "app": app, "permission": permission, "grant": grant == "grant" }),
            None,
        ),
        Cmd::Push {
            id,
            app,
            title,
            body,
            payload,
        } => {
            let payload = match payload.as_deref().map(str::trim) {
                Some(file) if file.starts_with('@') => Some(
                    std::fs::read_to_string(&file[1..])
                        .map_err(|e| format!("{}: {e}", &file[1..]))?,
                ),
                other => other.map(str::to_owned),
            };
            (
                "device_push",
                json!({ "id": id, "app": app, "title": title, "body": body, "payload": payload }),
                None,
            )
        }
        Cmd::Shutdown { id } => ("device_shutdown", json!({ "id": id }), None),
    })
}

/// A finger's points, "x,y x,y …" (or separated by `;`), as `[[x, y], …]`.
fn finger_path(text: &str) -> Result<Value, String> {
    let bad = || format!("`{text}`: a finger's points are \"x,y x,y …\".");
    let points = text
        .split(|c: char| c.is_whitespace() || c == ';')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (x, y) = p.split_once(',').ok_or_else(bad)?;
            let x: f64 = x.trim().parse().map_err(|_| bad())?;
            let y: f64 = y.trim().parse().map_err(|_| bad())?;
            Ok(json!([x, y]))
        })
        .collect::<Result<Vec<_>, String>>()?;
    if points.is_empty() {
        return Err(bad());
    }
    Ok(Value::Array(points))
}

/// A path the daemon can find: relative ones are from this shell's folder.
fn absolute(path: &str) -> String {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return path.to_owned();
    }
    std::env::current_dir()
        .map(|dir| dir.join(p).to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

/// Write a screenshot as a PNG; its path and size.
async fn save_shot(
    shot: mcp_devices::Shot,
    out: Option<PathBuf>,
    json: bool,
) -> Result<String, String> {
    let path = match out {
        Some(path) => path,
        None => {
            let dir = std::env::temp_dir().join("agent-device");
            let name: String = shot
                .id
                .as_str()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default();
            dir.join(format!("{name}-{stamp}.png"))
        }
    };
    let path = PathBuf::from(absolute(&path.to_string_lossy()));
    let ((w, h), (sw, sh)) = (shot.size(), shot.screen);
    let png = tokio::task::spawn_blocking(move || {
        mcp_devices::encode(&shot.image, image::ImageFormat::Png)
    })
    .await
    .map_err(|e| e.to_string())??;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, png).map_err(|e| format!("{}: {e}", path.display()))?;
    if json {
        return Ok(format!(
            "{}\n",
            json!({
                "path": path,
                "width": w,
                "height": h,
                "screen": { "width": sw, "height": sh },
            })
        ));
    }
    let scaled = if (w, h) == (sw, sh) {
        String::new()
    } else {
        format!(" (the screen is {sw}x{sh}, scaled down)")
    };
    Ok(format!(
        "{}\n{w}x{h} pixels{scaled}: tap, swipe, pinch and gesture take points in these pixels.\n",
        path.display()
    ))
}

/// `list`'s answer, a device a line.
fn list_text(v: &Value) -> String {
    let mut text = String::new();
    let devices = v["devices"].as_array().cloned().unwrap_or_default();
    if devices.is_empty() {
        text.push_str("No devices.\n");
    }
    for d in &devices {
        let s = |k: &str| d[k].as_str().unwrap_or_default().to_owned();
        let mut line = format!("{}  {}  {}  {}", s("id"), s("name"), s("os"), s("state"));
        if let Some(screen) = d["screen"].as_str() {
            line += &format!("  {screen}");
        }
        if d["physical"] == true {
            line += "  (a phone)";
        }
        if let Some(posture) = d["fold"]["posture"].as_str() {
            line += &format!("  folds: {posture}");
        }
        if let Some(by) = d["used_by"].as_str() {
            line += &format!("  used by {by}");
        }
        if let Some(error) = d["error"].as_str() {
            line += &format!("  error: {error}");
        }
        text += &line;
        text.push('\n');
    }
    if let Some(missing) = v["unavailable"].as_object() {
        for (platform, why) in missing {
            text += &format!("{platform}: {}\n", why.as_str().unwrap_or_default());
        }
    }
    text
}

/// An answer as `key: value` lines, objects within it indented.
fn render(v: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match v {
        Value::Object(map) => {
            for (k, v) in map {
                match v {
                    Value::Object(inner) if !inner.is_empty() => {
                        out.push_str(&format!("{pad}{k}:\n"));
                        render(v, indent + 2, out);
                    }
                    _ => out.push_str(&format!("{pad}{k}: {}\n", scalar(v))),
                }
            }
        }
        other => out.push_str(&format!("{pad}{}\n", scalar(other))),
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(scalar).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> (&'static str, Value) {
        let cli = Cli::try_parse_from(std::iter::once("agent-device").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let (name, args, _) = tool_call(cli.cmd).unwrap();
        (name, args)
    }

    #[test]
    fn commands_are_the_tools_arguments() {
        assert_eq!(
            parse(&["tap", "ios:X", "10", "20.5"]),
            (
                "device_tap",
                json!({"id": "ios:X", "x": 10.0, "y": 20.5, "hold_ms": null})
            )
        );
        assert_eq!(
            parse(&["press", "ios:X", "home"]).1["button"],
            json!("home")
        );
        assert_eq!(parse(&["press", "ios:X", "Enter"]).1["key"], json!("Enter"));
        assert_eq!(
            parse(&["press", "ios:X", "home", "--key"]).1["key"],
            json!("home")
        );
        assert_eq!(
            parse(&["type", "ios:X", "--submit", "--", "hello", "-world"]).1,
            json!({"id": "ios:X", "text": "hello -world", "submit": true})
        );
        assert_eq!(
            parse(&["gesture", "ios:X", "1,2 3,4", "5,6;7,8", "--ms", "200"]).1,
            json!({"id": "ios:X", "fingers": [[[1.0, 2.0], [3.0, 4.0]], [[5.0, 6.0], [7.0, 8.0]]], "duration_ms": 200})
        );
        assert_eq!(
            parse(&[
                "set",
                "ios:X",
                "contrast",
                "high",
                "location",
                "-33.86,151.2"
            ])
            .1,
            json!({"id": "ios:X", "high_contrast": "high", "location": "-33.86,151.2"})
        );
        assert_eq!(
            parse(&["app", "ios:X", "open-url", "https://example.com"]).1,
            json!({"id": "ios:X", "action": "open_url", "url": "https://example.com"})
        );
        let install = parse(&["app", "ios:X", "install", "build/My.app"]).1;
        assert!(
            PathBuf::from(install["path"].as_str().unwrap()).is_absolute(),
            "{install}"
        );
        assert_eq!(
            parse(&["fold", "android:F", "--hinge", "90"]).1,
            json!({"id": "android:F", "posture": null, "hinge": 90.0})
        );
        assert_eq!(
            parse(&["permission", "ios:X", "com.a", "camera", "revoke"]).1["grant"],
            json!(false)
        );
        assert!(finger_path("1,2 nonsense").is_err());
        assert!(Cli::try_parse_from(["agent-device", "fold", "android:F"]).is_err());
    }

    #[test]
    fn answers_read_as_lines() {
        let mut text = String::new();
        render(
            &json!({"set": ["network"], "settings": {"apps": ["a", "b"], "network": false}}),
            0,
            &mut text,
        );
        assert_eq!(
            text,
            "set: network\nsettings:\n  apps: a, b\n  network: false\n"
        );
        let list = list_text(&json!({
            "devices": [{"id": "ios:X", "name": "Phone", "os": "iOS 1", "state": "running", "screen": "390x844 pixels at 3x", "used_by": "this thread"}],
            "unavailable": {"android": "No SDK."}
        }));
        assert_eq!(
            list,
            "ios:X  Phone  iOS 1  running  390x844 pixels at 3x  used by this thread\nandroid: No SDK.\n"
        );
    }
}
