//! The Agentloft MCP server's automation tools: agents list, create, change,
//! pause, run and delete automations (prompts that start a thread on a
//! schedule) for the user.

use loft_client::Client;
use loft_protocol::{
    Automation, AutomationId, AutomationSpec, HarnessId, PermissionMode, ProjectId, Request,
    Response, Schedule, ThreadId,
};
use serde_json::{Value, json};

use crate::mcp::{find_project, json_result, outcome, text_result, tool};

/// How a schedule is given to the tools (any one of these).
fn schedule_properties() -> Value {
    json!({
        "every_minutes": { "type": "integer", "minimum": 1, "description": "Run every N minutes (60 = hourly), counted from the last run" },
        "time": { "type": "string", "description": "Run daily at this local time, HH:MM 24-hour (with `days` to pick weekdays)" },
        "days": {
            "type": "array",
            "items": { "type": "string" },
            "description": "With `time`: which days, e.g. [\"mon\",\"wed\"], [\"weekdays\"], [\"weekends\"] (default: every day)"
        },
        "cron": { "type": "string", "description": "A five-field cron expression in local time (minute hour day month weekday), e.g. \"0 9 * * 1-5\"" },
        "once_at": { "type": "string", "description": "Run once at this local date and time, \"YYYY-MM-DD HH:MM\"" }
    })
}

fn spec_properties(create: bool) -> Value {
    let mut p = json!({
        "name": { "type": "string", "description": "Short name; it is also each run's thread title" },
        "prompt": { "type": "string", "description": "What the agent is asked on each run. Each run is a fresh thread that sees nothing else, so it must stand alone" },
        "project": { "type": "string", "description": "Project to run in (name, path or id). Default: this thread's project" },
        "agent": { "type": "string", "description": "claude | codex | opencode | gemini (default: this thread's)" },
        "model": { "type": "string", "description": "Model id from list_agents (default: this thread's)" },
        "effort": { "type": "string", "description": "Reasoning effort (default: this thread's)" },
        "permission_mode": {
            "type": "string",
            "enum": ["read-only", "ask-all", "accept-edits", "auto", "full-access"],
            "description": "Default: this thread's. Runs start with nobody watching: a mode that asks before acting leaves them waiting for the user"
        },
        "worktree": { "type": "boolean", "description": "Each run in a new git worktree of its own instead of the project checkout (use when runs edit files)" },
        "enabled": { "type": "boolean", "description": "false pauses it (default when creating: true)" }
    });
    if let (Some(p), Some(s)) = (p.as_object_mut(), schedule_properties().as_object()) {
        p.extend(s.clone());
    }
    if !create {
        p["automation"] =
            json!({ "type": "string", "description": "The automation: its id or name" });
    }
    p
}

pub(crate) fn tools() -> Vec<Value> {
    vec![
        tool(
            "list_automations",
            "List the user's automations (prompts Agentloft runs on a schedule, each run in a new thread): id, name, prompt, project, schedule (in words too), agent settings, whether it's on, its next and last run, and the thread its last run started.",
            json!({}),
            &[],
        ),
        tool(
            "create_automation",
            "Create an automation for the user: Agentloft starts a new thread with `prompt` on a schedule (local time), even when nobody is looking. Give exactly one of every_minutes, time (+ days), cron or once_at. Only when the user asks for something recurring or scheduled.",
            spec_properties(true),
            &["name", "prompt"],
        ),
        tool(
            "update_automation",
            "Change an automation (by id or name): only what you pass changes. A schedule given (every_minutes, time + days, cron or once_at) replaces the old one; enabled false pauses it, true resumes it.",
            spec_properties(false),
            &["automation"],
        ),
        tool(
            "delete_automation",
            "Delete an automation (by id or name). The threads its runs started stay. Only when the user asks.",
            json!({ "automation": { "type": "string" } }),
            &["automation"],
        ),
        tool(
            "run_automation",
            "Run an automation now, whatever its schedule (its schedule is unchanged). Returns the run's thread_id; use wait_for_threads for its reply.",
            json!({ "automation": { "type": "string" } }),
            &["automation"],
        ),
    ]
}

async fn list(client: &Client) -> Result<Vec<Value>, String> {
    match client.request(Request::ListAutomations).await {
        Ok(Response::Json {
            value: Value::Array(list),
        }) => Ok(list),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(err) => Err(err.message),
    }
}

