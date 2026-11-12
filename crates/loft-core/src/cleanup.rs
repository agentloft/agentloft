//! Cleaning up worktrees whose work is already in their base branch
//! (`[worktrees] cleanup`). A worktree thread is *landed* when nothing is
//! uncommitted in its worktree and its branch's work is in the base (merged
//! back, squashed or not, or its pull request merged). Then its worktree,
//! and per the settings its branch, can go without losing anything. The
//! thread stays (`ThreadWorkspace::Released`); its next message gets a new
//! worktree at the same folder, from the base as it is then.
//!
//! Looked at when a turn ends (nothing fetched), when its pull request is
//! merged, after Merge, and every hour for every worktree thread (the base
//! fetched once per repository). `offer` shows it on the thread; `auto`
//! cleans up there and then, as long as nothing runs in the worktree.
//! Only local worktrees and branches go by themselves: a remote branch is
//! deleted only when the user cleans up and has asked for that.

use crate::thread::ThreadCmd;
use crate::worktrees::blocking;
use crate::{Core, CoreResult, lock};
use loft_git::worktree as wt;
use loft_protocol::{
    ErrorBody, ErrorCode, ProjectId, PullRequestState, Response, Thread, ThreadId, ThreadStatus,
    ThreadWorkspace, WorktreeLeftover,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

/// A turn ended: did the agent's work land (it may have merged it
/// itself)? Nothing is fetched.
pub(crate) fn turn_ended(core: &Arc<Core>, thread: &Thread) {
    if !matches!(thread.workspace, ThreadWorkspace::Worktree { .. }) {
        return;
    }
    let (core, id) = (core.clone(), thread.id.clone());
    tokio::spawn(async move { core.check_landed(&id, false).await });
}

/// The head of the thread's merged pull request, if it has one.
fn merged_head(thread: &Thread) -> Option<String> {
    thread
        .pull_request
        .as_ref()
        .filter(|pr| pr.state == PullRequestState::Merged)
        .and_then(|pr| pr.head.clone())
}

fn conflict(text: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::Conflict, text)
}

impl Core {
    /// Another thread that isn't archived works in this thread's folder (a
    /// fork sharing its worktree).
    pub(crate) fn shares_folder(&self, thread: &Thread) -> CoreResult<bool> {
        Ok(self.with_store(|s| s.threads())?.iter().any(|t| {
            t.id != thread.id && t.status != ThreadStatus::Archived && t.cwd == thread.cwd
        }))
    }

