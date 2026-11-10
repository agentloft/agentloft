use super::*;
use loft_harness::fake::FakeHarness;
use loft_protocol::{
    DecisionKind, ItemBody, PermissionMode, RequestStatus, Snapshot, ThreadWorkspace,
};
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
async fn drafts_persist_until_the_first_message() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    assert!(!thread.is_started());
    f.core
        .handle(Request::SetDraft {
            thread_id: thread.id.clone(),
            text: "half a thought".into(),
        })
        .await
        .unwrap();
    let (t, _, _, _) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(t.draft_text(), Some("half a thought"));
    assert_eq!(
        t.updated_at_ms, thread.updated_at_ms,
        "drafts don't re-sort"
    );

    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "half a thought, finished".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    let (t, _, _, _) = thread_snapshot(&f.core, &thread.id);
    assert!(t.is_started());
    assert_eq!(t.draft, None, "sending clears the draft");
}

#[tokio::test]
async fn delete_thread_erases_it() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "hello".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    f.core
        .handle(Request::DeleteThread {
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap();
    let threads = f.core.with_store(|s| s.threads()).unwrap();
    assert!(threads.iter().all(|t| t.id != thread.id));
    let (turns, items) = f
        .core
        .with_store(|s| Ok((s.turns(&thread.id)?, s.items(&thread.id)?)))
        .unwrap();
    assert!(turns.is_empty() && items.is_empty());
    // Gone for clients too.
    let err = f
        .core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "again".into(),
            attachments: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
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
async fn steering_goes_into_the_running_turn() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    // Idle: a steer is just a message.
    f.core
        .handle(Request::Steer {
            thread_id: thread.id.clone(),
            text: "slow one two three four five six".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    f.core
        .handle(Request::Steer {
            thread_id: thread.id.clone(),
            text: "also this".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    let (_, turns, items, _) = thread_snapshot(&f.core, &thread.id);
    // One turn, not stopped: both messages in it, and the agent heard the second.
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].status, TurnStatus::Completed);
    let users: Vec<&str> = items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(users, ["slow one two three four five six", "also this"]);
    assert!(items.iter().any(|i| matches!(&i.body,
        ItemBody::AssistantMessage { text } if text == "heard: also this")));
    assert!(
        items
            .iter()
            .all(|i| i.turn_id.as_ref() == Some(&turns[0].id))
    );
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
            reply: None,
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
async fn question_answers_reach_the_agent_and_stay_on_record() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "a question for you".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, |t, _| {
        t.status == ThreadStatus::WaitingApproval
    })
    .await;
    let (_, _, _, requests) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(requests[0].kind, loft_protocol::RequestKind::Question);
    let questions = requests[0].questions();
    assert_eq!(questions.len(), 2);
    let reply = loft_protocol::RequestReply {
        answers: vec![loft_protocol::QuestionAnswer {
            id: questions[0].id.clone(),
            answers: vec!["SQLite".into()],
        }],
        message: None,
    };
    f.core
        .handle(Request::RespondRequest {
            thread_id: thread.id.clone(),
            request_id: requests[0].id.clone(),
            decision: DecisionKind::AllowOnce,
            reply: Some(reply.clone()),
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    let (_, _, items, requests) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(requests[0].reply.as_ref(), Some(&reply));
    assert!(items.iter().any(|i| matches!(&i.body,
        ItemBody::AssistantMessage { text } if text.contains("SQLite"))));
}

#[tokio::test]
async fn approving_a_plan_leaves_read_only_mode() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SetPermissionMode {
            thread_id: thread.id.clone(),
            mode: Some(PermissionMode::ReadOnly),
        })
        .await
        .unwrap();
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "make a plan".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, |t, _| {
        t.status == ThreadStatus::WaitingApproval
    })
    .await;
    let (_, _, _, requests) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(requests[0].kind, loft_protocol::RequestKind::Plan);
    assert!(requests[0].plan().is_some_and(|p| p.contains("greet")));
    f.core
        .handle(Request::RespondRequest {
            thread_id: thread.id.clone(),
            request_id: requests[0].id.clone(),
            decision: DecisionKind::AllowSession,
            reply: None,
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    let (thread_now, _, _, _) = thread_snapshot(&f.core, &thread.id);
    assert_eq!(
        thread_now.permission_mode,
        Some(PermissionMode::AcceptEdits)
    );
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
            reply: None,
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
            base: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        thread.workspace,
        ThreadWorkspace::NewWorktree { base: None }
    );

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
            base: None,
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

    // Unarchive re-creates the worktree from the kept branch.
    let Response::Thread { thread } = core
        .handle(Request::UnarchiveThread {
            thread_id: thread.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(thread.status, ThreadStatus::Idle);
    assert_eq!(git(&wt, &["branch", "--show-current"]), branch);
    assert!(wt.join("greet.py").exists(), "merged work is on the branch");

    // Renames go through the actor, so they stick.
    let Response::Thread { thread } = core
        .handle(Request::RenameThread {
            thread_id: thread.id.clone(),
            title: "  Greeting work ".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(thread.title, "Greeting work");
    let (thread, ..) = thread_snapshot(&core, &thread.id);
    assert_eq!(thread.title, "Greeting work");
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

            at_turn: None,
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

            at_turn: None,
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
    assert_eq!(t.workspace, ThreadWorkspace::NewWorktree { base: None });

    // A broken file is reported and the last good config stays.
    std::fs::write(&config, "[worktrees\n").unwrap();
    let err = core.handle(Request::ReloadConfig).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);
    let t = new_thread(&core, &repo).await;
    assert_eq!(t.workspace, ThreadWorkspace::NewWorktree { base: None });
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
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
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
        base: None,
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

#[tokio::test]
async fn worktree_setup_runs_before_the_first_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let make_repo = |name: &str, setup: &str| {
        let repo = dir.path().join(name);
        std::fs::create_dir_all(repo.join(".agentloft")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["config", "user.email", "t@t"]);
        std::fs::write(
            repo.join(".agentloft/config.toml"),
            format!("[worktree]\nsetup = \"{setup}\"\ncopy = [\".env*\", \"secrets/*.json\"]\n"),
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), ".env*\nsecrets/\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        std::fs::write(repo.join(".env"), "KEY=1\n").unwrap();
        std::fs::create_dir_all(repo.join("secrets")).unwrap();
        std::fs::write(repo.join("secrets/api.json"), "{}").unwrap();
        repo
    };
    let ok_repo = make_repo("ok", "echo ready > setup.txt");
    let bad_repo = make_repo("bad", "echo boom; exit 3");
    let core = Core::new(
        Store::open(&dir.path().join("db.sqlite")).unwrap(),
        vec![Arc::new(FakeHarness)],
        CoreOptions {
            flush_interval: Duration::from_millis(20),
            worktrees_dir: dir.path().join("worktrees"),
            ..Default::default()
        },
    );
    let mut results = Vec::new();
    for repo in [&ok_repo, &bad_repo] {
        let thread = new_thread(&core, repo).await;
        core.handle(Request::SetThreadWorkspace {
            thread_id: thread.id.clone(),
            worktree: true,
            base: None,
        })
        .await
        .unwrap();
        core.handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "go".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
        wait_for(&core, &thread.id, turn_done).await;
        results.push(thread_snapshot(&core, &thread.id));
    }

    let (thread, turns, items, _) = &results[0];
    let wt = PathBuf::from(&thread.cwd);
    assert_eq!(
        std::fs::read_to_string(wt.join("setup.txt")).unwrap(),
        "ready\n"
    );
    assert!(wt.join(".env").exists() && wt.join("secrets/api.json").exists());
    assert!(items.iter().any(|i| matches!(&i.body,
        ItemBody::Notice { text, .. } if text.starts_with("Worktree set up: `echo ready > setup.txt`"))));
    // The turn's checkpoint was taken after setup, so its diff excludes it.
    assert!(turns[0].checkpoint_before.is_some());

    let (_, _, items, _) = &results[1];
    assert!(items.iter().any(|i| matches!(&i.body,
        ItemBody::Notice { level: loft_protocol::NoticeLevel::Warning, text } if text.contains("boom"))));
    let echoed: String = items
        .iter()
        .filter_map(|i| match &i.body {
            ItemBody::AssistantMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(echoed.contains("setup command failed"), "{echoed}");
}

#[test]
fn ansi_codes_are_stripped() {
    assert_eq!(
        crate::strip_ansi("\u{1b}[1;32mname\u{1b}[0m = 1"),
        "name = 1"
    );
}

#[test]
fn agent_defaults_for_new_threads() {
    let mut config = loft_config::Config::default();
    config.harness.claude.default_model = "opus".into();
    config.harness.claude.default_effort = "high".into();
    config.harness.claude.default_permission_mode = "accept-edits".into();
    config
        .harness
        .claude
        .env
        .insert("ANTHROPIC_BASE_URL".into(), "http://proxy".into());
    let (model, mode, effort) = crate::thread_defaults(&config, &HarnessId::claude());
    assert_eq!(model.as_deref(), Some("opus"));
    assert_eq!(effort.as_deref(), Some("high"));
    assert_eq!(mode, Some(loft_protocol::PermissionMode::AcceptEdits));
    let (_, _, effort) = crate::thread_defaults(&config, &HarnessId(HarnessId::CODEX.into()));
    assert_eq!(effort, None, "auto");
    let base = std::collections::HashMap::from([("PATH".to_owned(), "/bin".to_owned())]);
    let env = crate::agent_env(&base, &config.harness.claude);
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://proxy");
    assert_eq!(env["PATH"], "/bin");
}

#[tokio::test]
async fn search_finds_threads_by_their_messages() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "Fix the \"Quoted\" 100%_bug\nplease".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;

    let search = |q: &str| {
        let core = f.core.clone();
        let q = q.to_owned();
        async move {
            match core
                .handle(Request::SearchThreads {
                    query: q,
                    limit: 10,
                })
                .await
                .unwrap()
            {
                Response::SearchHits { hits } => hits,
                other => panic!("{other:?}"),
            }
        }
    };
    // The user's message and the agent's echo both match.
    let hits = search("quoted").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].thread_id, thread.id);
    assert!(hits[0].from_user);
    assert_eq!(hits[0].count, 2);
    assert_eq!(&hits[0].snippet[hits[0].start..hits[0].end], "Quoted");
    // Quotes, LIKE wildcards and newlines are matched literally.
    assert_eq!(search("\"quoted\" 100%_").await.len(), 1);
    assert_eq!(search("100%x").await.len(), 0);
    assert_eq!(search("bug\nplease").await.len(), 1);
    assert_eq!(search("nowhere").await.len(), 0);
}

#[tokio::test]
async fn spawned_threads_inherit_settings_and_report_back() {
    let f = fixture();
    let parent = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SetPermissionMode {
            thread_id: parent.id.clone(),
            mode: Some(loft_protocol::PermissionMode::AcceptEdits),
        })
        .await
        .unwrap();
    let spawn = |from: ThreadId, prompt: &str| {
        let core = f.core.clone();
        let prompt = prompt.to_owned();
        async move {
            core.handle(Request::SpawnThread {
                from,
                prompt,
                title: None,
                harness: None,
                model: None,
                effort: None,
                permission_mode: None,
                worktree: false,
                top_level: false,
                project: None,
                chat: false,
            })
            .await
        }
    };
    let Response::Thread { thread: child } =
        spawn(parent.id.clone(), "count the files").await.unwrap()
    else {
        panic!()
    };
    let link = child.parent.clone().unwrap();
    assert_eq!(link.thread_id, parent.id);
    assert_eq!(link.kind, loft_protocol::ForkKind::Spawned);
    assert_eq!(child.harness, parent.harness);
    assert_eq!(
        child.permission_mode,
        Some(loft_protocol::PermissionMode::AcceptEdits)
    );
    assert_eq!(child.cwd, parent.cwd);
    assert_eq!(child.title, "count the files");

    // It ran its prompt; the outcome carries the reply and who spawned it.
    wait_for(&f.core, &child.id, turn_done).await;
    let Response::Json { value } = f
        .core
        .handle(Request::ThreadOutcome {
            thread_id: child.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(value["status"], "idle");
    assert_eq!(value["latest_reply"], "echo: count the files");
    assert_eq!(value["spawned_by"], parent.id.0.as_str());

    // Spawning nests only so deep.
    let mut from = child.id.clone();
    for _ in 0..2 {
        let Response::Thread { thread } = spawn(from.clone(), "deeper").await.unwrap() else {
            panic!()
        };
        from = thread.id;
    }
    let err = spawn(from, "too deep").await.unwrap_err();
    assert!(err.message.contains("deeper"), "{}", err.message);
    assert!(spawn(parent.id.clone(), "  ").await.is_err());
}

#[tokio::test]
async fn chats_get_a_folder_of_their_own_in_one_project() {
    let f = fixture();
    let chat = || {
        let core = f.core.clone();
        async move {
            match core
                .handle(Request::CreateChat {
                    harness: HarnessId(HarnessId::FAKE.into()),
                })
                .await
                .unwrap()
            {
                Response::Thread { thread } => thread,
                other => panic!("{other:?}"),
            }
        }
    };
    let (a, b) = (chat().await, chat().await);
    assert_eq!(a.project_id, b.project_id);
    let project = f
        .core
        .with_store(|s| s.project(&a.project_id))
        .unwrap()
        .unwrap();
    assert_eq!(project.name, "Chats");
    assert_ne!(a.cwd, b.cwd);
    assert!(a.cwd.ends_with(a.id.as_str()), "{}", a.cwd);
    assert!(std::path::Path::new(&a.cwd).is_dir());
    // It's a thread like any other.
    f.core
        .handle(Request::SendMessage {
            thread_id: a.id.clone(),
            text: "hi".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &a.id, turn_done).await;
}

#[tokio::test]
async fn threads_can_be_spawned_in_another_project_or_as_a_chat() {
    let f = fixture();
    let caller = new_thread(&f.core, f._dir.path()).await;
    let other = tempfile::tempdir().unwrap();
    let Response::Project { project } = f
        .core
        .handle(Request::AddProject {
            path: other.path().to_string_lossy().into_owned(),
            name: Some("Dispatch".into()),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let spawn = |project: Option<&str>, chat: bool| {
        let (core, from) = (f.core.clone(), caller.id.clone());
        let project = project.map(str::to_owned);
        async move {
            core.handle(Request::SpawnThread {
                from,
                prompt: "look around".into(),
                title: None,
                harness: None,
                model: None,
                effort: None,
                permission_mode: None,
                worktree: false,
                top_level: false,
                project,
                chat,
            })
            .await
        }
    };
    // By name, case aside: in that project's folder, its own row there.
    let Response::Thread { thread } = spawn(Some("dispatch"), false).await.unwrap() else {
        panic!()
    };
    assert_eq!(thread.project_id, project.id);
    assert_eq!(thread.cwd, project.root_path);
    let link = thread.parent.unwrap();
    assert!(link.detached);
    assert_eq!(link.thread_id, caller.id);
    // As a chat.
    let Response::Thread { thread } = spawn(None, true).await.unwrap() else {
        panic!()
    };
    assert_ne!(thread.project_id, caller.project_id);
    assert!(thread.cwd.ends_with(thread.id.as_str()));
    // An unknown project names the ones there are.
    let err = spawn(Some("nowhere"), false).await.unwrap_err();
    assert!(err.message.contains("Dispatch"), "{}", err.message);
}

#[tokio::test]
async fn new_projects_are_fresh_git_repos() {
    let f = fixture();
    let parent = tempfile::tempdir().unwrap();
    let create = |name: &str| {
        let core = f.core.clone();
        let (parent, name) = (
            parent.path().to_string_lossy().into_owned(),
            name.to_owned(),
        );
        async move {
            core.handle(Request::CreateProject {
                parent: Some(parent),
                name,
                github: false,
            })
            .await
        }
    };
    let Response::Project { project } = create("Fresh Start").await.unwrap() else {
        panic!()
    };
    // Named as typed, in a folder named after it, with a README.
    assert_eq!(project.name, "Fresh Start");
    let root = std::path::Path::new(&project.root_path);
    assert_eq!(root.file_name().unwrap(), "fresh-start");
    assert!(root.join(".git").exists());
    assert!(root.join("README.md").exists());
    // The same name again gets a folder of its own.
    let Response::Project { project: again } = create("Fresh Start").await.unwrap() else {
        panic!()
    };
    assert!(
        again.root_path.ends_with("fresh-start-2"),
        "{}",
        again.root_path
    );
    assert!(create("  ").await.is_err());
}

#[tokio::test]
async fn removing_a_project_deletes_its_threads_not_its_files() {
    let f = fixture();
    let thread = new_thread(&f.core, f._dir.path()).await;
    let marker = f._dir.path().join("keep.txt");
    std::fs::write(&marker, "x").unwrap();
    f.core
        .handle(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "hi".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    wait_for(&f.core, &thread.id, turn_done).await;
    let Response::Project { project } = f
        .core
        .handle(Request::RenameProject {
            project_id: thread.project_id.clone(),
            name: "  Renamed ".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(project.name, "Renamed");
    f.core
        .handle(Request::RemoveProject {
            project_id: thread.project_id.clone(),
        })
        .await
        .unwrap();
    let (projects, threads) = f
        .core
        .with_store(|s| Ok((s.projects()?, s.threads()?)))
        .unwrap();
    assert!(projects.is_empty());
    assert!(threads.is_empty());
    assert!(marker.exists());
    // It can come back.
    let again = new_thread(&f.core, f._dir.path()).await;
    assert_ne!(again.project_id, thread.project_id);
}

#[tokio::test]
async fn project_defaults_shape_new_threads() {
    let f = fixture();
    let first = new_thread(&f.core, f._dir.path()).await;
    f.core
        .handle(Request::SetProjectDefaults {
            project_id: first.project_id.clone(),
            defaults: loft_protocol::ProjectDefaults {
                agent: Some(HarnessId(HarnessId::FAKE.into())),
                model: Some("fast".into()),
                effort: Some("low".into()),
                worktree: Some(false),
            },
        })
        .await
        .unwrap();
    let Response::Thread { thread } = f
        .core
        .handle(Request::CreateThread {
            project_id: first.project_id.clone(),
            harness: HarnessId(HarnessId::FAKE.into()),
            title: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(thread.model.as_deref(), Some("fast"));
    assert_eq!(thread.effort.as_deref(), Some("low"));
    assert_eq!(thread.workspace, ThreadWorkspace::Local);
}

#[tokio::test]
async fn agents_change_settings_and_add_themes() {
    let dir = tempfile::tempdir().unwrap();
    let config_file = dir.path().join("config.toml");
    let core = Core::new(
        Store::open(&dir.path().join("db.sqlite")).unwrap(),
        vec![Arc::new(FakeHarness)],
        CoreOptions {
            config_file: Some(config_file.clone()),
            ..Default::default()
        },
    );
    let call = |req: Request| {
        let core = core.clone();
        async move { core.handle(req).await }
    };
    call(Request::ChangeSetting {
        key: "ui.theme".into(),
        value: serde_json::json!("dark"),
    })
    .await
    .unwrap();
    call(Request::ChangeSetting {
        key: "harness.codex.default_model".into(),
        value: serde_json::json!("gpt-6"),
    })
    .await
    .unwrap();
    let text = std::fs::read_to_string(&config_file).unwrap();
    assert!(text.contains("theme = \"dark\""), "{text}");
    assert!(text.contains("default_model = \"gpt-6\""), "{text}");
    // Unknown keys and wrong values are refused.
    for (key, value) in [
        ("ui.nope", serde_json::json!(1)),
        ("ui.theme", serde_json::json!("purple")),
        ("ui.palette", serde_json::json!("custom:missing")),
    ] {
        assert!(
            call(Request::ChangeSetting {
                key: key.into(),
                value
            })
            .await
            .is_err()
        );
    }
    // A theme, saved and switched to.
    let theme = r##"name = "Ember"
[dark]
background = "#1a1110"
foreground = "#f5e6e0"
accent = "#ff7a45"
[light]
background = "#fffaf7"
foreground = "#2a1a14"
accent = "#d9480f"
"##;
    call(Request::SaveTheme {
        toml: theme.into(),
        select: true,
        id: None,
    })
    .await
    .unwrap();
    assert!(dir.path().join("themes/ember.toml").exists());
    // Under a given id: only a plain name, never a path.
    call(Request::SaveTheme {
        toml: theme.into(),
        select: false,
        id: Some("custom:ember-2".into()),
    })
    .await
    .unwrap();
    assert!(dir.path().join("themes/ember-2.toml").exists());
    call(Request::DeleteTheme {
        id: "custom:ember-2".into(),
    })
    .await
    .unwrap();
    assert!(!dir.path().join("themes/ember-2.toml").exists());
    for id in ["custom:../ember", "custom:a/b", "custom:", "../../x"] {
        assert!(
            call(Request::SaveTheme {
                toml: theme.into(),
                select: false,
                id: Some(id.into()),
            })
            .await
            .is_err(),
            "{id}"
        );
    }
    let text = std::fs::read_to_string(&config_file).unwrap();
    assert!(text.contains("palette = \"custom:ember\""), "{text}");
    assert!(
        call(Request::SaveTheme {
            toml: theme.replace("#ff7a45", "orange"),
            select: false,
            id: None,
        })
        .await
        .is_err()
    );
    // Reading them back.
    let Response::Json { value } = call(Request::GetSettings).await.unwrap() else {
        panic!()
    };
    assert_eq!(value["current"]["ui"]["theme"], "dark");
    assert_eq!(value["themes"]["custom"][0]["id"], "custom:ember");
}
