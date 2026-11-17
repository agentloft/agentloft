//! Live test against the installed `claude` CLI. Costs real tokens, so it is
//! ignored by default:
//!
//! ```sh
//! cargo test -p loftd --test live_claude -- --ignored --nocapture
//! ```

use loft_client::model::ThreadModel;
use loft_client::{Client, ClientEvent};
use loft_config::Paths;
use loft_harness::Harness;
use loft_protocol::*;
use loftd::{Daemon, DaemonOptions};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

struct Session {
    client: Client,
    rx: UnboundedReceiver<ClientEvent>,
    model: ThreadModel,
}

impl Session {
    /// Pump events until `done` holds (or time out).
    async fn until(&mut self, what: &str, done: impl Fn(&ThreadModel) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        while !done(&self.model) {
            let ev = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for: {what}"))
                .expect("disconnected");
            match ev {
                ClientEvent::Event { event, .. } => {
                    self.model.apply_event(&event);
                }
                ClientEvent::Delta { delta, .. } => {
                    self.model.apply_delta(&delta);
                }
                ClientEvent::Snapshot { snapshot, .. } => {
                    if let Some(m) = ThreadModel::from_snapshot(snapshot) {
                        self.model = m;
                    }
                }
                other => panic!("{other:?}"),
            }
        }
    }

    async fn send(&self, text: &str) {
        self.client
            .request(Request::SendMessage {
                thread_id: self.model.thread.id.clone(),
                text: text.into(),
                attachments: vec![],
            })
            .await
            .unwrap();
    }

    fn last_assistant_text(&self) -> String {
        self.model
            .items
            .iter()
            .rev()
            .find_map(|i| match &i.body {
                ItemBody::AssistantMessage { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

fn turns_done(n: usize) -> impl Fn(&ThreadModel) -> bool {
    move |m| m.turns.len() >= n && m.turns.iter().all(|t| t.status.is_terminal())
}

#[tokio::test]
#[ignore = "drives the real claude CLI"]
async fn live_claude_turns_approval_interrupt_resume() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(loft_harness_claude::ClaudeHarness)];
    let daemon = Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses,
        capture_login_env: true,
        mcp_bridge: None,
        web_bundle: None,
    })
    .await
    .unwrap();

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
            harness: HarnessId::claude(),
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
    let mut s = Session {
        client,
        rx,
        model: ThreadModel::from_snapshot(snapshot).unwrap(),
    };

    // 1. Plain streamed text turn.
    s.send("Remember the codeword AURORA-7. Reply with exactly: pong")
        .await;
    s.until("turn 1", turns_done(1)).await;
    assert_eq!(
        s.model.turns[0].status,
        TurnStatus::Completed,
        "{:?}",
        s.model.turns[0]
    );
    assert!(
        s.last_assistant_text().contains("pong"),
        "{}",
        s.last_assistant_text()
    );
    let session_id = s
        .model
        .thread
        .native_session_id
        .clone()
        .expect("session id");
    println!("turn 1 ok: {:?}", s.last_assistant_text());

    // 2. Tool approval (Claude's default mode asks before writing files).
    s.send("Use the Bash tool to run exactly: touch approved.txt . Then say done.")
        .await;
    s.until("approval request", |m| {
        m.open_requests().next().is_some() || turns_done(2)(m)
    })
    .await;
    if let Some(req) = s.model.open_requests().next().cloned() {
        println!("approval: {}", req.title);
        s.client
            .request(Request::RespondRequest {
                thread_id: s.model.thread.id.clone(),
                request_id: req.id,
                decision: DecisionKind::AllowOnce,
                reply: None,
            })
            .await
            .unwrap();
    } else {
        println!("note: no approval was requested (user settings may allow Bash)");
    }
    s.until("turn 2", turns_done(2)).await;
    assert!(
        repo.join("approved.txt").exists(),
        "tool ran after approval"
    );
    println!("turn 2 ok");

    // 3. Interrupt a long turn.
    s.send("Count slowly from 1 to 200, one number per line, with a short comment on each.")
        .await;
    s.until("streaming", |m| {
        m.turns.len() == 3
            && m.items.last().is_some_and(
                |i| matches!(&i.body, ItemBody::AssistantMessage { text } if text.len() > 20),
            )
    })
    .await;
    s.client
        .request(Request::Interrupt {
            thread_id: s.model.thread.id.clone(),
        })
        .await
        .unwrap();
    let t0 = std::time::Instant::now();
    s.until("turn 3 interrupted", turns_done(3)).await;
    println!("interrupt took {:?}", t0.elapsed());
    assert_eq!(s.model.turns[2].status, TurnStatus::Interrupted);
    assert!(t0.elapsed() < Duration::from_secs(5));

    // 4. Restart the daemon: the next message resumes the same session.
    let thread_id = s.model.thread.id.clone();
    drop(s);
    daemon.shutdown().await;
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(loft_harness_claude::ClaudeHarness)];
    let daemon = Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses,
        capture_login_env: true,
        mcp_bridge: None,
        web_bundle: None,
    })
    .await
    .unwrap();
    let (client, mut rx) = Client::connect(&paths.socket_file(), ClientKind::Test, "live")
        .await
        .unwrap();
    client.subscribe(Topic::Thread(thread_id));
    let ClientEvent::Snapshot { snapshot, .. } = rx.recv().await.unwrap() else {
        panic!()
    };
    let mut s = Session {
        client,
        rx,
        model: ThreadModel::from_snapshot(snapshot).unwrap(),
    };
    s.send("What codeword did I ask you to remember? Reply with just the codeword.")
        .await;
    s.until("turn 4", turns_done(4)).await;
    assert_eq!(
        s.model.thread.native_session_id.as_deref(),
        Some(session_id.as_str())
    );
    assert!(
        s.last_assistant_text().contains("AURORA-7"),
        "{}",
        s.last_assistant_text()
    );
    println!("resume ok: {:?}", s.last_assistant_text());
    daemon.shutdown().await;
}