    /// Stop the thread's agent session (its actor) and its terminals.
    pub(crate) async fn stop_thread_actor(&self, id: &ThreadId) {
        let handle = lock(&self.threads).remove(id);
        if let Some(handle) = handle {
            let (tx, rx) = oneshot::channel();
            if handle.send(ThreadCmd::Shutdown { done: tx }).is_ok() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(10), rx).await;
            }
        }
        self.terminals
            .close_prefix(&format!("thread:{}", id.as_str()));
    }

    /// Whether the thread's worktree can go now without losing anything.
    /// `fetch`: bring the base up to date from its remote first.
    async fn look_landed(&self, thread: &Thread, fetch: bool) -> CoreResult<bool> {
        let ThreadWorkspace::Worktree { path, branch, base } = thread.workspace.clone() else {
            return Ok(false);
        };
        if matches!(
            thread.status,
            ThreadStatus::Archived | ThreadStatus::Running | ThreadStatus::WaitingApproval
        ) || !thread.waiting_on.is_empty()
            || self.shares_folder(thread)?
        {
            return Ok(false);
        }
        let Some(project) = self.with_store(|s| s.project(&thread.project_id))? else {
            return Ok(false);
        };
        let repo = PathBuf::from(project.root_path);
        let head = merged_head(thread);
        let env = fetch.then(|| self.opts().env.clone());
        blocking(move || {
            wt::worktree_landed(
                &repo,
                Path::new(&path),
                &branch,
                &base,
                head.as_deref(),
                env.as_deref(),
            )
        })
        .await
    }

    /// Look whether a worktree thread's work has landed and record it on
    /// the thread; with `cleanup = "auto"`, clean up. Quiet: what fails is
    /// logged.
    pub(crate) async fn check_landed(self: &Arc<Self>, id: &ThreadId, fetch: bool) {
        let Ok(thread) = self.load_thread(id) else {
            return;
        };
        if !matches!(thread.workspace, ThreadWorkspace::Worktree { .. })
            || thread.status == ThreadStatus::Archived
        {
            return;
        }
        let cleans_up = self.config().worktrees.cleans_up();
        let landed = match self.look_landed(&thread, fetch).await {
            // Only shown (and acted on) when cleaning up is on.
            Ok(landed) => landed && cleans_up,
            Err(err) => {
                tracing::debug!(thread = %id.as_str(), err = %err.message, "landed check");
                return;
            }
        };
        if landed != thread.landed
            && let Err(err) = self
                .thread_call(id, |reply| ThreadCmd::SetLanded { landed, reply })
                .await
        {
            tracing::debug!(thread = %id.as_str(), err = %err.message, "landed not saved");
            return;
        }
        if landed && self.config().worktrees.cleans_up_automatically() {
            match self.clean_up_worktree(id, false).await {
                Ok(_) => tracing::info!(thread = %id.as_str(), "cleaned up a merged worktree"),
                Err(err) => {
                    tracing::debug!(thread = %id.as_str(), err = %err.message, "no automatic cleanup")
                }
            }
        }
    }

    /// Look at every worktree thread (the hourly sweep): each repository's
    /// bases are fetched once, then each thread is looked at.
    pub async fn check_all_landed(self: &Arc<Self>) {
        let Ok(threads) = self.with_store(|s| s.threads()) else {
            return;
        };
        let threads: Vec<Thread> = threads
            .into_iter()
            .filter(|t| {
                t.status != ThreadStatus::Archived
                    && matches!(t.workspace, ThreadWorkspace::Worktree { .. })
            })
            .collect();
        let mut fetched: HashSet<(ProjectId, String)> = HashSet::new();
        for thread in &threads {
            let ThreadWorkspace::Worktree { base, .. } = &thread.workspace else {
                continue;
            };
            if !fetched.insert((thread.project_id.clone(), base.clone())) {
                continue;
            }
            let Ok(Some(project)) = self.with_store(|s| s.project(&thread.project_id)) else {
                continue;
            };
            let (repo, base, env) = (
                PathBuf::from(project.root_path),
                base.clone(),
                self.opts().env.clone(),
            );
            let _ = blocking(move || {
                wt::base_revs(&repo, &base, Some(&env));
                Ok(())
            })
            .await;
        }
        for thread in threads {
            self.check_landed(&thread.id, false).await;
        }
    }

    /// Remove a landed worktree (and per the settings its branch); the
    /// thread is then `Released`. `by_user`: the user asked for it now, so
    /// the base is fetched first (the automatic cleanup comes right after a
    /// look that did) and, with `delete_remote_branch`, the remote branch
    /// goes too. Nothing on a remote is ever deleted unasked.
    pub(crate) async fn clean_up_worktree(
        self: &Arc<Self>,
        id: &ThreadId,
        by_user: bool,
    ) -> CoreResult<Response> {
        if by_user {
            // Outside the lock: other threads' worktrees needn't wait on
            // the network.
            let thread = self.load_thread(id)?;
            let _ = self.look_landed(&thread, true).await;
        }
        let _guard = self.worktree_lock.lock().await;
        let thread = self.load_thread(id)?;
        let ThreadWorkspace::Worktree { path, branch, base } = thread.workspace.clone() else {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "only a worktree thread has a worktree to clean up",
            ));
        };
        if thread.status == ThreadStatus::Archived {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "an archived thread's worktree is already gone",
            ));
        }
        if matches!(
            thread.status,
            ThreadStatus::Running | ThreadStatus::WaitingApproval
        ) {
            return Err(conflict("wait for the turn to finish before cleaning up"));
        }
        if !thread.waiting_on.is_empty() {
            return Err(conflict(
                "the agent still has work running in the background there",
            ));
        }
        if self.shares_folder(&thread)? {
            return Err(conflict("another thread works in this worktree"));
        }
        let prefix = format!("thread:{}", id.as_str());
        if self.terminals.busy_prefix(&prefix) {
            return Err(conflict(
                "a terminal of this thread is running something; stop it first",
            ));
        }
        // Decided on what is true now.
        if !self.look_landed(&thread, false).await? {
            if thread.landed {
                let _ = self
                    .thread_call(id, |reply| ThreadCmd::SetLanded {
                        landed: false,
                        reply,
                    })
                    .await;
            }
            return Err(conflict(format!(
                "not everything in this worktree is in {base} yet"
            )));
        }
        self.stop_thread_actor(id).await;
        // A message may have come in while the agent was stopping.
        let thread = self.load_thread(id)?;
        if matches!(
            thread.status,
            ThreadStatus::Running | ThreadStatus::WaitingApproval
        ) {
            return Err(conflict("a turn started; try again when it ends"));
        }
        let project = self
            .with_store(|s| s.project(&thread.project_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        let cfg = self.config().worktrees.clone();
        let env = self.opts().env.clone();
        let gh = loft_config::which("gh", env.get("PATH").map(String::as_str).unwrap_or(""));
        let pr_merged = merged_head(&thread).is_some();
        let (repo, dir, b) = (
            PathBuf::from(project.root_path),
            PathBuf::from(&path),
            branch.clone(),
        );
        let tip = blocking(move || {
            let tip = wt::tip(&repo, &b)?;
            wt::remove(&repo, &dir, false)?;
            let _ = wt::prune(&repo);
            if !cfg.delete_branch {
                return Ok(None);
            }
            if by_user
                && cfg.delete_remote_branch
                && pr_merged
                && let Some(remote) = wt::pushed_to(&repo, &b)
                && let Err(err) = wt::delete_remote_branch(&repo, &remote, &b, &env, gh.as_deref())
            {
                tracing::warn!(branch = b, "couldn't delete the remote branch: {err:#}");
            }
            wt::delete_branch(&repo, &b)?;
            Ok(Some(tip))
        })
        .await?;
        let mut thread = self.load_thread(id)?;
        thread.workspace = ThreadWorkspace::Released {
            path,
            branch,
            base,
            tip,
        };
        thread.landed = false;
        if cfg.archive_after_cleanup {
            thread.status = ThreadStatus::Archived;
            // Archiving stamps the time (for `delete_archived_after_days`).
            thread.updated_at_ms = loft_store::now_ms();
        }
        self.save_thread(&thread)?;
        Ok(Response::Thread { thread })
    }

    /// Put back the branch a cleanup deleted.
    pub(crate) async fn restore_branch(self: &Arc<Self>, id: &ThreadId) -> CoreResult<Response> {
        let _guard = self.worktree_lock.lock().await;
        let mut thread = self.load_thread(id)?;
        let ThreadWorkspace::Released {
            path,
            branch,
            base,
            tip: Some(tip),
        } = thread.workspace.clone()
        else {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "this thread has no deleted branch to bring back",
            ));
        };
        let project = self
            .with_store(|s| s.project(&thread.project_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        let (repo, b) = (PathBuf::from(project.root_path), branch.clone());
        blocking(move || {
            anyhow::ensure!(!wt::has_branch(&repo, &b), "a branch `{b}` exists already");
            wt::restore_branch(&repo, &b, &tip)
        })
        .await?;
        thread.workspace = ThreadWorkspace::Released {
            path,
            branch,
            base,
            tip: None,
        };
        self.save_thread(&thread)?;
        Ok(Response::Thread { thread })
    }

    /// Worktree folders and Agentloft branches no active thread works in.
    pub(crate) async fn worktree_leftovers(&self) -> CoreResult<Response> {
        let mut leftovers = Vec::new();
        for (project, scan) in self.leftover_scans()? {
            let found = blocking(move || scan.run(true)).await;
            match found {
                Ok(found) => leftovers.extend(found.into_iter().map(|f| f.into_leftover(&project))),
                Err(err) => {
                    tracing::debug!(project = %project.as_str(), err = %err.message, "leftovers")
                }
            }
        }
        Ok(Response::WorktreeLeftovers { leftovers })
    }

    /// Remove leftovers, each checked again first.
    pub(crate) async fn remove_worktree_leftovers(
        &self,
        wanted: Vec<WorktreeLeftover>,
    ) -> CoreResult<Response> {
        let _guard = self.worktree_lock.lock().await;
        let (mut folders, mut branches, mut kept) = (0, 0, 0);
        for (project, scan) in self.leftover_scans()? {
            let mine: Vec<&WorktreeLeftover> =
                wanted.iter().filter(|w| w.project_id == project).collect();
            if mine.is_empty() {
                continue;
            }
            let repo = scan.repo.clone();
            let found = blocking(move || scan.run(false)).await?;
            for want in mine {
                let Some(now) = found
                    .iter()
                    .find(|f| f.path == want.path && f.branch == want.branch)
                else {
                    continue;
                };
                if !now.landed {
                    kept += 1;
                    continue;
                }
                let (repo, path, branch) = (repo.clone(), now.path.clone(), now.branch.clone());
                let removed = blocking(move || {
                    if let Some(path) = &path {
                        wt::remove(&repo, Path::new(path), false)?;
                        let _ = wt::prune(&repo);
                    }
                    let tip = match &branch {
                        Some(b) if wt::has_branch(&repo, b) => {
                            let tip = wt::tip(&repo, b)?;
                            wt::delete_branch(&repo, b)?;
                            Some(tip)
                        }
                        _ => None,
                    };
                    Ok(tip)
                })
                .await;
                let tip = match removed {
                    Ok(tip) => tip,
                    Err(err) => {
                        tracing::warn!(err = %err.message, "couldn't remove a leftover");
                        kept += 1;
                        continue;
                    }
                };
                folders += usize::from(now.path.is_some());
                branches += usize::from(tip.is_some());
                // An archived thread's branch: it comes back as Released.
                if let Some(thread_id) = &now.thread_id
                    && let Ok(mut thread) = self.load_thread(thread_id)
                    && let ThreadWorkspace::Worktree { path, branch, base } =
                        thread.workspace.clone()
                    && now.branch.as_deref() == Some(branch.as_str())
                {
                    thread.workspace = ThreadWorkspace::Released {
                        path,
                        branch,
                        base,
                        tip,
                    };
                    self.save_thread(&thread)?;
                }
            }
        }
        let mut parts = Vec::new();
        let plural =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        if folders > 0 {
            parts.push(plural(folders, "worktree", "worktrees"));
        }
        if branches > 0 {
            parts.push(plural(branches, "branch", "branches"));
        }
        let mut text = if parts.is_empty() {
            "Nothing removed".to_owned()
        } else {
            format!("Removed {}", parts.join(" and "))
        };
        if kept > 0 {
            text.push_str(&format!(
                "; kept {} with work not in the main branch",
                plural(kept, "other", "others")
            ));
        }
        Ok(Response::Text { text })
    }

    /// What to look through in each git project.
    fn leftover_scans(&self) -> CoreResult<Vec<(ProjectId, LeftoverScan)>> {
        let threads = self.with_store(|s| s.threads())?;
        let prefix = self.config().worktrees.branch_prefix.clone();
        let worktrees_dir = self.opts().worktrees_dir.clone();
        let mut scans = Vec::new();
        for project in self.with_store(|s| s.projects())? {
            let mine = threads.iter().filter(|t| t.project_id == project.id);
            let mut scan = LeftoverScan {
                repo: PathBuf::from(&project.root_path),
                worktrees_dir: worktrees_dir.clone(),
                prefix: prefix.clone(),
                active_folders: HashSet::new(),
                active_branches: HashSet::new(),
                archived: HashMap::new(),
                env: self.opts().env.clone(),
            };
            for t in mine {
                let branch = match &t.workspace {
                    ThreadWorkspace::Worktree { branch, .. } => Some(branch.clone()),
                    _ => None,
                };
                if t.status == ThreadStatus::Archived {
                    if let Some(branch) = branch {
                        scan.archived.insert(branch, t.id.clone());
                    }
                } else {
                    scan.active_folders.insert(t.cwd.clone());
                    scan.active_branches.extend(branch);
                }
            }
            scans.push((project.id, scan));
        }
        Ok(scans)
    }
}

