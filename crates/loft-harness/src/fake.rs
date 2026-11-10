//! A scripted in-process harness for tests and UI development.
//!
//! Behaviour is driven by the prompt text:
//! - default: streams `echo: <text>` in a few chunks;
//! - contains `approve`: opens a tool approval first and waits for a decision;
//! - contains `question`: asks two questions first (`AskUserQuestion`);
//! - contains `plan`: proposes a plan first (`ExitPlanMode`);
//! - contains `slow`: streams slowly (≈2 s) so interrupts can be tested;
//! - contains `fail`: completes the turn as failed.

use crate::{
    Capabilities, Harness, HarnessEvent, SessionCommand, SessionConfig, SessionEnds, SessionHandle,
    ToolResult,
};
use loft_protocol::{
    DecisionKind, DeltaChannel, HarnessId, ItemBody, ItemStatus, RequestKind, ToolKind, TurnStatus,
    Usage,
};
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub struct FakeHarness;

impl Harness for FakeHarness {
    fn id(&self) -> HarnessId {
        HarnessId(HarnessId::FAKE.into())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            client_session_id: true,
            live_model_switch: true,
            live_mode_switch: true,
            steer: true,
            ..Default::default()
        }
    }

    fn open_session(&self, cfg: SessionConfig) -> anyhow::Result<SessionHandle> {
        let (handle, ends) = SessionHandle::pair();
        let session_id = cfg
            .resume
            .clone()
            .or(cfg.new_session_id.clone())
            .unwrap_or_else(|| "fake-session".into());
        tokio::spawn(run(ends, session_id));
        Ok(handle)
    }
}

async fn run(mut ends: SessionEnds, session_id: String) {
    ends.emit(HarnessEvent::SessionReady {
        models: vec![],
        native_session_id: session_id,
        model: Some("fake-model".into()),
        slash_commands: vec!["help".into()],
    });
    let mut turn_no = 0u32;
    while let Some(cmd) = ends.commands.recv().await {
        match cmd {
            SessionCommand::StartTurn(input) => {
                turn_no += 1;
                if !run_turn(&mut ends, turn_no, &input.text).await {
                    return;
                }
            }
            SessionCommand::Shutdown => break,
            // Outside a turn these are no-ops.
            _ => {}
        }
    }
    ends.emit(HarnessEvent::Exited {
        code: Some(0),
        stderr_tail: String::new(),
    });
}

