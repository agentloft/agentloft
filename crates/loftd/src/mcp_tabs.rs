//! The Agentloft MCP server's tab tools (ADR-0017 §9): `browser_tabs`,
//! `browser_handoff`, `browser_demonstrate` and `browser_status`, the `tab`
//! every other browser tool takes, and the line a result starts with to say
//! which tab it ran in.

use crate::mcp::tool;
use loft_protocol::BrowserReply;
use serde_json::{Value, json};

/// The tab tools' part of the server's instructions.
pub const INSTRUCTIONS: &str = "Tabs: this thread's browser tabs are shared with the user. \
Every `browser_*` tool takes `tab`, a tab's id or a name you gave it (default: the tab used \
last); a result says first which tab it ran in, and windows a page opens show up as new tabs. \
Prefer tabs of your own, opened with `browser_tabs` (`new`, optionally with `name`, \
`allowed_origins`, `read_only`), over the user's, and close them when you are done. When only a \
person can go on (a sign-in, 2FA, a captcha, a payment), call `browser_handoff` with the tab and \
what to do, and wait for it; never solve a captcha yourself. `browser_demonstrate` asks the user \
to show you how something is done and returns the steps they took. `browser_status` says \
whether the browser can run here and, if not, what to tell the user.";

const TABS: &str = "List, open or close this thread's browser tabs. The user sees and uses the \
same tabs; the ones they opened are marked `opened_by: person`: use those only when asked.\n\
- `list` (the default): every tab with its handle (`tab`, and `name` if it has one), url, title, \
who opened it, how long it has been idle, whether a call is running in it (`busy`), its limits, \
and whether it is waiting for the user.\n\
- `new`: opens a tab beside the user's (their window stays where it is), at `url` or blank, waits \
for it to load and returns it. With a `name` (lowercase letters, digits and dashes, e.g. `docs`), \
asking for that name again returns the same tab without moving it; the name works as `tab` in \
every tool, and a named tab is never closed for going unused. Fixed when the tab opens: \
`allowed_origins` (you may send the tab and the popups it opens only to these, written like \
`example.com`, `*.example.com` or `http://localhost:3000`) and `read_only` (after opening, only \
snapshots, screenshots, scrolling and eval: no clicks, typing or navigation; eval still runs in \
the page and can change it).\n\
- `close`: closes `tab`, or with `all` every tab you opened.";

const HANDOFF: &str = "Ask the user to do something in a tab that only a person can do (sign \
in, 2FA, a captcha, confirming a payment) and wait for them. Agentloft marks the thread as \
needing the user and shows a card on the tab with the site and your reason; they act in the page \
and press Continue on the card, which no page can press. Returns `outcome`: `continued` (they \
pressed Continue); `waiting` (this call's wait ran out, the card stays: call again with the same \
tab to keep waiting); `closed` (the tab went away); `stopped` (your turn ended); `expired` \
(nobody came), with the tab's url and title now. Never solve a captcha yourself. When they are \
done, read the page again: it has changed.";

const DEMONSTRATE: &str = "Ask the user to show you how something is done in a tab, and record \
what they do until they press Done on the card: clicks (with the element's role, name and a CSS \
selector), typing (nothing typed into a password or one-time-code field is recorded), keys \
pressed and pages visited. Use it when you can't find how to do something and the user can show \
you. Returns `steps` (structured) and `summary` (one line per step) to repeat with the other \
browser tools, and `outcome` as `browser_handoff` does (`waiting`: call again to keep \
recording).";

const STATUS: &str = "Whether Agentloft's browser can run here (with which program, or why not \
and what to tell the user), this thread's tabs, and what is waiting for the user.";

pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "browser_tabs",
            TABS,
            json!({
                "action": { "type": "string", "enum": ["list", "new", "close"], "default": "list" },
                "name": { "type": "string", "description": "For new: a name for the tab, like `docs`." },
                "url": { "type": "string", "description": "For new: where it opens (blank if left out)." },
                "allowed_origins": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "For new: the only origins you may send it to."
                },
                "read_only": {
                    "type": "boolean",
                    "default": false,
                    "description": "For new: you may read it but not act in it."
                },
                "all": {
                    "type": "boolean",
                    "default": false,
                    "description": "For close: every tab you opened."
                }
            }),
            &[],
        ),
        tool(
            "browser_handoff",
            HANDOFF,
            json!({
                "reason": {
                    "type": "string",
                    "description": "What the user should do, in one sentence, e.g. \"Sign in to GitHub, then press Continue.\""
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 3600,
                    "default": 600,
                    "description": "How long this call waits. If your client gives up on long calls, use about 50 and call again while the outcome is `waiting`."
                }
            }),
            &["reason"],
        ),
        tool(
            "browser_demonstrate",
            DEMONSTRATE,
            json!({
                "what": {
                    "type": "string",
                    "description": "What the user should show you, in one sentence, e.g. \"Show me how you export the monthly report, then press Done.\""
                },
                "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 600 }
            }),
            &["what"],
        ),
        tool("browser_status", STATUS, json!({}), &[]),
    ]
}