/// One project's leftovers, worked out off the async runtime.
struct LeftoverScan {
    repo: PathBuf,
    worktrees_dir: PathBuf,
    prefix: String,
    /// Folders and branches threads that aren't archived work in.
    active_folders: HashSet<String>,
    active_branches: HashSet<String>,
    /// Archived threads' branches.
    archived: HashMap<String, ThreadId>,
    /// The login environment, for fetching.
    env: Arc<HashMap<String, String>>,
}

struct Found {
    path: Option<String>,
    branch: Option<String>,
    thread_id: Option<ThreadId>,
    landed: bool,
    ahead: u32,
    dirty: bool,
}

impl Found {
    fn into_leftover(self, project: &ProjectId) -> WorktreeLeftover {
        WorktreeLeftover {
            project_id: project.clone(),
            path: self.path,
            branch: self.branch,
            thread_id: self.thread_id,
            landed: self.landed,
            ahead: self.ahead,
            dirty: self.dirty,
        }
    }
}

impl LeftoverScan {
    fn run(self, fetch: bool) -> anyhow::Result<Vec<Found>> {
        if loft_git::repo_root(&self.repo).is_none() {
            return Ok(Vec::new());
        }
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let ours = canon(&self.worktrees_dir);
        let active: HashSet<PathBuf> = self
            .active_folders
            .iter()
            .map(|f| canon(Path::new(f)))
            .collect();
        let main = wt::default_branch(&self.repo);
        let into: Vec<String> = match &main {
            Some(main) => wt::base_revs(&self.repo, main, fetch.then_some(&self.env)),
            None => Vec::new(),
        };
        let best = into.first().cloned();
        let judge = |rev: &str| -> (bool, u32) {
            let Some(best) = &best else {
                return (false, 0);
            };
            let Ok(ahead) = wt::ahead(&self.repo, rev, best) else {
                return (false, 0);
            };
            // Nothing of its own, or its work is in the main branch.
            let landed = ahead == 0
                || into
                    .iter()
                    .any(|i| wt::landed(&self.repo, rev, i).unwrap_or(false));
            (landed, ahead)
        };
        let mut found = Vec::new();
        let mut checked_out = HashSet::new();
        for entry in wt::list(&self.repo)?.into_iter().skip(1) {
            checked_out.extend(entry.branch.clone());
            let path = canon(&entry.path);
            if !path.starts_with(&ours) || active.contains(&path) {
                continue;
            }
            let dirty = !entry.path.exists() || wt::is_dirty(&entry.path).unwrap_or(true);
            let rev = entry.branch.clone().unwrap_or_else(|| {
                wt::tip(&entry.path, "HEAD").unwrap_or_else(|_| "HEAD".to_owned())
            });
            let (landed, ahead) = judge(&rev);
            found.push(Found {
                thread_id: entry
                    .branch
                    .as_ref()
                    .and_then(|b| self.archived.get(b).cloned()),
                path: Some(entry.path.to_string_lossy().into_owned()),
                branch: entry.branch,
                landed: landed && !dirty,
                ahead,
                dirty,
            });
        }
        for branch in wt::branches_under(&self.repo, &self.prefix)? {
            if checked_out.contains(&branch) || self.active_branches.contains(&branch) {
                continue;
            }
            let (landed, ahead) = judge(&branch);
            found.push(Found {
                path: None,
                thread_id: self.archived.get(&branch).cloned(),
                branch: Some(branch),
                landed,
                ahead,
                dirty: false,
            });
        }
        Ok(found)
    }
}
