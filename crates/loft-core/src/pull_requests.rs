//! A thread's pull request: found when Agentloft opens it or when the agent
//! did (asked for by branch as a turn ends), then kept current by asking
//! `gh pr view` while it is open. Everything here is quiet: no `gh`, not
//! signed in, or no pull request for the branch just means none is shown.

use crate::thread::ThreadCmd;
use crate::{Core, CoreResult, lock};
use loft_protocol::{
    ChecksState, Event, PullRequest, PullRequestChecks, PullRequestState, Response, ReviewDecision,
    Thread, ThreadId, ThreadStatus, ThreadWorkspace,
};
use loft_store::now_ms;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// What `gh pr view` is asked for.
const FIELDS: &str = "number,url,title,state,isDraft,statusCheckRollup,reviewDecision";
/// While checks are queued or running.
const PENDING_EVERY: Duration = Duration::from_secs(60);
/// While it is open and its checks have settled.
const SETTLED_EVERY: Duration = Duration::from_secs(5 * 60);
/// Just opened or just changed, and no checks yet: they take a moment to
/// be registered.
const AWAITING_CHECKS_EVERY: Duration = Duration::from_secs(20);
const AWAITING_CHECKS_FOR_MS: i64 = 3 * 60 * 1000;
/// How often every thread is looked over for open pull requests that
/// aren't followed yet (a thread brought back from the archive).
const RESCAN_EVERY: Duration = Duration::from_secs(10 * 60);
/// `gh` gets this long to answer.
const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// Pull requests asked about at the same time.
const AT_ONCE: usize = 4;
/// A thread is logged whole on every change: titles are kept short.
const TITLE_MAX: usize = 200;

/// The daemon's bookkeeping for pull requests.
#[derive(Default)]
pub(crate) struct PullRequests {
    /// Threads with an open pull request, and when each is asked about next.
    due: Mutex<HashMap<ThreadId, Instant>>,
    /// One question at a time per thread.
    asking: Mutex<HashMap<ThreadId, Arc<tokio::sync::Mutex<()>>>>,
    /// Wakes the poller when something is due sooner than it thought.
    wake: Arc<tokio::sync::Notify>,
    /// Said once that `gh` can't be used.
    unavailable_logged: AtomicBool,
}

/// How often an open pull request is asked about (`None`: never again).
fn poll_every(pr: &PullRequest, now_ms: i64) -> Option<Duration> {
    if !pr.is_open() {
        return None;
    }
    Some(if pr.checks.pending > 0 {
        PENDING_EVERY
    } else if pr.checks.is_empty() && now_ms - pr.refreshed_at_ms < AWAITING_CHECKS_FOR_MS {
        AWAITING_CHECKS_EVERY
    } else {
        SETTLED_EVERY
    })
}

/// gh's `state` and `isDraft` as ours. `None`: a state we don't know.
fn pr_state(state: &str, is_draft: bool) -> Option<PullRequestState> {
    match state.to_ascii_uppercase().as_str() {
        "MERGED" => Some(PullRequestState::Merged),
        "CLOSED" => Some(PullRequestState::Closed),
        "OPEN" if is_draft => Some(PullRequestState::Draft),
        "OPEN" => Some(PullRequestState::Open),
        _ => None,
    }
}

fn review_decision(decision: &str) -> Option<ReviewDecision> {
    match decision.to_ascii_uppercase().as_str() {
        "APPROVED" => Some(ReviewDecision::Approved),
        "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
        "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
        _ => None,
    }
}

fn text<'a>(entry: &'a Value, key: &str) -> &'a str {
    entry.get(key).and_then(Value::as_str).unwrap_or("")
}

/// How one entry of `statusCheckRollup` stands: a `CheckRun` (`status`,
/// `conclusion`) or a `StatusContext` (`state`). `None`: skipped, so not
/// counted. Anything we can't read counts as pending.
fn check_state(entry: &Value) -> Option<ChecksState> {
    let upper = |key: &str| text(entry, key).to_ascii_uppercase();
    let kind = text(entry, "__typename");
    let has = |key: &str| entry.get(key).is_some();
    if kind == "StatusContext" || (kind.is_empty() && has("state") && !has("status")) {
        return Some(match upper("state").as_str() {
            "SUCCESS" => ChecksState::Passing,
            "FAILURE" | "ERROR" => ChecksState::Failing,
            // PENDING, EXPECTED, or something new.
            _ => ChecksState::Pending,
        });
    }
    if kind == "CheckRun" || (kind.is_empty() && has("status")) {
        if upper("status") != "COMPLETED" {
            return Some(ChecksState::Pending);
        }
        return match upper("conclusion").as_str() {
            "SUCCESS" | "NEUTRAL" => Some(ChecksState::Passing),
            "SKIPPED" => None,
            "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
                Some(ChecksState::Failing)
            }
            // STALE, or a conclusion we don't know.
            _ => Some(ChecksState::Pending),
        };
    }
    Some(ChecksState::Pending)
}

