//! Scripts against a fake `Tabs`: no browser.

use super::*;
use serde_json::json;
use std::sync::Mutex;
use std::time::Instant;

/// One tab, `t1`, that answers a few commands and remembers every call.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, Value)>>,
    url: Mutex<Option<String>>,
    closed: Mutex<bool>,
}

impl Fake {
    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn url(&self) -> String {
        self.url
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "https://example.com/".into())
    }

    fn answer(&self, command: &Value) -> Result<Value, String> {
        let url = self.url();
        Ok(match command["action"].as_str().unwrap_or_default() {
            "navigate" => {
                let to = command["url"].as_str().unwrap_or_default().to_owned();
                *self.url.lock().unwrap() = Some(to.clone());
                json!({"url": to, "title": "T"})
            }
            "snapshot" => json!({"url": url, "title": "T", "snapshot": "- button \"Go\" [ref=4]"}),
            "eval" if command["script"] == "document.title" => json!("T"),
            "eval" => json!({"script": command["script"]}),
            "type" => json!({"element": command["ref_id"], "value": command["text"]}),
            "fill" => json!({"element": command["ref_id"], "value": command["value"]}),
            "click" => json!({"clicked": command["ref_id"], "navigated": false}),
            "screenshot" => json!({
                "mime_type": "image/png", "width": 1, "height": 1, "scale": 1.0,
                "data": "iVBORw0KGgo=",
            }),
            "locate" => match command["steps"][0].get("css").and_then(Value::as_str) {
                Some("li") => {
                    json!({"count": 2, "refs": [5, 6], "elements": ["listitem", "listitem"]})
                }
                Some("#missing") => json!({"count": 0, "refs": [], "elements": []}),
                _ => json!({"count": 1, "refs": [7], "elements": ["button \"Go\""]}),
            },
            "handoff" => json!({"done": true}),
            "tabs" if command["op"] == "close" => {
                *self.closed.lock().unwrap() = true;
                json!({"closed": ["t1"]})
            }
            other => return Err(format!("`{other}` isn't faked")),
        })
    }
}

impl Tabs for Fake {
    fn call<'a>(
        &'a self,
        tab: &'a str,
        command: BrowserCommand,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            let wire = serde_json::to_value(&command).unwrap();
            self.calls
                .lock()
                .unwrap()
                .push((tab.to_owned(), wire.clone()));
            if tab != "t1" {
                return Err(format!("no tab {tab}"));
            }
            if wire["action"] == "handoff" {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            self.answer(&wire)
        })
    }

    fn info(&self, tab: &str) -> Option<TabInfo> {
        (tab == "t1" && !*self.closed.lock().unwrap()).then(|| TabInfo {
            url: self.url(),
            title: "T".into(),
        })
    }
}

fn script(code: &str) -> Script {
    Script {
        code: code.into(),
        tab: "t1".into(),
        state: "{}".into(),
        timeout: Duration::from_secs(10),
    }
}

async fn go(code: &str) -> (Report, Fake) {
    let fake = Fake::default();
    let report = run(script(code), &fake).await;
    (report, fake)
}

/// The text after the `tab:` line.
fn body(report: &Report) -> &str {
    report.text.split_once("\n\n").map_or("", |(_, rest)| rest)
}

/// The value (second section).
fn value(report: &Report) -> &str {
    report.text.split("\n\n").nth(1).unwrap_or_default()
}

fn command(value: Value) -> Value {
    value
}

// -- values and logs ----------------------------------------------------------

#[tokio::test]
async fn expressions_and_statements_return_values() {
    let (report, _) = go("1 + 1").await;
    assert!(!report.is_error, "{}", report.text);
    assert_eq!(value(&report), "2");

    let (report, _) = go("'plain text'").await;
    assert_eq!(value(&report), "plain text");

    let (report, _) = go("const when = new Date(0);\nreturn { x: [1, 2], when };").await;
    assert_eq!(
        value(&report),
        r#"{"x":[1,2],"when":"1970-01-01T00:00:00.000Z"}"#
    );

    let (report, _) = go("async (page) => typeof page.goto").await;
    assert_eq!(value(&report), "function");

    let (report, _) = go("1;;  \n").await;
    assert_eq!(value(&report), "1");

    let (report, _) = go("const a = 1").await;
    assert_eq!(value(&report), "undefined");
}

#[tokio::test]
async fn the_tab_comes_first() {
    let (report, _) = go("1").await;
    assert!(report.text.starts_with("tab: t1\n\n1"), "{}", report.text);
    assert_eq!(report.to_json()["tab"], "t1");
    assert_eq!(report.to_json()["is_error"], false);
    assert_eq!(report.to_json()["images"], json!([]));
}