/// Give every browser tool but `browser_status` a `tab`, unless it has its
/// own.
pub fn add_tab_property(tools: &mut [Value]) {
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or_default();
        if !name.starts_with("browser_") || name == "browser_status" {
            continue;
        }
        let description = if name == "browser_tabs" {
            "For close: the tab to close, by its id or the name you gave it."
        } else {
            "Which of this thread's tabs: its id, or the name you gave it. Default: the tab you \
             or the user used last."
        };
        if let Some(properties) = tool["inputSchema"]["properties"].as_object_mut() {
            properties
                .entry("tab")
                .or_insert_with(|| json!({ "type": "string", "description": description }));
        }
    }
}

/// Take a call's `tab` out of its arguments.
pub fn take_tab(args: &mut Value) -> Option<String> {
    match args.as_object_mut()?.remove("tab")? {
        Value::String(tab) => Some(tab.trim().to_owned()).filter(|t| !t.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The tab tools' commands; `None` for any other tool.
pub fn browser_command(name: &str, args: &Value) -> Option<Value> {
    let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);
    let seconds = || {
        args.get("timeout_seconds").and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
        })
    };
    Some(match name {
        "browser_tabs" => {
            let mut command = json!({
                "action": "tabs",
                "op": args.get("action").and_then(Value::as_str).unwrap_or("list"),
                "read_only": flag("read_only"),
                "all": flag("all"),
                "allowed_origins": match args.get("allowed_origins") {
                    Some(list @ Value::Array(_)) => list.clone(),
                    Some(Value::String(one)) => json!([one]),
                    _ => json!([]),
                },
            });
            for key in ["name", "url"] {
                if let Some(value) = args.get(key).and_then(Value::as_str) {
                    command[key] = json!(value);
                }
            }
            command
        }
        "browser_handoff" => json!({
            "action": "handoff",
            "reason": args.get("reason"),
            "timeout_secs": seconds(),
        }),
        "browser_demonstrate" => json!({
            "action": "demonstrate",
            "what": args.get("what"),
            "timeout_secs": seconds(),
        }),
        "browser_status" => json!({ "action": "status" }),
        _ => return None,
    })
}

/// A tab as a header line names it: its handle, and where it is.
fn where_tab(tab: &loft_protocol::BrowserTab) -> String {
    if tab.url.is_empty() {
        tab.handle().to_owned()
    } else {
        format!("{} ({})", tab.handle(), tab.url)
    }
}

/// What a result adds after the command's own: the tab it ran in, tabs
/// pages opened meanwhile, and notes.
pub fn reply_header(reply: &BrowserReply) -> String {
    let mut lines = Vec::new();
    if let Some(tab) = &reply.tab {
        lines.push(format!("tab: {}", where_tab(tab)));
    }
    for tab in &reply.new_tabs {
        let title = if tab.title.is_empty() {
            String::new()
        } else {
            format!(" \"{}\"", tab.title)
        };
        lines.push(format!(
            "A page opened a new tab, {}{title}: pass `tab: {}` to use it.",
            where_tab(tab),
            tab.handle()
        ));
    }
    lines.extend(reply.notes.iter().cloned());
    lines.join("\n")
}

