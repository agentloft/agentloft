//! Drives the browser panel through a scripted sequence and prints every
//! command and result, plus PASS/FAIL checks. Exits non-zero on failures.
//!
//! ```sh
//! cargo run -p loft-browser --example browser_automation -- \
//!     [--out-dir DIR] [--full] [--offline] [--keep-open]
//! ```
//!
//! 1. A local form served by a tiny HTTP server in this process:
//!    navigate → snapshot → type → click → snapshot → eval, then a link
//!    click, back, and Enter-to-submit.
//! 2. A React controlled input (React from unpkg; skipped `--offline`).
//! 3. https://example.com: navigate → snapshot → click → snapshot → eval → back.
//! 4. Hacker News and a GitHub repository page: navigate → snapshot.
//! 5. Policy and error cases, and a screenshot written to `--out-dir`.
//!
//! `--full` prints whole snapshots (otherwise long ones are shortened).

use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Bounds, Entity, WindowBounds, WindowOptions, px,
    size,
};
use loft_browser::{BrowserCommand, BrowserError, BrowserView};
use serde_json::Value;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const FORM_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Loft test form</title>
<style>body{font:15px -apple-system,sans-serif;margin:24px;max-width:640px}
label{display:block;margin:10px 0}input,select{margin-left:6px}</style></head>
<body>
<h1>Create an account</h1>
<p>Fill in the form and press <b>Create account</b>.</p>
<form id="signup">
  <label>Name <input name="name" placeholder="Ada Lovelace" autocomplete="off"></label>
  <label for="email">Email</label><input id="email" name="email" type="email" placeholder="you@example.com">
  <label>Plan <select name="plan"><option>Free</option><option>Pro</option><option>Team</option></select></label>
  <label><input type="checkbox" name="terms"> I accept the terms</label>
  <button type="submit">Create account</button>
  <button type="button" disabled>Disabled button</button>
</form>
<p id="result">Not submitted yet.</p>
<ul><li><a href="/second">Go to the second page</a></li><li><a href="/second" target="_blank">Open in a new window</a></li></ul>
<div style="display:none"><a href="/hidden">Hidden link</a></div>
<script>
  // Like a framework-controlled input: state only changes through input events.
  const state = { name: '' };
  document.querySelector('[name=name]').addEventListener('input', (e) => { state.name = e.target.value; });
  let keys = 0;
  document.addEventListener('keydown', () => { keys++; document.body.dataset.keys = keys; });
  document.getElementById('signup').addEventListener('submit', (e) => {
    e.preventDefault();
    const data = new FormData(e.target);
    document.getElementById('result').textContent =
      `Submitted: name=${state.name} email=${data.get('email')} plan=${data.get('plan')} terms=${data.get('terms') ? 'yes' : 'no'}`;
    document.title = 'Submitted';
  });
</script>
</body></html>"#;

const SECOND_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Second page</title></head>
<body><h1>Second page</h1><p>You followed the link.</p><a href="/">Back to the form</a></body></html>"#;

const REACT_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>React controlled input</title>
<script src="https://unpkg.com/react@18.3.1/umd/react.production.min.js"></script>
<script src="https://unpkg.com/react-dom@18.3.1/umd/react-dom.production.min.js"></script>
</head><body><div id="root"></div>
<script>
  const e = React.createElement;
  function App() {
    const [query, setQuery] = React.useState('');
    const [sent, setSent] = React.useState('');
    return e('form', { onSubmit: (ev) => { ev.preventDefault(); setSent(query); } },
      e('label', null, 'Query ', e('input', { value: query, placeholder: 'Type here',
        onChange: (ev) => setQuery(ev.target.value) })),
      e('button', { type: 'submit' }, 'Send'),
      e('p', { id: 'echo' }, 'Echo: ' + query),
      e('p', { id: 'sent' }, 'Sent: ' + sent));
  }
  ReactDOM.createRoot(document.getElementById('root')).render(e(App));
</script></body></html>"#;

fn serve(listener: TcpListener) {
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || handle(stream));
    }
}

