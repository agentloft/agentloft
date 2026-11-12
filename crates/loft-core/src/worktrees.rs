//! Per-thread worktrees (ADR-0008, PLAN §5.2): chosen before the first
//! message, created when it is sent (branch named after it), squash-merged
//! back into the base branch, removed on archive or once its work is in the
//! base (`crate::cleanup`), and made again by the next message.

use crate::thread::ThreadCmd;
use crate::{Core, CoreResult, internal, lock};
use loft_git::worktree as wt;
use loft_protocol::{
    ErrorBody, ErrorCode, Event, ItemBody, ProjectId, Response, Thread, ThreadId, ThreadStatus,
    ThreadWorkspace, WorktreeBase,
};
use loft_store::now_ms;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn not_found() -> ErrorBody {
    ErrorBody::new(ErrorCode::NotFound, "thread not found")
}

pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> CoreResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(internal)?
        .map_err(|e| ErrorBody::new(ErrorCode::Conflict, format!("{e:#}")))
}

/// Where a new worktree starts (`start`, a rev) and the branch it merges
/// back into (`base`). A remote base is fetched first; offline, the last
/// fetched copy is used. No choice means the main branch, from `origin`
/// when the repository has one.
fn worktree_start(
    repo: &Path,
    requested: Option<WorktreeBase>,
    env: &std::collections::HashMap<String, String>,
) -> anyhow::Result<(String, String)> {
    let from_remote = |remote: &str, branch: &str| -> anyhow::Result<String> {
        let rev = format!("{remote}/{branch}");
        if let Err(err) = wt::fetch(repo, remote, branch, env) {
            if !wt::rev_exists(repo, &rev) {
                return Err(err.context(format!("couldn't fetch {rev}")));
            }
            tracing::warn!("fetch {rev} failed, using the last fetched copy: {err:#}");
        }
        Ok(rev)
    };
    match requested {
        Some(WorktreeBase {
            branch,
            remote: Some(remote),
        }) => Ok((from_remote(&remote, &branch)?, branch)),
        Some(WorktreeBase {
            branch,
            remote: None,
        }) => {
            anyhow::ensure!(wt::rev_exists(repo, &branch), "no branch `{branch}`");
            Ok((branch.clone(), branch))
        }
        None => match wt::default_branch(repo) {
            Some(branch) if wt::rev_exists(repo, &format!("origin/{branch}")) => {
                Ok((from_remote("origin", &branch)?, branch))
            }
            Some(branch) => Ok((branch.clone(), branch)),
            // No branches yet (or detached): start from the commit.
            None => {
                loft_git::repo_root(repo).ok_or_else(|| anyhow::anyhow!("not a git repository"))?;
                Ok(("HEAD".to_owned(), "HEAD".to_owned()))
            }
        },
    }
}

