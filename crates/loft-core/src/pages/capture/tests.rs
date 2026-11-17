//! Agents' capture tools against a real browser. Each needs Chrome (or
//! Chromium, Edge, Brave): `cargo test -p loft-core pages::capture -- --ignored`.

use super::pixeldiff::image_size;
use crate::BusMsg;
use crate::pages::{AGENT_PAGE_SIZE, Pages, Which};
use base64::Engine as _;
use loft_page_script::{BrowserCommand, BrowserError};
use loft_protocol::{PageId, ThreadId};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::broadcast;

/// A route's answer: content type, extra headers, body.
type Answer = (&'static str, String, String);

/// Serves `route(method, path, body)` on a port of this computer, each
/// connection on its own (Chrome opens spare ones it may never use).
async fn serve(route: fn(&str, &str, &str) -> Answer) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                // The head, then as much body as it says.
                loop {
                    let Ok(n) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&request).to_string();
                let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                let mut first = head.lines().next().unwrap_or("").split(' ');
                let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or("/"));
                let (kind, headers, reply) = route(method, path, body);
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(answer.as_bytes()).await;
            });
        }
    });
    base
}

fn html(body: &str) -> Answer {
    ("text/html; charset=utf-8", String::new(), body.to_owned())
}

struct Tab {
    pages: Pages,
    id: PageId,
    heard: broadcast::Receiver<BusMsg>,
    _dir: tempfile::TempDir,
}

impl Tab {
    /// A tab of a new thread, the size an agent's has.
    async fn open() -> Tab {
        let dir = tempfile::tempdir().unwrap();
        let (bus, heard) = broadcast::channel(4096);
        let pages = Pages::default();
        let (width, height, scale) = AGENT_PAGE_SIZE;
        let info = pages
            .open(
                ThreadId::new(),
                Which::Current,
                None,
                width,
                height,
                scale,
                None,
                &dir.path().join("profile"),
                bus,
            )
            .await
            .unwrap();
        Tab {
            pages,
            id: info.id,
            heard,
            _dir: dir,
        }
    }

    async fn drive(&self, command: Value) -> Result<Value, BrowserError> {
        let command: BrowserCommand = serde_json::from_value(command).unwrap();
        self.pages
            .drive(&self.id, command, loft_page_script::DEFAULT_SEARCH_TEMPLATE)
            .await
    }

    async fn ok(&self, command: Value) -> Value {
        let what = command.to_string();
        self.drive(command)
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"))
    }

    async fn eval(&self, script: &str) -> Value {
        self.ok(json!({ "action": "eval", "script": script })).await
    }

    async fn close(self) {
        self.pages.close(&self.id).await.unwrap();
        self.pages.shutdown().await;
    }
}

/// An image result's width and height, read from the image itself.
fn picture(result: &Value) -> (u32, u32) {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            result["data"]
                .as_str()
                .unwrap_or_else(|| panic!("no image in {result}")),
        )
        .unwrap();
    image_size(&bytes).unwrap()
}

/// The ref of the element an outline describes as `what` (`button "Go"`).
fn ref_of(outline: &str, what: &str) -> u32 {
    outline
        .split("[ref=")
        .skip(1)
        .find_map(|rest| {
            let (id, element) = rest.split_once("] ")?;
            element.starts_with(what).then(|| id.parse().ok())?
        })
        .unwrap_or_else(|| panic!("no {what} in {outline}"))
}