/// Returns `false` if the session should end.
async fn run_turn(ends: &mut SessionEnds, turn_no: u32, text: &str) -> bool {
    ends.emit(HarnessEvent::TurnStarted);

    // What the user said to a question or plan, echoed in the reply.
    let mut heard_back: Option<String> = None;
    let ask = if text.contains("approve") {
        Some(Ask::Approval)
    } else if text.contains("question") {
        Some(Ask::Question)
    } else if text.contains("plan") {
        Some(Ask::Plan)
    } else {
        None
    };
    if let Some(ask) = ask {
        let tool_id = format!("tool-{turn_no}");
        let (tool, kind, title, input) = match ask {
            Ask::Approval => (
                "Bash",
                ToolKind::Command,
                "touch fake.txt",
                serde_json::json!({ "command": "touch fake.txt" }),
            ),
            Ask::Question => (
                "AskUserQuestion",
                ToolKind::Other,
                "AskUserQuestion",
                fake_questions(),
            ),
            Ask::Plan => (
                "ExitPlanMode",
                ToolKind::Other,
                "ExitPlanMode",
                serde_json::json!({ "plan": FAKE_PLAN }),
            ),
        };
        ends.emit(HarnessEvent::ItemStarted {
            parent: None,
            native_id: tool_id.clone(),
            body: ItemBody::ToolCall {
                tool: tool.into(),
                tool_kind: kind,
                title: title.into(),
                input: input.clone(),
                output: None,
                is_error: false,
            },
        });
        let req_id = format!("req-{turn_no}");
        let (kind, detail, options) = match ask {
            Ask::Approval => (
                RequestKind::ToolApproval,
                serde_json::json!({ "command": "touch fake.txt" }),
                vec![
                    DecisionKind::AllowOnce,
                    DecisionKind::AllowSession,
                    DecisionKind::Deny,
                    DecisionKind::DenyAndStop,
                ],
            ),
            Ask::Question => (
                RequestKind::Question,
                input,
                vec![
                    DecisionKind::AllowOnce,
                    DecisionKind::Deny,
                    DecisionKind::DenyAndStop,
                ],
            ),
            Ask::Plan => (
                RequestKind::Plan,
                input,
                vec![
                    DecisionKind::AllowSession,
                    DecisionKind::AllowOnce,
                    DecisionKind::Deny,
                ],
            ),
        };
        ends.emit(HarnessEvent::RequestOpened {
            native_id: req_id.clone(),
            kind,
            title: format!("{tool}: {title}"),
            detail,
            options,
            related_item: Some(tool_id.clone()),
        });
        let (decision, reply) = loop {
            match ends.commands.recv().await {
                Some(SessionCommand::Respond {
                    native_request_id,
                    decision,
                    reply,
                }) if native_request_id == req_id => break (decision, reply),
                Some(SessionCommand::Interrupt) => break (DecisionKind::DenyAndStop, None),
                Some(SessionCommand::Shutdown) | None => return false,
                Some(_) => {}
            }
        };
        ends.emit(HarnessEvent::RequestResolved { native_id: req_id });
        let allowed = matches!(
            decision,
            DecisionKind::AllowOnce | DecisionKind::AllowSession
        );
        let said = reply.map(|r| {
            let mut parts: Vec<String> = r
                .answers
                .iter()
                .map(|a| format!("{} = {}", a.id, a.answers.join(", ")))
                .collect();
            parts.extend(r.message);
            parts.join("; ")
        });
        heard_back = said.clone().filter(|s| !s.is_empty());
        ends.emit(HarnessEvent::ItemCompleted {
            native_id: tool_id,
            body: None,
            tool_result: Some(ToolResult {
                output: match (allowed, said) {
                    (true, Some(said)) if !said.is_empty() => said,
                    (true, _) => "".into(),
                    (false, _) => "denied by user".into(),
                },
                is_error: !allowed,
            }),
            status: if allowed {
                ItemStatus::Completed
            } else {
                ItemStatus::Declined
            },
        });
        if decision == DecisionKind::DenyAndStop {
            ends.emit(HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                usage: None,
                error: None,
            });
            return true;
        }
    }

    let msg_id = format!("msg-{turn_no}");
    ends.emit(HarnessEvent::ItemStarted {
        parent: None,
        native_id: msg_id.clone(),
        body: ItemBody::AssistantMessage {
            text: String::new(),
        },
    });
    let reply = match heard_back {
        Some(heard) => format!("echo: {text} (heard: {heard})"),
        None => format!("echo: {text}"),
    };
    let chunk_delay = if text.contains("slow") {
        Duration::from_millis(400)
    } else {
        Duration::from_millis(5)
    };
    let words: Vec<&str> = reply.split_inclusive(' ').collect();
    // Messages sent into the turn, answered before it ends.
    let mut steered: Vec<String> = Vec::new();
    for word in words {
        tokio::select! {
            _ = tokio::time::sleep(chunk_delay) => {}
            cmd = ends.commands.recv() => match cmd {
                Some(SessionCommand::Interrupt) => {
                    ends.emit(HarnessEvent::ItemCompleted {
                        native_id: msg_id,
                        body: None,
                        tool_result: None,
                        status: ItemStatus::Completed,
                    });
                    ends.emit(HarnessEvent::TurnCompleted {
                        status: TurnStatus::Interrupted,
                        usage: None,
                        error: None,
                    });
                    return true;
                }
                Some(SessionCommand::Shutdown) | None => return false,
                Some(SessionCommand::Steer(input)) => steered.push(input.text),
                Some(_) => {}
            }
        }
        ends.emit(HarnessEvent::ItemDelta {
            native_id: msg_id.clone(),
            channel: DeltaChannel::Text,
            text: word.to_owned(),
        });
    }
    ends.emit(HarnessEvent::ItemCompleted {
        native_id: msg_id,
        body: None,
        tool_result: None,
        status: ItemStatus::Completed,
    });
    for (i, heard) in steered.into_iter().enumerate() {
        let id = format!("msg-{turn_no}-steer-{i}");
        ends.emit(HarnessEvent::ItemStarted {
            parent: None,
            native_id: id.clone(),
            body: ItemBody::AssistantMessage {
                text: format!("heard: {heard}"),
            },
        });
        ends.emit(HarnessEvent::ItemCompleted {
            native_id: id,
            body: None,
            tool_result: None,
            status: ItemStatus::Completed,
        });
    }
    let failed = text.contains("fail");
    ends.emit(HarnessEvent::TurnCompleted {
        status: if failed {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        },
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: reply.len() as u64,
            ..Default::default()
        }),
        error: failed.then(|| "fake failure".into()),
    });
    true
}

#[derive(Debug, Clone, Copy)]
enum Ask {
    Approval,
    Question,
    Plan,
}

const FAKE_PLAN: &str = "## Plan: add a greeting\n\n1. Add `greet(name)` to `src/lib.rs`.\n2. Call it from `main`.\n3. Cover it with a unit test.\n\n```rust\npub fn greet(name: &str) -> String {\n    format!(\"Hello, {name}!\")\n}\n```";

fn fake_questions() -> serde_json::Value {
    serde_json::json!({ "questions": [
        {
            "id": "Which database should the service use?",
            "header": "Database",
            "question": "Which database should the service use?",
            "options": [
                { "label": "SQLite", "description": "One file, nothing to run. Good for a single machine." },
                { "label": "Postgres", "description": "A server; scales out and has richer types." },
            ],
            "multi_select": false,
            "allow_other": true,
        },
        {
            "id": "Which extras do you want?",
            "header": "Extras",
            "question": "Which extras do you want?",
            "options": [
                { "label": "Tests", "description": "Unit tests for each module." },
                { "label": "Docs", "description": "A README section." },
                { "label": "CI", "description": "A GitHub Actions workflow." },
            ],
            "multi_select": true,
            "allow_other": true,
        },
    ]})
}