/// What makes two entries the same check: a job run again is listed next to
/// its earlier attempt (same workflow run, same name), and only the latest
/// counts. The same job started by another event (push and pull request) is
/// another workflow run, so it stays a check of its own.
fn check_key(entry: &Value) -> Option<String> {
    let context = text(entry, "context");
    if !context.is_empty() {
        return Some(format!("status\n{context}"));
    }
    let name = text(entry, "name");
    if name.is_empty() {
        return None;
    }
    let run = text(entry, "detailsUrl")
        .split_once("/actions/runs/")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
        .unwrap_or("");
    Some(format!(
        "check\n{}\n{name}\n{run}",
        text(entry, "workflowName")
    ))
}

/// `statusCheckRollup` counted. `null` or missing: no checks.
fn count_checks(rollup: &Value) -> PullRequestChecks {
    let mut checks = PullRequestChecks::default();
    let Some(entries) = rollup.as_array() else {
        return checks;
    };
    // The latest attempt of each (ISO timestamps sort as text).
    let mut latest: HashMap<String, &Value> = HashMap::new();
    let mut unnamed = Vec::new();
    for entry in entries {
        match check_key(entry) {
            Some(key) => {
                let newer = latest
                    .get(&key)
                    .is_none_or(|known| text(entry, "startedAt") >= text(known, "startedAt"));
                if newer {
                    latest.insert(key, entry);
                }
            }
            None => unnamed.push(entry),
        }
    }
    for entry in latest.into_values().chain(unnamed) {
        match check_state(entry) {
            Some(ChecksState::Passing) => checks.passing += 1,
            Some(ChecksState::Failing) => checks.failing += 1,
            Some(ChecksState::Pending) => checks.pending += 1,
            None => {}
        }
    }
    checks
}

/// What `gh pr view --json` printed, as a [`PullRequest`].
fn parse(json: &str, now_ms: i64) -> anyhow::Result<PullRequest> {
    let v: Value = serde_json::from_str(json.trim())?;
    let number = v
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("no number"))?;
    let url = text(&v, "url");
    anyhow::ensure!(!url.is_empty(), "no url");
    let is_draft = v.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    let state = pr_state(text(&v, "state"), is_draft)
        .ok_or_else(|| anyhow::anyhow!("unknown state `{}`", text(&v, "state")))?;
    Ok(PullRequest {
        number,
        url: url.to_owned(),
        title: text(&v, "title").chars().take(TITLE_MAX).collect(),
        state,
        checks: count_checks(v.get("statusCheckRollup").unwrap_or(&Value::Null)),
        review: review_decision(text(&v, "reviewDecision")),
        refreshed_at_ms: now_ms,
    })
}