async fn wait_for(what: &str, mut done: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !done().await {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

const PAGE: &str = "<!doctype html><meta charset=utf-8><title>Shots</title>\
    <body style='margin:0;height:3000px;font:16px sans-serif;background:#fff'>\
    <div id=box style='position:absolute;left:40px;top:60px;width:200px;height:100px;background:rgb(0,128,255)'></div>\
    <button id=go style='position:absolute;left:300px;top:60px;width:120px;height:40px'>Go</button>\
    </body>";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a real browser"]
async fn live_screenshots_take_any_region() {
    let base = serve(|_, _, _| html(PAGE)).await;
    let tab = Tab::open().await;
    tab.ok(json!({ "action": "navigate", "url": base })).await;

    // As it always was: the viewport, every device pixel, a PNG.
    let plain = tab.ok(json!({ "action": "screenshot" })).await;
    assert_eq!(plain["mime_type"], "image/png");
    assert_eq!(picture(&plain), (2560, 1600));
    assert_eq!(
        (&plain["width"], &plain["height"], &plain["scale"]),
        (&json!(2560), &json!(1600), &json!(2.0))
    );
    assert!(
        plain.get("note").is_none() && plain.get("path").is_none(),
        "{plain}"
    );

    // One image pixel per CSS pixel.
    let css = tab
        .ok(json!({ "action": "screenshot", "scale": "css" }))
        .await;
    assert_eq!(picture(&css), (1280, 800));
    // The whole page.
    let full = tab
        .ok(json!({ "action": "screenshot", "scale": "css", "full_page": true }))
        .await;
    assert_eq!(picture(&full), (1280, 3000));
    // An element, by selector and by ref.
    let by_selector = tab
        .ok(json!({ "action": "screenshot", "scale": "css", "selector": "#box" }))
        .await;
    assert_eq!(picture(&by_selector), (200, 100));
    let outline = tab.ok(json!({ "action": "snapshot" })).await;
    let go = ref_of(outline["snapshot"].as_str().unwrap(), "button \"Go\"");
    let by_ref = tab
        .ok(json!({ "action": "screenshot", "ref_id": go }))
        .await;
    assert_eq!(picture(&by_ref), (240, 80), "the button at device scale");
    // A rectangle, as a JPEG.
    let clip = tab
        .ok(json!({ "action": "screenshot", "scale": "css", "clip": { "x": 40, "y": 60, "width": 50, "height": 20 },
                    "format": "jpeg", "quality": 50 }))
        .await;
    assert_eq!(clip["mime_type"], "image/jpeg");
    assert_eq!(picture(&clip), (50, 20));
    // Its pixels are the box's blue.
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(clip["data"].as_str().unwrap())
        .unwrap();
    let rgb = image::load_from_memory(&jpeg).unwrap().to_rgb8();
    let [r, g, b] = rgb.get_pixel(25, 10).0;
    assert!(r < 30 && (110..150).contains(&g) && b > 230, "{r} {g} {b}");

    // Numbered elements, drawn on the picture and listed.
    let labelled = tab
        .ok(json!({ "action": "screenshot", "labels": true }))
        .await;
    let labels = labelled["labels"].as_array().unwrap();
    let button = labels
        .iter()
        .find(|l| l["role"] == "button")
        .unwrap_or_else(|| panic!("{labels:?}"));
    assert_eq!(button["selector"], "#go");
    assert_eq!(button["rect"], json!([300.0, 60.0, 120.0, 40.0]));
    // Saved instead of returned.
    let saved = tab
        .ok(json!({ "action": "screenshot", "save": true, "scale": "css" }))
        .await;
    let path = std::path::PathBuf::from(saved["path"].as_str().unwrap());
    assert!(saved.get("data").is_none());
    assert_eq!(
        image_size(&std::fs::read(&path).unwrap()),
        Some((1280, 800))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let _ = std::fs::remove_dir_all(path.parent().unwrap());

    // What can't be.
    let both = tab
        .drive(json!({ "action": "screenshot", "ref_id": go, "selector": "#go" }))
        .await;
    assert!(matches!(both, Err(BrowserError::Action(_))), "{both:?}");
    let missing = tab
        .drive(json!({ "action": "screenshot", "selector": "#nope" }))
        .await
        .unwrap_err();
    assert!(
        missing.to_string().contains("no element matches"),
        "{missing}"
    );
    assert_eq!(
        tab.drive(json!({ "action": "screenshot", "ref_id": 9999 }))
            .await
            .unwrap_err(),
        BrowserError::RefNotFound(9999)
    );
    let bad = tab
        .drive(json!({ "action": "screenshot", "selector": "##" }))
        .await
        .unwrap_err();
    assert!(bad.to_string().contains("isn't a valid selector"), "{bad}");
    assert!(
        tab.drive(json!({ "action": "screenshot", "labels": true, "full_page": true }))
            .await
            .is_err()
    );
    tab.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a real browser"]
async fn live_screenshot_diff_says_what_changed_and_where() {
    let base = serve(|_, _, _| html(PAGE)).await;
    let tab = Tab::open().await;
    tab.ok(json!({ "action": "navigate", "url": base })).await;
    let none = tab
        .drive(json!({ "action": "screenshot_diff" }))
        .await
        .unwrap_err();
    assert!(none.to_string().contains("take one first"), "{none}");

    tab.ok(json!({ "action": "screenshot", "scale": "css", "baseline": "before" }))
        .await;
    let same = tab.ok(json!({ "action": "screenshot_diff" })).await;
    assert_eq!(same["matches"], true, "{same}");
    assert!(same.get("data").is_none());

    tab.eval("document.querySelector('#box').style.background = 'rgb(255, 0, 0)'")
        .await;
    let changed = tab
        .ok(json!({ "action": "screenshot_diff", "baseline": "before" }))
        .await;
    assert_eq!(changed["matches"], false);
    assert_eq!(changed["changed_pixels"], 200 * 100, "{changed}");
    assert_eq!(
        (&changed["width"], &changed["height"]),
        (&json!(1280), &json!(800))
    );
    let region = &changed["regions"][0];
    let (x, y, w, h) = (
        region["x"].as_f64().unwrap(),
        region["y"].as_f64().unwrap(),
        region["width"].as_f64().unwrap(),
        region["height"].as_f64().unwrap(),
    );
    assert!(
        x <= 40.0 && y <= 60.0 && x + w >= 240.0 && y + h >= 160.0 && w <= 240.0 && h <= 140.0,
        "{region}"
    );
    assert_eq!(changed["regions"].as_array().unwrap().len(), 1);
    assert_eq!(picture(&changed), (1280, 800), "the diff image");
    // The diff kept its picture as the last one: the same page again matches.
    let again = tab.ok(json!({ "action": "screenshot_diff" })).await;
    assert_eq!(again["matches"], true, "{again}");
    // A named baseline stays as it was unless updated.
    let still = tab
        .ok(json!({ "action": "screenshot_diff", "baseline": "before", "update": true }))
        .await;
    assert_eq!(still["matches"], false);
    let updated = tab
        .ok(json!({ "action": "screenshot_diff", "baseline": "before" }))
        .await;
    assert_eq!(updated["matches"], true);
    // An element's baseline is taken again where the element is now.
    tab.ok(json!({ "action": "screenshot", "selector": "#box", "baseline": "box" }))
        .await;
    tab.eval("document.querySelector('#box').style.left = '500px'")
        .await;
    let moved = tab
        .ok(json!({ "action": "screenshot_diff", "baseline": "box" }))
        .await;
    assert_eq!(moved["matches"], true, "{moved}");
    let unknown = tab
        .drive(json!({ "action": "screenshot_diff", "baseline": "nope" }))
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("there are"), "{unknown}");
    tab.close().await;
}

fn api(method: &str, path: &str, body: &str) -> Answer {
    match (method, path.split('?').next().unwrap_or(path)) {
        ("POST", "/api/login") => (
            "application/json",
            "Set-Cookie: sid=s3cr3t-session-value; Path=/; HttpOnly\r\n".into(),
            format!(
                "{{\"access_token\":\"tok_live_1234567890abcdef\",\"user\":\"ada\",\"echo\":{}}}",
                serde_json::to_string(body).unwrap()
            ),
        ),
        ("GET", "/api/items") => (
            "application/json",
            String::new(),
            "{\"items\":[1,2,3]}".into(),
        ),
        ("GET", "/app.css") => ("text/css", String::new(), "body { color: #333 }".into()),
        _ => html("<!doctype html><title>App</title><link rel=stylesheet href=/app.css><p>app</p>"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a real browser"]
async fn live_network_capture_returns_exchanges_with_credentials_redacted() {
    let base = serve(api).await;
    let tab = Tab::open().await;
    assert_eq!(
        tab.ok(json!({ "action": "network_status" })).await["active"],
        false
    );
    let none = tab
        .drive(json!({ "action": "network_stop" }))
        .await
        .unwrap_err();
    assert!(none.to_string().contains("no network capture"), "{none}");

    let started = tab.ok(json!({ "action": "network_start" })).await;
    assert_eq!(started["active"], true, "{started}");
    assert!(
        tab.drive(json!({ "action": "network_start" }))
            .await
            .is_err(),
        "one at a time"
    );
    tab.ok(json!({ "action": "navigate", "url": base })).await;
    tab.eval(
        "fetch('/api/login', { method: 'POST', headers: { 'Content-Type': 'application/json', \
         'Authorization': 'Bearer abc.def.ghi-1234567890' }, body: JSON.stringify({ user: 'ada', password: 'hunter2hunter2' }) }) \
         .then(r => r.json()).then(() => fetch('/api/items?api_key=k-1234567890')).then(r => r.text())",
    )
    .await;
    wait_for("the exchanges", async || {
        let status = tab.ok(json!({ "action": "network_status" })).await;
        status["entry_count"].as_u64().unwrap() >= 4 && status["pending_count"] == 0
    })
    .await;

    let stopped = tab.ok(json!({ "action": "network_stop" })).await;
    let text = stopped.to_string();
    for secret in [
        "hunter2hunter2",
        "tok_live_1234567890abcdef",
        "s3cr3t-session-value",
        "abc.def.ghi-1234567890",
        "k-1234567890",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let exchanges = stopped["exchanges"].as_array().unwrap();
    let find = |path: &str| {
        exchanges
            .iter()
            .find(|e| e["request"]["url"].as_str().unwrap_or("").contains(path))
            .unwrap_or_else(|| panic!("no {path} in {exchanges:?}"))
    };
    let document = exchanges
        .iter()
        .find(|e| e["request"]["resource_type"] == "Document")
        .unwrap();
    assert_eq!(document["request"]["url"], format!("{base}/"));
    assert_eq!(document["request"]["resource_type"], "Document");
    assert_eq!(document["response"]["status"], 200);
    assert!(
        document["response"]["body"]["text"].is_null(),
        "HTML isn't returned: {document}"
    );
    let css = find("/app.css");
    assert_eq!(css["request"]["resource_type"], "Stylesheet");
    let login = find("/api/login");
    assert_eq!(login["request"]["method"], "POST");
    assert_eq!(login["request"]["resource_type"], "Fetch");
    let sent: Value =
        serde_json::from_str(login["request"]["body"]["text"].as_str().unwrap()).unwrap();
    assert_eq!(sent, json!({ "user": "ada", "password": "[redacted]" }));
    let got: Value =
        serde_json::from_str(login["response"]["body"]["text"].as_str().unwrap()).unwrap();
    assert_eq!(got["access_token"], "[redacted]");
    assert_eq!(got["user"], "ada");
    let authorization = login["request"]["headers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| {
            h["name"]
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case("authorization")
        })
        .unwrap();
    assert_eq!(authorization["value"], "Bearer [redacted]");
    assert!(login["duration_ms"].is_u64() && login["started"].is_string());
    assert!(login["transfer_size"].as_u64().unwrap() > 0, "{login}");
    let items = find("/api/items");
    assert!(
        items["request"]["url"]
            .as_str()
            .unwrap()
            .ends_with("api_key=[redacted]"),
        "{items}"
    );
    let items_body = items["response"]["body"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{items}"));
    assert_eq!(
        serde_json::from_str::<Value>(items_body).unwrap(),
        json!({ "items": [1, 2, 3] })
    );

    // Filtered, without bodies; saved; cancelled.
    tab.ok(json!({ "action": "network_start", "resource_types": ["Fetch"], "bodies": false }))
        .await;
    tab.eval("fetch('/api/items').then(r => r.text())").await;
    tab.ok(json!({ "action": "reload" })).await;
    let only = tab
        .ok(json!({ "action": "network_stop", "save": true }))
        .await;
    let only_exchanges = only["exchanges"].as_array().unwrap();
    assert_eq!(only_exchanges.len(), 1, "{only}");
    assert!(only_exchanges[0]["response"]["body"].is_null());
    let saved = std::path::PathBuf::from(only["path"].as_str().unwrap());
    let file: Value = serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
    assert_eq!(file["exchanges"], only["exchanges"]);
    let _ = std::fs::remove_dir_all(saved.parent().unwrap());
    tab.ok(json!({ "action": "network_start" })).await;
    assert_eq!(
        tab.ok(json!({ "action": "network_cancel" })).await["cancelled"],
        true
    );
    assert_eq!(
        tab.ok(json!({ "action": "network_status" })).await["active"],
        false
    );
    // A capture that ends by itself says why.
    tab.ok(json!({ "action": "network_start", "max_duration_ms": 1000 }))
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let ended = tab
        .drive(json!({ "action": "network_stop" }))
        .await
        .unwrap_err();
    assert!(ended.to_string().contains("max_duration_ms"), "{ended}");
    tab.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a real browser"]
async fn live_console_capture_keeps_the_pages_messages_only_while_on() {
    let base = serve(|_, _, _| {
        html("<!doctype html><title>Console</title><script>console.log('at load')</script>")
    })
    .await;
    let tab = Tab::open().await;
    tab.ok(json!({ "action": "navigate", "url": base })).await;
    assert!(
        tab.drive(json!({ "action": "console_read" }))
            .await
            .is_err()
    );
    tab.eval("console.log('before the start')").await;

    tab.ok(json!({ "action": "console_start" })).await;
    tab.eval(
        "console.log('hello', 42, { a: 1 }); console.warn('careful'); console.error('broken'); \
              setTimeout(() => { throw new TypeError('nope') }, 0); 1",
    )
    .await;
    tab.ok(json!({ "action": "snapshot" })).await;
    let mut read = Value::Null;
    wait_for("the uncaught error", async || {
        read = tab
            .ok(json!({ "action": "console_read", "clear": false }))
            .await;
        read["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "pageerror")
    })
    .await;
    let entries = read["entries"].as_array().unwrap();
    let texts: Vec<&str> = entries
        .iter()
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("before the start") || t.contains("at load")),
        "{texts:?}"
    );
    assert_eq!(entries[0]["type"], "log");
    assert_eq!(entries[0]["text"], "hello 42 Object", "{entries:?}");
    assert!(
        entries
            .iter()
            .any(|e| e["type"] == "warning" && e["text"] == "careful")
    );
    let errors = tab
        .ok(json!({ "action": "console_read", "errors_only": true }))
        .await;
    let kinds: Vec<&str> = errors["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["error", "pageerror"], "{errors}");
    assert!(
        errors["entries"][1]["text"]
            .as_str()
            .unwrap()
            .contains("TypeError: nope")
    );
    // A reload's messages are the new document's.
    tab.ok(json!({ "action": "reload" })).await;
    let mut after = Value::Null;
    wait_for("the reloaded page's message", async || {
        after = tab
            .ok(json!({ "action": "console_read", "levels": ["log"] }))
            .await;
        after["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["text"] == "at load")
    })
    .await;

    tab.ok(json!({ "action": "console_stop" })).await;
    assert!(
        tab.drive(json!({ "action": "console_read" }))
            .await
            .is_err()
    );
    tab.close().await;
}

/// A page on 127.0.0.1 with a frame from localhost (another site, so a
/// renderer and a DevTools session of its own) that fetches and logs at
/// load, and again for each message it is sent.
fn framed(_: &str, path: &str, _: &str) -> Answer {
    match path.split('?').next().unwrap_or(path) {
        "/frame" => html(
            "<!doctype html><meta charset=utf-8><title>Frame</title><script>\
             const ask = (n, say) => fetch('/frame-data?n=' + n).then(r => r.json()).then(d => say('frame got', d.n));\
             ask(1, console.log);\
             addEventListener('message', e => ask(e.data, console.warn));\
             </script>",
        ),
        "/frame-data" => (
            "application/json",
            String::new(),
            format!(
                "{{\"n\":{}}}",
                path.split("n=")
                    .nth(1)
                    .unwrap_or("0")
                    .parse::<u32>()
                    .unwrap_or(0)
            ),
        ),
        "/top-data" => ("text/plain", String::new(), "top".into()),
        _ => html(
            "<!doctype html><meta charset=utf-8><title>Top</title><iframe id=f></iframe><script>\
             console.log('top says hi'); fetch('/top-data').then(r => r.text());\
             document.getElementById('f').src = 'http://localhost:' + location.port + '/frame';\
             </script>",
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a real browser"]
async fn live_captures_follow_frames_of_other_sites() {
    let base = serve(framed).await;
    let frame_origin = base.replace("127.0.0.1", "localhost");
    let tab = Tab::open().await;
    let has = |read: &Value, text: &str| {
        read["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["text"] == text)
            .cloned()
    };
    let sent = async |n: u32| {
        tab.eval(&format!(
            "document.getElementById('f').contentWindow.postMessage({n}, '*'); 1"
        ))
        .await;
    };
    let exchanges = async || {
        let status = tab.ok(json!({ "action": "network_status" })).await;
        (
            status["entry_count"].as_u64().unwrap(),
            status["pending_count"].as_u64().unwrap(),
        )
    };

    // A frame already there when the captures start.
    tab.ok(json!({ "action": "navigate", "url": base })).await;
    tab.ok(json!({ "action": "network_start" })).await;
    tab.ok(json!({ "action": "console_start" })).await;
    sent(5).await;
    let mut read = Value::Null;
    wait_for("the frame's message, frame already there", async || {
        read = tab
            .ok(json!({ "action": "console_read", "clear": false }))
            .await;
        has(&read, "frame got 5").is_some()
    })
    .await;
    let five = has(&read, "frame got 5").unwrap();
    assert_eq!(five["type"], "warning", "{five}");
    assert_eq!(five["frame"], frame_origin, "{five}");

    // A frame made while they run: followed from its first request on.
    tab.ok(json!({ "action": "reload" })).await;
    wait_for("the new frame's message", async || {
        read = tab
            .ok(json!({ "action": "console_read", "clear": false }))
            .await;
        has(&read, "frame got 1").is_some() && has(&read, "top says hi").is_some()
    })
    .await;
    sent(2).await;
    wait_for("the frame's later message", async || {
        read = tab
            .ok(json!({ "action": "console_read", "clear": false }))
            .await;
        has(&read, "frame got 2").is_some()
    })
    .await;
    let top = has(&read, "top says hi").unwrap();
    assert!(top.get("frame").is_none(), "the page's own: {top}");
    assert_eq!(has(&read, "frame got 1").unwrap()["frame"], frame_origin);
    let order: Vec<&str> = read["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["text"].as_str().unwrap())
        .filter(|t| t.starts_with("frame got"))
        .collect();
    assert_eq!(
        order,
        ["frame got 5", "frame got 1", "frame got 2"],
        "{read}"
    );
    wait_for("every exchange finished", async || {
        let (count, pending) = exchanges().await;
        count >= 6 && pending == 0
    })
    .await;

    let stopped = tab.ok(json!({ "action": "network_stop" })).await;
    let all = stopped["exchanges"].as_array().unwrap();
    let find = |path: &str| {
        all.iter()
            .filter(|e| e["request"]["url"].as_str().unwrap_or("").ends_with(path))
            .collect::<Vec<_>>()
    };
    for n in [5, 1, 2] {
        let asked = find(&format!("/frame-data?n={n}"));
        assert_eq!(asked.len(), 1, "n={n}: {all:?}");
        let asked = asked[0];
        assert_eq!(asked["frame"], frame_origin, "{asked}");
        assert_eq!(asked["response"]["status"], 200, "{asked}");
        assert_eq!(
            serde_json::from_str::<Value>(asked["response"]["body"]["text"].as_str().unwrap())
                .unwrap(),
            json!({ "n": n }),
            "{asked}"
        );
    }
    // In the order they were made, the page's and the frame's together.
    let position = |path: &str| {
        all.iter()
            .position(|e| e["request"]["url"].as_str().unwrap_or("").ends_with(path))
            .unwrap()
    };
    assert!(position("n=5") < position("/top-data"));
    assert!(position("/top-data") < position("n=1"));
    assert!(position("n=1") < position("n=2"));
    let top_data = find("/top-data");
    assert!(top_data[0].get("frame").is_none(), "{:?}", top_data[0]);
    // The frame's document finished (its response came over its session).
    let document = find("/frame");
    assert_eq!(document.len(), 1, "{all:?}");
    assert_eq!(document[0]["response"]["status"], 200, "{:?}", document[0]);
    assert!(document[0]["failure"].is_null(), "{:?}", document[0]);

    // Off again: the frame's messages are no longer kept.
    tab.ok(json!({ "action": "console_stop" })).await;
    sent(3).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    tab.ok(json!({ "action": "console_start" })).await;
    sent(4).await;
    wait_for("the message after starting again", async || {
        read = tab.ok(json!({ "action": "console_read" })).await;
        has(&read, "frame got 4").is_some()
    })
    .await;
    assert!(has(&read, "frame got 3").is_none(), "{read}");
    tab.ok(json!({ "action": "console_stop" })).await;
    tab.close().await;
}

const MOVING: &str = "<!doctype html><title>Moving</title><body style='margin:0'>\
    <div id=b style='width:100vw;height:100vh'></div><script>\
    let n = 0; setInterval(() => { n++; document.getElementById('b').style.background = `hsl(${(n * 37) % 360} 80% 50%)`; }, 50);\
    </script>";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a real browser"]
async fn live_a_recording_is_one_more_watcher() {
    let base = serve(|_, _, _| html(MOVING)).await;
    let mut tab = Tab::open().await;
    tab.ok(json!({ "action": "navigate", "url": base })).await;
    assert!(
        tab.drive(json!({ "action": "recording_stop" }))
            .await
            .is_err()
    );

    // Nobody watches: the recording is pictured for alone.
    let started = tab
        .ok(json!({ "action": "recording_start", "frame_rate": 10 }))
        .await;
    let path = std::path::PathBuf::from(started["path"].as_str().unwrap());
    assert!(path.extension().is_some_and(|e| e == "mkv"), "{started}");
    assert!(
        tab.drive(json!({ "action": "recording_start" }))
            .await
            .is_err(),
        "one at a time"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let status = tab.ok(json!({ "action": "recording_status" })).await;
    assert!(
        status["quality"]["source_frame_count"].as_u64().unwrap() >= 3,
        "{status}"
    );
    let stopped = tab.ok(json!({ "action": "recording_stop" })).await;
    assert_eq!(stopped["path"], started["path"]);
    assert!(stopped["frame_count"].as_u64().unwrap() >= 3, "{stopped}");
    assert_eq!(
        (&stopped["width"], &stopped["height"]),
        (&json!(1280), &json!(800)),
        "one picture per CSS pixel"
    );
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..4], &[0x1A, 0x45, 0xDF, 0xA3], "a Matroska file");
    assert_eq!(stopped["bytes"], bytes.len());
    // Nobody watches it now: no pictures.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while tab.heard.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(600)).await;
    while let Ok(msg) = tab.heard.try_recv() {
        assert!(
            !matches!(msg, BusMsg::PageFrame { .. }),
            "pictures after the recording stopped"
        );
    }

    // A viewer watches: its pictures go on through a recording and after it.
    tab.pages.watching(&tab.id, true).await;
    let frames = async |tab: &mut Tab| {
        while tab.heard.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(800)).await;
        let mut n = 0;
        while let Ok(msg) = tab.heard.try_recv() {
            n += usize::from(matches!(msg, BusMsg::PageFrame { .. }));
        }
        n
    };
    assert!(frames(&mut tab).await >= 3, "the viewer's pictures");
    let mp4 = super::recording::find_ffmpeg().is_some();
    let name = if mp4 { "demo.mp4" } else { "demo.mkv" };
    let second = tab
        .ok(json!({ "action": "recording_start", "name": name }))
        .await;
    assert!(
        frames(&mut tab).await >= 3,
        "the viewer's pictures while recording"
    );
    let done = tab.ok(json!({ "action": "recording_stop" })).await;
    assert!(
        frames(&mut tab).await >= 3,
        "the viewer's pictures after the recording"
    );
    let second_path = std::path::PathBuf::from(second["path"].as_str().unwrap());
    assert_eq!(second_path.file_name().unwrap(), name);
    assert!(
        std::fs::metadata(&second_path).unwrap().len() > 1000,
        "{done}"
    );
    if mp4 {
        assert_eq!(done["mime_type"], "video/mp4");
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,width,height",
                "-of",
                "compact",
            ])
            .arg(&second_path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout);
        assert!(
            text.contains("codec_name=h264|width=1280|height=800"),
            "{text}"
        );
    }
    // A name already taken gets another; a cancelled one leaves nothing.
    let third = tab
        .ok(json!({ "action": "recording_start", "name": name }))
        .await;
    let third_path = std::path::PathBuf::from(third["path"].as_str().unwrap());
    assert_ne!(third_path, second_path);
    assert_eq!(
        tab.ok(json!({ "action": "recording_cancel" })).await["cancelled"],
        true
    );
    assert!(!third_path.exists());
    assert!(
        std::fs::read_dir(third_path.parent().unwrap())
            .unwrap()
            .all(|f| !f.unwrap().file_name().to_string_lossy().contains("partial"))
    );
    assert!(
        tab.drive(json!({ "action": "recording_start", "name": "../x.mkv" }))
            .await
            .is_err()
    );
    assert!(
        tab.drive(json!({ "action": "recording_start", "name": "x.gif" }))
            .await
            .is_err()
    );
    // A recording that reaches its longest finishes by itself, and says so.
    tab.ok(json!({ "action": "recording_start", "name": "short.mkv", "max_duration_ms": 1000 }))
        .await;
    tokio::time::sleep(Duration::from_millis(1800)).await;
    let ended = tab.ok(json!({ "action": "recording_status" })).await;
    assert!(
        ended["ended_because"]
            .as_str()
            .unwrap()
            .contains("max_duration_ms"),
        "{ended}"
    );
    let collected = tab.ok(json!({ "action": "recording_stop" })).await;
    assert_eq!(collected["path"], ended["path"]);
    assert!(std::path::Path::new(collected["path"].as_str().unwrap()).exists());
    assert!(
        collected["frame_count"].as_u64().unwrap() >= 1,
        "{collected}"
    );
    assert!(
        tab.drive(json!({ "action": "recording_stop" }))
            .await
            .is_err()
    );
    assert!(frames(&mut tab).await >= 3, "the viewer still watches");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    tab.pages.watching(&tab.id, false).await;
    tab.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a real browser"]
async fn live_emulation_changes_how_the_tab_presents_itself() {
    let base = serve(|_, path, _| match path {
        "/data" => ("application/json", String::new(), "{\"ok\":true}".into()),
        _ => html("<!doctype html><meta name=viewport content=\"width=device-width\"><title>Emulated</title><p>hi</p>"),
    })
    .await;
    let tab = Tab::open().await;
    tab.ok(json!({ "action": "navigate", "url": base })).await;
    assert!(
        tab.drive(json!({ "action": "emulate" })).await.is_err(),
        "something to set"
    );
    assert!(
        tab.drive(json!({ "action": "emulate", "color_scheme": "purple" }))
            .await
            .is_err()
    );
    // The browser follows this computer's own appearance until told otherwise.
    let dark = "matchMedia('(prefers-color-scheme: dark)').matches";
    let own = tab.eval(dark).await;
    let opposite = if own == json!(true) { "light" } else { "dark" };

    // A phone: every viewer shows it (responsive mode).
    let phone = tab
        .ok(json!({ "action": "emulate", "viewport": { "width": 390, "height": 844, "mobile": true } }))
        .await;
    assert_eq!(phone["applied"]["viewport"]["width"], 390, "{phone}");
    let device = tab.pages.info(&tab.id).unwrap().device.unwrap();
    assert_eq!(
        (device.width, device.height, device.mobile),
        (390, 844, true)
    );
    assert_eq!(tab.eval("innerWidth").await, json!(390));
    let shot = tab
        .ok(json!({ "action": "screenshot", "scale": "css" }))
        .await;
    assert_eq!(picture(&shot), (390, 844));

    let set = tab
        .ok(
            json!({ "action": "emulate", "color_scheme": opposite, "reduced_motion": "reduce",
                    "timezone": "Asia/Tokyo", "locale": "de-DE", "cpu_throttling": 2,
                    "geolocation": { "latitude": 48.8566, "longitude": 2.3522 } }),
        )
        .await;
    assert!(set.get("failed").is_none(), "{set}");
    assert_ne!(tab.eval(dark).await, own);
    assert_eq!(
        tab.eval("matchMedia('(prefers-reduced-motion: reduce)').matches")
            .await,
        json!(true)
    );
    assert_eq!(
        tab.eval("Intl.DateTimeFormat().resolvedOptions().timeZone")
            .await,
        json!("Asia/Tokyo")
    );
    assert_eq!(
        tab.eval("new Intl.NumberFormat().format(1234.5)").await,
        json!("1.234,5")
    );
    // Another time zone over the first.
    tab.ok(json!({ "action": "emulate", "timezone": "Pacific/Chatham" }))
        .await;
    assert_eq!(
        tab.eval("Intl.DateTimeFormat().resolvedOptions().timeZone")
            .await,
        json!("Pacific/Chatham")
    );
    let where_ = tab
        .eval("new Promise((ok, no) => navigator.geolocation.getCurrentPosition(p => ok([p.coords.latitude, p.coords.longitude]), e => no(e.message), { timeout: 5000 }))")
        .await;
    assert_eq!(where_, json!([48.8566, 2.3522]));
    // One media feature changed keeps the others.
    tab.ok(json!({ "action": "emulate", "contrast": "more" }))
        .await;
    assert_ne!(tab.eval(dark).await, own);
    assert_eq!(
        tab.eval("matchMedia('(prefers-contrast: more)').matches")
            .await,
        json!(true)
    );

    // Offline, then a slow network, then back.
    tab.ok(json!({ "action": "emulate", "offline": true }))
        .await;
    let offline = tab
        .eval("fetch('/data').then(() => 'loaded', () => 'failed')")
        .await;
    assert_eq!(offline, json!("failed"));
    let slow = tab
        .ok(json!({ "action": "emulate", "network": { "latency_ms": 600 } }))
        .await;
    assert_eq!(slow["applied"]["network"]["latency_ms"], 600.0, "{slow}");
    let took = tab.eval("(async () => { const t = performance.now(); await fetch('/data?' + Math.random()); return performance.now() - t })()").await;
    assert!(took.as_f64().unwrap() >= 550.0, "{took}");

    // Everything back.
    let reset = tab.ok(json!({ "action": "emulate", "reset": true })).await;
    assert!(reset.get("failed").is_none(), "{reset}");
    assert!(tab.pages.info(&tab.id).unwrap().device.is_none());
    assert_eq!(tab.eval("innerWidth").await, json!(1280));
    assert_eq!(tab.eval(dark).await, own);
    assert_eq!(
        tab.eval("matchMedia('(prefers-contrast: more)').matches")
            .await,
        json!(false)
    );
    assert_ne!(
        tab.eval("Intl.DateTimeFormat().resolvedOptions().timeZone")
            .await,
        json!("Pacific/Chatham")
    );
    let quick = tab.eval("(async () => { const t = performance.now(); await fetch('/data?' + Math.random()); return performance.now() - t })()").await;
    assert!(quick.as_f64().unwrap() < 500.0, "{quick}");
    tab.close().await;
}