impl Core {
    pub(crate) async fn list_branches(&self, project_id: &ProjectId) -> CoreResult<Response> {
        let project = self
            .with_store(|s| s.project(project_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        let repo = PathBuf::from(project.root_path);
        let list = blocking(move || wt::branches(&repo)).await?;
        Ok(Response::Branches {
            branches: loft_protocol::BranchList {
                local: list.local,
                remote: list
                    .remote
                    .into_iter()
                    .map(|(remote, branch)| loft_protocol::RemoteBranch { remote, branch })
                    .collect(),
                default_branch: list.default,
                current: list.current,
            },
        })
    }

    pub(crate) fn load_thread(&self, id: &ThreadId) -> CoreResult<Thread> {
        self.with_store(|s| s.thread(id))?.ok_or_else(not_found)
    }

    pub(crate) fn save_thread(&self, thread: &Thread) -> CoreResult<()> {
        self.commit(vec![Event::ThreadUpdated {
            thread: thread.clone(),
        }])
        .map(|_| ())
    }

    /// Workspace for a new thread from `[worktrees] mode`.
    pub(crate) fn default_workspace(&self, project_root: &str) -> ThreadWorkspace {
        let wants = self.config().worktrees.mode == "worktree";
        if wants && loft_git::repo_root(Path::new(project_root)).is_some() {
            ThreadWorkspace::NewWorktree { base: None }
        } else {
            ThreadWorkspace::Local
        }
    }

    pub(crate) fn set_thread_workspace(
        &self,
        id: &ThreadId,
        worktree: bool,
        base: Option<WorktreeBase>,
    ) -> CoreResult<Response> {
        let mut thread = self.load_thread(id)?;
        if !self.with_store(|s| s.turns(id))?.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "a thread keeps its workspace once it has started; fork it instead",
            ));
        }
        if worktree && loft_git::repo_root(Path::new(&thread.cwd)).is_none() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "worktrees need a git repository",
            ));
        }
        thread.workspace = if worktree {
            ThreadWorkspace::NewWorktree { base }
        } else {
            ThreadWorkspace::Local
        };
        thread.updated_at_ms = now_ms();
        lock(&self.threads).remove(id);
        self.save_thread(&thread)?;
        Ok(Response::Thread { thread })
    }

    /// Before a thread's first message: create its pending worktree, named
    /// after the message, and move the thread into it. A thread whose
    /// worktree was cleaned up gets a new one, at the same folder, from its
    /// base as it is now.
    pub(crate) async fn ensure_workspace(
        self: &Arc<Self>,
        id: &ThreadId,
        text: &str,
    ) -> CoreResult<()> {
        let _guard = self.worktree_lock.lock().await;
        let mut thread = self.load_thread(id)?;
        let (requested, again) = match thread.workspace.clone() {
            ThreadWorkspace::NewWorktree { base } => (base, None),
            ThreadWorkspace::Released {
                path, branch, base, ..
            } => (None, Some((path, branch, base))),
            _ => return Ok(()),
        };
        let env = self.opts().env.clone();
        let project = self
            .with_store(|s| s.project(&thread.project_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        let repo = PathBuf::from(&project.root_path);
        let prefix = self.config().worktrees.branch_prefix.clone();
        let dir = self
            .opts()
            .worktrees_dir
            .join(slug_or(&project.name, "project"));
        let slug = match &again {
            // The old branch's name (`-2` if it is still there).
            Some((_, branch, _)) => branch
                .strip_prefix(&format!("{prefix}/"))
                .or_else(|| branch.rsplit('/').next())
                .unwrap_or("thread")
                .to_owned(),
            None => slug_or(text, "thread"),
        };
        let project_cfg = loft_config::ProjectConfig::load(&repo);
        let setup = project_cfg.worktree.setup.clone();
        let wanted = again
            .as_ref()
            .map(|(path, _, base)| (path.clone(), base.clone()));
        let (workspace, start) = blocking(move || {
            let requested = match &wanted {
                // Its base as the remote has it now, if it has it.
                Some((_, base)) if base != "HEAD" => Some(WorktreeBase {
                    branch: base.clone(),
                    remote: wt::base_remote(&repo, base)
                        .filter(|r| wt::rev_exists(&repo, &format!("{r}/{base}"))),
                }),
                Some(_) => None,
                None => requested,
            };
            let (start, base) = worktree_start(&repo, requested, &env)?;
            let branch = wt::unique_branch(&repo, &prefix, &slug);
            let leaf = branch.rsplit('/').next().unwrap_or(&slug).to_owned();
            // The same folder as before, so the agent's session (kept per
            // folder) carries on.
            let mut path = match &wanted {
                Some((old, _)) if !Path::new(old).exists() => PathBuf::from(old),
                _ => dir.join(&leaf),
            };
            let mut n = 2;
            while path.exists() {
                path = dir.join(format!("{leaf}-{n}"));
                n += 1;
            }
            wt::add(&repo, &path, &branch, &start)?;
            let copied = wt::copy_untracked(&repo, &path, &project_cfg.worktree.copy);
            tracing::info!(branch, path = %path.display(), ?copied, "worktree created");
            Ok((
                ThreadWorkspace::Worktree {
                    path: path.to_string_lossy().into_owned(),
                    branch,
                    base,
                },
                start,
            ))
        })
        .await?;
        if let (Some((old_path, old_branch, _)), ThreadWorkspace::Worktree { path, branch, .. }) =
            (&again, &workspace)
        {
            let notice = format!("New worktree on {branch} from {start}");
            let moved = if old_path == path {
                String::new()
            } else {
                format!(" at {path}")
            };
            let note = format!(
                "[Agentloft: this thread's worktree (branch `{old_branch}`) was removed once its work was in {start}. You are in a new worktree{moved} on branch `{branch}`, made from {start} as it is now. Files may differ from what you saw earlier; re-read them before relying on earlier edits.]"
            );
            lock(&self.pending_notes).insert(id.clone(), (notice, note));
        }
        if let ThreadWorkspace::Worktree { path, .. } = &workspace {
            thread.cwd = path.clone();
        }
        thread.workspace = workspace;
        thread.updated_at_ms = now_ms();
        if let Some(cmd) = setup.filter(|c| !c.trim().is_empty()) {
            lock(&self.pending_setup).insert(id.clone(), cmd);
        }
        // An actor spawned earlier (e.g. by a model switch) has the old cwd.
        lock(&self.threads).remove(id);
        self.save_thread(&thread)
    }

    pub(crate) async fn merge_thread(
        self: &Arc<Self>,
        id: &ThreadId,
        message: Option<String>,
    ) -> CoreResult<Response> {
        let thread = self.load_thread(id)?;
        let ThreadWorkspace::Worktree { path, branch, base } = thread.workspace.clone() else {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "only worktree threads can be merged back",
            ));
        };
        if matches!(
            thread.status,
            ThreadStatus::Running | ThreadStatus::WaitingApproval
        ) {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "wait for the turn to finish before merging",
            ));
        }
        let project = self
            .with_store(|s| s.project(&thread.project_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        let first_prompt =
            self.with_store(|s| s.items(id))?
                .into_iter()
                .find_map(|i| match i.body {
                    ItemBody::UserMessage { text, .. } => Some(text),
                    _ => None,
                });
        let message = message
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| commit_message(first_prompt.as_deref(), &thread.title));
        let repo = PathBuf::from(project.root_path);
        let base2 = base.clone();
        let out =
            blocking(move || wt::squash_merge(&repo, Path::new(&path), &branch, &base2, &message))
                .await?;
        // Now in the base: the thread may offer (or do) its cleanup.
        let (core, tid) = (self.clone(), id.clone());
        tokio::spawn(async move { core.check_landed(&tid, false).await });
        let short = &out.commit[..out.commit.len().min(7)];
        let files = if out.files == 1 { "file" } else { "files" };
        Ok(Response::Text {
            text: format!("Merged {} {files} into {base} ({short})", out.files),
        })
    }

    /// Restore files to a turn's before-checkpoint (PLAN §5.3).
    pub(crate) async fn revert_turn(
        self: &Arc<Self>,
        id: &ThreadId,
        turn_id: &loft_protocol::TurnId,
    ) -> CoreResult<Response> {
        let thread = self.load_thread(id)?;
        if matches!(
            thread.status,
            ThreadStatus::Running | ThreadStatus::WaitingApproval
        ) {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "stop the running turn before reverting",
            ));
        }
        if matches!(thread.workspace, ThreadWorkspace::Released { .. }) {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "this thread's worktree was cleaned up; its next message gets a new one",
            ));
        }
        let turn = self
            .with_store(|s| s.turns(id))?
            .into_iter()
            .find(|t| &t.id == turn_id)
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "turn not found"))?;
        let before = turn.checkpoint_before.clone().ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::BadRequest,
                "this turn has no checkpoint (not a git repository?)",
            )
        })?;
        let dir = PathBuf::from(&thread.cwd);
        let changed = blocking(move || loft_git::restore_worktree(&dir, &before)).await?;
        let ordinal = turn.ordinal;
        let files = changed.len();
        self.thread_call(id, |reply| ThreadCmd::Reverted {
            ordinal,
            files,
            reply,
        })
        .await
    }

    /// Commit + push the branch, then `gh pr create` (PLAN §5.2).
    pub(crate) async fn create_pull_request(
        self: &Arc<Self>,
        id: &ThreadId,
    ) -> CoreResult<Response> {
        let thread = self.load_thread(id)?;
        let ThreadWorkspace::Worktree { path, branch, base } = thread.workspace.clone() else {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "only worktree threads can open a pull request",
            ));
        };
        if matches!(
            thread.status,
            ThreadStatus::Running | ThreadStatus::WaitingApproval
        ) {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "wait for the turn to finish before opening a pull request",
            ));
        }
        let env = self.opts().env.clone();
        let gh = loft_config::which("gh", env.get("PATH").map(String::as_str).unwrap_or(""))
            .ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::BadRequest,
                    "GitHub CLI (`gh`) not found — install it and run `gh auth login`",
                )
            })?;
        let items = self.with_store(|s| s.items(id))?;
        let first_prompt = items.iter().find_map(|i| match &i.body {
            ItemBody::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        });
        let message = commit_message(first_prompt.as_deref(), &thread.title);
        let title = message.lines().next().unwrap_or(&thread.title).to_owned();
        let files = self
            .diff(id, loft_protocol::DiffScope::Thread)
            .await
            .map(|d| d.files)
            .unwrap_or_default();
        let body = crate::handoff::pr_body(
            &thread,
            &crate::forks::harness_display(&thread.harness),
            &items,
            &files,
        );
        let pr_title = title.clone();
        let url = blocking(move || {
            let dir = Path::new(&path);
            wt::commit_and_push(dir, &branch, &base, &message, &env, Some(&gh))?;
            gh_pr_create(&gh, dir, &branch, &base, &title, &body, &env)
        })
        .await?;
        // On the thread before the answer; its checks and review follow.
        self.pull_request_created(id, &url, &pr_title).await;
        Ok(Response::Text { text: url })
    }

    pub(crate) async fn unarchive_thread(self: &Arc<Self>, id: &ThreadId) -> CoreResult<Response> {
        let mut thread = self.load_thread(id)?;
        if thread.status != ThreadStatus::Archived {
            return Ok(Response::Thread { thread });
        }
        if let ThreadWorkspace::Worktree { path, branch, base } = &thread.workspace
            && !Path::new(path).exists()
        {
            let project = self
                .with_store(|s| s.project(&thread.project_id))?
                .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
            let (repo, dir, b) = (
                PathBuf::from(project.root_path),
                PathBuf::from(path),
                branch.clone(),
            );
            let restored = blocking(move || {
                wt::prune(&repo)?;
                if !wt::has_branch(&repo, &b) {
                    return Ok(false);
                }
                wt::add_existing(&repo, &dir, &b)?;
                Ok(true)
            })
            .await?;
            // Its branch is gone: the next message makes a new worktree.
            if !restored {
                thread.workspace = ThreadWorkspace::Released {
                    path: path.clone(),
                    branch: branch.clone(),
                    base: base.clone(),
                    tip: None,
                };
            }
        }
        thread.status = ThreadStatus::Idle;
        thread.updated_at_ms = now_ms();
        self.save_thread(&thread)?;
        Ok(Response::Thread { thread })
    }

    pub(crate) async fn archive_thread(
        self: &Arc<Self>,
        id: &ThreadId,
        force: bool,
    ) -> CoreResult<Response> {
        let thread = self.load_thread(id)?;
        // A fork sharing this worktree keeps it alive.
        let shared = self.shares_folder(&thread)?;
        if let ThreadWorkspace::Worktree { path, .. } = &thread.workspace
            && !shared
        {
            let dir = PathBuf::from(path);
            if !force && dir.exists() {
                let d = dir.clone();
                if blocking(move || wt::is_dirty(&d)).await? {
                    return Err(ErrorBody::new(
                        ErrorCode::Conflict,
                        "The worktree has uncommitted changes",
                    ));
                }
            }
        }
        // Everything in it is in the base: its branch can go too.
        let cfg = self.config().worktrees.clone();
        let drop_branch = cfg.cleans_up() && cfg.delete_branch && !shared;
        // Stop the agent session, then the thread's shell.
        self.stop_thread_actor(id).await;
        let mut thread = self.load_thread(id)?;
        if let ThreadWorkspace::Worktree { path, branch, base } = thread.workspace.clone()
            && !shared
        {
            let project = self
                .with_store(|s| s.project(&thread.project_id))?
                .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
            let (repo, dir, b) = (
                PathBuf::from(project.root_path),
                PathBuf::from(&path),
                branch.clone(),
            );
            let head = thread
                .pull_request
                .as_ref()
                .filter(|pr| pr.state == loft_protocol::PullRequestState::Merged)
                .and_then(|pr| pr.head.clone());
            let base2 = base.clone();
            let tip = blocking(move || {
                // Looked at again on the spot: nothing new since.
                let landed = drop_branch
                    && wt::worktree_landed(&repo, &dir, &b, &base2, head.as_deref(), None)
                        .unwrap_or(false);
                if dir.exists() {
                    wt::remove(&repo, &dir, force)?;
                }
                wt::prune(&repo)?;
                if !landed {
                    return Ok(None);
                }
                let tip = wt::tip(&repo, &b)?;
                wt::delete_branch(&repo, &b)?;
                Ok(Some(tip))
            })
            .await?;
            if tip.is_some() {
                thread.workspace = ThreadWorkspace::Released {
                    path,
                    branch,
                    base,
                    tip,
                };
            }
        }
        thread.landed = false;
        thread.status = ThreadStatus::Archived;
        thread.updated_at_ms = now_ms();
        self.save_thread(&thread)?;
        Ok(Response::Thread { thread })
    }
}

