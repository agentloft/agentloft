//! `agentloftd mcp --thread <id>`: a stdio MCP server attached to every agent
//! session (ADR-0013). It bridges tool calls to the daemon:
//! - `browser_*` drive the browser panel in the user's Agentloft window;
//! - `list_threads` / `read_thread` / `get_thread_diff` read other threads.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout. Nothing else
//! may be written to stdout.

use anyhow::{Context as _, Result};
use loft_client::Client;
use loft_config::Paths;
use loft_protocol::{ClientKind, DiffScope, ReadDetail, Request, Response, ThreadId};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub async fn run(paths: Paths, thread_id: ThreadId) -> Result<()> {
    let (client, _events) =
        Client::connect(&paths.socket_file(), ClientKind::McpBridge, crate::BUILD)
            .await
            .context("connect to agentloftd")?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                write(
                    &mut stdout,
                    &error_reply(Value::Null, -32700, &format!("parse error: {err}")),
                )
                .await?;
                continue;
            }
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // notification
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = match method {
            "initialize" => ok_reply(
                id,
                json!({
                    "protocolVersion": params
                        .get("protocolVersion")
                        .and_then(Value::as_str)
                        .unwrap_or(PROTOCOL_VERSION),
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "agentloft", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }),
            ),
            "ping" => ok_reply(id, json!({})),
            "tools/list" => ok_reply(id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = call_tool(&client, &thread_id, name, &args).await;
                ok_reply(id, result)
            }
            other => error_reply(id, -32601, &format!("method not found: {other}")),
        };
        write(&mut stdout, &reply).await?;
    }
    Ok(())
}

const INSTRUCTIONS: &str = "Agentloft tools. `browser_*` control the browser panel the \
user sees next to this chat: navigate, then `browser_snapshot` to read the page \
(interactive elements carry [ref=N] ids for `browser_click`/`browser_type`). \
`list_threads`/`read_thread` read other Agentloft threads (other agents' work) in this project.";

async fn write(out: &mut tokio::io::Stdout, v: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(v)?;
    line.push(b'\n');
    out.write_all(&line).await?;
    out.flush().await?;
    Ok(())
}

fn ok_reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn text_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": is_error })
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "browser_navigate",
            "Open a URL in the user's Agentloft browser panel and wait for it to load. Returns the final URL and title.",
            json!({ "url": { "type": "string", "description": "http(s) URL, or words to search for" } }),
            &["url"],
        ),
        tool(
            "browser_snapshot",
            "Read the current page as a compact outline. Interactive elements are tagged [ref=N] for browser_click / browser_type.",
            json!({}),
            &[],
        ),
        tool(
            "browser_click",
            "Click the element with the given ref from the latest snapshot.",
            json!({ "ref": { "type": "integer" } }),
            &["ref"],
        ),
        tool(
            "browser_type",
            "Type text into the input with the given ref. Set submit to press Enter / submit the form afterwards.",
            json!({
                "ref": { "type": "integer" },
                "text": { "type": "string" },
                "submit": { "type": "boolean", "default": false }
            }),
            &["ref", "text"],
        ),
        tool(
            "browser_press",
            "Press a key in the focused element (Enter, Tab, Escape, ArrowDown, …).",
            json!({ "key": { "type": "string" } }),
            &["key"],
        ),
        tool(
            "browser_scroll",
            "Scroll the page vertically by dy pixels (negative scrolls up).",
            json!({ "dy": { "type": "number" } }),
            &["dy"],
        ),
        tool("browser_back", "Go back in history.", json!({}), &[]),
        tool("browser_forward", "Go forward in history.", json!({}), &[]),
        tool(
            "browser_eval",
            "Evaluate a JavaScript expression in the page and return its JSON result.",
            json!({ "script": { "type": "string" } }),
            &["script"],
        ),
        tool(
            "browser_screenshot",
            "Capture the visible page as a PNG image.",
            json!({}),
            &[],
        ),
        tool(
            "list_threads",
            "List Agentloft threads (other agent conversations), optionally filtered by a title query.",
            json!({ "query": { "type": "string" } }),
            &[],
        ),
        tool(
            "read_thread",
            "Read another thread's transcript. detail=summary (default) gives requests and answers; full adds tool calls.",
            json!({
                "thread_id": { "type": "string" },
                "detail": { "type": "string", "enum": ["summary", "full"] }
            }),
            &["thread_id"],
        ),
        tool(
            "get_thread_diff",
            "Get the code changes of a thread as a unified diff (defaults to this thread). scope: last_turn | thread | working_tree.",
            json!({
                "thread_id": { "type": "string" },
                "scope": { "type": "string", "enum": ["last_turn", "thread", "working_tree"] }
            }),
            &[],
        ),
    ]
}

