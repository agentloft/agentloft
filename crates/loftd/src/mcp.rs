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
use loft_protocol::{
    ClientKind, DiffScope, HarnessId, PermissionMode, ReadDetail, Request, Response, ThreadId,
};
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
`list_threads`/`read_thread` read other Agentloft threads (other agents' work) in this project. \
Orchestration: `spawn_thread` starts another agent in its own Agentloft thread (the user sees \
it in the sidebar); it runs in parallel, so spawn several before waiting. `wait_for_threads` \
waits for them and returns each one's latest reply; `send_to_thread` follows up and \
`stop_thread` stops one. Spawned threads use this thread's agent, model, reasoning effort and \
permissions unless the user explicitly asked for something else; `list_agents` lists what's \
available.";

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
            "list_agents",
            "List the coding agents available to spawn (Claude Code, Codex, …): whether each is installed, its models with their reasoning efforts, the permission modes, and this thread's own settings (the defaults for spawn_thread).",
            json!({}),
            &[],
        ),
        tool(
            "spawn_thread",
            "Start another agent in a new Agentloft thread (this is what the user means by spawning, starting or creating threads; prefer it over built-in subagents when they say threads) with `prompt` as its first message, and return its thread_id at once (it runs in parallel; use wait_for_threads for the result). By default it gets THIS thread's agent, model, reasoning effort and permissions and works in the same checkout: only set agent/model/effort/permission_mode when the user explicitly asked for them. The prompt must stand alone: the new agent doesn't see this conversation.",
            json!({
                "prompt": { "type": "string", "description": "Complete, self-contained task for the new agent" },
                "title": { "type": "string", "description": "Short sidebar title (default: the prompt's first line)" },
                "agent": { "type": "string", "description": "claude | codex | opencode | gemini (default: this thread's)" },
                "model": { "type": "string", "description": "Model id from list_agents (default: this thread's, or the agent's default)" },
                "effort": { "type": "string", "description": "Reasoning effort, e.g. low | medium | high | xhigh | max" },
                "permission_mode": { "type": "string", "enum": ["read-only", "ask-all", "accept-edits", "auto", "full-access"] },
                "worktree": { "type": "boolean", "description": "Work in a new git worktree branched from this thread's current state instead of sharing this checkout (use when agents edit files in parallel)", "default": false }
            }),
            &["prompt"],
        ),
        tool(
            "thread_status",
            "Where a thread stands: running / idle / waiting_for_approval / error, its settings and its latest reply.",
            json!({ "thread_id": { "type": "string" } }),
            &["thread_id"],
        ),
        tool(
            "wait_for_threads",
            "Wait until the given threads are finished (not running and not waiting on background work they started), failed, or waiting for the user's approval, up to timeout_seconds, and return each one's status and latest reply. If some are still running when it returns, call it again.",
            json!({
                "thread_ids": { "type": "array", "items": { "type": "string" } },
                "timeout_seconds": { "type": "integer", "default": 50, "minimum": 1, "maximum": 600 }
            }),
            &["thread_ids"],
        ),
        tool(
            "send_to_thread",
            "Send a follow-up message to a thread this thread spawned (it must not be running).",
            json!({ "thread_id": { "type": "string" }, "message": { "type": "string" } }),
            &["thread_id", "message"],
        ),
        tool(
            "stop_thread",
            "Stop the current turn of a thread this thread spawned.",
            json!({ "thread_id": { "type": "string" } }),
            &["thread_id"],
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
    if let Some(result) = orchestrate(client, thread, name, args).await {
        return result;
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

/// The agents a thread may spawn, in the order they're offered.
const AGENTS: [&str; 4] = [
    HarnessId::CLAUDE,
    HarnessId::CODEX,
    HarnessId::OPENCODE,
    HarnessId::GEMINI,
];

async fn outcome(client: &Client, id: &ThreadId) -> Result<Value, String> {
    match client
        .request(Request::ThreadOutcome {
            thread_id: id.clone(),
        })
        .await
    {
        Ok(Response::Json { value }) => Ok(value),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(err) => Err(err.message),
    }
}

fn json_result(v: &Value) -> Value {
    text_result(serde_json::to_string_pretty(v).unwrap_or_default(), false)
}

/// A thread this one spawned (only those may be messaged or stopped).
async fn own_child(
    client: &Client,
    me: &ThreadId,
    args: &Value,
) -> Result<(ThreadId, Value), Value> {
    let Some(id) = args.get("thread_id").and_then(Value::as_str) else {
        return Err(text_result("thread_id is required", true));
    };
    let id = ThreadId(id.to_owned());
    let state = outcome(client, &id)
        .await
        .map_err(|e| text_result(e, true))?;
    if state["spawned_by"].as_str() != Some(me.as_str()) {
        return Err(text_result(
            "only threads this thread spawned can be messaged or stopped",
            true,
        ));
    }
    Ok((id, state))
}

/// Orchestration tools; `None` for any other tool.
async fn orchestrate(client: &Client, me: &ThreadId, name: &str, args: &Value) -> Option<Value> {
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(match name {
        "list_agents" => {
            let mut agents = Vec::new();
            for id in AGENTS {
                let harness = HarnessId(id.into());
                let status = match client
                    .request(Request::ProbeHarness {
                        harness: harness.clone(),
                    })
                    .await
                {
                    Ok(Response::HarnessStatus { status }) => status,
                    _ => continue,
                };
                let models = if status.installed {
                    match client.request(Request::ListModels { harness }).await {
                        Ok(Response::Models { models }) => models
                            .into_iter()
                            .map(|m| {
                                json!({
                                    "id": m.id, "name": m.display_name, "default": m.is_default,
                                    "efforts": m.efforts, "default_effort": m.default_effort,
                                })
                            })
                            .collect(),
                        _ => Vec::new(),
                    }
                } else {
                    Vec::new()
                };
                agents.push(json!({
                    "agent": id, "installed": status.installed,
                    "version": status.version, "models": models,
                }));
            }
            let current = outcome(client, me).await.unwrap_or(Value::Null);
            json_result(&json!({
                "this_thread": {
                    "agent": current["agent"], "model": current["model"],
                    "effort": current["effort"], "permission_mode": current["permission_mode"],
                },
                "agents": agents,
                "permission_modes": ["read-only", "ask-all", "accept-edits", "auto", "full-access"],
            }))
        }
        "spawn_thread" => {
            let Some(prompt) = str_arg("prompt") else {
                return Some(text_result("prompt is required", true));
            };
            let permission_mode = match str_arg("permission_mode") {
                None => None,
                Some(m) => match serde_json::from_value::<PermissionMode>(json!(m)) {
                    Ok(m) => Some(m),
                    Err(_) => {
                        return Some(text_result(format!("unknown permission_mode `{m}`"), true));
                    }
                },
            };
            let request = Request::SpawnThread {
                from: me.clone(),
                prompt,
                title: str_arg("title"),
                harness: str_arg("agent").map(HarnessId),
                model: str_arg("model"),
                effort: str_arg("effort"),
                permission_mode,
                worktree: args
                    .get("worktree")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            match client.request(request).await {
                Ok(Response::Thread { thread }) => json_result(&json!({
                    "thread_id": thread.id, "title": thread.title, "agent": thread.harness,
                    "model": thread.model, "effort": thread.effort,
                    "permission_mode": thread.permission_mode, "cwd": thread.cwd,
                    "next": "It is running. Call wait_for_threads with this thread_id for its reply.",
                })),
                Ok(other) => text_result(format!("unexpected response: {other:?}"), true),
                Err(err) => text_result(err.message, true),
            }
        }
        "thread_status" => match str_arg("thread_id") {
            None => text_result("thread_id is required", true),
            Some(id) => match outcome(client, &ThreadId(id)).await {
                Ok(v) => json_result(&v),
                Err(e) => text_result(e, true),
            },
        },
        "wait_for_threads" => {
            let ids: Vec<ThreadId> = args
                .get("thread_ids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|s| ThreadId(s.to_owned()))
                        .collect()
                })
                .unwrap_or_default();
            if ids.is_empty() {
                return Some(text_result("thread_ids is required", true));
            }
            let timeout = args
                .get("timeout_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 600);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            loop {
                let mut states = Vec::new();
                for id in &ids {
                    states.push(match outcome(client, id).await {
                        Ok(v) => v,
                        Err(e) => json!({ "thread_id": id, "status": "unknown", "error": e }),
                    });
                }
                // Still at it: a turn running, or background work it started.
                let running = states
                    .iter()
                    .filter(|s| {
                        s["status"] == "running" || s["status"] == "waiting_on_background_work"
                    })
                    .count();
                if running == 0 || std::time::Instant::now() >= deadline {
                    break json_result(&json!({
                        "still_running": running,
                        "threads": states,
                    }));
                }
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            }
        }
        "send_to_thread" => {
            let (id, state) = match own_child(client, me, args).await {
                Ok(c) => c,
                Err(e) => return Some(e),
            };
            let Some(message) = str_arg("message") else {
                return Some(text_result("message is required", true));
            };
            if state["status"] == "running" {
                return Some(text_result(
                    "that thread is still running: wait_for_threads first, or stop_thread",
                    true,
                ));
            }
            match client
                .request(Request::SendMessage {
                    thread_id: id,
                    text: message,
                    attachments: Vec::new(),
                })
                .await
            {
                Ok(_) => text_result("Sent. Call wait_for_threads for the reply.", false),
                Err(err) => text_result(err.message, true),
            }
        }
        "stop_thread" => {
            let (id, _) = match own_child(client, me, args).await {
                Ok(c) => c,
                Err(e) => return Some(e),
            };
            match client.request(Request::Interrupt { thread_id: id }).await {
                Ok(_) => text_result("Stopped.", false),
                Err(err) => text_result(err.message, true),
            }
        }
        _ => return None,
    })
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
