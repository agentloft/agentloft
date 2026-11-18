//! Workbench features end to end over the real socket: scoped files,
//! checkpoints + diffs, daemon-owned terminals, and the agent → MCP bridge →
//! daemon → browser path (ADR-0017).

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
    setup_with(Vec::new()).await
}

/// A daemon whose simulators and emulators are `devices`.
async fn setup_with(devices: Vec<Arc<dyn loft_core::devices::Backend>>) -> Env {
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
        check_agent_updates: false,
        mcp_bridge: None,
        web_bundle: None,
        device_backends: Some(devices),
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
            offset: 0,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(file.bytes.0, b"fn main() {}\n");
    assert!(!file.binary);
    assert!(!file.truncated);

    // A file larger than one answer holds comes in pieces, each asked for
    // from where the last one ended.
    let big: Vec<u8> = (0..11 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(env.repo.join("big.bin"), &big).unwrap();
    let mut got = Vec::new();
    loop {
        let Response::File { file } = env
            .client
            .request(Request::ReadFile {
                root: root.clone(),
                rel_path: "big.bin".into(),
                offset: got.len() as u64,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(file.offset, got.len() as u64);
        assert_eq!(file.size, big.len() as u64);
        assert!(!file.bytes.0.is_empty());
        got.extend(file.bytes.0);
        if !file.truncated {
            break;
        }
        assert!(got.len() < big.len(), "more was promised");
    }
    assert!(got == big, "the pieces make the file");

    let err = env
        .client
        .request(Request::ReadFile {
            root,
            rel_path: "../../etc/passwd".into(),
            offset: 0,
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
            attachments: vec![],
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

    // The same turn asked for by id (its "changed files" card).
    let turn_id = model.turns[0].id.clone();
    let Response::Diff { diff: by_id } = env
        .client
        .request(Request::GetDiff {
            thread_id: thread.id.clone(),
            scope: DiffScope::Turn(turn_id),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(by_id.files, diff.files);
    // …and the turn already carries them, worked out when it ended.
    assert_eq!(model.turns[0].changes.as_ref(), Some(&diff.files));

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
            command: None,
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
            command: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(again.id, terminal.id);
    assert_eq!((again.cols, again.rows), (90, 20));

    // A second tab under the same prefix; both are listed, in key order.
    let Response::Terminal { terminal: second } = late
        .request(Request::TerminalOpen {
            root: FsRoot::Project(env.project.id.clone()),
            cols: 80,
            rows: 24,
            key: Some("main#2".into()),
            command: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let Response::Terminals { terminals } = late
        .request(Request::ListTerminals {
            key_prefix: "main".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let keys: Vec<_> = terminals.iter().map(|t| t.key.clone().unwrap()).collect();
    assert_eq!(keys, vec!["main", "main#2"]);
    assert_eq!(terminals[1].id, second.id);

    env.client
        .request(Request::TerminalClose { id: terminal.id })
        .await
        .unwrap();
    env.daemon.shutdown().await;
}

/// A terminal opened with a command (a project action) types it into the
/// login shell; asking again under the same key leaves it alone while it
/// runs, and types it again once it has ended.
#[cfg(unix)]
#[tokio::test]
async fn a_terminal_runs_its_command_and_not_a_second_copy() {
    let mut env = setup().await;
    let key = action_terminal_key("thread:t", "Dev");
    assert_eq!(action_of_terminal_key(&key), Some("Dev"));
    let open = |command: &str| Request::TerminalOpen {
        root: FsRoot::Project(env.project.id.clone()),
        cols: 80,
        rows: 24,
        key: Some(key.clone()),
        command: Some(command.into()),
    };
    // `$((…))` so that only the command's output matches, never its echo.
    let Response::Terminal { terminal } = env
        .client
        .request(open("echo started-$((40+2)) && sleep 600"))
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
    // Output until `needle` shows, or for `wait` at most.
    async fn read(
        rx: &mut UnboundedReceiver<ClientEvent>,
        seen: &mut Vec<u8>,
        needle: &str,
        wait: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        while !String::from_utf8_lossy(seen).contains(needle) {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ClientEvent::TerminalOutput { data, .. })) => seen.extend(data),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return false,
            }
        }
        true
    }
    let long = Duration::from_secs(15);
    assert!(
        read(&mut env.rx, &mut seen, "started-42", long).await,
        "the command didn't run: {}",
        String::from_utf8_lossy(&seen)
    );

    // Still running (and no longer just started): the same shell is
    // answered with, nothing typed.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let Response::Terminal { terminal: again } = env
        .client
        .request(open("echo second-$((20+1))"))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(again.id, terminal.id);
    assert!(
        !read(
            &mut env.rx,
            &mut seen,
            "second-",
            Duration::from_millis(700)
        )
        .await
    );

    // Stopped (Ctrl-C): once the shell is back at its prompt, it runs.
    env.client
        .request(Request::TerminalInput {
            id: terminal.id.clone(),
            data: vec![0x03].into(),
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + long;
    loop {
        let Response::Terminal { terminal: again } = env
            .client
            .request(open("echo third-$((30+3))"))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(again.id, terminal.id);
        if read(
            &mut env.rx,
            &mut seen,
            "third-33",
            Duration::from_millis(500),
        )
        .await
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "not run again: {}",
            String::from_utf8_lossy(&seen)
        );
    }

    env.client
        .request(Request::TerminalClose { id: terminal.id })
        .await
        .unwrap();
    env.daemon.shutdown().await;
}

/// A thread of the fake agent, for browser calls to be about.
async fn fake_thread(env: &Env) -> Thread {
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
    thread
}

/// The real MCP bridge binary, as an agent CLI runs it.
struct Bridge {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl Bridge {
    async fn start(paths: &Paths, thread: &ThreadId) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_agentloftd"))
            .args(["mcp", "--thread", thread.as_str(), "--home"])
            .arg(&paths.home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut bridge = Self {
            child,
            stdin,
            lines,
        };
        let init = bridge.rpc(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}})).await;
        assert_eq!(init["result"]["serverInfo"]["name"], "agentloft");
        bridge
    }

    /// Send a JSON-RPC message; its answer.
    async fn rpc(&mut self, msg: serde_json::Value) -> serde_json::Value {
        let mut line = serde_json::to_vec(&msg).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
            .await
            .expect("mcp timeout")
            .unwrap()
            .expect("mcp closed");
        serde_json::from_str(&reply).unwrap()
    }

    /// Call a tool; its result.
    async fn tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let reply = self
            .rpc(serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
            .await;
        reply["result"].clone()
    }

    async fn stop(mut self) {
        drop(self.stdin);
        let _ = self.child.wait().await;
    }
}

/// An agent's browser tools reach the daemon's browser through the MCP
/// bridge, with no app window involved. Here that browser can't start (the
/// configured one isn't there), which the agent is told in so many words.
#[tokio::test]
async fn agents_drive_the_browser_through_the_mcp_bridge() {
    let env = setup().await;
    let missing = env.paths.home.join("no-such-chrome");
    std::fs::write(
        env.paths.config_file(),
        format!("[browser]\nexecutable = {:?}\n", missing.to_string_lossy()),
    )
    .unwrap();
    env.client.request(Request::ReloadConfig).await.unwrap();
    let thread = fake_thread(&env).await;

    // Straight to the daemon: a clear error, not a hang.
    let err = env
        .client
        .request(Request::BrowserCall {
            thread_id: thread.id.clone(),
            command: serde_json::json!({"action": "back"}),
            tab: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unsupported);
    assert!(err.message.contains("No browser at"), "{}", err.message);
    // Nonsense, or a thread that isn't one, is refused before anything starts.
    let err = env
        .client
        .request(Request::BrowserCall {
            thread_id: thread.id.clone(),
            command: serde_json::json!({"action": "fly"}),
            tab: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);
    let err = env
        .client
        .request(Request::BrowserCall {
            thread_id: ThreadId::new(),
            command: serde_json::json!({"action": "back"}),
            tab: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);

    let mut bridge = Bridge::start(&env.paths, &thread.id).await;
    let list = bridge
        .rpc(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await;
    assert!(list["result"]["tools"].as_array().unwrap().len() >= 12);
    let nav = bridge
        .tool(
            "browser_navigate",
            serde_json::json!({"url": "https://example.com"}),
        )
        .await;
    assert_eq!(nav["isError"], true, "{nav}");
    assert!(
        nav["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("No browser at"),
        "{nav}"
    );
    let threads = bridge.tool("list_threads", serde_json::json!({})).await;
    assert_eq!(threads["isError"], false);

    bridge.stop().await;
    env.daemon.shutdown().await;
}

/// An agent's device tools reach the simulators and emulators through the
/// MCP bridge (ADR-0020): here two fake phones, which keep everything they
/// were sent. Points are pixels of the agent's screenshot; the devices get
/// fractions of the screen.
#[tokio::test]
async fn agents_use_devices_through_the_mcp_bridge() {
    use loft_core::devices::fake::{FakeBackend, FakeEvent};
    use serde_json::json;
    let ios = Arc::new(FakeBackend::new(
        DevicePlatform::Ios,
        Duration::from_millis(200),
    ));
    let android = Arc::new(FakeBackend::new(
        DevicePlatform::Android,
        Duration::from_millis(200),
    ));
    let mut env = setup_with(vec![ios.clone(), android.clone()]).await;
    let thread = fake_thread(&env).await;
    env.client.subscribe(Topic::Thread(thread.id.clone()));
    let mut bridge = Bridge::start(&env.paths, &thread.id).await;
    let text = |v: &serde_json::Value| -> String {
        v["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let ok = |v: &serde_json::Value| assert_eq!(v["isError"], false, "{v}");
    let failed = |v: &serde_json::Value, says: &str| {
        assert_eq!(v["isError"], true, "{v}");
        assert!(text(v).contains(says), "{v}");
    };
    let id = ios.id().to_string();

    let list = bridge.tool("device_list", json!({})).await;
    ok(&list);
    let listed: serde_json::Value = serde_json::from_str(&text(&list)).unwrap();
    let devices = listed["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2, "{listed}");
    let phone = devices.iter().find(|d| d["id"] == id).unwrap();
    assert_eq!(phone["state"], "stopped");
    assert_eq!(phone["screen"], "390x844 pixels at 1x");
    assert!(listed.get("unavailable").is_none(), "{listed}");

    // A stopped device has to be booted first, and the agent is told so.
    let tap = bridge
        .tool("device_tap", json!({"id": id, "x": 10, "y": 10}))
        .await;
    failed(&tap, "boot it first");
    failed(
        &bridge.tool("device_tap", json!({"x": 1, "y": 1})).await,
        "id is required",
    );

    // Booting waits until it runs, and the thread's windows are told an
    // agent is using it.
    let boot = bridge.tool("device_boot", json!({"id": id})).await;
    ok(&boot);
    assert!(text(&boot).contains("\"running\""), "{boot}");
    assert_eq!(ios.log(), vec![FakeEvent::Boot]);
    loop {
        if let ClientEvent::DeviceDriven { thread_id, device } = next(&mut env.rx).await {
            assert_eq!(thread_id, thread.id);
            assert_eq!(device.id, ios.id());
            break;
        }
    }

    let shot = bridge.tool("device_screenshot", json!({"id": id})).await;
    ok(&shot);
    assert_eq!(shot["content"][0]["type"], "image");
    assert_eq!(shot["content"][0]["mimeType"], "image/jpeg");
    let jpeg = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(shot["content"][0]["data"].as_str().unwrap())
            .unwrap()
    };
    let picture = image::load_from_memory(&jpeg).unwrap();
    assert_eq!((picture.width(), picture.height()), (390, 844));
    assert!(text(&shot).contains("390x844 pixels"), "{shot}");

    // A tap is a finger down and up, at fractions of the screen.
    ok(&bridge
        .tool("device_tap", json!({"id": id, "x": 195, "y": 211}))
        .await);
    let touch = |phase, x, y| FakeEvent::Input(DeviceInput::Touch { phase, x, y });
    assert_eq!(
        ios.log()[1..],
        [
            touch(TouchPhase::Down, 0.5, 0.25),
            touch(TouchPhase::Up, 0.5, 0.25)
        ]
    );
    failed(
        &bridge
            .tool("device_tap", json!({"id": id, "x": 391, "y": 10}))
            .await,
        "outside the screenshot",
    );

    // A swipe goes down, moves on its way and comes up at its end.
    let before = ios.log().len();
    ok(&bridge
        .tool(
            "device_swipe",
            json!({"id": id, "from_x": 195, "from_y": 633, "to_x": 195, "to_y": 211, "duration_ms": 100}),
        )
        .await);
    let swipe = ios.log()[before..].to_vec();
    assert_eq!(swipe[0], touch(TouchPhase::Down, 0.5, 0.75));
    assert_eq!(*swipe.last().unwrap(), touch(TouchPhase::Up, 0.5, 0.25));
    let ys: Vec<f32> = swipe[1..swipe.len() - 1]
        .iter()
        .map(|e| match e {
            FakeEvent::Input(DeviceInput::Touch {
                phase: TouchPhase::Move,
                y,
                ..
            }) => *y,
            other => panic!("not a move: {other:?}"),
        })
        .collect();
    assert!(ys.len() >= 4, "{ys:?}");
    assert!(ys.windows(2).all(|w| w[1] < w[0]), "{ys:?}");
    assert!((ys.last().unwrap() - 0.25).abs() < 1e-6);

    // Text, keys and buttons.
    let before = ios.log().len();
    ok(&bridge
        .tool(
            "device_type",
            json!({"id": id, "text": "hello", "submit": true}),
        )
        .await);
    ok(&bridge
        .tool(
            "device_press",
            json!({"id": id, "key": "Backspace", "count": 2}),
        )
        .await);
    ok(&bridge
        .tool("device_press", json!({"id": id, "button": "home"}))
        .await);
    let key = |k: &str| {
        FakeEvent::Input(DeviceInput::Key {
            key: k.into(),
            modifiers: 0,
        })
    };
    assert_eq!(
        ios.log()[before..],
        [
            FakeEvent::Input(DeviceInput::Text {
                text: "hello".into()
            }),
            key("Enter"),
            key("Backspace"),
            key("Backspace"),
            FakeEvent::Input(DeviceInput::Button {
                button: DeviceButton::Home
            }),
        ]
    );
    failed(
        &bridge.tool("device_press", json!({"id": id})).await,
        "exactly one",
    );
    failed(
        &bridge
            .tool("device_press", json!({"id": id, "button": "jump"}))
            .await,
        "Unknown button",
    );

    // Turned, the screen is wider than tall: points follow without a new
    // screenshot.
    ok(&bridge
        .tool("device_press", json!({"id": id, "button": "rotate_left"}))
        .await);
    ok(&bridge
        .tool("device_tap", json!({"id": id, "x": 844, "y": 0}))
        .await);
    assert_eq!(*ios.log().last().unwrap(), touch(TouchPhase::Up, 1.0, 0.0));

    // Apps: a missing build fails before it reaches the device.
    let before = ios.log().len();
    let missing = env.paths.home.join("build/Missing.app");
    failed(
        &bridge
            .tool(
                "device_app",
                json!({"id": id, "action": "install", "path": missing}),
            )
            .await,
        "No app at",
    );
    failed(
        &bridge
            .tool("device_app", json!({"id": id, "action": "launch"}))
            .await,
        "app is required",
    );
    assert_eq!(ios.log().len(), before);
    ok(&bridge
        .tool(
            "device_app",
            json!({"id": id, "action": "launch", "app": "com.example.App"}),
        )
        .await);
    assert_eq!(
        *ios.log().last().unwrap(),
        FakeEvent::App(DeviceApp::Launch {
            app: "com.example.App".into()
        })
    );

    // The other platform is its own backend.
    let droid = android.id().to_string();
    ok(&bridge.tool("device_boot", json!({"id": droid})).await);
    ok(&bridge
        .tool("device_press", json!({"id": droid, "button": "back"}))
        .await);
    assert_eq!(
        *android.log().last().unwrap(),
        FakeEvent::Input(DeviceInput::Button {
            button: DeviceButton::Back
        })
    );

    ok(&bridge.tool("device_shutdown", json!({"id": id})).await);
    assert_eq!(*ios.log().last().unwrap(), FakeEvent::Shutdown);
    let list = bridge.tool("device_list", json!({})).await;
    let listed: serde_json::Value = serde_json::from_str(&text(&list)).unwrap();
    let phone = listed["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == id)
        .unwrap();
    assert_eq!(phone["state"], "stopped");

    bridge.stop().await;
    env.daemon.shutdown().await;
}

/// The MCP tools for several fingers, folding, settings, permissions and
/// pushes reach the fake devices as the daemon's own requests.
#[tokio::test]
async fn agents_pinch_fold_and_change_settings_through_the_mcp_bridge() {
    use loft_core::devices::fake::{FakeBackend, FakeEvent};
    use serde_json::json;
    let phone = Arc::new(FakeBackend::new(
        DevicePlatform::Android,
        Duration::from_millis(50),
    ));
    let fold = Arc::new(FakeBackend::foldable(Duration::from_millis(50)));
    let env = setup_with(vec![phone.clone(), fold.clone()]).await;
    let thread = fake_thread(&env).await;
    let mut bridge = Bridge::start(&env.paths, &thread.id).await;
    let text = |v: &serde_json::Value| -> String {
        v["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let ok = |v: &serde_json::Value| {
        assert_eq!(v["isError"], false, "{v}");
        serde_json::from_str::<serde_json::Value>(&text(v)).unwrap_or_default()
    };
    let failed = |v: &serde_json::Value, says: &str| {
        assert_eq!(v["isError"], true, "{v}");
        assert!(text(v).contains(says), "{v}");
    };
    let id = phone.id().to_string();
    ok(&bridge.tool("device_boot", json!({"id": id})).await);
    ok(&bridge.tool("device_screenshot", json!({"id": id})).await);

    // A pinch: two fingers down together, apart, and up together, around
    // the point (390 wide: a quarter of it is 97.5 pixels, 0.25).
    let before = phone.log().len();
    ok(&bridge
        .tool(
            "device_pinch",
            json!({"id": id, "x": 195, "y": 422, "scale": 2, "duration_ms": 100}),
        )
        .await);
    let pinch: Vec<(TouchPhase, Vec<TouchPoint>)> = phone.log()[before..]
        .iter()
        .map(|e| match e {
            FakeEvent::Input(DeviceInput::Touches { phase, touches }) => (*phase, touches.clone()),
            other => panic!("not two fingers: {other:?}"),
        })
        .collect();
    let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
    let (first, last) = (&pinch[0], pinch.last().unwrap());
    assert_eq!(first.0, TouchPhase::Down);
    assert!(
        near(first.1[0].x, 0.375) && near(first.1[1].x, 0.625),
        "{first:?}"
    );
    assert_eq!(last.0, TouchPhase::Up);
    assert!(
        near(last.1[0].x, 0.25) && near(last.1[1].x, 0.75),
        "{last:?}"
    );
    assert!(pinch.iter().flat_map(|p| &p.1).all(|t| near(t.y, 0.5)));
    assert!(
        pinch[1..pinch.len() - 1]
            .iter()
            .all(|p| p.0 == TouchPhase::Move)
    );
    failed(
        &bridge
            .tool(
                "device_pinch",
                json!({"id": id, "x": 1, "y": 1, "scale": 1}),
            )
            .await,
        "not 1",
    );

    // A gesture: each finger along its own path; one finger is a touch.
    let before = phone.log().len();
    ok(&bridge
        .tool(
            "device_gesture",
            json!({"id": id, "fingers": [[[0, 0], [390, 0]], [[0, 844]]], "duration_ms": 50}),
        )
        .await);
    let log = phone.log()[before..].to_vec();
    assert_eq!(
        log.last().unwrap(),
        &FakeEvent::Input(DeviceInput::Touches {
            phase: TouchPhase::Up,
            touches: vec![TouchPoint { x: 1.0, y: 0.0 }, TouchPoint { x: 0.0, y: 1.0 }],
        })
    );
    ok(&bridge
        .tool(
            "device_gesture",
            json!({"id": id, "fingers": [[[195, 844], [195, 422]]], "duration_ms": 50}),
        )
        .await);
    assert_eq!(
        *phone.log().last().unwrap(),
        FakeEvent::Input(DeviceInput::Touch {
            phase: TouchPhase::Up,
            x: 0.5,
            y: 0.5
        })
    );
    failed(
        &bridge
            .tool(
                "device_gesture",
                json!({"id": id, "fingers": [[[1, 2, 3]]]}),
            )
            .await,
        "Finger 1",
    );

    // Settings: changed together, then read back.
    let set = ok(&bridge
        .tool(
            "device_set",
            json!({"id": id, "appearance": "dark", "network": false, "text_size": "largest", "location": {"latitude": 48.85, "longitude": 2.29}}),
        )
        .await);
    assert_eq!(
        set["set"],
        json!(["appearance", "text_size", "network", "location"])
    );
    assert_eq!(set["settings"]["dark"], true);
    assert_eq!(set["settings"]["network"], false);
    assert_eq!(set["settings"]["text_size"], "largest");
    assert!(
        phone
            .log()
            .contains(&FakeEvent::Set(DeviceSetting::Location {
                latitude: 48.85,
                longitude: 2.29
            }))
    );
    let settings = ok(&bridge.tool("device_settings", json!({"id": id})).await);
    assert_eq!(settings["dark"], true);
    assert!(
        settings["supported"]
            .as_array()
            .unwrap()
            .contains(&json!("push"))
    );
    assert!(settings.get("fold").is_none());
    // Nothing is changed when one of them isn't understood.
    let before = phone.log().len();
    failed(
        &bridge
            .tool(
                "device_set",
                json!({"id": id, "appearance": "light", "volume": 3}),
            )
            .await,
        "Unknown setting `volume`",
    );
    failed(
        &bridge
            .tool(
                "device_set",
                json!({"id": id, "appearance": "light", "network": "maybe"}),
            )
            .await,
        "network is on or off",
    );
    assert_eq!(phone.log().len(), before);

    // Permissions and pushes.
    ok(&bridge
        .tool(
            "device_permission",
            json!({"id": id, "app": "com.example.fake", "permission": "CAMERA", "grant": true}),
        )
        .await);
    assert_eq!(
        *phone.log().last().unwrap(),
        FakeEvent::Set(DeviceSetting::Permission {
            app: "com.example.fake".into(),
            permission: "CAMERA".into(),
            grant: true
        })
    );
    ok(&bridge
        .tool(
            "device_push",
            json!({"id": id, "app": "com.example.fake", "title": "Hi", "body": "There"}),
        )
        .await);
    assert_eq!(
        *phone.log().last().unwrap(),
        FakeEvent::Set(DeviceSetting::Push {
            app: "com.example.fake".into(),
            payload: json!({"aps": {"alert": {"title": "Hi", "body": "There"}}})
        })
    );

    // A phone that doesn't fold says so; the foldable folds shut onto its
    // cover screen, and the list and its settings show how it is folded.
    failed(
        &bridge
            .tool("device_fold", json!({"id": id, "posture": "closed"}))
            .await,
        "doesn't fold",
    );
    let fid = fold.id().to_string();
    ok(&bridge.tool("device_boot", json!({"id": fid})).await);
    let folded = ok(&bridge
        .tool("device_fold", json!({"id": fid, "posture": "closed"}))
        .await);
    assert_eq!(folded["fold"]["posture"], "closed");
    assert_eq!(folded["fold"]["on_cover"], true);
    assert_eq!(folded["screen"], "360x780 pixels");
    assert_eq!(
        *fold.log().last().unwrap(),
        FakeEvent::Set(DeviceSetting::Posture {
            posture: DevicePosture::Closed
        })
    );
    let half = ok(&bridge
        .tool("device_fold", json!({"id": fid, "hinge": 90}))
        .await);
    assert_eq!(half["fold"]["posture"], "half_opened");
    let settings = ok(&bridge.tool("device_settings", json!({"id": fid})).await);
    assert_eq!(settings["fold"]["hinge"], 90.0);
    let listed = ok(&bridge.tool("device_list", json!({})).await);
    let listed_fold = listed["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == fid.as_str())
        .unwrap()
        .clone();
    assert_eq!(listed_fold["fold"]["posture"], "half_opened");
    failed(
        &bridge
            .tool(
                "device_fold",
                json!({"id": fid, "posture": "closed", "hinge": 3}),
            )
            .await,
        "exactly one",
    );

    bridge.stop().await;
    env.daemon.shutdown().await;
}

/// `agent-device`, the same tools as a command line in an agent's shell:
/// the real binary against a daemon with fake devices. Its screenshots set
/// what points are in for the thread's MCP tools too.
#[tokio::test]
async fn agent_device_runs_the_device_tools_from_a_shell() {
    use loft_core::devices::fake::{FakeBackend, FakeEvent};
    let phone = Arc::new(FakeBackend::new(
        DevicePlatform::Ios,
        Duration::from_millis(50),
    ));
    let fold = Arc::new(FakeBackend::foldable(Duration::from_millis(50)));
    let env = setup_with(vec![phone.clone(), fold.clone()]).await;
    let thread = fake_thread(&env).await;
    let home = env.paths.home.clone();
    let thread_id = thread.id.clone();
    let cli = move |args: &[&str]| {
        let (home, thread_id) = (home.clone(), thread_id.clone());
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        async move {
            let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_agentloftd"))
                .arg("--home")
                .arg(&home)
                .arg("device")
                .args(&args)
                .env(loft_core::THREAD_ENV, thread_id.as_str())
                .output()
                .await
                .unwrap();
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }
    };
    let json_of = |(code, stdout, stderr): (Option<i32>, String, String)| {
        assert_eq!(code, Some(0), "{stdout}{stderr}");
        serde_json::from_str::<serde_json::Value>(&stdout)
            .unwrap_or_else(|e| panic!("{e}: {stdout}"))
    };
    let id = phone.id().to_string();

    let (code, help, _) = cli(&["help"]).await;
    assert_eq!(code, Some(0));
    assert!(
        help.contains("Usage: agent-device") && help.contains("Coordinates"),
        "{help}"
    );

    let (code, list, _) = cli(&["list"]).await;
    assert_eq!(code, Some(0));
    assert!(
        list.contains(&format!("{id}  Fake Phone  iOS 0.0  stopped")),
        "{list}"
    );
    let listed = json_of(cli(&["list", "--json"]).await);
    assert_eq!(listed["devices"].as_array().unwrap().len(), 2);

    // Booting tells the thread's windows its agent uses the device.
    let booted = json_of(cli(&["--json", "boot", &id]).await);
    assert_eq!(booted["state"], "running");
    assert_eq!(booted["used_by"], "this thread");

    // A screenshot is a PNG where it was asked for, of the size it says.
    let out = env.paths.home.join("shots/phone.png");
    let (code, said, err) = cli(&["screenshot", &id, "--out", &out.to_string_lossy()]).await;
    assert_eq!(code, Some(0), "{err}");
    assert!(said.starts_with(&*out.to_string_lossy()), "{said}");
    assert!(said.contains("390x844 pixels"), "{said}");
    let png = std::fs::read(&out).unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    let picture = image::load_from_memory(&png).unwrap();
    assert_eq!((picture.width(), picture.height()), (390, 844));
    let shot = json_of(cli(&["screenshot", &id, "--json"]).await);
    assert!(std::path::Path::new(shot["path"].as_str().unwrap()).is_file());
    assert_eq!(shot["width"], 390);
    std::fs::remove_file(shot["path"].as_str().unwrap()).unwrap();

    // Taps are points of that screenshot.
    let (code, said, _) = cli(&["tap", &id, "195", "211"]).await;
    assert_eq!(
        (code, said.trim()),
        (Some(0), "Done. Take a new screenshot to see the result.")
    );
    let touch = |phase, x, y| FakeEvent::Input(DeviceInput::Touch { phase, x, y });
    assert_eq!(
        phone.log()[phone.log().len() - 2..],
        [
            touch(TouchPhase::Down, 0.5, 0.25),
            touch(TouchPhase::Up, 0.5, 0.25)
        ]
    );
    let done = json_of(cli(&["--json", "press", &id, "home"]).await);
    assert_eq!(done["ok"], true);
    assert_eq!(
        *phone.log().last().unwrap(),
        FakeEvent::Input(DeviceInput::Button {
            button: DeviceButton::Home
        })
    );
    ok_typed(&cli(&["type", &id, "hello", "world"]).await);
    assert_eq!(
        *phone.log().last().unwrap(),
        FakeEvent::Input(DeviceInput::Text {
            text: "hello world".into()
        })
    );
    ok_typed(&cli(&["pinch", &id, "195", "422", "0.5", "--ms", "60"]).await);
    assert!(matches!(
        phone.log().last().unwrap(),
        FakeEvent::Input(DeviceInput::Touches {
            phase: TouchPhase::Up,
            ..
        })
    ));

    // Mistakes: the status says so, and --json says why.
    let (code, _, err) = cli(&["tap", &id, "391", "0"]).await;
    assert_eq!(code, Some(1));
    assert!(
        err.contains("outside the screenshot, which is 390x844"),
        "{err}"
    );
    let (code, out, _) = cli(&["--json", "tap", "ios:NOPE", "1", "1"]).await;
    assert_eq!(code, Some(1));
    assert!(serde_json::from_str::<serde_json::Value>(&out).unwrap()["error"].is_string());
    let (code, _, err) = cli(&["tap", &id]).await;
    assert_eq!(code, Some(2), "{err}");

    // Settings, as words.
    let set = json_of(
        cli(&[
            "set",
            &id,
            "appearance",
            "dark",
            "reduce-motion",
            "on",
            "--json",
        ])
        .await,
    );
    assert_eq!(set["settings"]["dark"], true);
    assert_eq!(set["settings"]["reduce_motion"], true);
    let settings = json_of(cli(&["settings", &id, "--json"]).await);
    assert_eq!(settings["dark"], true);
    let (code, said, _) = cli(&["settings", &id]).await;
    assert_eq!(code, Some(0));
    assert!(said.contains("dark: true"), "{said}");
    ok_typed(&cli(&["permission", &id, "com.example.fake", "photos", "revoke"]).await);
    assert_eq!(
        *phone.log().last().unwrap(),
        FakeEvent::Set(DeviceSetting::Permission {
            app: "com.example.fake".into(),
            permission: "photos".into(),
            grant: false
        })
    );
    ok_typed(
        &cli(&[
            "push",
            &id,
            "com.example.fake",
            "--payload",
            r#"{"aps":{"badge":3}}"#,
        ])
        .await,
    );
    assert_eq!(
        *phone.log().last().unwrap(),
        FakeEvent::Set(DeviceSetting::Push {
            app: "com.example.fake".into(),
            payload: serde_json::json!({"aps": {"badge": 3}})
        })
    );

    // Folding.
    let fid = fold.id().to_string();
    json_of(cli(&["boot", &fid, "--json"]).await);
    let folded = json_of(cli(&["fold", &fid, "half-opened", "--json"]).await);
    assert_eq!(folded["fold"]["posture"], "half_opened");
    let folded = json_of(cli(&["fold", &fid, "--hinge", "0", "--json"]).await);
    assert_eq!(folded["fold"]["posture"], "closed");
    assert_eq!(
        *fold.log().last().unwrap(),
        FakeEvent::Set(DeviceSetting::Hinge { degrees: 0.0 })
    );

    // The thread's MCP tools take points in the CLI's screenshot's pixels.
    let mut bridge = Bridge::start(&env.paths, &thread.id).await;
    let tapped = bridge
        .tool(
            "device_tap",
            serde_json::json!({"id": id, "x": 390, "y": 844}),
        )
        .await;
    assert_eq!(tapped["isError"], false, "{tapped}");
    assert_eq!(
        *phone.log().last().unwrap(),
        touch(TouchPhase::Up, 1.0, 1.0)
    );
    bridge.stop().await;

    ok_typed(&cli(&["shutdown", &id]).await);
    assert_eq!(*phone.log().last().unwrap(), FakeEvent::Shutdown);
    env.daemon.shutdown().await;
}

/// A command that worked.
fn ok_typed((code, out, err): &(Option<i32>, String, String)) {
    assert_eq!(*code, Some(0), "{out}{err}");
}

/// The whole path with a real browser: an agent's tools (through the MCP
/// bridge) drive the thread's page of the daemon's browser, and a client
/// that has the thread open is told so. Needs Chrome (or Chromium, Edge,
/// Brave) installed:
/// `cargo test -p loftd --test workbench live -- --ignored`.
#[tokio::test]
#[ignore = "starts a real browser"]
async fn live_agents_drive_the_daemons_browser() {
    let mut env = setup().await;
    let thread = fake_thread(&env).await;
    env.client.subscribe(Topic::Thread(thread.id.clone()));
    let page = env.repo.join("page.html");
    std::fs::write(
        &page,
        "<!doctype html><title>Built</title><h1>Report</h1>\
         <input placeholder=\"Name\" oninput=\"document.title = 'typed:' + this.value\">\
         <button onclick=\"document.body.dataset.clicked = event.isTrusted\">Done</button>",
    )
    .unwrap();

    let mut bridge = Bridge::start(&env.paths, &thread.id).await;
    // An agent opens a page it built: a local file.
    let nav = bridge
        .tool(
            "browser_navigate",
            serde_json::json!({"url": page.to_string_lossy()}),
        )
        .await;
    assert_eq!(nav["isError"], false, "{nav}");
    assert!(
        nav["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"title\": \"Built\""),
        "{nav}"
    );
    // The window that has the thread open hears that an agent is at work.
    let driven = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(ClientEvent::PageDriven { thread_id, page }) = env.rx.recv().await {
                return (thread_id, page);
            }
        }
    })
    .await
    .expect("a PageDriven event");
    assert_eq!(driven.0, thread.id);
    assert_eq!(driven.1.thread_id, thread.id);

    let snap = bridge.tool("browser_snapshot", serde_json::json!({})).await;
    let text = snap["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(text.contains("# Report"), "{text}");
    let ref_of = |what: &str| -> u32 {
        text.split("[ref=")
            .skip(1)
            .find_map(|rest| {
                let (id, element) = rest.split_once("] ")?;
                element.starts_with(what).then(|| id.parse().ok())?
            })
            .unwrap_or_else(|| panic!("no {what} in {text}"))
    };
    let typed = bridge
        .tool(
            "browser_type",
            serde_json::json!({"ref": ref_of("input"), "text": "Ada"}),
        )
        .await;
    assert!(
        typed["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("typed:Ada"),
        "{typed}"
    );
    let clicked = bridge
        .tool(
            "browser_click",
            serde_json::json!({"ref": ref_of("button \"Done\"")}),
        )
        .await;
    assert_eq!(clicked["isError"], false, "{clicked}");
    // A real click, as a person's would be.
    let eval = bridge
        .tool(
            "browser_eval",
            serde_json::json!({"script": "document.body.dataset.clicked"}),
        )
        .await;
    assert_eq!(eval["content"][0]["text"], "true", "{eval}");
    let shot = bridge
        .tool("browser_screenshot", serde_json::json!({}))
        .await;
    assert_eq!(shot["content"][0]["type"], "image", "{shot}");
    assert_eq!(shot["content"][0]["mimeType"], "image/png");
    // A plain screenshot is the picture, then only the tab it was taken in.
    assert_eq!(shot["content"].as_array().unwrap().len(), 2, "{shot}");
    assert!(
        shot["content"][1]["text"]
            .as_str()
            .unwrap()
            .starts_with("tab: "),
        "{shot}"
    );

    // Capture (B): the capture tools, the same way.
    let element = bridge
        .tool(
            "browser_screenshot",
            serde_json::json!({"ref": ref_of("button \"Done\""), "scale": "css", "format": "jpeg"}),
        )
        .await;
    assert_eq!(element["content"][0]["mimeType"], "image/jpeg", "{element}");
    assert!(
        element["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"format\": \"jpeg\""),
        "{element}"
    );
    let labelled = bridge
        .tool("browser_screenshot", serde_json::json!({"labels": true}))
        .await;
    assert!(
        labelled["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"labels\""),
        "{labelled}"
    );
    let same = bridge
        .tool("browser_screenshot_diff", serde_json::json!({}))
        .await;
    assert!(
        same["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"matches\": true"),
        "{same}"
    );
    let console = bridge
        .tool("browser_console", serde_json::json!({"action": "start"}))
        .await;
    assert_eq!(console["isError"], false, "{console}");
    bridge
        .tool("browser_eval", serde_json::json!({"script": "console.error('from the page'); document.body.style.background = 'red'"}))
        .await;
    let read = bridge
        .tool("browser_console", serde_json::json!({"action": "read"}))
        .await;
    assert!(
        read["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("from the page"),
        "{read}"
    );
    bridge
        .tool("browser_console", serde_json::json!({"action": "stop"}))
        .await;
    let changed = bridge
        .tool("browser_screenshot_diff", serde_json::json!({}))
        .await;
    assert_eq!(changed["content"][0]["type"], "image", "{changed}");
    assert!(
        changed["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"matches\": false"),
        "{changed}"
    );
    let network = bridge
        .tool("browser_network", serde_json::json!({"action": "start"}))
        .await;
    assert_eq!(network["isError"], false, "{network}");
    let stopped = bridge
        .tool("browser_network", serde_json::json!({"action": "stop"}))
        .await;
    assert!(
        stopped["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"exchanges\""),
        "{stopped}"
    );
    let emulated = bridge
        .tool(
            "browser_emulate",
            serde_json::json!({"timezone": "Asia/Tokyo", "media": "print"}),
        )
        .await;
    assert!(
        emulated["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Asia/Tokyo"),
        "{emulated}"
    );
    let zone = bridge
        .tool(
            "browser_eval",
            serde_json::json!({"script": "Intl.DateTimeFormat().resolvedOptions().timeZone"}),
        )
        .await;
    assert_eq!(zone["content"][0]["text"], "Asia/Tokyo", "{zone}");
    bridge
        .tool("browser_emulate", serde_json::json!({"reset": true}))
        .await;
    let recording = bridge
        .tool(
            "browser_recording",
            serde_json::json!({"action": "start", "name": "bridge.mkv"}),
        )
        .await;
    assert_eq!(recording["isError"], false, "{recording}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let recorded = bridge
        .tool("browser_recording", serde_json::json!({"action": "stop"}))
        .await;
    let recorded: serde_json::Value =
        serde_json::from_str(recorded["content"][0]["text"].as_str().unwrap()).unwrap();
    let file = std::path::PathBuf::from(recorded["path"].as_str().unwrap());
    assert!(file.ends_with("bridge.mkv") && file.exists(), "{recorded}");
    let _ = std::fs::remove_dir_all(file.parent().unwrap());

    // The same page is the thread's for viewers too.
    let Response::Page { page: opened } = env
        .client
        .request(Request::PageOpen {
            thread_id: thread.id.clone(),
            url: None,
            width: 800,
            height: 600,
            scale: 1.0,
            tab: None,
            new_tab: false,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(opened.id, driven.1.id);
    assert!(opened.url.ends_with("page.html"), "{opened:?}");

    bridge.stop().await;
    env.daemon.shutdown().await;
}

// Scripts (D)

/// `browser_execute` goes to the daemon and is checked there: an empty
/// script is refused, and with no browser to run the page on the agent is
/// told so, as for the plain tools.
#[tokio::test]
async fn scripts_reach_the_daemon_through_the_mcp_bridge() {
    let env = setup().await;
    let missing = env.paths.home.join("no-such-chrome");
    std::fs::write(
        env.paths.config_file(),
        format!("[browser]\nexecutable = {:?}\n", missing.to_string_lossy()),
    )
    .unwrap();
    env.client.request(Request::ReloadConfig).await.unwrap();
    let thread = fake_thread(&env).await;
    let mut bridge = Bridge::start(&env.paths, &thread.id).await;
    let empty = bridge
        .tool("browser_execute", serde_json::json!({ "script": " " }))
        .await;
    assert_eq!(empty["isError"], true, "{empty}");
    let blind = bridge
        .tool(
            "browser_execute",
            serde_json::json!({ "script": "await page.goto('https://example.com')" }),
        )
        .await;
    assert_eq!(blind["isError"], true, "{blind}");
    assert!(
        blind["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("No browser at"),
        "{blind}"
    );
    let elsewhere = bridge
        .tool(
            "browser_execute",
            serde_json::json!({ "script": "1", "tab": "not-a-tab" }),
        )
        .await;
    assert!(
        elsewhere["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("has no browser tab `not-a-tab`"),
        "{elsewhere}"
    );
    bridge.stop().await;
    env.daemon.shutdown().await;
}

/// A script drives the daemon's browser in several steps in one call:
/// navigates, reads the outline, fills and clicks through locators, follows
/// a link, keeps `state` with the tab for the next run, and returns a
/// screenshot as an image. Needs Chrome (or Chromium, Edge, Brave):
/// `cargo test -p loftd --test workbench live -- --ignored`.
#[tokio::test]
#[ignore = "starts a real browser"]
async fn live_a_script_drives_the_daemons_browser() {
    use tokio::io::AsyncReadExt as _;
    const FORM: &str = "<!doctype html><meta charset=utf-8><title>Form</title>\
        <h1>Sign up</h1>\
        <input placeholder=\"Your name\" oninput=\"document.title = 'typed:' + this.value\">\
        <button onclick=\"document.body.dataset.clicked = event.isTrusted\">Go</button>\
        <ul><li>One</li><li>Two</li><li style=\"display:none\">Hidden</li></ul>\
        <a href=\"/next\">Next page</a>";
    const NEXT: &str = "<!doctype html><meta charset=utf-8><title>Next</title><h1>Second</h1>";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        // Each connection in its own task: Chrome opens spare ones.
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 2048];
                let n = socket.read(&mut request).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&request[..n]);
                let body = if head.starts_with("GET /next") {
                    NEXT
                } else {
                    FORM
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    let env = setup().await;
    let thread = fake_thread(&env).await;
    let mut bridge = Bridge::start(&env.paths, &thread.id).await;

    let script = format!(
        r#"
        await page.goto({base:?})
        const outline = await snapshot()
        await page.getByPlaceholder('Your name').fill('Ada')
        const typed = await page.title()
        await page.getByRole('button', {{ name: 'Go' }}).click()
        const trusted = await page.evaluate(() => document.body.dataset.clicked)
        const items = await page.locator('li').allTextContents()
        const visible = await page.getByRole('listitem').count()
        let strict
        try {{ await page.locator('li').click({{ timeout: 500 }}) }} catch (e) {{ strict = e.message }}
        await page.getByText('Next page').click()
        await page.waitForURL('**/next')
        state.visits = (state.visits || 0) + 1
        console.log('now at', page.url())
        return {{ outline: outline.includes('# Sign up'), typed, trusted, items, visible, strict, heading: await page.textContent('h1'), shot: await page.screenshot() }}
        "#
    );
    let first = bridge
        .tool("browser_execute", serde_json::json!({ "script": script }))
        .await;
    assert_eq!(first["isError"], false, "{first}");
    let text = first["content"][0]["text"].as_str().unwrap();
    let tab = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("tab: "))
        .unwrap_or_else(|| panic!("no tab in {text}"))
        .to_owned();
    let value: serde_json::Value =
        serde_json::from_str(text.split("\n\n").nth(1).unwrap()).unwrap();
    assert_eq!(value["outline"], true, "{text}");
    assert_eq!(value["typed"], "typed:Ada", "{text}");
    // Clicks are real input.
    assert_eq!(value["trusted"], "true", "{text}");
    assert_eq!(
        value["items"],
        serde_json::json!(["One", "Two", "Hidden"]),
        "{text}"
    );
    // Roles leave hidden elements out, as a person would.
    assert_eq!(value["visible"], 2, "{text}");
    assert!(
        value["strict"]
            .as_str()
            .is_some_and(|s| s.contains("matched 3 elements")),
        "{text}"
    );
    assert_eq!(value["heading"], "Second", "{text}");
    assert_eq!(value["shot"]["type"], "image", "{text}");
    assert!(text.contains("Page: moved from"), "{text}");
    assert!(text.contains("Console:\nnow at"), "{text}");
    // The screenshot the script returned comes back as an image.
    assert_eq!(first["content"][1]["type"], "image", "{first}");
    assert_eq!(first["content"][1]["mimeType"], "image/png");

    // The next run in the same tab has its state, and the page as left.
    let second = bridge
        .tool(
            "browser_execute",
            serde_json::json!({ "script": "return { visits: state.visits, url: page.url() }", "tab": tab }),
        )
        .await;
    let text = second["content"][0]["text"].as_str().unwrap();
    assert_eq!(second["isError"], false, "{second}");
    assert!(text.starts_with(&format!("tab: {tab}\n\n")), "{text}");
    assert!(
        text.contains(r#""visits":1"#) && text.contains("/next"),
        "{text}"
    );

    // A failing script is a result, with what happened.
    let failed = bridge
        .tool(
            "browser_execute",
            serde_json::json!({ "script": "await page.click('#nothing', { timeout: 300 })" }),
        )
        .await;
    assert_eq!(failed["isError"], true, "{failed}");
    assert!(
        failed["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("TimeoutError"),
        "{failed}"
    );

    bridge.stop().await;
    env.daemon.shutdown().await;
}

/// A script types, inserts and pastes into a rich editor that redraws
/// itself after every change (`loft-core`'s `pages/testdata/editor.html`):
/// paragraphs typed with `keyboard.type` and Enter all arrive whole, and
/// pastes become the editor's own paragraphs. Needs Chrome:
/// `cargo test -p loftd --test workbench live -- --ignored`.
#[tokio::test]
#[ignore = "starts a real browser"]
async fn live_a_script_types_and_pastes_into_an_editor() {
    use tokio::io::AsyncReadExt as _;
    const EDITOR: &str = include_str!("../../loft-core/src/pages/testdata/editor.html");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{EDITOR}",
                    EDITOR.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    let env = setup().await;
    let thread = fake_thread(&env).await;
    let mut bridge = Bridge::start(&env.paths, &thread.id).await;

    let script = format!(
        r#"
        await page.goto({base:?})
        await page.locator('#ed').click()
        const paras = []
        for (let n = 1; n <= 5; n++) {{
          const words = Array.from({{ length: 20 }}, (_, i) => `w${{n}}x${{i}}`).join(' ')
          paras.push(`Paragraph ${{n}}: ${{words}} isn’t it — every turn.`)
        }}
        for (const p of paras) {{
          await page.keyboard.type(p)
          await page.keyboard.press('Enter')
        }}
        const typed = await page.evaluate(() => window.paragraphs())
        await page.fill('#ed', '')
        await page.keyboard.insertText('Héllo\nwörld')
        const inserted = await page.evaluate(() => window.paragraphs())
        await page.fill('#ed', '')
        const pasted = await page.locator('#ed').paste('One\nTwo')
        await page.keyboard.paste({{ text: 'Three', html: '<p>Three</p><p>Four</p>' }})
        await page.paste('#notes', 'a\nb')
        return {{
          paras, typed, inserted, pasted,
          after: await page.evaluate(() => window.paragraphs()),
          notes: await page.inputValue('#notes'),
          events: await page.evaluate(() => window.pastes.map((p) => [p.trusted, p.html])),
        }}
        "#
    );
    let result = bridge
        .tool(
            "browser_execute",
            serde_json::json!({ "script": script, "timeout_ms": 120000 }),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.split("\n\n").nth(1).unwrap()).unwrap();
    // Every character of every paragraph, and the empty one Enter left.
    let mut want = value["paras"].as_array().unwrap().clone();
    want.push(serde_json::json!(""));
    assert_eq!(value["typed"], serde_json::Value::Array(want), "{text}");
    assert_eq!(
        value["inserted"],
        serde_json::json!(["Héllo", "wörld"]),
        "{text}"
    );
    assert_eq!(value["pasted"]["pasted"], 7, "{text}");
    assert_eq!(
        value["after"],
        serde_json::json!(["One", "TwoThree", "Four"]),
        "{text}"
    );
    assert_eq!(value["notes"], "a\nb", "{text}");
    assert_eq!(
        value["events"],
        serde_json::json!([[true, false], [true, true]]),
        "{text}"
    );

    bridge.stop().await;
    env.daemon.shutdown().await;
}

/// The last text of a tool's result: the tab it ran in.
fn tab_line(result: &serde_json::Value) -> String {
    result["content"]
        .as_array()
        .and_then(|c| c.last())
        .and_then(|c| c["text"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// An agent's tabs through the MCP bridge (ADR-0017 §9): a tab of its own
/// by name, which every window hears of; results that say which tab they
/// ran in; and a tab handed to the person, which the thread waits on until
/// they press Continue from a window. Needs Chrome:
/// `cargo test -p loftd --test workbench live -- --ignored`.
#[tokio::test]
#[ignore = "starts a real browser"]
async fn live_agents_name_their_tabs_and_hand_them_over() {
    let mut env = setup().await;
    let thread = fake_thread(&env).await;
    env.client.subscribe(Topic::Thread(thread.id.clone()));
    env.client.subscribe(Topic::Workspace);
    let page = env.repo.join("docs.html");
    std::fs::write(&page, "<!doctype html><title>Docs</title><h1>Docs</h1>").unwrap();
    let mut bridge = Bridge::start(&env.paths, &thread.id).await;

    let opened = bridge
        .tool(
            "browser_tabs",
            serde_json::json!({"action": "new", "name": "docs", "url": page.to_string_lossy()}),
        )
        .await;
    assert_eq!(opened["isError"], false, "{opened}");
    assert!(tab_line(&opened).starts_with("tab: docs"), "{opened}");
    // Asked again: the same tab, as it is.
    let again = bridge
        .tool(
            "browser_tabs",
            serde_json::json!({"action": "new", "name": "docs"}),
        )
        .await;
    assert!(
        again["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"existed\": true"),
        "{again}"
    );
    let snap = bridge
        .tool("browser_snapshot", serde_json::json!({"tab": "docs"}))
        .await;
    assert!(
        snap["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("# Docs"),
        "{snap}"
    );
    assert!(tab_line(&snap).starts_with("tab: docs"), "{snap}");
    let missing = bridge
        .tool("browser_snapshot", serde_json::json!({"tab": "nope"}))
        .await;
    assert_eq!(missing["isError"], true, "{missing}");

    // Every window showing the thread hears of the agent's tab.
    let docs = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(ClientEvent::PageTabs { pages, .. }) = env.rx.recv().await
                && let Some(docs) = pages
                    .into_iter()
                    .find(|p| p.agent.name.as_deref() == Some("docs"))
            {
                return docs;
            }
        }
    })
    .await
    .expect("the docs tab");
    assert!(docs.agent.opened);

    // Handed to the person: the call waits, the thread waits for them.
    let waiting = bridge
        .tool(
            "browser_handoff",
            serde_json::json!({"tab": "docs", "reason": "Sign in", "timeout_seconds": 1}),
        )
        .await;
    assert!(
        waiting["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"outcome\": \"waiting\""),
        "{waiting}"
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(ClientEvent::Event { event, .. }) = env.rx.recv().await
                && let Event::ThreadUpdated { thread: t } = event.event
                && t.id == thread.id
                && t.status == ThreadStatus::WaitingApproval
                && t.handover.as_deref() == Some("Sign in")
            {
                return;
            }
        }
    })
    .await
    .expect("the thread waits for its person");
    // The person presses Continue while the agent asks again.
    let client = env.client.clone();
    let id = docs.id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        client
            .request(Request::PageHandoffDone { id })
            .await
            .unwrap();
    });
    let done = bridge
        .tool(
            "browser_handoff",
            serde_json::json!({"tab": "docs", "reason": "Sign in", "timeout_seconds": 20}),
        )
        .await;
    let text = done["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"outcome\": \"continued\""), "{done}");
    assert!(text.contains("\"title\": \"Docs\""), "{done}");
    assert_eq!(done["isError"], false);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(ClientEvent::Event { event, .. }) = env.rx.recv().await
                && let Event::ThreadUpdated { thread: t } = event.event
                && t.id == thread.id
                && t.handover.is_none()
                && t.status == ThreadStatus::Idle
            {
                return;
            }
        }
    })
    .await
    .expect("the thread no longer waits");

    let status = bridge.tool("browser_status", serde_json::json!({})).await;
    let text = status["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"running\": true"), "{status}");
    let closed = bridge
        .tool(
            "browser_tabs",
            serde_json::json!({"action": "close", "all": true}),
        )
        .await;
    assert!(
        closed["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("docs"),
        "{closed}"
    );

    bridge.stop().await;
    env.daemon.shutdown().await;
}