/// `result` with the reply's header after it, and a handoff that ended
/// without the person marked as an error.
pub fn with_header(mut result: Value, reply: &BrowserReply, tool: &str) -> Value {
    let header = reply_header(reply);
    if !header.is_empty()
        && let Some(content) = result["content"].as_array_mut()
    {
        content.push(json!({ "type": "text", "text": header }));
    }
    if matches!(tool, "browser_handoff" | "browser_demonstrate")
        && matches!(
            reply.value["outcome"].as_str(),
            Some("closed" | "stopped" | "expired")
        )
    {
        result["isError"] = json!(true);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::text_result;
    use loft_protocol::{BrowserTab, PageId};

    #[test]
    fn browser_tools_take_a_tab() {
        let tools = crate::mcp::tools();
        let mut seen = Vec::new();
        for tool in &tools {
            let name = tool["name"].as_str().unwrap();
            if !name.starts_with("browser_") {
                continue;
            }
            seen.push(name.to_owned());
            let tab = &tool["inputSchema"]["properties"]["tab"];
            if name == "browser_status" {
                assert!(tab.is_null(), "{name}");
            } else {
                assert_eq!(tab["type"], "string", "{name}");
            }
        }
        for name in [
            "browser_tabs",
            "browser_handoff",
            "browser_demonstrate",
            "browser_status",
        ] {
            assert!(seen.iter().any(|s| s == name), "{name}");
        }
        let tabs = tools.iter().find(|t| t["name"] == "browser_tabs").unwrap();
        assert!(
            tabs["inputSchema"]["properties"]["tab"]["description"]
                .as_str()
                .unwrap()
                .contains("close")
        );
        assert_eq!(tabs["inputSchema"]["required"], json!([]));
        let handoff = tools
            .iter()
            .find(|t| t["name"] == "browser_handoff")
            .unwrap();
        assert_eq!(handoff["inputSchema"]["required"], json!(["reason"]));
    }

    #[test]
    fn a_tab_is_taken_out_of_the_arguments() {
        let mut args = json!({ "ref": 3, "tab": " docs " });
        assert_eq!(take_tab(&mut args).as_deref(), Some("docs"));
        assert_eq!(args, json!({ "ref": 3 }));
        assert_eq!(take_tab(&mut json!({ "tab": 7 })).as_deref(), Some("7"));
        assert_eq!(take_tab(&mut json!({})), None);
        let mut blank = json!({ "tab": "  ", "x": 1 });
        assert_eq!(take_tab(&mut blank), None);
        assert_eq!(blank, json!({ "x": 1 }));
        assert_eq!(take_tab(&mut json!({ "tab": null })), None);
        assert_eq!(take_tab(&mut json!("docs")), None);
    }

    #[test]
    fn tab_tools_map_to_commands() {
        assert_eq!(
            browser_command(
                "browser_tabs",
                &json!({ "action": "new", "name": "docs", "allowed_origins": "example.com" })
            )
            .unwrap(),
            json!({
                "action": "tabs", "op": "new", "name": "docs", "read_only": false,
                "all": false, "allowed_origins": ["example.com"]
            })
        );
        assert_eq!(
            browser_command("browser_tabs", &json!({})).unwrap(),
            json!({ "action": "tabs", "op": "list", "read_only": false, "all": false, "allowed_origins": [] })
        );
        assert_eq!(
            browser_command("browser_handoff", &json!({ "reason": "Sign in" })).unwrap(),
            json!({ "action": "handoff", "reason": "Sign in", "timeout_secs": null })
        );
        assert_eq!(
            browser_command(
                "browser_demonstrate",
                &json!({ "what": "Export", "timeout_seconds": 50 })
            )
            .unwrap(),
            json!({ "action": "demonstrate", "what": "Export", "timeout_secs": 50 })
        );
        assert_eq!(
            browser_command("browser_status", &json!({})).unwrap(),
            json!({ "action": "status" })
        );
        assert!(browser_command("browser_click", &json!({})).is_none());
        assert!(browser_command("list_threads", &json!({})).is_none());

        for (name, args) in [
            ("browser_tabs", json!({})),
            (
                "browser_tabs",
                json!({ "action": "new", "name": "docs", "url": "https://example.com",
                        "allowed_origins": ["example.com", "*.github.com"], "read_only": true }),
            ),
            ("browser_tabs", json!({ "action": "close", "all": true })),
            (
                "browser_handoff",
                json!({ "reason": "Sign in", "timeout_seconds": 50 }),
            ),
            ("browser_demonstrate", json!({ "what": "Show me" })),
            ("browser_status", json!({})),
        ] {
            let command = browser_command(name, &args).unwrap();
            serde_json::from_value::<loft_page_script::BrowserCommand>(command.clone())
                .unwrap_or_else(|e| panic!("{name}: {command}: {e}"));
        }
    }

    #[test]
    fn results_say_which_tab() {
        let reply = BrowserReply {
            value: json!({ "outcome": "closed" }),
            tab: Some(BrowserTab {
                id: PageId("0190-docs".into()),
                name: Some("docs".into()),
                url: "https://example.com/".into(),
                title: "Example".into(),
            }),
            new_tabs: vec![BrowserTab {
                id: PageId("0190-id".into()),
                name: None,
                url: "https://example.com/popup".into(),
                title: "Popup".into(),
            }],
            notes: vec!["A note.".into()],
        };
        let header = reply_header(&reply);
        assert!(
            header.starts_with("tab: docs (https://example.com/)"),
            "{header}"
        );
        assert!(header.contains("pass `tab: 0190-id`"), "{header}");
        assert!(header.contains("\"Popup\""), "{header}");
        assert!(header.ends_with("A note."), "{header}");
        let result = with_header(text_result("ok", false), &reply, "browser_handoff");
        assert_eq!(result["content"][0]["text"], "ok");
        assert_eq!(result["content"][1]["text"], header.as_str());
        assert_eq!(result["isError"], true);
        // Another tool's result isn't an error for it.
        let other = with_header(text_result("ok", false), &reply, "browser_snapshot");
        assert_eq!(other["isError"], false);
        for (outcome, error) in [
            ("continued", false),
            ("waiting", false),
            ("stopped", true),
            ("expired", true),
        ] {
            let reply = BrowserReply {
                value: json!({ "outcome": outcome }),
                tab: None,
                new_tabs: Vec::new(),
                notes: Vec::new(),
            };
            assert_eq!(reply_header(&reply), "");
            let result = with_header(text_result("ok", false), &reply, "browser_demonstrate");
            assert_eq!(result["isError"], error, "{outcome}");
            assert_eq!(result["content"].as_array().unwrap().len(), 1);
        }
    }
}