/// An automation by id or name (any case).
async fn find(client: &Client, want: &str) -> Result<Automation, String> {
    let all = list(client).await?;
    let want = want.trim();
    let found = all
        .iter()
        .find(|a| a["id"].as_str() == Some(want))
        .or_else(|| {
            all.iter().find(|a| {
                a["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(want))
            })
        });
    match found {
        Some(a) => serde_json::from_value(a.clone()).map_err(|e| e.to_string()),
        None => {
            let names: Vec<&str> = all.iter().filter_map(|a| a["name"].as_str()).collect();
            Err(if names.is_empty() {
                format!("no automation `{want}`; there are none yet")
            } else {
                format!("no automation `{want}`; automations: {}", names.join(", "))
            })
        }
    }
}

/// A day by name or number (0 = Sunday); `weekdays` and `weekends` are
/// several.
fn days(v: &Value) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let items: Vec<Value> = match v {
        Value::Array(a) => a.clone(),
        Value::String(s) => s.split(',').map(|d| json!(d.trim())).collect(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    };
    for item in items {
        if let Some(n) = item.as_u64() {
            if n > 7 {
                return Err(format!("day {n}: days are 0 (Sunday) to 6"));
            }
            out.push((n % 7) as u8);
            continue;
        }
        let s = item
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match s.as_str() {
            "weekdays" | "weekday" => out.extend(1..=5),
            "weekends" | "weekend" => out.extend([0, 6]),
            "daily" | "every day" | "all" | "" => {}
            _ => {
                let n = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"]
                    .iter()
                    .position(|d| s.starts_with(d))
                    .ok_or_else(|| format!("`{s}` isn't a day"))?;
                out.push(n as u8);
            }
        }
    }
    Ok(out)
}

/// The schedule the arguments give, if they give one.
fn schedule(args: &Value) -> Result<Option<Schedule>, String> {
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let mut given = Vec::new();
    if let Some(m) = args.get("every_minutes").filter(|v| !v.is_null()) {
        let minutes = m
            .as_u64()
            .filter(|m| *m >= 1)
            .ok_or("every_minutes is a whole number of minutes, 1 or more")?;
        given.push(Schedule::Interval {
            minutes: u32::try_from(minutes).map_err(|_| "every_minutes is too large")?,
        });
    }
    if let Some(time) = s("time") {
        given.push(Schedule::Daily {
            time: time.to_owned(),
            days: days(args.get("days").unwrap_or(&Value::Null))?,
        });
    }
    if let Some(expr) = s("cron") {
        given.push(Schedule::Cron {
            expr: expr.to_owned(),
        });
    }
    if let Some(at) = s("once_at") {
        let at_ms = loft_core::parse_local(at)
            .ok_or_else(|| format!("once_at `{at}`: use \"YYYY-MM-DD HH:MM\" (local time)"))?;
        given.push(Schedule::Once { at_ms });
    }
    match given.len() {
        0 if args.get("days").is_some_and(|d| !d.is_null()) => {
            Err("days goes with time (HH:MM)".into())
        }
        0 => Ok(None),
        1 => Ok(given.pop()),
        _ => Err("give one schedule: every_minutes, time (+ days), cron or once_at".into()),
    }
}

/// What the agent hears about a saved automation.
fn saved(a: &Automation, verb: &str) -> Value {
    json_result(&json!({
        "result": format!("{verb} \"{}\".", a.spec.name),
        "id": a.id,
        "name": a.spec.name,
        "schedule": loft_core::describe_schedule(&a.spec.schedule),
        "enabled": a.spec.enabled,
        "next_run": a.next_run_at_ms.map(loft_core::local_text),
    }))
}

/// Automation tools; `None` for any other tool.
pub(crate) async fn call(
    client: &Client,
    me: &ThreadId,
    name: &str,
    args: &Value,
) -> Option<Value> {
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let err = |e: String| Some(text_result(e, true));
    Some(match name {
        "list_automations" => match client.request(Request::ListAutomations).await {
            Ok(Response::Json { value }) => json_result(&value),
            Ok(other) => text_result(format!("unexpected response: {other:?}"), true),
            Err(e) => text_result(e.message, true),
        },
        "create_automation" | "update_automation" => {
            let create = name == "create_automation";
            let schedule = match schedule(args) {
                Ok(s) => s,
                Err(e) => return err(e),
            };
            let permission_mode = match str_arg("permission_mode") {
                None => None,
                Some(m) => match serde_json::from_value::<PermissionMode>(json!(m)) {
                    Ok(m) => Some(m),
                    Err(_) => return err(format!("unknown permission_mode `{m}`")),
                },
            };
            let project = match str_arg("project") {
                Some(want) => match find_project(client, &want).await {
                    Ok(p) => p["id"].as_str().map(|id| ProjectId(id.to_owned())),
                    Err(e) => return err(e),
                },
                None => None,
            };
            let (id, mut spec) = if create {
                let Some(schedule) = schedule.clone() else {
                    return err(
                        "a schedule is required: every_minutes, time (+ days), cron or once_at"
                            .into(),
                    );
                };
                // This thread's settings, unless given.
                let here = match outcome(client, me).await {
                    Ok(v) => v,
                    Err(e) => return err(e),
                };
                let s = |k: &str| here[k].as_str().map(str::to_owned);
                let Some(project_id) = project.clone().or_else(|| s("project_id").map(ProjectId))
                else {
                    return err("which project? pass `project`".into());
                };
                let spec = AutomationSpec {
                    name: String::new(),
                    prompt: String::new(),
                    project_id,
                    schedule,
                    enabled: true,
                    harness: s("agent").map(HarnessId),
                    model: s("model"),
                    effort: s("effort"),
                    permission_mode: serde_json::from_value(here["permission_mode"].clone()).ok(),
                    worktree: false,
                };
                (None, spec)
            } else {
                let Some(want) = str_arg("automation") else {
                    return err("automation is required (its id or name)".into());
                };
                match find(client, &want).await {
                    Ok(a) => (Some(a.id), a.spec),
                    Err(e) => return err(e),
                }
            };
            let mut changed = create;
            let mut set = |b: bool| changed |= b;
            if let Some(v) = str_arg("name") {
                spec.name = v;
                set(true);
            }
            if let Some(v) = str_arg("prompt") {
                spec.prompt = v;
                set(true);
            }
            if let Some(p) = project {
                spec.project_id = p;
                set(true);
            }
            if let Some(s) = schedule {
                spec.schedule = s;
                set(true);
            }
            if let Some(agent) = str_arg("agent") {
                // Another agent: its own default model and effort, unless given.
                if spec.harness.as_ref().map(|h| h.as_str()) != Some(agent.as_str()) {
                    spec.model = None;
                    spec.effort = None;
                }
                spec.harness = Some(HarnessId(agent));
                set(true);
            }
            if let Some(v) = str_arg("model") {
                spec.model = Some(v);
                set(true);
            }
            if let Some(v) = str_arg("effort") {
                spec.effort = Some(v);
                set(true);
            }
            if permission_mode.is_some() {
                spec.permission_mode = permission_mode;
                set(true);
            }
            if let Some(v) = args.get("worktree").and_then(Value::as_bool) {
                spec.worktree = v;
                set(true);
            }
            if let Some(v) = args.get("enabled").and_then(Value::as_bool) {
                spec.enabled = v;
                set(true);
            }
            if !changed {
                return err("nothing to change: pass the fields to set".into());
            }
            match client
                .request(Request::SaveAutomation {
                    id,
                    spec,
                    from: create.then(|| me.clone()),
                })
                .await
            {
                Ok(Response::Automation { automation }) => {
                    saved(&automation, if create { "Created" } else { "Updated" })
                }
                Ok(other) => text_result(format!("unexpected response: {other:?}"), true),
                Err(e) => text_result(e.message, true),
            }
        }
        "delete_automation" | "run_automation" => {
            let Some(want) = str_arg("automation") else {
                return err("automation is required (its id or name)".into());
            };
            let a = match find(client, &want).await {
                Ok(a) => a,
                Err(e) => return err(e),
            };
            let id: AutomationId = a.id.clone();
            if name == "delete_automation" {
                match client.request(Request::DeleteAutomation { id }).await {
                    Ok(_) => text_result(
                        format!(
                            "Deleted \"{}\". The threads its runs started stay.",
                            a.spec.name
                        ),
                        false,
                    ),
                    Err(e) => text_result(e.message, true),
                }
            } else {
                match client.request(Request::RunAutomation { id }).await {
                    Ok(Response::Thread { thread }) => json_result(&json!({
                        "thread_id": thread.id, "title": thread.title,
                        "next": "It is running. Call wait_for_threads with this thread_id for its reply.",
                    })),
                    Ok(other) => text_result(format!("unexpected response: {other:?}"), true),
                    Err(e) => text_result(e.message, true),
                }
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_by_name() {
        assert_eq!(days(&json!(["Mon", "friday"])).unwrap(), vec![1, 5]);
        assert_eq!(days(&json!(["weekdays"])).unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(days(&json!("sat, sun")).unwrap(), vec![6, 0]);
        assert_eq!(days(&json!([7])).unwrap(), vec![0]);
        assert!(days(&json!(["someday"])).is_err());
    }

    #[test]
    fn one_schedule_at_a_time() {
        assert_eq!(
            schedule(&json!({ "every_minutes": 30 })).unwrap(),
            Some(Schedule::Interval { minutes: 30 })
        );
        assert_eq!(
            schedule(&json!({ "time": "9:00", "days": ["weekends"] })).unwrap(),
            Some(Schedule::Daily {
                time: "9:00".into(),
                days: vec![0, 6]
            })
        );
        assert!(schedule(&json!({ "every_minutes": 5, "cron": "* * * * *" })).is_err());
        assert!(schedule(&json!({ "days": ["mon"] })).is_err());
        assert_eq!(schedule(&json!({ "name": "x" })).unwrap(), None);
        assert!(schedule(&json!({ "once_at": "tomorrow" })).is_err());
        assert!(matches!(
            schedule(&json!({ "once_at": "2030-01-02 03:04" })).unwrap(),
            Some(Schedule::Once { .. })
        ));
    }
}