/// The number at the end of a pull request's address (`…/pull/7`).
fn number_from_url(url: &str) -> Option<u64> {
    let (_, rest) = url.rsplit_once("/pull/")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// What asking `gh` came to.
enum Asked {
    Found(PullRequest),
    /// The branch has no pull request (the usual answer).
    None,
    /// `gh` isn't installed or isn't signed in.
    Unavailable(String),
    Failed(String),
}

/// `gh pr view <target>`: a pull request's address or number, or a branch.
async fn gh_pr_view(
    gh: &Path,
    dir: Option<&Path>,
    target: &str,
    env: &HashMap<String, String>,
) -> Asked {
    let mut cmd = tokio::process::Command::new(gh);
    cmd.env_clear()
        .envs(env)
        .env("GH_PROMPT_DISABLED", "1")
        .args(["pr", "view", target, "--json", FIELDS])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let out = match tokio::time::timeout(GH_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => return Asked::Failed(format!("couldn't run gh: {err}")),
        Err(_) => return Asked::Failed("gh didn't answer in time".into()),
    };
    if out.status.success() {
        return match parse(&String::from_utf8_lossy(&out.stdout), now_ms()) {
            Ok(pr) => Asked::Found(pr),
            Err(err) => Asked::Failed(format!("couldn't read gh's answer: {err}")),
        };
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    if stderr.contains("no pull requests found") {
        Asked::None
    } else if out.status.code() == Some(4) || stderr.contains("gh auth login") {
        Asked::Unavailable(stderr)
    } else {
        Asked::Failed(stderr)
    }
}

/// What to ask `gh` about: the pull request we know while it is open, else
/// whatever the thread's branch has now (the agent may have opened one, or
/// another after the last was merged or closed).
fn target(thread: &Thread) -> Option<String> {
    match (&thread.pull_request, thread.workspace.branch()) {
        (Some(pr), _) if pr.is_open() => Some(pr.url.clone()),
        (_, Some(branch)) => Some(branch.to_owned()),
        _ => None,
    }
}

/// A folder of the thread's repository for `gh` to run in.
fn folder(thread: &Thread) -> Option<PathBuf> {
    let worktree = match &thread.workspace {
        ThreadWorkspace::Worktree { path, .. } => Some(path.as_str()),
        _ => None,
    };
    worktree
        .into_iter()
        .chain([thread.cwd.as_str()])
        .map(PathBuf::from)
        .find(|dir| dir.is_dir())
}

/// A turn ended: look for a pull request the agent opened on the thread's
/// branch, or see what its push did to the one we know. In the background.
pub(crate) fn turn_ended(core: &Arc<Core>, thread: &Thread) {
    if thread.workspace.branch().is_none() {
        return;
    }
    let (core, id) = (core.clone(), thread.id.clone());
    tokio::spawn(async move { core.look_at_pull_request(&id, true).await });
}

/// The thread's actor takes what GitHub said (`ThreadCmd::SetPullRequest`).
/// Saved only when something changed, and without touching `updated_at_ms`:
/// a check that finished mustn't re-sort the sidebar.
pub(crate) fn apply(core: &Core, thread: &mut Thread, pr: PullRequest) -> CoreResult<Response> {
    let same = thread
        .pull_request
        .as_ref()
        .is_some_and(|known| known.same_as(&pr));
    if same || thread.status == ThreadStatus::Archived {
        return Ok(Response::Ok);
    }
    thread.pull_request = Some(pr);
    core.commit(vec![Event::ThreadUpdated {
        thread: thread.clone(),
    }])
    .map(|_| Response::Ok)
}

impl Core {
    /// Keep open pull requests current: each is asked about when it is due
    /// (every minute while checks run, every five once they have settled,
    /// never once merged or closed). Ends when the core is dropped (or
    /// abort it).
    pub fn spawn_pull_requests(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let core = Arc::downgrade(self);
        let wake = self.pull_requests.wake.clone();
        tokio::spawn(async move {
            let mut scanned: Option<Instant> = None;
            loop {
                let Some(c) = core.upgrade() else { return };
                if scanned.is_none_or(|at| at.elapsed() >= RESCAN_EVERY) {
                    c.follow_open_pull_requests();
                    scanned = Some(Instant::now());
                }
                let wait = c.poll_due_pull_requests().await;
                drop(c);
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = wake.notified() => {}
                }
            }
        })
    }

    /// Start following every thread with an open pull request that isn't
    /// followed yet (at startup: all of them, asked about at once).
    fn follow_open_pull_requests(&self) {
        let Ok(threads) = self.with_store(|s| s.threads()) else {
            return;
        };
        let now = Instant::now();
        let mut due = lock(&self.pull_requests.due);
        for thread in threads {
            let open = thread.pull_request.as_ref().is_some_and(|pr| pr.is_open());
            if open && thread.status != ThreadStatus::Archived {
                due.entry(thread.id).or_insert(now);
            }
        }
    }

    /// Ask about every pull request that is due; returns how long until the
    /// next one is.
    async fn poll_due_pull_requests(self: &Arc<Self>) -> Duration {
        let now = Instant::now();
        let ids: Vec<ThreadId> = {
            let mut due = lock(&self.pull_requests.due);
            let ids: Vec<ThreadId> = due
                .iter()
                .filter(|(_, at)| **at <= now)
                .map(|(id, _)| id.clone())
                .collect();
            // Until each answer says when it is due next.
            for id in &ids {
                due.insert(id.clone(), now + PENDING_EVERY);
            }
            ids
        };
        for some in ids.chunks(AT_ONCE) {
            let mut asking = tokio::task::JoinSet::new();
            for id in some {
                let (core, id) = (self.clone(), id.clone());
                asking.spawn(async move { core.look_at_pull_request(&id, false).await });
            }
            while asking.join_next().await.is_some() {}
        }
        let next = lock(&self.pull_requests.due).values().min().copied();
        next.map(|at| at.saturating_duration_since(Instant::now()))
            .unwrap_or(RESCAN_EVERY)
            .clamp(Duration::from_secs(1), RESCAN_EVERY)
    }

    /// `Request::RefreshPullRequest`: the user is looking, so ask now.
    pub(crate) async fn refresh_pull_request(
        self: &Arc<Self>,
        id: &ThreadId,
    ) -> CoreResult<Response> {
        self.load_thread(id)?;
        self.look_at_pull_request(id, true).await;
        Ok(Response::Ok)
    }

    /// Agentloft opened (or found) the thread's pull request at `url`: the
    /// thread has it at once, and what GitHub says about it follows.
    pub(crate) async fn pull_request_created(
        self: &Arc<Self>,
        id: &ThreadId,
        url: &str,
        title: &str,
    ) {
        let known = self
            .load_thread(id)
            .ok()
            .and_then(|t| t.pull_request)
            .is_some_and(|pr| pr.url == url);
        if !known && let Some(number) = number_from_url(url) {
            let pr = PullRequest {
                number,
                url: url.to_owned(),
                title: title.chars().take(TITLE_MAX).collect(),
                state: PullRequestState::Open,
                checks: PullRequestChecks::default(),
                review: None,
                refreshed_at_ms: now_ms(),
            };
            self.set_pull_request(id, pr).await;
        }
        let (core, id) = (self.clone(), id.clone());
        tokio::spawn(async move { core.look_at_pull_request(&id, true).await });
    }

    /// Hand the thread's actor what we learned; it owns the thread. This
    /// starts no agent session.
    async fn set_pull_request(self: &Arc<Self>, id: &ThreadId, pr: PullRequest) {
        let every = poll_every(&pr, now_ms());
        let sent = self
            .thread_call(id, |reply| ThreadCmd::SetPullRequest {
                pull_request: pr,
                reply,
            })
            .await;
        if let Err(err) = sent {
            tracing::debug!(thread = %id.as_str(), err = %err.message, "pull request not saved");
            return;
        }
        let mut due = lock(&self.pull_requests.due);
        match every {
            Some(every) => {
                due.insert(id.clone(), Instant::now() + every);
                self.pull_requests.wake.notify_one();
            }
            None => {
                due.remove(id);
            }
        }
    }

    fn forget_pull_request(&self, id: &ThreadId) {
        lock(&self.pull_requests.due).remove(id);
        lock(&self.pull_requests.asking).remove(id);
    }

    /// Ask `gh` about the thread's pull request (or its branch) and keep
    /// what is new. `wait`: if the thread is being asked about already,
    /// ask again after that (else leave it to the one in flight).
    async fn look_at_pull_request(self: &Arc<Self>, id: &ThreadId, wait: bool) {
        let gate = lock(&self.pull_requests.asking)
            .entry(id.clone())
            .or_default()
            .clone();
        let _one_at_a_time = if wait {
            gate.lock().await
        } else {
            match gate.try_lock() {
                Ok(guard) => guard,
                Err(_) => return,
            }
        };
        // Archived, deleted, or nothing to ask about: not followed.
        let thread = match self.load_thread(id) {
            Ok(thread) if thread.status != ThreadStatus::Archived => thread,
            _ => return self.forget_pull_request(id),
        };
        let Some(target) = target(&thread) else {
            return self.forget_pull_request(id);
        };
        let dir = folder(&thread);
        if dir.is_none() && !target.starts_with("https://") {
            return;
        }
        let env = self.opts().env.clone();
        let gh = loft_config::which("gh", env.get("PATH").map(String::as_str).unwrap_or(""));
        let asked = match gh {
            Some(gh) => gh_pr_view(&gh, dir.as_deref(), &target, &env).await,
            None => Asked::Unavailable("gh not found".into()),
        };
        match asked {
            Asked::Found(pr) => return self.set_pull_request(id, pr).await,
            Asked::None => {}
            Asked::Unavailable(why) => {
                if !self
                    .pull_requests
                    .unavailable_logged
                    .swap(true, Ordering::Relaxed)
                {
                    tracing::debug!(%why, "pull requests aren't shown: gh is missing or not signed in");
                }
            }
            Asked::Failed(why) => {
                tracing::debug!(thread = %id.as_str(), target, %why, "gh pr view failed");
            }
        }
        // No answer this time: an open one is asked about again later.
        let mut due = lock(&self.pull_requests.due);
        match thread
            .pull_request
            .as_ref()
            .and_then(|pr| poll_every(pr, now_ms()))
        {
            Some(every) => {
                due.insert(id.clone(), Instant::now() + every);
            }
            None => {
                due.remove(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BusMsg, CoreOptions};
    use loft_harness::fake::FakeHarness;
    use loft_protocol::{HarnessId, Request, Snapshot, Topic};
    use loft_store::Store;

    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn gh_states_map_to_ours() {
        assert_eq!(pr_state("OPEN", false), Some(PullRequestState::Open));
        assert_eq!(pr_state("OPEN", true), Some(PullRequestState::Draft));
        // A draft that was merged or closed is merged or closed.
        assert_eq!(pr_state("MERGED", true), Some(PullRequestState::Merged));
        assert_eq!(pr_state("CLOSED", false), Some(PullRequestState::Closed));
        assert_eq!(pr_state("closed", true), Some(PullRequestState::Closed));
        assert_eq!(pr_state("", false), None);
        assert_eq!(pr_state("LOCKED", false), None);
    }

    #[test]
    fn check_runs_and_status_contexts_are_counted() {
        let json = r#"{
            "number": 42,
            "url": "https://github.com/acme/repo/pull/42",
            "title": "Add a changelog",
            "state": "OPEN",
            "isDraft": false,
            "reviewDecision": "CHANGES_REQUESTED",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "name": "build", "workflowName": "CI",
                 "status": "COMPLETED", "conclusion": "SUCCESS",
                 "startedAt": "2026-01-01T10:00:00Z",
                 "detailsUrl": "https://github.com/acme/repo/actions/runs/1/job/10"},
                {"__typename": "CheckRun", "name": "lint", "workflowName": "CI",
                 "status": "COMPLETED", "conclusion": "NEUTRAL",
                 "detailsUrl": "https://github.com/acme/repo/actions/runs/1/job/11"},
                {"__typename": "CheckRun", "name": "test", "workflowName": "CI",
                 "status": "COMPLETED", "conclusion": "FAILURE",
                 "detailsUrl": "https://github.com/acme/repo/actions/runs/1/job/12"},
                {"__typename": "CheckRun", "name": "deploy", "workflowName": "CI",
                 "status": "COMPLETED", "conclusion": "TIMED_OUT"},
                {"__typename": "CheckRun", "name": "docs", "workflowName": "CI",
                 "status": "COMPLETED", "conclusion": "SKIPPED"},
                {"__typename": "CheckRun", "name": "e2e", "workflowName": "CI",
                 "status": "IN_PROGRESS", "conclusion": ""},
                {"__typename": "CheckRun", "name": "bench", "workflowName": "CI",
                 "status": "QUEUED", "conclusion": null},
                {"__typename": "StatusContext", "context": "ci/circleci", "state": "SUCCESS"},
                {"__typename": "StatusContext", "context": "security/scan", "state": "ERROR"},
                {"__typename": "StatusContext", "context": "license/cla", "state": "PENDING"},
                {"__typename": "StatusContext", "context": "required", "state": "EXPECTED"}
            ]
        }"#;
        let pr = parse(json, NOW).unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.url, "https://github.com/acme/repo/pull/42");
        assert_eq!(pr.title, "Add a changelog");
        assert_eq!(pr.state, PullRequestState::Open);
        assert_eq!(pr.review, Some(ReviewDecision::ChangesRequested));
        assert_eq!(pr.refreshed_at_ms, NOW);
        assert_eq!(
            pr.checks,
            PullRequestChecks {
                passing: 3,
                failing: 3,
                pending: 4,
            }
        );
        assert_eq!(pr.checks.rollup(), Some(ChecksState::Failing));
        assert_eq!(
            pr.checks.summary().as_deref(),
            Some("3 passing, 3 failing, 4 running")
        );
    }

    #[test]
    fn unknown_shapes_count_as_pending() {
        let rollup = serde_json::json!([
            // No type: told apart by their fields.
            {"name": "a", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"context": "b", "state": "FAILURE"},
            // A type, a status, a conclusion or a state we don't know.
            {"__typename": "DeploymentGate", "name": "c"},
            {"__typename": "CheckRun", "name": "d", "status": "COMPLETED", "conclusion": "NEW"},
            {"__typename": "CheckRun", "name": "e", "status": "WAITING"},
            {"__typename": "StatusContext", "context": "f", "state": "NEW"},
            {},
            "text",
            null
        ]);
        assert_eq!(
            count_checks(&rollup),
            PullRequestChecks {
                passing: 1,
                failing: 1,
                pending: 7,
            }
        );
        // No checks at all.
        for none in [Value::Null, serde_json::json!([]), serde_json::json!("x")] {
            assert!(count_checks(&none).is_empty());
        }
        assert_eq!(PullRequestChecks::default().rollup(), None);
        assert_eq!(PullRequestChecks::default().summary(), None);
    }

    #[test]
    fn a_job_run_again_counts_once_as_its_latest_attempt() {
        let run = |job: u32, started: &str, conclusion: &str, run: u32| {
            serde_json::json!({
                "__typename": "CheckRun", "name": "test", "workflowName": "CI",
                "status": "COMPLETED", "conclusion": conclusion, "startedAt": started,
                "detailsUrl": format!("https://github.com/acme/repo/actions/runs/{run}/job/{job}"),
            })
        };
        // Failed, then run again and passed: passing. Listed either way round.
        let first = run(1, "2026-01-01T10:00:00Z", "FAILURE", 7);
        let again = run(2, "2026-01-01T10:30:00Z", "SUCCESS", 7);
        for rollup in [
            serde_json::json!([first.clone(), again.clone()]),
            serde_json::json!([again.clone(), first.clone()]),
        ] {
            let checks = count_checks(&rollup);
            assert_eq!((checks.passing, checks.failing), (1, 0), "{rollup}");
        }
        // The same job in another workflow run (push and pull request) is
        // a check of its own.
        let other = run(3, "2026-01-01T10:00:00Z", "FAILURE", 8);
        let checks = count_checks(&serde_json::json!([again, other]));
        assert_eq!((checks.passing, checks.failing), (1, 1));
    }

    #[test]
    fn drafts_merged_and_odd_answers() {
        let draft = r#"{"number":3,"url":"https://github.com/a/b/pull/3","title":"WIP",
            "state":"OPEN","isDraft":true,"reviewDecision":"","statusCheckRollup":null}"#;
        let pr = parse(draft, NOW).unwrap();
        assert_eq!(pr.state, PullRequestState::Draft);
        assert_eq!(pr.review, None);
        assert!(pr.checks.is_empty());
        // Fields gh may leave out.
        let merged = r#"{"number":3,"url":"https://github.com/a/b/pull/3","state":"MERGED"}"#;
        let pr = parse(merged, NOW).unwrap();
        assert_eq!(pr.state, PullRequestState::Merged);
        assert_eq!(pr.title, "");
        // A long title is cut.
        let long = serde_json::json!({
            "number": 1, "url": "https://github.com/a/b/pull/1", "state": "OPEN",
            "title": "é".repeat(500),
        });
        assert_eq!(
            parse(&long.to_string(), NOW).unwrap().title.chars().count(),
            TITLE_MAX
        );
        // Not a pull request we can show.
        assert!(parse("https://github.com/a/b/pull/3", NOW).is_err());
        assert!(parse(r#"{"number":3,"state":"OPEN"}"#, NOW).is_err());
        assert!(parse(r#"{"number":3,"url":"u","state":"ODD"}"#, NOW).is_err());
        assert_eq!(number_from_url("https://github.com/a/b/pull/7"), Some(7));
        assert_eq!(
            number_from_url("https://github.com/a/b/pull/7/files"),
            Some(7)
        );
        assert_eq!(number_from_url("https://github.com/a/b"), None);
    }

    #[test]
    fn how_often_a_pull_request_is_asked_about() {
        let pr = |state, checks: (u32, u32, u32), age_ms: i64| PullRequest {
            number: 1,
            url: "https://github.com/a/b/pull/1".into(),
            title: "t".into(),
            state,
            checks: PullRequestChecks {
                passing: checks.0,
                failing: checks.1,
                pending: checks.2,
            },
            review: None,
            refreshed_at_ms: NOW - age_ms,
        };
        let old = 60 * 60 * 1000;
        use PullRequestState::*;
        assert_eq!(
            poll_every(&pr(Open, (1, 0, 2), old), NOW),
            Some(PENDING_EVERY)
        );
        assert_eq!(
            poll_every(&pr(Draft, (0, 1, 1), old), NOW),
            Some(PENDING_EVERY)
        );
        assert_eq!(
            poll_every(&pr(Open, (3, 1, 0), old), NOW),
            Some(SETTLED_EVERY)
        );
        assert_eq!(
            poll_every(&pr(Open, (0, 0, 0), old), NOW),
            Some(SETTLED_EVERY)
        );
        // Just opened: its checks haven't been registered yet.
        assert_eq!(
            poll_every(&pr(Open, (0, 0, 0), 5_000), NOW),
            Some(AWAITING_CHECKS_EVERY)
        );
        // Never once merged or closed, whatever its checks said last.
        assert_eq!(poll_every(&pr(Merged, (1, 0, 2), 0), NOW), None);
        assert_eq!(poll_every(&pr(Closed, (0, 0, 1), 0), NOW), None);
    }

    // -- with a stand-in `gh` -------------------------------------------------

    fn git(dir: &Path, args: &[&str]) -> String {
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

    /// A repository with a bare remote, a core whose PATH starts with a
    /// stand-in `gh`, and one worktree thread.
    ///
    /// The stand-in: `pr create` prints pull request 7's address; `pr view`
    /// prints `pr.json` when the test has written one, else says the branch
    /// has no pull request. Every call is added to `gh.log`.
    struct Fixture {
        core: Arc<Core>,
        dir: tempfile::TempDir,
        thread: ThreadId,
    }

    impl Fixture {
        async fn new() -> Self {
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
            let remote_path = remote.to_str().unwrap();
            git(
                dir.path(),
                &["init", "-q", "--bare", "-b", "main", remote_path],
            );
            git(&repo, &["remote", "add", "origin", remote_path]);
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let gh = bin.join("gh");
            let script = format!(
                "#!/bin/sh\n\
                 echo \"$@\" >> '{d}/gh.log'\n\
                 case \"$1 $2\" in\n\
                 'pr create') cat > /dev/null; echo https://github.com/acme/repo/pull/7 ;;\n\
                 'pr view')\n\
                   if [ -f '{d}/pr.json' ]; then cat '{d}/pr.json'\n\
                   else echo 'no pull requests found for branch \"x\"' >&2; exit 1; fi ;;\n\
                 *) exit 2 ;;\n\
                 esac\n",
                d = dir.path().display()
            );
            std::fs::write(&gh, script).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            // The stand-in is the only `gh` there is: this machine's own is
            // never run, even once the stand-in is taken away.
            let mut env: HashMap<String, String> = std::env::vars().collect();
            let rest = env.get("PATH").cloned().unwrap_or_default();
            let rest: Vec<&str> = rest
                .split(':')
                .filter(|d| !Path::new(d).join("gh").exists())
                .collect();
            let path = format!("{}:{}", bin.display(), rest.join(":"));
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
            let Response::Project { project } = core
                .handle(Request::AddProject {
                    path: repo.to_string_lossy().into_owned(),
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
            core.handle(Request::SetThreadWorkspace {
                thread_id: thread.id.clone(),
                worktree: true,
                base: None,
            })
            .await
            .unwrap();
            Self {
                core,
                dir,
                thread: thread.id,
            }
        }

        fn thread(&self) -> Thread {
            match self.core.snapshot(&Topic::Thread(self.thread.clone())) {
                Ok(Snapshot::Thread { thread, .. }) => thread,
                _ => panic!(),
            }
        }

        /// What GitHub says about the pull request from now on.
        fn github_says(&self, number: u64, state: &str, checks: &[(&str, &str, &str)]) {
            let rollup: Vec<Value> = checks
                .iter()
                .map(|(name, status, conclusion)| {
                    serde_json::json!({
                        "__typename": "CheckRun", "name": name, "workflowName": "CI",
                        "status": status, "conclusion": conclusion,
                    })
                })
                .collect();
            let pr = serde_json::json!({
                "number": number,
                "url": format!("https://github.com/acme/repo/pull/{number}"),
                "title": "Add a changelog",
                "state": state,
                "isDraft": false,
                "reviewDecision": "REVIEW_REQUIRED",
                "statusCheckRollup": rollup,
            });
            // Whole or not at all: `gh` may be reading it.
            let tmp = self.dir.path().join("pr.json.tmp");
            std::fs::write(&tmp, pr.to_string()).unwrap();
            std::fs::rename(tmp, self.dir.path().join("pr.json")).unwrap();
        }

        /// The stand-in's `pr view` calls so far.
        fn views(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("gh.log"))
                .unwrap_or_default()
                .lines()
                .filter(|l| l.starts_with("pr view "))
                .map(str::to_owned)
                .collect()
        }

        /// Send a message and wait for its turn to end.
        async fn turn(&self, text: &str) {
            let before = self.turns();
            self.core
                .handle(Request::SendMessage {
                    thread_id: self.thread.clone(),
                    text: text.into(),
                    attachments: vec![],
                })
                .await
                .unwrap();
            self.until("the turn ends", || {
                let turns = match self.core.snapshot(&Topic::Thread(self.thread.clone())) {
                    Ok(Snapshot::Thread { turns, .. }) => turns,
                    _ => panic!(),
                };
                turns.len() > before && turns.last().is_some_and(|t| t.status.is_terminal())
            })
            .await;
        }

        fn turns(&self) -> usize {
            match self.core.snapshot(&Topic::Thread(self.thread.clone())) {
                Ok(Snapshot::Thread { turns, .. }) => turns.len(),
                _ => panic!(),
            }
        }

        async fn until(&self, what: &str, done: impl Fn() -> bool) {
            for _ in 0..500 {
                if done() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("never happened: {what}");
        }

        async fn refresh(&self) {
            let answer = self
                .core
                .handle(Request::RefreshPullRequest {
                    thread_id: self.thread.clone(),
                })
                .await;
            assert_eq!(answer, Ok(Response::Ok));
        }
    }

    /// Thread updates on the bus since it was last drained.
    fn thread_updates(bus: &mut tokio::sync::broadcast::Receiver<BusMsg>) -> usize {
        let mut n = 0;
        while let Ok(msg) = bus.try_recv() {
            if let BusMsg::Event(env) = msg
                && matches!(env.event, Event::ThreadUpdated { .. })
            {
                n += 1;
            }
        }
        n
    }

    #[tokio::test]
    async fn a_pull_request_agentloft_opens_is_on_the_thread_at_once() {
        let f = Fixture::new().await;
        f.turn("Add a changelog").await;
        // The branch has no pull request yet: asked, and nothing shown.
        f.until("the branch is asked about", || !f.views().is_empty())
            .await;
        let thread = f.thread();
        let branch = thread.workspace.branch().unwrap().to_owned();
        assert_eq!(
            f.views()[0],
            format!("pr view {branch} --json {FIELDS}"),
            "looked for by branch"
        );
        assert_eq!(thread.pull_request, None);

        std::fs::write(PathBuf::from(&thread.cwd).join("CHANGELOG.md"), "# 1.0\n").unwrap();
        let answer = f
            .core
            .handle(Request::CreatePullRequest {
                thread_id: f.thread.clone(),
            })
            .await
            .unwrap();
        let url = "https://github.com/acme/repo/pull/7";
        assert_eq!(answer, Response::Text { text: url.into() });
        // Already there when the request is answered.
        let pr = f.thread().pull_request.expect("the thread has its PR");
        assert_eq!((pr.number, pr.url.as_str()), (7, url));
        assert_eq!(pr.title, "Add a changelog");
        assert_eq!(pr.state, PullRequestState::Open);
        assert!(pr.checks.is_empty());
        // Opening it is not activity: the sidebar keeps its order.
        assert_eq!(f.thread().updated_at_ms, thread.updated_at_ms);
        // Then GitHub is asked about it, by its address.
        f.until("the new PR is asked about", || {
            f.views()
                .iter()
                .any(|v| v.starts_with(&format!("pr view {url} ")))
        })
        .await;
        // And it is followed.
        assert!(lock(&f.core.pull_requests.due).contains_key(&f.thread));
    }

    #[tokio::test]
    async fn found_after_a_turn_refreshed_and_quiet_when_nothing_changed() {
        let f = Fixture::new().await;
        // The agent opened one itself during the turn.
        f.github_says(
            12,
            "OPEN",
            &[("build", "IN_PROGRESS", ""), ("lint", "QUEUED", "")],
        );
        f.turn("Open a pull request").await;
        f.until("the PR is found", || f.thread().pull_request.is_some())
            .await;
        let found = f.thread();
        let pr = found.pull_request.clone().unwrap();
        assert_eq!(pr.number, 12);
        assert_eq!(pr.title, "Add a changelog");
        assert_eq!(pr.state, PullRequestState::Open);
        assert_eq!(pr.review, Some(ReviewDecision::ReviewRequired));
        assert_eq!(
            (pr.checks.passing, pr.checks.failing, pr.checks.pending),
            (0, 0, 2)
        );
        assert!(found.native_session_id.is_some());

        // Checks finish: a refresh brings them, as one thread update that
        // leaves the thread where it is in the sidebar.
        let mut bus = f.core.subscribe();
        f.github_says(
            12,
            "OPEN",
            &[
                ("build", "COMPLETED", "SUCCESS"),
                ("lint", "COMPLETED", "FAILURE"),
            ],
        );
        f.refresh().await;
        let now = f.thread();
        let pr = now.pull_request.clone().unwrap();
        assert_eq!(
            (pr.checks.passing, pr.checks.failing, pr.checks.pending),
            (1, 1, 0)
        );
        assert_eq!(pr.checks.rollup(), Some(ChecksState::Failing));
        assert_eq!(now.updated_at_ms, found.updated_at_ms);
        assert_eq!(thread_updates(&mut bus), 1);
        // It was asked about by its address, not by branch.
        let views = f.views();
        assert!(
            views
                .last()
                .unwrap()
                .starts_with("pr view https://github.com/acme/repo/pull/12 "),
            "{views:?}"
        );

        // Nothing new: `gh` is asked, and no thread update goes out.
        let asked = f.views().len();
        f.refresh().await;
        assert_eq!(f.views().len(), asked + 1);
        assert_eq!(thread_updates(&mut bus), 0);
        assert_eq!(f.thread(), now);

        // The poller asks about what is due, and only that.
        f.core.poll_due_pull_requests().await;
        assert_eq!(f.views().len(), asked + 1, "not due yet");
        lock(&f.core.pull_requests.due).insert(f.thread.clone(), Instant::now());
        f.github_says(
            12,
            "OPEN",
            &[
                ("build", "COMPLETED", "SUCCESS"),
                ("lint", "IN_PROGRESS", ""),
            ],
        );
        f.core.poll_due_pull_requests().await;
        assert_eq!(f.views().len(), asked + 2);
        assert_eq!(f.thread().pull_request.unwrap().checks.pending, 1);
        assert_eq!(thread_updates(&mut bus), 1);

        // Merged: shown, and never asked about again.
        f.github_says(12, "MERGED", &[("build", "COMPLETED", "SUCCESS")]);
        lock(&f.core.pull_requests.due).insert(f.thread.clone(), Instant::now());
        f.core.poll_due_pull_requests().await;
        assert_eq!(
            f.thread().pull_request.unwrap().state,
            PullRequestState::Merged
        );
        assert!(!lock(&f.core.pull_requests.due).contains_key(&f.thread));
        let asked = f.views().len();
        f.core.follow_open_pull_requests();
        f.core.poll_due_pull_requests().await;
        assert_eq!(f.views().len(), asked);
    }

    #[tokio::test]
    async fn archived_threads_are_left_alone() {
        let f = Fixture::new().await;
        f.github_says(12, "OPEN", &[("build", "IN_PROGRESS", "")]);
        f.turn("Open a pull request").await;
        f.until("the PR is found", || f.thread().pull_request.is_some())
            .await;
        assert!(lock(&f.core.pull_requests.due).contains_key(&f.thread));
        f.core
            .handle(Request::ArchiveThread {
                thread_id: f.thread.clone(),
                force: true,
            })
            .await
            .unwrap();
        // Still on the thread, for when it comes back.
        let archived = f.thread();
        assert_eq!(archived.status, ThreadStatus::Archived);
        assert_eq!(archived.pull_request.as_ref().unwrap().number, 12);

        let asked = f.views().len();
        let mut bus = f.core.subscribe();
        f.github_says(12, "OPEN", &[("build", "COMPLETED", "SUCCESS")]);
        // Due, and asked for by the user too: neither reaches `gh`.
        lock(&f.core.pull_requests.due).insert(f.thread.clone(), Instant::now());
        f.core.poll_due_pull_requests().await;
        f.refresh().await;
        f.core.follow_open_pull_requests();
        f.core.poll_due_pull_requests().await;
        assert_eq!(f.views().len(), asked);
        assert_eq!(thread_updates(&mut bus), 0);
        assert!(!lock(&f.core.pull_requests.due).contains_key(&f.thread));
        assert_eq!(f.thread(), archived);
    }

    #[tokio::test]
    async fn without_gh_nothing_is_shown_and_nothing_fails() {
        let f = Fixture::new().await;
        // GitHub has a pull request, but there is no `gh` to ask.
        f.github_says(12, "OPEN", &[("build", "IN_PROGRESS", "")]);
        std::fs::remove_file(f.dir.path().join("bin/gh")).unwrap();
        let said = &f.core.pull_requests.unavailable_logged;
        assert!(!said.load(Ordering::Relaxed));
        let mut bus = f.core.subscribe();
        f.turn("Add a changelog").await;
        // The turn's own look, then the user's: both answered quietly.
        f.until("the missing gh is noticed", || said.load(Ordering::Relaxed))
            .await;
        f.refresh().await;
        let thread = f.thread();
        assert_eq!(thread.pull_request, None);
        assert_eq!(thread.status, ThreadStatus::Idle);
        assert!(lock(&f.core.pull_requests.due).is_empty());
        // Nothing was said in the thread about it.
        while let Ok(msg) = bus.try_recv() {
            if let BusMsg::Event(env) = msg
                && let Event::ItemUpserted { item } = &env.event
            {
                assert!(
                    !matches!(
                        item.body,
                        loft_protocol::ItemBody::Notice { .. }
                            | loft_protocol::ItemBody::Error { .. }
                    ),
                    "{item:?}"
                );
            }
        }
    }
}