/// Translate a browser tool call into a `BrowserCommand` JSON value.
pub fn browser_command(name: &str, args: &Value) -> Option<Value> {
    let r = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    Some(match name {
        "browser_navigate" => json!({ "action": "navigate", "url": r("url") }),
        "browser_snapshot" => json!({ "action": "snapshot" }),
        "browser_click" => json!({ "action": "click", "ref_id": r("ref") }),
        "browser_type" => json!({
            "action": "type", "ref_id": r("ref"), "text": r("text"),
            "submit": args.get("submit").and_then(Value::as_bool).unwrap_or(false)
        }),
        "browser_press" => json!({ "action": "press", "key": r("key") }),
        "browser_scroll" => json!({ "action": "scroll", "dy": r("dy") }),
        "browser_back" => json!({ "action": "back" }),
        "browser_forward" => json!({ "action": "forward" }),
        "browser_eval" => json!({ "action": "eval", "script": r("script") }),
        "browser_screenshot" => json!({ "action": "screenshot" }),
        _ => return None,
    })
}

async fn call_tool(client: &Client, thread: &ThreadId, name: &str, args: &Value) -> Value {
    if let Some(command) = browser_command(name, args) {
        let screenshot = name == "browser_screenshot";
        return match client
            .request(Request::BrowserCall {
                thread_id: thread.clone(),
                command,
            })
            .await
        {
            Ok(Response::Json { value }) => browser_result(value, screenshot),
            Ok(other) => text_result(format!("unexpected response: {other:?}"), true),
            Err(err) => text_result(err.message, true),
        };
    }
    let request = match name {
        "list_threads" => Request::ListThreads {
            query: args.get("query").and_then(Value::as_str).map(str::to_owned),
            project_id: None,
        },
        "read_thread" => match args.get("thread_id").and_then(Value::as_str) {
            Some(id) => Request::ReadThread {
                thread_id: ThreadId(id.to_owned()),
                detail: match args.get("detail").and_then(Value::as_str) {
                    Some("full") => ReadDetail::Full,
                    _ => ReadDetail::Summary,
                },
            },
            None => return text_result("thread_id is required", true),
        },
        "get_thread_diff" => Request::GetDiff {
            thread_id: args
                .get("thread_id")
                .and_then(Value::as_str)
                .map(|s| ThreadId(s.to_owned()))
                .unwrap_or_else(|| thread.clone()),
            scope: match args.get("scope").and_then(Value::as_str) {
                Some("thread") => DiffScope::Thread,
                Some("working_tree") => DiffScope::WorkingTree,
                _ => DiffScope::LastTurn,
            },
        },
        other => return text_result(format!("unknown tool: {other}"), true),
    };
    match client.request(request).await {
        Ok(Response::Json { value }) => text_result(
            serde_json::to_string_pretty(&value).unwrap_or_default(),
            false,
        ),
        Ok(Response::Text { text }) => text_result(text, false),
        Ok(Response::Diff { diff }) => {
            let mut text = String::new();
            if let Some(note) = &diff.note {
                text.push_str(note);
                text.push('\n');
            }
            for f in &diff.files {
                text.push_str(&format!("{} (+{} -{})\n", f.path, f.added, f.removed));
            }
            const MAX: usize = 80_000;
            let patch: String = diff.patch.chars().take(MAX).collect();
            text.push('\n');
            text.push_str(&patch);
            if diff.patch.len() > MAX {
                text.push_str("\n[diff truncated]");
            }
            text_result(text, false)
        }
        Ok(other) => text_result(format!("unexpected response: {other:?}"), true),
        Err(err) => text_result(err.message, true),
    }
}

/// Render a browser result; screenshots become MCP image content.
fn browser_result(value: Value, screenshot: bool) -> Value {
    if screenshot
        && let Some(data) = ["png_base64", "data", "png"]
            .iter()
            .find_map(|k| value.get(k).and_then(Value::as_str))
    {
        return json!({
            "content": [{ "type": "image", "data": data, "mimeType": "image/png" }],
            "isError": false,
        });
    }
    let text = match &value {
        Value::String(s) => s.clone(),
        Value::Object(map) if map.get("snapshot").and_then(Value::as_str).is_some() => {
            map["snapshot"].as_str().unwrap_or_default().to_owned()
        }
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    text_result(text, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_schemas_are_well_formed() {
        let tools = tools();
        assert!(tools.len() >= 12);
        for t in &tools {
            assert!(t["name"].as_str().unwrap().len() < 64);
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn browser_tools_map_to_commands() {
        assert_eq!(
            browser_command("browser_click", &json!({"ref": 3})).unwrap(),
            json!({"action": "click", "ref_id": 3})
        );
        let typed = browser_command("browser_type", &json!({"ref": 1, "text": "hi"})).unwrap();
        assert_eq!(typed["submit"], false);
        assert!(browser_command("list_threads", &json!({})).is_none());
    }

    #[test]
    fn screenshot_results_become_images() {
        let v = browser_result(json!({"png_base64": "AAAA"}), true);
        assert_eq!(v["content"][0]["type"], "image");
        let t = browser_result(json!("plain"), false);
        assert_eq!(t["content"][0]["text"], "plain");
    }
}