impl Core {
    /// Delete a thread: stop it, remove its worktree (keeping the branch)
    /// and checkpoint refs, then erase it from the store.
    pub(crate) async fn delete_thread(self: &Arc<Self>, id: &ThreadId) -> CoreResult<Response> {
        let thread = self.load_thread(id)?;
        if thread.status != ThreadStatus::Archived {
            self.archive_thread(id, true).await?;
        }
        // Archived before, its branch kept: it goes if its work is in the
        // base (and nobody else is on it).
        let cfg = self.config().worktrees.clone();
        let thread = self.load_thread(id)?;
        let landed_branch = match &thread.workspace {
            ThreadWorkspace::Worktree { path, branch, base }
                if cfg.cleans_up() && cfg.delete_branch && !Path::new(path).exists() =>
            {
                let head = thread
                    .pull_request
                    .as_ref()
                    .filter(|pr| pr.state == loft_protocol::PullRequestState::Merged)
                    .and_then(|pr| pr.head.clone());
                Some((branch.clone(), base.clone(), head))
            }
            _ => None,
        };
        let others_on_it = landed_branch.as_ref().is_some_and(|(branch, ..)| {
            self.with_store(|s| s.threads()).is_ok_and(|threads| {
                threads
                    .iter()
                    .any(|t| t.id != *id && t.workspace.branch() == Some(branch.as_str()))
            })
        });
        if let Some(project) = self.with_store(|s| s.project(&thread.project_id))? {
            let repo = PathBuf::from(project.root_path);
            let prefix = format!("{}/{}", loft_git::REF_PREFIX, id.as_str());
            let landed_branch = landed_branch.filter(|_| !others_on_it);
            let _ = blocking(move || {
                if loft_git::repo_root(&repo).is_some() {
                    loft_git::delete_refs(&repo, &prefix)?;
                    if let Some((branch, base, head)) = landed_branch
                        && wt::has_branch(&repo, &branch)
                        && wt::branch_landed(&repo, &branch, &base, head.as_deref(), None)?
                    {
                        wt::delete_branch(&repo, &branch)?;
                    }
                }
                Ok(())
            })
            .await;
        }
        // Its uploaded images, and a chat's own folder.
        let attachments = self.opts().attachments_dir.join(id.as_str());
        let chats = self.opts().chats_dir.clone();
        let cwd = PathBuf::from(&thread.cwd);
        let own_chat_dir = cwd.file_name().is_some_and(|n| n == id.as_str())
            && cwd.parent().is_some_and(|parent| same_dir(parent, &chats));
        let _ = blocking(move || {
            let _ = std::fs::remove_dir_all(&attachments);
            if own_chat_dir {
                let _ = std::fs::remove_dir_all(&cwd);
            }
            Ok(())
        })
        .await;
        self.commit(vec![Event::ThreadDeleted {
            thread_id: id.clone(),
        }])?;
        Ok(Response::Ok)
    }

