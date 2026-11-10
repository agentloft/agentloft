//! Live test against the installed `codex` CLI (app-server). Uses real
//! tokens, so it is ignored by default:
//!
//! ```sh
//! cargo test -p loftd --test live_codex -- --ignored --nocapture
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
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(loft_harness_codex::CodexHarness)];
    Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses,
        capture_login_env: true,
        mcp_bridge: Some(loft_core::McpBridge {
            exe: env!("CARGO_BIN_EXE_agentloftd").into(),
            home: paths.home.clone(),
        }),
        web_bundle: None,
    })
    .await
    .unwrap()
}

/// Send a message and pump events (auto-approving) until the turn ends.
async fn turn(
    client: &Client,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientEvent>,
    model: &mut ThreadModel,
    text: &str,
) {
    let n = model.turns.len() + 1;
    client
        .request(Request::SendMessage {
            thread_id: model.thread.id.clone(),
            text: text.into(),
            attachments: vec![],
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
            println!("approve: {}", req.title);
            let _ = client
                .request(Request::RespondRequest {
                    thread_id: model.thread.id.clone(),
                    request_id: req.id.clone(),
                    decision: DecisionKind::AllowOnce,
                })
                .await;
        }
        if model.turns.len() >= n && model.turns.iter().all(|t| t.status.is_terminal()) {
            return;
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

#[tokio::test]
#[ignore = "drives the real codex CLI"]
async fn live_codex_turn_approval_mcp_resume() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();

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
            harness: HarnessId(HarnessId::CODEX.into()),
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
    turn(&client, &mut rx, &mut model, "Remember the codeword NEBULA-9. Run the shell command `touch codex-live.txt`, then reply with the single word: done").await;
    let t1 = &model.turns[0];
    assert_eq!(t1.status, TurnStatus::Completed, "{t1:?}");
    assert!(repo.join("codex-live.txt").exists(), "command ran");
    assert!(
        model
            .items
            .iter()
            .any(|i| matches!(&i.body, ItemBody::ToolCall { tool, .. } if tool == "Shell"))
    );
    let session = model
        .thread
        .native_session_id
        .clone()
        .expect("codex thread id");
    println!("turn 1 ok: {:?} usage={:?}", last_text(&model), t1.usage);

    // 2. The Agentloft MCP server is attached: list threads.
    turn(&client, &mut rx, &mut model, "Call the agentloft MCP tool list_threads and tell me how many threads it returned, as a single number.").await;
    let used_mcp = model
        .items
        .iter()
        .any(|i| matches!(&i.body, ItemBody::ToolCall { tool, .. } if tool.contains("agentloft")));
    let mcp_ok = model.items.iter().any(|i| {
        matches!(&i.body, ItemBody::ToolCall { tool, is_error: false, .. } if tool.contains("agentloft"))
            && i.status == ItemStatus::Completed
    });
    println!(
        "turn 2: mcp used={used_mcp} ok={mcp_ok} answer={:?}",
        last_text(&model)
    );

    // 3. Restart the daemon; the next turn resumes the same Codex thread.
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
    turn(
        &client,
        &mut rx,
        &mut model,
        "What codeword did I ask you to remember? Reply with just the codeword.",
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
    println!("resume ok: {:?}", last_text(&model));
    assert!(used_mcp, "codex called the agentloft MCP tool");
    assert!(mcp_ok, "the agentloft MCP call succeeded");
    d.shutdown().await;
}