fn handle(mut stream: TcpStream) {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    let (status, body) = match path.split('?').next().unwrap_or("/") {
        "/" => ("200 OK", FORM_PAGE),
        "/second" => ("200 OK", SECOND_PAGE),
        "/react" => ("200 OK", REACT_PAGE),
        _ => ("404 Not Found", "<h1>Not found</h1>"),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

struct Options {
    out_dir: PathBuf,
    full: bool,
    offline: bool,
    keep_open: bool,
}

struct Driver {
    window: AnyWindowHandle,
    browser: Entity<BrowserView>,
    full: bool,
    passed: usize,
    failures: Vec<String>,
}

impl Driver {
    async fn exec(&mut self, cx: &mut AsyncApp, cmd: BrowserCommand) -> anyhow::Result<Value> {
        let shown = match &cmd {
            BrowserCommand::Eval { script } if script.len() > 120 => {
                format!("{{\"action\":\"eval\",\"script\":\"{}…\"}}", &script[..100])
            }
            _ => serde_json::to_string(&cmd)?,
        };
        println!("\n>>> {shown}");
        let start = Instant::now();
        let browser = self.browser.clone();
        let task = self.window.update(cx, |_, window, cx| {
            browser.update(cx, |browser, cx| browser.execute(cmd, window, cx))
        })?;
        let result = task.await;
        let secs = start.elapsed().as_secs_f32();
        match &result {
            Ok(value) => print_result(value, secs, self.full),
            Err(err) => println!("<<< error after {secs:.2}s: {err:#}"),
        }
        result
    }

    fn check(&mut self, ok: bool, what: &str) {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        if ok {
            self.passed += 1;
        } else {
            self.failures.push(what.to_owned());
        }
    }

    async fn snapshot(&mut self, cx: &mut AsyncApp) -> String {
        match self.exec(cx, BrowserCommand::Snapshot).await {
            Ok(v) => v["snapshot"].as_str().unwrap_or_default().to_owned(),
            Err(_) => String::new(),
        }
    }
}

fn print_result(value: &Value, secs: f32, full: bool) {
    if let Some(snapshot) = value.get("snapshot").and_then(Value::as_str) {
        println!(
            "<<< ok in {secs:.2}s: snapshot of {} ({} chars, {} refs, truncated: {})",
            value["url"],
            snapshot.chars().count(),
            value["refs"],
            value["truncated"]
        );
        let lines: Vec<&str> = snapshot.lines().collect();
        let limit = if full { usize::MAX } else { 45 };
        for line in lines.iter().take(limit) {
            println!("    | {line}");
        }
        if lines.len() > limit {
            println!(
                "    | … {} more lines (run with --full)",
                lines.len() - limit
            );
            if let Some(last) = lines.last().filter(|l| l.starts_with("[Truncated")) {
                println!("    | {last}");
            }
        }
        return;
    }
    let mut value = value.clone();
    if let Some(data) = value.get_mut("data") {
        *data = format!("<{} base64 chars>", data.as_str().map_or(0, str::len)).into();
    }
    println!(
        "<<< ok in {secs:.2}s: {}",
        serde_json::to_string_pretty(&value).unwrap_or_default()
    );
}

/// Finds `[ref=N] role "name…"` in a snapshot where `name` contains `needle`.
fn find_ref(snapshot: &str, role: &str, needle: &str) -> Option<u32> {
    let needle = needle.to_lowercase();
    let mut rest = snapshot;
    while let Some(ix) = rest.find("[ref=") {
        rest = &rest[ix + 5..];
        let end = rest.find(']')?;
        let id: u32 = rest[..end].parse().ok()?;
        let after = rest[end + 1..].trim_start();
        let (found_role, tail) = after.split_once(' ').unwrap_or((after, ""));
        if found_role == role {
            let name = tail
                .strip_prefix('"')
                .and_then(|t| t.split('"').next())
                .unwrap_or_default();
            if name.to_lowercase().contains(&needle) {
                return Some(id);
            }
        }
    }
    None
}

fn is_browser_error(err: &anyhow::Error, f: impl Fn(&BrowserError) -> bool) -> bool {
    err.downcast_ref::<BrowserError>().is_some_and(f)
}

async fn script(mut d: Driver, cx: &mut AsyncApp, base: String, opts: Options) {
    let executor = cx.background_executor().clone();
    let sleep = |ms| executor.timer(Duration::from_millis(ms));

    // ---- 1. Local form --------------------------------------------------------
    println!("\n======== Local form at {base}/ ========");
    let nav = d
        .exec(
            cx,
            BrowserCommand::Navigate {
                url: format!("{base}/"),
            },
        )
        .await;
    d.check(
        nav.as_ref().is_ok_and(|v| v["title"] == "Loft test form"),
        "navigate resolves with the page title",
    );
    let snap = d.snapshot(cx).await;
    d.check(
        snap.contains("# Create an account"),
        "snapshot has the heading",
    );
    d.check(
        !snap.contains("Hidden link"),
        "snapshot skips hidden content",
    );
    let name = find_ref(&snap, "input", "Name");
    let email = find_ref(&snap, "input", "Email");
    let plan = find_ref(&snap, "select", "Plan");
    let terms = find_ref(&snap, "checkbox", "accept");
    let create = find_ref(&snap, "button", "Create account");
    let disabled = find_ref(&snap, "button", "Disabled");
    let second = find_ref(&snap, "link", "second page");
    d.check(
        [name, email, plan, terms, create, disabled, second]
            .iter()
            .all(Option::is_some),
        "snapshot lists inputs, select, checkbox, buttons and links with refs",
    );

    if let (Some(name), Some(email), Some(plan), Some(terms), Some(create)) =
        (name, email, plan, terms, create)
    {
        let r = d
            .exec(
                cx,
                BrowserCommand::Type {
                    ref_id: name,
                    text: "Ada Lovelace".into(),
                    submit: false,
                },
            )
            .await;
        d.check(
            r.is_ok_and(|v| v["value"] == "Ada Lovelace"),
            "type into the name input",
        );
        let r = d
            .exec(
                cx,
                BrowserCommand::Type {
                    ref_id: email,
                    text: "ada@example.com".into(),
                    submit: false,
                },
            )
            .await;
        d.check(r.is_ok(), "type into the email input");
        let r = d
            .exec(
                cx,
                BrowserCommand::Type {
                    ref_id: plan,
                    text: "pro".into(),
                    submit: false,
                },
            )
            .await;
        d.check(
            r.is_ok_and(|v| v["value"] == "Pro"),
            "type picks a select option",
        );
        let r = d.exec(cx, BrowserCommand::Click { ref_id: terms }).await;
        d.check(r.is_ok(), "click the checkbox");
        let r = d.exec(cx, BrowserCommand::Click { ref_id: create }).await;
        d.check(
            r.is_ok_and(|v| v["navigated"] == false),
            "click the submit button",
        );
        let snap2 = d.snapshot(cx).await;
        let expected = "Submitted: name=Ada Lovelace email=ada@example.com plan=Pro terms=yes";
        d.check(
            snap2.contains(expected),
            "second snapshot shows the submitted form",
        );
        d.check(
            snap2.contains("(checked)"),
            "second snapshot shows the checkbox checked",
        );
        let same_refs = find_ref(&snap2, "button", "Create account") == Some(create);
        d.check(same_refs, "refs are stable across snapshots");
        let r = d
            .exec(
                cx,
                BrowserCommand::Eval {
                    script: "document.querySelector('#result').textContent".into(),
                },
            )
            .await;
        d.check(
            r.is_ok_and(|v| v == expected),
            "eval returns the expression's value",
        );
        let r = d
            .exec(cx, BrowserCommand::Eval {
                script: "const path = await new Promise(r => setTimeout(() => r(location.pathname), 30));\n\
                         return { path, forms: document.forms.length, title: document.title, el: document.body.firstElementChild };"
                    .into(),
            })
            .await;
        d.check(
            r.is_ok_and(|v| {
                v["path"] == "/"
                    && v["forms"] == 1
                    && v["el"].as_str().is_some_and(|s| s.starts_with("<h1>"))
            }),
            "eval runs an async function body and serializes DOM nodes",
        );
        let r = d
            .exec(
                cx,
                BrowserCommand::Eval {
                    script: "undefinedVariable.foo".into(),
                },
            )
            .await;
        d.check(
            r.as_ref().is_err_and(|e| {
                is_browser_error(
                    e,
                    |e| matches!(e, BrowserError::Script(m) if m.contains("ReferenceError")),
                )
            }),
            "eval reports JavaScript exceptions",
        );

        // Typing with submit: true presses Enter in the field (implicit submission).
        let r = d
            .exec(
                cx,
                BrowserCommand::Type {
                    ref_id: name,
                    text: "Grace Hopper".into(),
                    submit: true,
                },
            )
            .await;
        d.check(
            r.is_ok_and(|v| v["submit"] == "submitted the form"),
            "type with submit submits the form",
        );
        let r = d
            .exec(
                cx,
                BrowserCommand::Eval {
                    script: "document.querySelector('#result').textContent".into(),
                },
            )
            .await;
        d.check(
            r.is_ok_and(|v| v.as_str().is_some_and(|s| s.contains("name=Grace Hopper"))),
            "the framework-style state saw the typed value",
        );
        let r = d
            .exec(cx, BrowserCommand::Press { key: "Tab".into() })
            .await;
        d.check(
            r.is_ok_and(|v| {
                v["default_action"]
                    .as_str()
                    .is_some_and(|s| s.contains("Email"))
            }),
            "press Tab moves focus to the next field",
        );
        let r = d.exec(cx, BrowserCommand::Press { key: "x".into() }).await;
        d.check(r.is_ok(), "press a printable key");
        let r = d.exec(cx, BrowserCommand::Eval { script: "[document.activeElement.name, document.activeElement.value, document.body.dataset.keys]".into() }).await;
        // Tab selected the field's text, as browsers do, so the key replaced it.
        d.check(
            r.is_ok_and(|v| v[0] == "email" && v[1] == "x" && v[2] == "3"),
            "the key replaced the selected text of the focused field",
        );
    }
    if let Some(disabled) = disabled {
        let r = d.exec(cx, BrowserCommand::Click { ref_id: disabled }).await;
        d.check(r.is_err(), "clicking a disabled button is an error");
    }
    if let Some(second) = second {
        let r = d.exec(cx, BrowserCommand::Click { ref_id: second }).await;
        d.check(
            r.is_ok_and(|v| v["navigated"] == true && v["title"] == "Second page"),
            "clicking a link navigates and waits for the new page",
        );
        let r = d.exec(cx, BrowserCommand::Back).await;
        // The form page comes back from the page cache, with its "Submitted" title.
        d.check(
            r.is_ok_and(|v| v["url"] == format!("{base}/").as_str()),
            "back returns to the form",
        );
    }
    let snap = d.snapshot(cx).await;
    if let Some(popup) = find_ref(&snap, "link", "new window") {
        let r = d.exec(cx, BrowserCommand::Click { ref_id: popup }).await;
        d.check(
            r.is_ok_and(|v| v["title"] == "Second page"),
            "target=_blank links open in place",
        );
    }
    let r = d.exec(cx, BrowserCommand::Scroll { dy: 300. }).await;
    d.check(r.is_ok(), "scroll");

    // ---- policy & errors -----------------------------------------------------------
    println!("\n======== Policy and errors ========");
    let r = d
        .exec(
            cx,
            BrowserCommand::Navigate {
                url: "javascript:alert(1)".into(),
            },
        )
        .await;
    d.check(
        r.as_ref()
            .is_err_and(|e| is_browser_error(e, |e| matches!(e, BrowserError::Blocked(_)))),
        "javascript: URLs are blocked",
    );
    let r = d.exec(cx, BrowserCommand::Click { ref_id: 999_999 }).await;
    d.check(
        r.as_ref()
            .is_err_and(|e| is_browser_error(e, |e| *e == BrowserError::RefNotFound(999_999))),
        "unknown refs report RefNotFound",
    );
    let started = Instant::now();
    let r = d
        .exec(
            cx,
            BrowserCommand::Navigate {
                url: "https://no-such-host.invalid/".into(),
            },
        )
        .await;
    d.check(
        r.as_ref().is_err_and(|e| {
            is_browser_error(e, |e| matches!(e, BrowserError::NavigationFailed(_)))
        }) && started.elapsed() < Duration::from_secs(15),
        "an unreachable host fails fast with NavigationFailed",
    );
    let file = opts.out_dir.join("loft-form.html");
    let _ = std::fs::write(&file, FORM_PAGE);
    let file_url = url::Url::from_file_path(&file)
        .map(|u| u.to_string())
        .unwrap_or_default();
    let r = d
        .exec(
            cx,
            BrowserCommand::Navigate {
                url: file_url.clone(),
            },
        )
        .await;
    d.check(
        r.is_ok_and(|v| v["url"] == file_url.as_str()),
        "the host may navigate to a file: URL",
    );
    let _ = d
        .exec(
            cx,
            BrowserCommand::Navigate {
                url: format!("{base}/"),
            },
        )
        .await;
    let r = d
        .exec(
            cx,
            BrowserCommand::Eval {
                script: format!("location.href = {:?}; 'requested'", file_url),
            },
        )
        .await;
    d.check(r.is_ok(), "a page can request a file: navigation …");
    sleep(1500).await;
    let r = d
        .exec(
            cx,
            BrowserCommand::Eval {
                script: "location.href".into(),
            },
        )
        .await;
    d.check(
        r.is_ok_and(|v| v.as_str().is_some_and(|u| u.starts_with("http://"))),
        "… but the policy keeps it on the http page",
    );

    // ---- 2. React -------------------------------------------------------------------
    if !opts.offline {
        println!("\n======== React controlled input ========");
        let r = d
            .exec(
                cx,
                BrowserCommand::Navigate {
                    url: format!("{base}/react"),
                },
            )
            .await;
        if r.is_ok() {
            sleep(300).await;
            let snap = d.snapshot(cx).await;
            if let Some(query) = find_ref(&snap, "input", "Query") {
                let r = d
                    .exec(
                        cx,
                        BrowserCommand::Type {
                            ref_id: query,
                            text: "hello react".into(),
                            submit: true,
                        },
                    )
                    .await;
                d.check(r.is_ok(), "type into a React controlled input with submit");
                let snap = d.snapshot(cx).await;
                d.check(
                    snap.contains("Echo: hello react") && snap.contains("Sent: hello react"),
                    "React state updated from typing and the submit",
                );
            } else {
                d.check(false, "React page rendered its input (needs unpkg.com)");
            }
        }
    }

    // ---- 3. example.com ---------------------------------------------------------------
    if !opts.offline {
        println!("\n======== https://example.com ========");
        let r = d
            .exec(
                cx,
                BrowserCommand::Navigate {
                    url: "example.com".into(),
                },
            )
            .await;
        d.check(
            r.is_ok_and(|v| v["title"] == "Example Domain"),
            "navigate to example.com",
        );
        let snap = d.snapshot(cx).await;
        d.check(
            snap.contains("This domain is for use in") && snap.contains("] link \""),
            "example.com snapshot has its text and link",
        );
        let link = find_ref(&snap, "link", "");
        if let Some(link) = link {
            let r = d.exec(cx, BrowserCommand::Click { ref_id: link }).await;
            d.check(
                r.is_ok_and(|v| v["navigated"] == true),
                "click the example.com link",
            );
            let snap = d.snapshot(cx).await;
            d.check(snap.contains("[ref="), "snapshot of the linked page");
            let r = d
                .exec(
                    cx,
                    BrowserCommand::Eval {
                        script: "document.title".into(),
                    },
                )
                .await;
            d.check(r.is_ok_and(|v| v.is_string()), "eval document.title");
            let r = d.exec(cx, BrowserCommand::Back).await;
            d.check(
                r.is_ok_and(|v| v["title"] == "Example Domain"),
                "back to example.com",
            );
        } else {
            d.check(false, "example.com has a link");
        }

        // ---- 4. Real sites ------------------------------------------------------------
        for (url, expect) in [
            ("https://news.ycombinator.com", "Hacker News"),
            ("https://github.com/rust-lang/rust", "rust-lang"),
        ] {
            println!("\n======== {url} ========");
            let r = d
                .exec(cx, BrowserCommand::Navigate { url: url.into() })
                .await;
            d.check(r.is_ok(), &format!("navigate to {url}"));
            let snap = d.snapshot(cx).await;
            let refs = snap.matches("[ref=").count();
            d.check(
                snap.contains(expect) && refs > 20 && snap.chars().count() <= 25_500,
                &format!("{url} snapshot has content, {refs} refs, within the cap"),
            );
        }
    }

    // ---- 5. Screenshot ------------------------------------------------------------------
    println!("\n======== Screenshot ========");
    let r = d.exec(cx, BrowserCommand::Screenshot).await;
    match r {
        Ok(v) => {
            use base64::Engine as _;
            let png = base64::engine::general_purpose::STANDARD
                .decode(v["data"].as_str().unwrap_or_default())
                .unwrap_or_default();
            let path = opts.out_dir.join("loft-browser-screenshot.png");
            let saved = std::fs::write(&path, &png).is_ok();
            println!("saved {} ({} bytes)", path.display(), png.len());
            d.check(
                saved && png.starts_with(b"\x89PNG"),
                "screenshot returns a PNG",
            );
        }
        Err(_) => d.check(false, "screenshot returns a PNG"),
    }

    // ---- 6. Hidden panel --------------------------------------------------------------
    println!("\n======== Hidden panel (set_visible(false)) ========");
    let set_visible = |visible: bool, d: &Driver, cx: &mut AsyncApp| {
        let browser = d.browser.clone();
        d.window
            .update(cx, |_, _, cx| {
                browser.update(cx, |b, cx| b.set_visible(visible, cx))
            })
            .ok();
    };
    set_visible(false, &d, cx);
    let r = d
        .exec(
            cx,
            BrowserCommand::Navigate {
                url: format!("{base}/second"),
            },
        )
        .await;
    d.check(r.is_ok(), "navigate while the panel is hidden");
    let snap = d.snapshot(cx).await;
    d.check(
        snap.contains("# Second page"),
        "snapshot while the panel is hidden",
    );
    let r = d.exec(cx, BrowserCommand::Screenshot).await;
    println!(
        "(informational) screenshot while hidden: {}",
        match &r {
            Ok(v) => format!("ok, {}x{}", v["width"], v["height"]),
            Err(e) => format!("error: {e:#}"),
        }
    );
    set_visible(true, &d, cx);

    println!(
        "\n======== {} passed, {} failed ========",
        d.passed,
        d.failures.len()
    );
    for failure in &d.failures {
        println!("FAILED: {failure}");
    }
    let code = if d.failures.is_empty() { 0 } else { 1 };
    if !opts.keep_open {
        std::process::exit(code);
    }
}

fn main() {
    let mut opts = Options {
        out_dir: std::env::temp_dir().join("loft-browser-automation"),
        full: false,
        offline: false,
        keep_open: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out-dir" => opts.out_dir = args.next().expect("--out-dir DIR").into(),
            "--full" => opts.full = true,
            "--offline" => opts.offline = true,
            "--keep-open" => opts.keep_open = true,
            other => panic!("unknown argument {other}"),
        }
    }
    std::fs::create_dir_all(&opts.out_dir).expect("create --out-dir");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    std::thread::spawn(move || serve(listener));

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            loft_browser::bind_keys(cx);
            let bounds = Bounds::centered(None, size(px(1100.), px(800.)), cx);
            let (window, browser) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| BrowserView::new(window, cx)),
            )
            .expect("failed to open window");
            cx.activate(true);
            let driver = Driver {
                window,
                browser,
                full: opts.full,
                passed: 0,
                failures: Vec::new(),
            };
            cx.spawn(async move |cx| script(driver, cx, base, opts).await)
                .detach();
        });
}
