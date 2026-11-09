//! Workbench features end to end over the real socket: scoped files,
//! checkpoints + diffs, daemon-owned terminals, and the agent → MCP bridge →
//! daemon → UI browser-host path.

use loft_client::model::ThreadModel;
use loft_client::{Client, ClientEvent};
use loft_config::Paths;
use loft_harness::Harness;
use loft_protocol::*;
use loftd::{Daemon, DaemonOptions};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;

struct Env {
    _dir: tempfile::TempDir,
    paths: Paths,
    repo: std::path::PathBuf,
    daemon: Daemon,
    client: Client,
    rx: UnboundedReceiver<ClientEvent>,
    project: Project,
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("README.md"), "# Hello\n").unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir_all(repo.join("target")).unwrap();
    std::fs::write(repo.join("target/junk"), "x").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);

    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(loft_harness::fake::FakeHarness)];
    let daemon = Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses,
        capture_login_env: false,
        mcp_bridge: None,
    })
    .await
    .unwrap();
    let (client, rx) = Client::connect(&paths.socket_file(), ClientKind::Test, "t")
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
    Env {
        _dir: dir,
        paths,
        repo,
        daemon,
        client,
        rx,
        project,
    }
}

async fn next(rx: &mut UnboundedReceiver<ClientEvent>) -> ClientEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out")
        .expect("closed")
}

