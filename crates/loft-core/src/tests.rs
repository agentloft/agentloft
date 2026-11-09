use super::*;
use loft_harness::fake::FakeHarness;
use loft_protocol::{DecisionKind, ItemBody, RequestStatus, Snapshot, ThreadWorkspace};
use std::time::Duration;

struct Fixture {
    core: Arc<Core>,
    _dir: tempfile::TempDir,
    db: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite");
    let core = Core::new(
        Store::open(&db).unwrap(),
        vec![Arc::new(FakeHarness)],
        CoreOptions {
            flush_interval: Duration::from_millis(20),
            ..Default::default()
        },
    );
    Fixture {
        core,
        _dir: dir,
        db,
    }
}

async fn new_thread(core: &Arc<Core>, dir: &std::path::Path) -> Thread {
    let Response::Project { project } = core
        .handle(Request::AddProject {
            path: dir.to_string_lossy().into_owned(),
            name: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let Response::Thread { thread } = core
        .handle(Request::CreateThread {
            project_id: project.id,
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

fn thread_snapshot(
    core: &Core,
    id: &ThreadId,
) -> (
    Thread,
    Vec<loft_protocol::Turn>,
    Vec<Item>,
    Vec<loft_protocol::RuntimeRequest>,
) {
    match core.snapshot(&Topic::Thread(id.clone())).unwrap() {
        Snapshot::Thread {
            thread,
            turns,
            items,
            requests,
            ..
        } => (thread, turns, items, requests),
        _ => panic!(),
    }
}

async fn wait_for(
    core: &Core,
    id: &ThreadId,
    pred: impl Fn(&Thread, &[loft_protocol::Turn]) -> bool,
) {
    for _ in 0..300 {
        let (thread, turns, _, _) = thread_snapshot(core, id);
        if pred(&thread, &turns) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

fn turn_done(_: &Thread, turns: &[loft_protocol::Turn]) -> bool {
    turns.last().is_some_and(|t| t.status.is_terminal())
}

#[tokio::test]
async fn message_streams_and_completes() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    let mut bus = f.core.subscribe();

    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "hello world".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;

    let (thread_now, turns, items, _) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(thread_now.title, "hello world");
    assert_eq!(thread_now.status, ThreadStatus::Idle);
    assert!(thread_now.native_session_id.is_some());
    assert_eq!(turns[0].status, TurnStatus::Completed);
    assert!(matches!(&items[0].body, ItemBody::UserMessage { text, .. } if text == "hello world"));
    assert!(
        matches!(&items[1].body, ItemBody::AssistantMessage { text } if text == "echo: hello world"),
        "{:?}",
        items[1]
    );
    assert_eq!(items[1].status, ItemStatus::Completed);

    // Deltas were broadcast with contiguous offsets.
    let mut rebuilt = String::new();
    while let Ok(msg) = bus.try_recv() {
        if let BusMsg::Delta(d) = msg {
            assert_eq!(d.offset, rebuilt.len());
            rebuilt.push_str(&d.text);
        }
    }
    assert_eq!(rebuilt, "echo: hello world");
}

#[tokio::test]
async fn second_turn_rejected_while_running_then_interrupt() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "slow one two three four five six".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    let err = f
        .core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "again".into(),
            attachments: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    tokio::time::sleep(Duration::from_millis(500)).await;
    // Mid-stream, the snapshot overlays live text.
    let (_, _, items, _) = thread_snapshot(&f.core, &thread.id);
    assert!(matches!(&items[1].body, ItemBody::AssistantMessage { text } if !text.is_empty()));

    f.core
        .handle(Request::Interrupt {
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    let (_, turns, _, _) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(turns[0].status, TurnStatus::Interrupted);
}

#[tokio::test]
async fn approval_allow_flow() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "please approve this".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, |t, _| {
        t.status == ThreadStatus::WaitingApproval
    })
    .await;
    let (_, _, items, requests) = thread_snapshot(&f.core, &thread.id);
    let request = &requests[0];
    assert_eq!(request.status, RequestStatus::Open);
    assert!(request.item_id.is_some(), "request linked to its tool item");
    assert!(
        items
            .iter()
            .any(|i| Some(&i.id) == request.item_id.as_ref())
    );

    f.core
        .handle(Request::RespondRequest {
            thread_id: thread.id.clone(),
            request_id: request.id.clone(),
            decision: DecisionKind::AllowOnce,
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    let (thread_now, turns, items, requests) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(turns[0].status, TurnStatus::Completed);
    assert_eq!(thread_now.status, ThreadStatus::Idle);
    assert_eq!(requests[0].status, RequestStatus::Resolved);
    assert_eq!(requests[0].decision, Some(DecisionKind::AllowOnce));
    let tool = items
        .iter()
        .find(|i| matches!(i.body, ItemBody::ToolCall { .. }))
        .unwrap();
    assert_eq!(tool.status, ItemStatus::Completed);
}

#[tokio::test]
async fn deny_and_stop_interrupts() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "approve please".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, |t, _| {
        t.status == ThreadStatus::WaitingApproval
    })
    .await;
    let (_, _, _, requests) = thread_snapshot(&f.core, &thread.id);
    f.core
        .handle(Request::RespondRequest {
            thread_id: thread.id.clone(),
            request_id: requests[0].id.clone(),
            decision: DecisionKind::DenyAndStop,
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    let (_, turns, items, _) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(turns[0].status, TurnStatus::Interrupted);
    let tool = items
        .iter()
        .find(|i| matches!(i.body, ItemBody::ToolCall { .. }))
        .unwrap();
    assert_eq!(tool.status, ItemStatus::Declined);
}

#[tokio::test]
async fn failed_turn_records_error_item() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "please fail".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    let (_, turns, items, _) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(turns[0].status, TurnStatus::Failed);
    assert!(
        items
            .iter()
            .any(|i| matches!(i.body, ItemBody::Error { .. }))
    );
}

#[tokio::test]
async fn recovery_after_crash_interrupts_running_turns() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "slow a b c d e f g h".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Simulate a crash: open a second core on the same database.
    let core2 = Core::new(
        Store::open(&f.db).unwrap(),
        vec![Arc::new(FakeHarness)],
        CoreOptions::default(),
    );
    assert_eq!(core2.recover().unwrap(), 1);
    let (thread_now, turns, _, _) = thread_snapshot(&core2, &thread.id);
    assert_eq!(turns[0].status, TurnStatus::Interrupted);
    assert_eq!(thread_now.status, ThreadStatus::Idle);

    // The next message resumes the same native session.
    let session_before = thread_now.native_session_id.clone();
    core2
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "after crash".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&core2, &thread.id, |_, turns| {
        turns.len() == 2 && turns[1].status.is_terminal()
    })
    .await;
    let (thread_after, _, _, _) = thread_snapshot(&core2, &thread.id);
    assert_eq!(thread_after.native_session_id, session_before);
}

#[tokio::test]
async fn unknown_harness_and_missing_thread_are_errors() {
    let f = fixture();
    let err = f
        .core
        .handle(Request::Interrupt {
            thread_id: ThreadId::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    let err = f
        .core
        .handle(Request::AddProject {
            path: "/definitely/not/here".into(),
            name: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test]
async fn worktree_thread_created_on_first_message_merged_and_archived() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let worktrees = dir.path().join("worktrees");
    let core = Core::new(
        Store::open(&dir.path().join("db.sqlite")).unwrap(),
        vec![Arc::new(FakeHarness)],
        CoreOptions {
            flush_interval: Duration::from_millis(20),
            worktrees_dir: worktrees.clone(),
            ..Default::default()
        },
    );
    let thread = new_thread(&core, &repo).await;
    assert_eq!(
        thread.workspace,
        ThreadWorkspace::Local,
        "mode `ask` starts local"
    );

    let Response::Thread { thread } = core
        .handle(Request::SetThreadWorkspace {
            thread_id: thread.id.clone(),
            worktree: true,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(thread.workspace, ThreadWorkspace::NewWorktree);

    core.handle(Request::SendMessage {
        thread_id: thread.id.clone(),
        text: "Add a greet() function!".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    wait_for(&core, &thread.id, turn_done).await;
    let (thread, turns, _, _) = thread_snapshot(&core, &thread.id);
    let ThreadWorkspace::Worktree { path, branch, base } = thread.workspace.clone() else {
        panic!("{:?}", thread.workspace)
    };
    assert_eq!(branch, "agentloft/add-a-greet-function");
    assert_eq!(base, "main");
    assert_eq!(thread.cwd, path);
    let wt = PathBuf::from(&path);
    assert!(wt.starts_with(worktrees.join("repo")), "{path}");
    assert_eq!(git(&wt, &["branch", "--show-current"]), branch);
    assert!(
        turns[0].checkpoint_before.is_some(),
        "checkpoints in the worktree"
    );

    // The workspace is fixed once the thread has started.
    assert!(
        core.handle(Request::SetThreadWorkspace {
            thread_id: thread.id.clone(),
            worktree: false,
        })
        .await
        .is_err()
    );

    // The agent's (uncommitted) edits merge back into main as one commit.
    std::fs::write(wt.join("greet.py"), "def greet(): ...\n").unwrap();
    let Response::Text { text } = core
        .handle(Request::MergeThread {
            thread_id: thread.id.clone(),
            message: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(text.starts_with("Merged 1 file into main"), "{text}");
    assert!(repo.join("greet.py").exists());
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Add a greet() function!"
    );

    // Archiving refuses a dirty worktree unless forced, then removes it.
    std::fs::write(wt.join("wip.txt"), "x").unwrap();
    let err = core
        .handle(Request::ArchiveThread {
            thread_id: thread.id.clone(),
            force: false,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    let Response::Thread { thread } = core
        .handle(Request::ArchiveThread {
            thread_id: thread.id.clone(),
            force: true,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(thread.status, ThreadStatus::Archived);
    assert!(!wt.exists());
    assert_eq!(
        git(&repo, &["branch", "--list", &branch]).trim_start_matches("* "),
        branch,
        "branch kept"
    );
}

#[tokio::test]
async fn fork_gets_own_worktree_and_handoff_draft() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let core = Core::new(
        Store::open(&dir.path().join("db.sqlite")).unwrap(),
        vec![Arc::new(FakeHarness)],
        CoreOptions {
            flush_interval: Duration::from_millis(20),
            worktrees_dir: dir.path().join("worktrees"),
            ..Default::default()
        },
    );
    let parent = new_thread(&core, &repo).await;
    core.handle(Request::SendMessage {
        thread_id: parent.id.clone(),
        text: "Make the greeting friendlier".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    wait_for(&core, &parent.id, turn_done).await;
    // Uncommitted work in the parent's checkout at fork time.
    std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();

    let Response::Forked { thread, draft } = core
        .handle(Request::ForkThread {
            thread_id: parent.id.clone(),
            harness: None,
            share_workspace: false,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let p = thread.parent.clone().expect("parent link");
    assert_eq!(p.thread_id, parent.id);
    assert_eq!(
        p.kind,
        loft_protocol::ForkKind::Handoff,
        "fake has no native fork"
    );
    let draft = draft.expect("handoff draft");
    assert!(draft.contains("- Make the greeting friendlier"), "{draft}");
    assert!(draft.contains(&format!("thread `{}`", parent.id)));
    let ThreadWorkspace::Worktree { path, branch, base } = &thread.workspace else {
        panic!("{:?}", thread.workspace)
    };
    assert_eq!(branch, "agentloft/make-the-greeting-friendlier-fork");
    assert_eq!(base, "main");
    assert_eq!(
        std::fs::read_to_string(PathBuf::from(path).join("a.txt")).unwrap(),
        "one\ntwo\n",
        "fork starts from the parent's current state"
    );
    // The parent's checkout is untouched.
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");

    // Sharing the workspace instead keeps the parent's cwd.
    let Response::Forked { thread: shared, .. } = core
        .handle(Request::ForkThread {
            thread_id: parent.id.clone(),
            harness: None,
            share_workspace: true,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(shared.cwd, parent.cwd);
}

#[tokio::test]
async fn config_reload_applies_to_new_threads() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    let config = dir.path().join("config.toml");
    let core = Core::new(
        Store::open(&dir.path().join("db.sqlite")).unwrap(),
        vec![Arc::new(FakeHarness)],
        CoreOptions {
            config_file: Some(config.clone()),
            ..Default::default()
        },
    );
    let t = new_thread(&core, &repo).await;
    assert_eq!(t.workspace, ThreadWorkspace::Local);

    std::fs::write(&config, "[worktrees]\nmode = \"worktree\"\n").unwrap();
    core.handle(Request::ReloadConfig).await.unwrap();
    let t = new_thread(&core, &repo).await;
    assert_eq!(t.workspace, ThreadWorkspace::NewWorktree);

    // A broken file is reported and the last good config stays.
    std::fs::write(&config, "[worktrees\n").unwrap();
    let err = core.handle(Request::ReloadConfig).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);
    let t = new_thread(&core, &repo).await;
    assert_eq!(t.workspace, ThreadWorkspace::NewWorktree);
}

#[tokio::test]
async fn pull_request_pushes_branch_and_calls_gh() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let remote = dir.path().join("remote.git");
    git(
        dir.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    // A stand-in `gh` that records its arguments and the PR body.
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    let log = dir.path().join("gh.log");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"$@\" > '{log}'\ncat >> '{log}'\necho https://github.com/acme/repo/pull/7\n",
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut env: HashMap<String, String> = std::env::vars().collect();
    let path = format!(
        "{}:{}",
        bin.display(),
        env.get("PATH").cloned().unwrap_or_default()
    );
    env.insert("PATH".into(), path);
    let core = Core::new(
        Store::open(&dir.path().join("db.sqlite")).unwrap(),
        vec![Arc::new(FakeHarness)],
        CoreOptions {
            env: Arc::new(env),
            flush_interval: Duration::from_millis(20),
            worktrees_dir: dir.path().join("worktrees"),
            ..Default::default()
        },
    );
    let thread = new_thread(&core, &repo).await;
    core.handle(Request::SetThreadWorkspace {
        thread_id: thread.id.clone(),
        worktree: true,
    })
    .await
    .unwrap();
    core.handle(Request::SendMessage {
        thread_id: thread.id.clone(),
        text: "Add a changelog".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    wait_for(&core, &thread.id, turn_done).await;
    let (thread, ..) = thread_snapshot(&core, &thread.id);
    std::fs::write(PathBuf::from(&thread.cwd).join("CHANGELOG.md"), "# 1.0\n").unwrap();

    let Response::Text { text: url } = core
        .handle(Request::CreatePullRequest {
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(url, "https://github.com/acme/repo/pull/7");
    let branch = thread.workspace.branch().unwrap().to_owned();
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%s", &branch]),
        "Add a changelog",
        "the agent's work was committed and pushed"
    );
    let gh_log = std::fs::read_to_string(&log).unwrap();
    assert!(
        gh_log.starts_with(&format!(
            "pr create --base main --head {branch} --title Add a changelog --body-file -"
        )),
        "{gh_log}"
    );
    assert!(gh_log.contains("## Requests") && gh_log.contains("- Add a changelog"));
}

#[tokio::test]
async fn revert_turn_restores_files_and_tells_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let core = Core::new(
        Store::open(&dir.path().join("db.sqlite")).unwrap(),
        vec![Arc::new(FakeHarness)],
        CoreOptions {
            flush_interval: Duration::from_millis(20),
            ..Default::default()
        },
    );
    let thread = new_thread(&core, &repo).await;
    core.handle(Request::SendMessage {
        thread_id: thread.id.clone(),
        text: "edit things".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    wait_for(&core, &thread.id, turn_done).await;
    // What "the agent" did during the turn.
    std::fs::write(repo.join("a.txt"), "edited\n").unwrap();
    std::fs::write(repo.join("new.txt"), "new\n").unwrap();
    let (_, turns, ..) = thread_snapshot(&core, &thread.id);

    let Response::Text { text } = core
        .handle(Request::RevertTurn {
            thread_id: thread.id.clone(),
            turn_id: turns[0].id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(text, "Restored 2 files to how they were before turn 1.");
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!repo.join("new.txt").exists());
    let (_, _, items, _) = thread_snapshot(&core, &thread.id);
    assert!(
        items
            .iter()
            .any(|i| matches!(&i.body, ItemBody::Notice { text: t, .. } if *t == text))
    );

    // The next prompt carries the note to the agent; the user's bubble doesn't.
    core.handle(Request::SendMessage {
        thread_id: thread.id.clone(),
        text: "continue".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    wait_for(&core, &thread.id, |_, turns| {
        turns.len() == 2 && turns[1].status.is_terminal()
    })
    .await;
    let (_, _, items, _) = thread_snapshot(&core, &thread.id);
    let echoed: String = items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::AssistantMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        echoed.contains("user reverted the workspace files"),
        "{echoed}"
    );
    assert!(
        items
            .iter()
            .any(|i| matches!(&i.body, ItemBody::UserMessage { text, .. } if text == "continue"))
    );
}