#[tokio::test]
async fn console_lines_follow_the_value() {
    let (report, _) = go("console.log('hello', {a: 1}); console.error('bad'); return 3").await;
    let body = body(&report);
    assert!(body.starts_with("3\n"), "{body}");
    assert!(
        body.contains("Console:\nhello {\"a\":1}\n[error] bad"),
        "{body}"
    );

    let (report, _) = go(
        "console.assert(1 === 2, 'math'); console.log(undefined, () => 1, new Error('e'), 2n); return 1",
    )
    .await;
    assert!(
        report.text.contains("[error] Assertion failed: math"),
        "{}",
        report.text
    );
    assert!(
        report
            .text
            .contains("undefined [Function: anonymous] Error: e \"2\""),
        "{}",
        report.text
    );
}

#[tokio::test]
async fn console_is_bounded() {
    let (report, _) =
        go("for (let i = 0; i < 250; i++) console.log('line ' + i); console.log('x'.repeat(5000))")
            .await;
    assert!(report.text.contains("line 199"), "{}", report.text);
    assert!(!report.text.contains("line 200"), "{}", report.text);
    assert!(
        report.text.contains("51 more lines not shown"),
        "{}",
        report.text
    );
}

#[tokio::test]
async fn values_that_are_not_json_are_explained() {
    let (report, _) = go("return page").await;
    assert!(!report.is_error, "{}", report.text);
    assert!(
        report.text.contains("could not be represented as JSON"),
        "{}",
        report.text
    );

    let (report, _) = go("const a = {n: 1}; a.self = a; return a").await;
    assert_eq!(value(&report), r#"{"n":1}"#);
    assert!(
        report.text.contains("a circular reference"),
        "{}",
        report.text
    );

    let (report, _) = go(
        "return { m: new Map([[1, 'a']]), s: new Set([1, 1, 2]), t: new Uint8Array([1, 2]), n: NaN, u: undefined, f() {}, b: 10n, e: new TypeError('no') }",
    )
    .await;
    assert_eq!(
        value(&report),
        r#"{"m":{"1":"a"},"s":[1,2],"t":[1,2],"n":null,"b":"10","e":{"name":"TypeError","message":"no"}}"#
    );
    assert!(!report.text.contains("Warnings"), "{}", report.text);
}

#[tokio::test]
async fn long_results_are_cut() {
    let (report, _) = go("'x'.repeat(100005)").await;
    assert!(
        report.text.contains("5 more characters cut"),
        "{}",
        &report.text[report.text.len() - 300..]
    );
    assert!(
        report.text.contains("100005 characters"),
        "{}",
        &report.text[report.text.len() - 300..]
    );
}

// -- browse -------------------------------------------------------------------

#[tokio::test]
async fn browse_sends_commands() {
    let (report, fake) = go(
        "const r = await browse.click({ref: '[ref=3]'}); await browse.browser_snapshot(); return r",
    )
    .await;
    assert!(!report.is_error, "{}", report.text);
    assert_eq!(value(&report), r#"{"clicked":3,"navigated":false}"#);
    assert_eq!(
        fake.calls(),
        vec![
            (
                "t1".to_owned(),
                command(json!({"action": "click", "ref_id": 3}))
            ),
            ("t1".to_owned(), command(json!({"action": "snapshot"}))),
        ]
    );
}

#[tokio::test]
async fn browse_errors_name_what_went_wrong() {
    let (report, _) = go("await browse.teleport({to: 'mars'})").await;
    assert!(report.is_error);
    assert!(
        report.text.contains("browse.teleport is not supported"),
        "{}",
        report.text
    );
    assert!(report.text.contains("snapshot"), "{}", report.text);

    let (report, _) = go("const x = 1\nawait browse.click({ref: 1, tab: 'elsewhere'})").await;
    assert!(report.is_error);
    assert!(
        report.text.contains("Error: no tab elsewhere"),
        "{}",
        report.text
    );
    assert!(report.text.contains("script.js:2"), "{}", report.text);

    let (report, _) = go("try { await browse.nope() } catch (e) { return e.name }").await;
    assert_eq!(value(&report), "Error");

    let (report, _) = go("return (").await;
    assert!(report.is_error);
    assert!(report.text.contains("SyntaxError"), "{}", report.text);

    let (report, _) = go("await fetch('https://example.com')").await;
    assert!(report.is_error);
    assert!(report.text.contains("page.evaluate"), "{}", report.text);

    let (report, _) = go("await files.write('x', 'y')").await;
    assert!(report.is_error);
    assert!(report.text.contains("no files"), "{}", report.text);

    let (report, _) = go("require('fs')").await;
    assert!(report.text.contains("no Node modules"), "{}", report.text);

    let (report, _) = go("throw 'plain'").await;
    assert!(
        value(&report).starts_with("Thrown: plain"),
        "{}",
        report.text
    );
}

#[tokio::test]
async fn timeouts_from_commands_are_timeout_errors() {
    let (report, _) =
        go("try { await browse.wait_for({text: 'x'}) } catch (e) { return [e.name, e.message] }")
            .await;
    // The fake doesn't know wait_for: an ordinary error.
    assert!(
        value(&report).starts_with(r#"["Error","`wait_for` isn't faked"#),
        "{}",
        report.text
    );
}

#[tokio::test]
async fn unhandled_rejections_are_warned_about() {
    let (report, _) =
        go("browse.nope(); await new Promise((r) => setTimeout(r, 20)); return 1").await;
    assert!(!report.is_error);
    assert!(
        report
            .text
            .contains("Unhandled promise rejection: Error: browse.nope is not supported"),
        "{}",
        report.text
    );
}

// -- the page layer ------------------------------------------------------------

#[tokio::test]
async fn page_locators_become_refs() {
    let (report, fake) = go(
        "const response = await page.goto('https://example.com/a')\n\
         await page.getByRole('button', {name: 'Go'}).click()\n\
         await page.locator('li').nth(1).fill('hi')\n\
         const title = await page.title()\n\
         return {url: page.url(), ok: response.ok(), title, outline: await snapshot()}",
    )
    .await;
    assert!(!report.is_error, "{}", report.text);
    assert!(
        value(&report).starts_with(
            r#"{"url":"https://example.com/a","ok":true,"title":"T","outline":"- button \"Go\" [ref=4]"}"#
        ),
        "{}",
        report.text
    );
    assert!(
        report.text.contains(
            "Page: moved from https://example.com/ to https://example.com/a; 1 navigation"
        ),
        "{}",
        report.text
    );
    let calls: Vec<Value> = fake.calls().into_iter().map(|(_, c)| c).collect();
    assert_eq!(
        calls[0],
        json!({"action": "navigate", "url": "https://example.com/a"})
    );
    assert_eq!(
        calls[1],
        json!({"action": "locate", "steps": [{"role": "button", "name": "Go", "exact": false}], "limit": 5})
    );
    assert_eq!(calls[2], json!({"action": "click", "ref_id": 7}));
    assert_eq!(
        calls[3],
        json!({"action": "locate", "steps": [{"css": "li"}, {"nth": 1}], "limit": 5})
    );
    assert_eq!(
        calls[4],
        json!({"action": "fill", "ref_id": 5, "value": "hi"})
    );
    assert_eq!(
        calls[5],
        json!({"action": "eval", "script": "document.title"})
    );
    assert_eq!(calls[6], json!({"action": "snapshot"}));
}

#[tokio::test]
async fn locators_are_strict() {
    let (report, _) = go("await page.locator('li').click()").await;
    assert!(report.is_error);
    assert!(
        report.text.contains("matched 2 elements"),
        "{}",
        report.text
    );
    assert!(report.text.contains("listitem"), "{}", report.text);
    assert!(report.text.contains("script.js:1"), "{}", report.text);
}

#[tokio::test]
async fn locators_wait_then_time_out() {
    let started = Instant::now();
    let (report, fake) = go("await page.click('#missing', {timeout: 300})").await;
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(report.is_error);
    assert!(report.text.contains("TimeoutError"), "{}", report.text);
    assert!(report.text.contains("found nothing"), "{}", report.text);
    assert!(fake.calls().len() >= 3, "it retried");
}

#[tokio::test]
async fn locators_count_and_read() {
    let (report, _) = go("await page.locator('li').count()").await;
    assert_eq!(value(&report), "2");

    let (_, fake) =
        go("await page.locator('role=link[name=\"Docs\"i] >> text=Read').count()").await;
    assert_eq!(
        fake.calls()[0].1,
        json!({"action": "locate", "steps": [
            {"role": "link", "name": "Docs", "exact": false},
            {"text": "Read", "exact": false}
        ], "limit": 0})
    );

    let (_, fake) = go(
        "await page.locator('#a >> nth=-1 >> \"Save\" >> data-testid=go >> id=x').count();\
         await page.getByText(/sa+ve/i).filter({hasText: 'x', visible: true}).count();\
         await page.getByTestId('t').count()",
    )
    .await;
    assert_eq!(
        fake.calls()[0].1["steps"],
        json!([
            {"css": "#a"}, {"nth": -1}, {"text": "Save", "exact": true},
            {"testid": "go", "exact": true}, {"css": "[id=\"x\"]"}
        ])
    );
    assert_eq!(
        fake.calls()[1].1["steps"],
        json!([
            {"text": "sa+ve", "regex": "i"}, {"has_text": "x", "exact": false}, {"visible": true}
        ])
    );
    assert_eq!(
        fake.calls()[2].1["steps"],
        json!([{"testid": "t", "exact": true}])
    );

    let (report, fake) = go("await ref('[ref=9]').textContent()").await;
    assert!(report.is_error, "the fake has no eval_on");
    assert_eq!(fake.calls()[0].1["action"], "eval_on");
    assert_eq!(fake.calls()[0].1["refs"], json!([9]));
}

#[tokio::test]
async fn unsupported_calls_say_what_to_use() {
    let (report, _) = go("await page.route('**', () => {})").await;
    assert!(report.is_error);
    assert!(
        report.text.contains("page.route() is not available"),
        "{}",
        report.text
    );

    let (report, _) = go("page.locator('xpath=//a')").await;
    assert!(report.text.contains("XPath"), "{}", report.text);

    let (report, _) = go("ref(3).locator('a')").await;
    assert!(report.text.contains("ref(3).locator()"), "{}", report.text);

    let (report, _) = go("page.on('dialog', (d) => d.dismiss()); return 1").await;
    assert!(!report.is_error);
    assert!(report.text.contains("dismiss them"), "{}", report.text);
}

#[tokio::test]
async fn handoffs_pause_the_clock() {
    let fake = Fake::default();
    let report = run(
        Script {
            timeout: Duration::from_millis(200),
            ..script("await handoff('Sign in please'); return 1")
        },
        &fake,
    )
    .await;
    assert!(!report.is_error, "{}", report.text);
    assert!(report.text.contains("1 handoff"), "{}", report.text);
    assert_eq!(
        fake.calls()[0].1,
        json!({"action": "handoff", "reason": "Sign in please"})
    );
}

#[tokio::test]
async fn closing_the_tab_is_reported() {
    let (report, _) = go("await page.close(); return page.isClosed()").await;
    assert_eq!(value(&report), "true");
    assert!(
        report.text.contains("Page: the tab closed"),
        "{}",
        report.text
    );
}

// -- pictures -------------------------------------------------------------------

#[tokio::test]
async fn returned_pictures_are_attached() {
    let (report, _) = go("const shot = await page.screenshot(); return {shot}").await;
    assert!(!report.is_error, "{}", report.text);
    assert!(
        value(&report)
            .starts_with(r#"{"shot":{"type":"image","mimeType":"image/png","width":1,"height":1"#),
        "{}",
        report.text
    );
    assert_eq!(report.images.len(), 1);
    assert_eq!(report.images[0].data, "iVBORw0KGgo=");
    assert_eq!(report.to_json()["images"][0]["mime_type"], "image/png");
    assert!(!report.text.contains("not returned"), "{}", report.text);
}

#[tokio::test]
async fn pictures_not_returned_are_counted() {
    let (report, _) = go("await page.screenshot(); return 1").await;
    assert!(report.images.is_empty());
    assert!(
        report.text.contains("1 picture not returned"),
        "{}",
        report.text
    );

    let (report, _) = go("(await page.screenshot()).toString('base64')").await;
    assert!(
        body(&report).starts_with("iVBORw0KGgo=\n"),
        "{}",
        report.text
    );
}

// -- timers and state -------------------------------------------------------------

#[tokio::test]
async fn timers_run() {
    let (report, _) = go(
        "let n = 0; const t = setInterval(() => n++, 10); await new Promise((r) => setTimeout(r, 120)); clearInterval(t); return n > 3",
    )
    .await;
    assert_eq!(value(&report), "true", "{}", report.text);

    let (report, _) =
        go("setTimeout(() => { throw new Error('late') }, 1); await new Promise((r) => setTimeout(r, 30)); return 2").await;
    assert_eq!(value(&report), "2");
    assert!(
        report
            .text
            .contains("[error] Uncaught error in a timer: Error: late"),
        "{}",
        report.text
    );

    let (report, _) = go("setTimeout('x', 1)").await;
    assert!(report.text.contains("TypeError"), "{}", report.text);
}

#[tokio::test]
async fn a_promise_nothing_settles_ends_the_run() {
    let started = Instant::now();
    let (report, _) = go("await new Promise(() => {})").await;
    assert!(report.is_error);
    assert!(
        report.text.contains("nothing will ever settle"),
        "{}",
        report.text
    );
    assert!(started.elapsed() < Duration::from_secs(2));

    // A cleared timer is nothing pending either.
    let (report, _) =
        go("await new Promise((r) => { const t = setTimeout(r, 60000); clearTimeout(t) })").await;
    assert!(
        report.text.contains("nothing will ever settle"),
        "{}",
        report.text
    );
}

#[tokio::test]
async fn state_is_kept_between_runs() {
    let fake = Fake::default();
    let first = run(
        script("state.count = (state.count || 0) + 1; return state.count"),
        &fake,
    )
    .await;
    assert_eq!(value(&first), "1");
    assert_eq!(first.state.as_deref(), Some("{\"count\":1}"));
    let second = run(
        Script {
            state: first.state.unwrap(),
            ..script("state.count += 1; return state.count")
        },
        &fake,
    )
    .await;
    assert_eq!(value(&second), "2");

    // Anything but an object is a fresh state.
    let report = run(
        Script {
            state: "[1]".into(),
            ..script("JSON.stringify(state)")
        },
        &fake,
    )
    .await;
    assert_eq!(value(&report), "{}");
}

#[tokio::test]
async fn state_too_big_is_not_kept() {
    let (report, _) = go("state.big = 'x'.repeat(2 * 1024 * 1024); return 1").await;
    assert_eq!(report.state, None);
    assert!(
        report
            .text
            .contains("this run's changes to it were dropped"),
        "{}",
        report.text
    );
}

// -- limits ----------------------------------------------------------------------

#[tokio::test]
async fn a_loop_that_never_awaits_is_stopped() {
    let fake = Fake::default();
    let started = Instant::now();
    let report = run(
        Script {
            timeout: Duration::from_secs(30),
            ..script("state.mark = 'x'; while (true) {}")
        },
        &fake,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(6));
    assert!(report.is_error);
    assert!(
        report.text.contains("more than 2 seconds without awaiting"),
        "{}",
        report.text
    );
    assert_eq!(report.state.as_deref(), Some("{\"mark\":\"x\"}"));
}

#[tokio::test]
async fn the_overall_time_limit_ends_the_run() {
    let fake = Fake::default();
    let started = Instant::now();
    let report = run(
        Script {
            timeout: Duration::from_millis(200),
            ..script("state.slow = 1; console.log('waiting'); await new Promise((r) => setTimeout(r, 5000))")
        },
        &fake,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(report.is_error);
    assert!(
        report.text.contains("timed out after 200 ms"),
        "{}",
        report.text
    );
    assert!(report.text.contains("Console:\nwaiting"), "{}", report.text);
    assert_eq!(report.state.as_deref(), Some("{\"slow\":1}"));
}

#[tokio::test]
async fn running_out_of_memory_is_an_error() {
    let (report, _) = go(
        "const a = []; for (let i = 0; i < 1e8; i++) a.push('x'.repeat(100) + i); return a.length",
    )
    .await;
    assert!(report.is_error);
    assert!(report.text.contains("out of memory"), "{}", report.text);
}

#[tokio::test]
async fn deep_recursion_is_an_error() {
    let (report, _) = go("const f = (n) => f(n + 1); f(0)").await;
    assert!(report.is_error);
    let text = report.text.to_lowercase();
    assert!(
        text.contains("stack") || text.contains("rangeerror") || text.contains("internalerror"),
        "{}",
        report.text
    );
}

#[tokio::test]
async fn dropping_the_run_stops_the_engine() {
    let ended = |code: &str| {
        crate::engine::ENDED
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == code)
    };
    for code in [
        "/* busy */ while (true) {}",
        "/* waiting */ await new Promise((r) => setTimeout(r, 60000))",
        "/* asking */ while (true) page.url()",
    ] {
        let fake = Fake::default();
        let running = run(
            Script {
                timeout: Duration::from_secs(60),
                ..script(code)
            },
            &fake,
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), running)
                .await
                .is_err()
        );
        let dropped = Instant::now();
        while !ended(code) {
            assert!(
                dropped.elapsed() < Duration::from_secs(1),
                "{code} still runs"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[test]
fn cutting_keeps_whole_characters() {
    assert_eq!(cut("héllo", 10), "héllo");
    assert_eq!(cut("héllo", 2), "hé\n… (3 more characters cut)");
}