#[tokio::test]
async fn files_are_listed_and_read_within_the_project_only() {
    let env = setup().await;
    let root = FsRoot::Project(env.project.id.clone());
    let Response::DirListing { entries } = env
        .client
        .request(Request::ListDir {
            root: root.clone(),
            rel_path: String::new(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names[0], "src", "dirs first: {names:?}");
    assert!(names.contains(&"README.md") && names.contains(&".gitignore"));
    assert!(!names.contains(&"target"), "gitignored: {names:?}");
    assert!(!names.contains(&".git"));

    let Response::File { file } = env
        .client
        .request(Request::ReadFile {
            root: root.clone(),
            rel_path: "src/main.rs".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(file.bytes.0, b"fn main() {}\n");
    assert!(!file.binary);

    let err = env
        .client
        .request(Request::ReadFile {
            root,
            rel_path: "../../etc/passwd".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);
    env.daemon.shutdown().await;
}

#[tokio::test]
async fn turns_are_checkpointed_and_diffable() {
    let mut env = setup().await;
    let Response::Thread { thread } = env
        .client
        .request(Request::CreateThread {
            project_id: env.project.id.clone(),
            harness: HarnessId(HarnessId::FAKE.into()),
            title: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    env.client.subscribe(Topic::Thread(thread.id.clone()));
    let ClientEvent::Snapshot { snapshot, .. } = next(&mut env.rx).await else {
        panic!()
    };
    let mut model = ThreadModel::from_snapshot(snapshot).unwrap();

    env.client
        .request(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "slow edit a b c d".into(),
        })
        .await
        .unwrap();
    // Wait for the "before" checkpoint, then change files mid-turn (as the
    // agent would), then wait for the turn and its "after" checkpoint.
    let mut edited = false;
    loop {
        if let ClientEvent::Event { event, .. } = next(&mut env.rx).await {
            model.apply_event(&event);
        }
        let turn = model.turns.first();
        if !edited && turn.is_some_and(|t| t.checkpoint_before.is_some()) {
            std::fs::write(
                env.repo.join("src/main.rs"),
                "fn main() { println!(\"hi\"); }\n",
            )
            .unwrap();
            std::fs::write(env.repo.join("NEW.md"), "new\n").unwrap();
            edited = true;
        }
        if turn.is_some_and(|t| t.checkpoint_after.is_some()) {
            break;
        }
    }
    assert!(edited);

    let Response::Diff { diff } = env
        .client
        .request(Request::GetDiff {
            thread_id: thread.id.clone(),
            scope: DiffScope::LastTurn,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let paths: Vec<_> = diff.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["NEW.md", "src/main.rs"], "{diff:?}");
    assert!(diff.patch.contains("+fn main() { println!(\"hi\"); }"));

    // The user's git state is untouched.
    let status = Command::new("git")
        .current_dir(&env.repo)
        .args(["diff", "--cached", "--name-only"])
        .output()
        .unwrap();
    assert!(status.stdout.is_empty());

    let Response::Diff { diff } = env
        .client
        .request(Request::GetDiff {
            thread_id: thread.id.clone(),
            scope: DiffScope::WorkingTree,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(diff.files.len(), 2);
    env.daemon.shutdown().await;
}

#[tokio::test]
async fn terminals_stream_output_and_accept_input() {
    let mut env = setup().await;
    let Response::Terminal { terminal } = env
        .client
        .request(Request::TerminalOpen {
            root: FsRoot::Project(env.project.id.clone()),
            cols: 80,
            rows: 24,
            key: Some("main".into()),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    env.client.subscribe(Topic::Terminal(terminal.id.clone()));
    let mut seen = Vec::new();
    match next(&mut env.rx).await {
        ClientEvent::Snapshot {
            snapshot: Snapshot::Terminal { replay, .. },
            ..
        } => seen.extend(replay.0),
        other => panic!("{other:?}"),
    }
    env.client
        .request(Request::TerminalInput {
            id: terminal.id.clone(),
            data: b"echo loft-$((40+2)) && pwd\r".to_vec().into(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&seen).contains("loft-42") {
        let ev = tokio::time::timeout_at(deadline, env.rx.recv())
            .await
            .expect("no echo output")
            .unwrap();
        if let ClientEvent::TerminalOutput { data, .. } = ev {
            seen.extend(data);
        }
    }
    env.client
        .request(Request::TerminalResize {
            id: terminal.id.clone(),
            cols: 100,
            rows: 30,
        })
        .await
        .unwrap();

    // A late subscriber gets the scrollback replayed.
    let (late, mut late_rx) = Client::connect(&env.paths.socket_file(), ClientKind::Test, "t")
        .await
        .unwrap();
    late.subscribe(Topic::Terminal(terminal.id.clone()));
    let ClientEvent::Snapshot {
        snapshot:
            Snapshot::Terminal {
                replay,
                terminal: info,
                ..
            },
        ..
    } = next(&mut late_rx).await
    else {
        panic!()
    };
    assert!(String::from_utf8_lossy(&replay.0).contains("loft-42"));
    assert_eq!((info.cols, info.rows), (100, 30));

    // Opening with the same key reattaches to the live shell.
    let Response::Terminal { terminal: again } = late
        .request(Request::TerminalOpen {
            root: FsRoot::Project(env.project.id.clone()),
            cols: 90,
            rows: 20,
            key: Some("main".into()),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(again.id, terminal.id);
    assert_eq!((again.cols, again.rows), (90, 20));

    env.client
        .request(Request::TerminalClose { id: terminal.id })
        .await
        .unwrap();
    env.daemon.shutdown().await;
}

/// A stand-in for the app window: answers browser calls.
async fn fake_browser_host(paths: &Paths) -> tokio::task::JoinHandle<()> {
    let (host, mut rx) =
        Client::connect_with(&paths.socket_file(), ClientKind::LocalUi, "t", &["browser"])
            .await
            .unwrap();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let ClientEvent::ClientCall {
                call_id,
                call: ClientCall::Browser { command, .. },
            } = ev
            {
                let reply = match command["action"].as_str() {
                    Some("snapshot") => {
                        serde_json::json!({ "snapshot": "Example Domain [ref=1] link More information" })
                    }
                    Some("screenshot") => serde_json::json!({ "png_base64": "iVBORw0KGgo=" }),
                    _ => serde_json::json!({ "ok": true, "echo": command }),
                };
                host.respond_call(call_id, Ok(reply));
            }
        }
    })
}

#[tokio::test]
async fn agents_drive_the_browser_through_the_mcp_bridge() {
    let env = setup().await;
    // No browser host yet: a clear error, not a hang.
    let thread_id = ThreadId::new();
    let err = env
        .client
        .request(Request::BrowserCall {
            thread_id: thread_id.clone(),
            command: serde_json::json!({"action": "back"}),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unsupported);

    let host = fake_browser_host(&env.paths).await;

    // Spawn the real MCP bridge binary, as an agent CLI would.
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_agentloftd"))
        .args(["mcp", "--thread", thread_id.as_str(), "--home"])
        .arg(&env.paths.home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut rpc = async |msg: serde_json::Value| -> serde_json::Value {
        let mut line = serde_json::to_vec(&msg).unwrap();
        line.push(b'\n');
        stdin.write_all(&line).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("mcp timeout")
            .unwrap()
            .expect("mcp closed");
        serde_json::from_str(&reply).unwrap()
    };

    let init = rpc(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}})).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "agentloft");
    let list = rpc(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
    assert!(list["result"]["tools"].as_array().unwrap().len() >= 12);

    let nav = rpc(serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com"}}})).await;
    assert_eq!(nav["result"]["isError"], false, "{nav}");
    assert!(
        nav["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("example.com")
    );

    let snap = rpc(serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_snapshot","arguments":{}}})).await;
    assert!(
        snap["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[ref=1]")
    );

    let shot = rpc(serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_screenshot","arguments":{}}})).await;
    assert_eq!(shot["result"]["content"][0]["type"], "image");

    let threads = rpc(serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_threads","arguments":{}}})).await;
    assert_eq!(threads["result"]["isError"], false);

    drop(stdin);
    let _ = child.wait().await;
    host.abort();
    env.daemon.shutdown().await;
}
