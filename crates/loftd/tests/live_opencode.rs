//! Live test against the installed `opencode` CLI (`opencode acp`). Uses the
//! agent's configured model (OpenCode's free default works without an
//! account), so it is ignored by default:
//!
//! ```sh
//! cargo test -p loftd --test live_opencode -- --ignored --nocapture
//! # raw ACP traffic: AGENTLOFT_LOG=harness.acp=trace
//! ```

use loft_client::model::ThreadModel;
use loft_client::{Client, ClientEvent};
use loft_config::Paths;
use loft_harness::Harness;
use loft_protocol::*;
use loftd::{Daemon, DaemonOptions};
use std::sync::Arc;
use std::time::Duration;

async fn daemon(paths: &Paths) -> Daemon {
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(loft_harness_acp::AcpHarness::opencode())];
    Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses,
        capture_login_env: true,
        mcp_bridge: Some(loft_core::McpBridge {
            exe: env!("CARGO_BIN_EXE_agentloftd").into(),
            home: paths.home.clone(),
        }),
    })
    .await
    .unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum OnApproval {
    Allow,
    Interrupt,
}

/// Send a message and pump events until the turn ends. Returns the titles
/// of the approvals that were asked.
async fn turn(
    client: &Client,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
    model: &mut ThreadModel,
    text: &str,
    on_approval: OnApproval,
) -> Vec<String> {
    let n = model.turns.len() + 1;
    let mut asked = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut interrupted = false;
    client
        .request(Request::SendMessage {
            thread_id: model.thread.id.clone(),
            text: text.into(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("turn timed out")
            .unwrap();
        match ev {
            ClientEvent::Event { event, .. } => {
                model.apply_event(&event);
            }
            ClientEvent::Delta { delta, .. } => {
                model.apply_delta(&delta);
            }
            _ => {}
        }
        for req in model.open_requests().cloned().collect::<Vec<_>>() {
            if !seen.insert(req.id.clone()) {
                continue;
            }
            println!(
                "approval: {} ({:?}, options {:?})",
                req.title, req.kind, req.options
            );
            asked.push(req.title.clone());
            match on_approval {
                OnApproval::Allow => {
                    let _ = client
                        .request(Request::RespondRequest {
                            thread_id: model.thread.id.clone(),
                            request_id: req.id.clone(),
                            decision: DecisionKind::AllowOnce,
                        })
                        .await;
                }
                OnApproval::Interrupt if !interrupted => {
                    interrupted = true;
                    println!("interrupt");
                    client
                        .request(Request::Interrupt {
                            thread_id: model.thread.id.clone(),
                        })
                        .await
                        .unwrap();
                }
                OnApproval::Interrupt => {}
            }
        }
        if model.turns.len() >= n && model.turns.iter().all(|t| t.status.is_terminal()) {
            return asked;
        }
    }
}

fn last_text(model: &ThreadModel) -> String {
    model
        .items
        .iter()
        .rev()
        .find_map(|i| match &i.body {
            ItemBody::AssistantMessage { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn tools(model: &ThreadModel) -> Vec<(String, String, ItemStatus)> {
    model
        .items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::ToolCall { tool, title, .. } => Some((tool.clone(), title.clone(), i.status)),
            _ => None,
        })
        .collect()
}

/// Completed tool calls that used the Agentloft MCP server, either directly
/// (`mcp__agentloft__*`) or from OpenCode's code-mode `execute` tool
/// (`tools.agentloft.list_threads()`).
fn agentloft_calls(model: &ThreadModel) -> Vec<(String, String)> {
    model
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::Completed)
        .filter_map(|i| match &i.body {
            ItemBody::ToolCall {
                tool, title, input, ..
            } if tool.contains("agentloft") || input.to_string().contains("agentloft") => {
                Some((tool.clone(), title.clone()))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "drives the real opencode CLI"]
async fn live_opencode_turn_approval_interrupt_mcp_resume() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AGENTLOFT_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_test_writer()
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    // Make OpenCode ask before shell commands (its default is to allow).
    std::fs::write(
        repo.join("opencode.json"),
        r#"{ "$schema": "https://opencode.ai/config.json", "permission": { "bash": "ask" } }"#,
    )
    .unwrap();

    let d = daemon(&paths).await;
    let (client, mut rx) = Client::connect(&paths.socket_file(), ClientKind::Test, "live")
        .await
        .unwrap();
    let Response::Project { project } = client
        .request(Request::AddProject {
            path: repo.to_string_lossy().into_owned(),
            name: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let Response::Thread { thread } = client
        .request(Request::CreateThread {
            project_id: project.id,
            harness: HarnessId(HarnessId::OPENCODE.into()),
            title: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    client.subscribe(Topic::Thread(thread.id.clone()));
    let ClientEvent::Snapshot { snapshot, .. } = rx.recv().await.unwrap() else {
        panic!()
    };
    let mut model = ThreadModel::from_snapshot(snapshot).unwrap();

    // 1. Shell command behind an approval + streamed answer.
    let asked = turn(&client, &mut rx, &mut model, "Remember the codeword NEBULA-9. Run the shell command `touch opencode-live.txt` with your bash tool, then reply with the single word: done", OnApproval::Allow).await;
    let t1 = model.turns[0].clone();
    assert_eq!(t1.status, TurnStatus::Completed, "{t1:?}");
    assert!(repo.join("opencode-live.txt").exists(), "command ran");
    assert!(
        asked.iter().any(|t| t.starts_with("Shell: ")),
        "asked: {asked:?}"
    );
    assert!(
        tools(&model)
            .iter()
            .any(|(tool, _, s)| tool == "Shell" && *s == ItemStatus::Completed),
        "{:?}",
        tools(&model)
    );
    let session = model
        .thread
        .native_session_id
        .clone()
        .expect("ACP session id");
    println!(
        "turn 1 ok: session={session} model={:?} answer={:?} tools={:?} usage={:?}",
        model.thread.model,
        last_text(&model),
        tools(&model),
        t1.usage
    );

    // 2. Interrupt while an approval is open (session/cancel + `cancelled`).
    let asked = turn(
        &client,
        &mut rx,
        &mut model,
        "Run the shell command `sleep 20 && touch late.txt` with your bash tool, then reply done.",
        OnApproval::Interrupt,
    )
    .await;
    let t2 = model.turns.last().unwrap().clone();
    println!("turn 2: status={:?} asked={asked:?}", t2.status);
    assert_eq!(t2.status, TurnStatus::Interrupted, "{t2:?}");
    assert!(
        !repo.join("late.txt").exists(),
        "cancelled command must not run"
    );

    // 3. The Agentloft MCP server is attached: list threads (auto-approved).
    let asked = turn(&client, &mut rx, &mut model, "Call the agentloft MCP tool list_threads and tell me how many threads it returned, as a single number.", OnApproval::Allow).await;
    let mcp = agentloft_calls(&model);
    println!(
        "turn 3: agentloft calls={mcp:?} asked={asked:?} answer={:?}",
        last_text(&model)
    );
    assert!(
        !mcp.is_empty(),
        "the agentloft MCP call succeeded: {:?}",
        tools(&model)
    );
    // This test's daemon has exactly one thread.
    assert!(last_text(&model).contains('1'), "{}", last_text(&model));
    assert!(
        !asked.iter().any(|t| t.contains("agentloft")),
        "agentloft tools are pre-approved: {asked:?}"
    );

    // 4. Restart the daemon; the next turn resumes the same ACP session.
    let thread_id = model.thread.id.clone();
    drop(client);
    drop(rx);
    d.shutdown().await;
    let d = daemon(&paths).await;
    let (client, mut rx) = Client::connect(&paths.socket_file(), ClientKind::Test, "live")
        .await
        .unwrap();
    client.subscribe(Topic::Thread(thread_id));
    let ClientEvent::Snapshot { snapshot, .. } = rx.recv().await.unwrap() else {
        panic!()
    };
    let mut model = ThreadModel::from_snapshot(snapshot).unwrap();
    let items_before = model.items.len();
    turn(
        &client,
        &mut rx,
        &mut model,
        "What codeword did I ask you to remember? Reply with just the codeword.",
        OnApproval::Allow,
    )
    .await;
    assert_eq!(
        model.thread.native_session_id.as_deref(),
        Some(session.as_str())
    );
    assert!(
        last_text(&model).contains("NEBULA-9"),
        "{}",
        last_text(&model)
    );
    // session/load's history replay must not duplicate the timeline.
    let new_items = model.items.len() - items_before;
    assert!(new_items <= 4, "replay leaked {new_items} items");
    println!("resume ok: {:?} (+{new_items} items)", last_text(&model));
    d.shutdown().await;
}
