//! Deterministic handoff digest (ADR-0011): what a fork into another
//! provider starts from. Built only from the parent's event log and diff,
//! so it's instant, free and reproducible. The user reviews and edits it as
//! the fork's first message.

use loft_protocol::{DiffFileStat, Item, ItemBody, Thread};

/// Rough cap on the digest (~2.5k tokens).
const BUDGET: usize = 10_000;
const MAX_REQUESTS: usize = 8;
const MAX_FILES: usize = 40;

pub(crate) fn digest(
    parent: &Thread,
    harness_name: &str,
    items: &[Item],
    files: &[DiffFileStat],
) -> String {
    let requests: Vec<&str> = items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::UserMessage { text, .. } => Some(text.trim()),
            _ => None,
        })
        .filter(|t| !t.is_empty())
        .collect();
    let outcome = items.iter().rev().find_map(|i| match &i.body {
        ItemBody::AssistantMessage { text } if !text.trim().is_empty() => Some(text.trim()),
        _ => None,
    });

    let mut out = format!(
        "I'm continuing a task that another agent ({harness_name}) worked on in the thread \"{}\". Here's where it stands.\n",
        parent.title
    );

    if !requests.is_empty() {
        out.push_str("\n## What was asked\n");
        let n = requests.len();
        for (ix, req) in requests.iter().enumerate() {
            // Keep the first two and the latest ones.
            if n > MAX_REQUESTS && ix >= 2 && ix < n - (MAX_REQUESTS - 2) {
                if ix == 2 {
                    out.push_str(&format!("- … {} more …\n", n - MAX_REQUESTS));
                }
                continue;
            }
            out.push_str(&format!("- {}\n", one_line(req, 400)));
        }
    }

    if let Some(text) = outcome {
        out.push_str("\n## Where it ended\n");
        out.push_str(&clip(text, 3_000));
        out.push('\n');
    }

    if !files.is_empty() {
        out.push_str("\n## Files changed\n");
        for f in files.iter().take(MAX_FILES) {
            if f.binary {
                out.push_str(&format!("- {} (binary)\n", f.path));
            } else {
                out.push_str(&format!("- {} (+{} −{})\n", f.path, f.added, f.removed));
            }
        }
        if files.len() > MAX_FILES {
            out.push_str(&format!("- … and {} more\n", files.len() - MAX_FILES));
        }
    }

    let todos = open_todos(items);
    if !todos.is_empty() {
        out.push_str("\n## Open items\n");
        for t in todos {
            out.push_str(&format!("- [ ] {}\n", one_line(&t, 200)));
        }
    }

    let mut out = clip(&out, BUDGET);
    out.push_str(&format!(
        "\nThe full history is in Agentloft thread `{}`. Use the agentloft MCP tools (`read_thread`, `get_thread_diff`) if you need more detail.\n\nNext: ",
        parent.id
    ));
    out
}

/// Pull request body for a worktree thread: outcome, requests, files.
pub(crate) fn pr_body(
    thread: &Thread,
    harness_name: &str,
    items: &[Item],
    files: &[DiffFileStat],
) -> String {
    let mut out = String::new();
    if let Some(text) = items.iter().rev().find_map(|i| match &i.body {
        ItemBody::AssistantMessage { text } if !text.trim().is_empty() => Some(text.trim()),
        _ => None,
    }) {
        out.push_str("## Summary\n\n");
        out.push_str(&clip(text, 4_000));
        out.push_str("\n\n");
    }
    let requests: Vec<&str> = items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::UserMessage { text, .. } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .collect();
    if !requests.is_empty() {
        out.push_str("## Requests\n\n");
        for r in requests.iter().take(MAX_REQUESTS) {
            out.push_str(&format!("- {}\n", one_line(r, 300)));
        }
        out.push('\n');
    }
    if !files.is_empty() {
        out.push_str("## Files changed\n\n");
        for f in files.iter().take(MAX_FILES) {
            out.push_str(&format!("- `{}` (+{} −{})\n", f.path, f.added, f.removed));
        }
        if files.len() > MAX_FILES {
            out.push_str(&format!("- … and {} more\n", files.len() - MAX_FILES));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "---\nCreated with Agentloft from a {harness_name} thread (`{}`).\n",
        thread.id
    ));
    out
}

/// Unfinished entries of the latest todo list (Claude `TodoWrite`).
fn open_todos(items: &[Item]) -> Vec<String> {
    let Some(input) = items.iter().rev().find_map(|i| match &i.body {
        ItemBody::ToolCall { tool, input, .. } if tool == "TodoWrite" => Some(input),
        _ => None,
    }) else {
        return Vec::new();
    };
    input
        .get("todos")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .filter(|t| t.get("status").and_then(|s| s.as_str()) != Some("completed"))
        .filter_map(|t| t.get("content").and_then(|c| c.as_str()).map(str::to_owned))
        .collect()
}

fn one_line(text: &str, max: usize) -> String {
    clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut s: String = text.chars().take(max).collect();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::{ItemId, ItemStatus, ThreadId, ToolKind, TurnId};

    fn item(body: ItemBody) -> Item {
        Item {
            id: ItemId::new(),
            thread_id: ThreadId::new(),
            turn_id: Some(TurnId::new()),
            ordinal: 0,
            status: ItemStatus::Completed,
            body,
            updated_at_ms: 0,
            parent_id: None,
            started_at_ms: None,
        }
    }

    #[test]
    fn digest_is_deterministic_and_complete() {
        let parent = Thread {
            id: ThreadId("T1".into()),
            project_id: loft_protocol::ProjectId::new(),
            title: "Greet".into(),
            harness: loft_protocol::HarnessId::claude(),
            cwd: "/r".into(),
            model: None,
            permission_mode: None,
            effort: None,
            native_session_id: None,
            status: loft_protocol::ThreadStatus::Idle,
            created_at_ms: 0,
            updated_at_ms: 0,
            workspace: Default::default(),
            parent: None,
            slash_commands: Vec::new(),
            command_info: Vec::new(),
            started_at_ms: None,
            draft: None,
            waiting_on: Vec::new(),
            automation_id: None,
        };
        let items = vec![
            item(ItemBody::UserMessage {
                text: "Add a greet()\nfunction".into(),
                attachments: vec![],
            }),
            item(ItemBody::ToolCall {
                tool: "TodoWrite".into(),
                tool_kind: ToolKind::Other,
                title: "todos".into(),
                input: serde_json::json!({"todos": [
                    {"content": "Write greet", "status": "completed"},
                    {"content": "Add tests", "status": "pending"},
                ]}),
                output: None,
                is_error: false,
            }),
            item(ItemBody::AssistantMessage {
                text: "Added greet() to main.py.".into(),
            }),
        ];
        let files = vec![DiffFileStat {
            path: "main.py".into(),
            added: 3,
            removed: 1,
            binary: false,
        }];
        let a = digest(&parent, "Claude", &items, &files);
        assert_eq!(a, digest(&parent, "Claude", &items, &files));
        assert!(a.contains("- Add a greet() function\n"), "{a}");
        assert!(a.contains("## Where it ended\nAdded greet() to main.py."));
        assert!(a.contains("- main.py (+3 −1)"));
        assert!(a.contains("- [ ] Add tests") && !a.contains("Write greet"));
        assert!(a.contains("thread `T1`") && a.ends_with("Next: "));
    }
}