    /// Delete the archived threads that have been archived for longer than
    /// `threads.delete_archived_after_days` (0 = keep them). Returns how
    /// many went.
    pub async fn delete_expired_archived(self: &Arc<Self>) -> usize {
        let days = self.config().threads.delete_archived_after_days;
        if days == 0 {
            return 0;
        }
        let cutoff = now_ms() - i64::from(days) * 24 * 60 * 60 * 1000;
        let expired: Vec<ThreadId> = match self.with_store(|s| s.threads()) {
            Ok(threads) => threads
                .into_iter()
                // Archiving stamps `updated_at_ms`.
                .filter(|t| t.status == ThreadStatus::Archived && t.updated_at_ms < cutoff)
                .map(|t| t.id)
                .collect(),
            Err(_) => return 0,
        };
        let mut deleted = 0;
        for id in expired {
            match self.delete_thread(&id).await {
                Ok(_) => deleted += 1,
                Err(err) => {
                    tracing::warn!(thread = %id.as_str(), err = %err.message, "couldn't delete expired archived thread")
                }
            }
        }
        if deleted > 0 {
            tracing::info!(deleted, days, "deleted expired archived threads");
        }
        deleted
    }

    /// Run [`Self::delete_expired_archived`] now, every hour, and whenever
    /// the config is reloaded; [`Self::check_all_landed`] now and hourly. Ends when the core is dropped (or abort it).
    pub fn spawn_archive_cleanup(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let core = Arc::downgrade(self);
        let mut changes = self.config_changes();
        tokio::spawn(async move {
            let mut swept: Option<std::time::Instant> = None;
            loop {
                let Some(c) = core.upgrade() else { return };
                c.delete_expired_archived().await;
                // Worktrees whose work went into their base meanwhile
                // (merged on GitHub, or elsewhere). Hourly, not on every
                // settings change: it fetches.
                if swept.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(55 * 60)) {
                    swept = Some(std::time::Instant::now());
                    c.check_all_landed().await;
                }
                drop(c);
                tokio::select! {
                    () = tokio::time::sleep(std::time::Duration::from_secs(60 * 60)) => {}
                    changed = changes.changed() => if changed.is_err() { return },
                }
            }
        })
    }
}

/// Whether two paths name the same folder (following symlinks when they
/// exist, e.g. /tmp vs /private/tmp).
fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// `gh pr create`; an already-open PR for the branch is returned instead.
fn gh_pr_create(
    gh: &Path,
    dir: &Path,
    branch: &str,
    base: &str,
    title: &str,
    body: &str,
    env: &std::collections::HashMap<String, String>,
) -> anyhow::Result<String> {
    use std::io::Write as _;
    let mut child = std::process::Command::new(gh)
        .current_dir(dir)
        .env_clear()
        .envs(env)
        .env("GH_PROMPT_DISABLED", "1")
        .args([
            "pr", "create", "--base", base, "--head", branch, "--title", title,
        ])
        .args(["--body-file", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let url = |text: &str| {
        text.split_whitespace()
            .find(|w| w.starts_with("https://"))
            .map(str::to_owned)
    };
    if out.status.success() {
        return url(&stdout).ok_or_else(|| anyhow::anyhow!("gh printed no URL: {stdout}"));
    }
    // "a pull request for branch … already exists: https://…"
    if stderr.contains("already exists")
        && let Some(existing) = url(&stderr)
    {
        return Ok(existing);
    }
    anyhow::bail!("gh pr create failed: {}", stderr.trim())
}

/// Squash commit message: the first prompt's first line as the subject
/// (≤72 chars, cut at a word), the rest of the prompt as the body.
fn commit_message(prompt: Option<&str>, title: &str) -> String {
    let Some(prompt) = prompt.map(str::trim).filter(|p| !p.is_empty()) else {
        return title.to_owned();
    };
    let (first, rest) = prompt.split_once('\n').unwrap_or((prompt, ""));
    let first = first.trim();
    let subject = if first.chars().count() <= 72 {
        first.to_owned()
    } else {
        let cut: String = first.chars().take(72).collect();
        match cut.rfind(' ') {
            Some(i) if i > 30 => cut[..i].trim_end_matches([',', '.', ':', ';']).to_owned(),
            _ => cut,
        }
    };
    let mut msg = subject.clone();
    let body = if subject == first {
        rest.trim()
    } else {
        prompt
    };
    if !body.is_empty() {
        msg.push_str("\n\n");
        msg.push_str(body);
    }
    msg.push_str("\n\nMerged from an Agentloft worktree thread.");
    msg
}

pub(crate) fn slug_or(text: &str, fallback: &str) -> String {
    let slug = wt::slugify(text, 40);
    if slug.is_empty() {
        fallback.to_owned()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::{WorktreeBase, commit_message, worktree_start};
    use std::path::Path;
    use std::process::Command;

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

    #[test]
    fn worktree_start_points() {
        let repo = tempfile::tempdir().unwrap();
        let r = repo.path();
        git(r, &["init", "-q", "-b", "main"]);
        git(r, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let env = std::env::vars().collect();
        // No remote: the main branch, as the local copy has it.
        assert_eq!(
            worktree_start(r, None, &env).unwrap(),
            ("main".into(), "main".into())
        );
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        git(
            r,
            &["remote", "add", "origin", &remote.path().to_string_lossy()],
        );
        git(r, &["push", "-q", "origin", "main"]);
        // With origin: fetched, from origin/main; merges back into main.
        assert_eq!(
            worktree_start(r, None, &env).unwrap(),
            ("origin/main".into(), "main".into())
        );
        let local = WorktreeBase {
            branch: "main".into(),
            remote: None,
        };
        assert_eq!(
            worktree_start(r, Some(local), &env).unwrap(),
            ("main".into(), "main".into())
        );
        let missing = WorktreeBase {
            branch: "nope".into(),
            remote: Some("origin".into()),
        };
        assert!(worktree_start(r, Some(missing), &env).is_err());
    }

    #[test]
    fn commit_messages() {
        assert_eq!(
            commit_message(Some("Add a greet function"), "t"),
            "Add a greet function\n\nMerged from an Agentloft worktree thread."
        );
        let long =
            "Create hello.py with a function hello(name) that returns a greeting, plus a tiny test";
        let msg = commit_message(Some(long), "t");
        let subject = msg.lines().next().unwrap();
        assert_eq!(
            subject,
            "Create hello.py with a function hello(name) that returns a greeting"
        );
        assert!(msg.contains(long), "full prompt kept in the body");
        assert_eq!(commit_message(None, "Title"), "Title");
    }
}
