//! Per-thread worktrees (ADR-0008, PLAN §5.2): chosen before the first
//! message, created when it is sent (branch named after it), squash-merged
//! back into the base branch, removed on archive.

use crate::thread::ThreadCmd;
use crate::{Core, CoreResult, internal, lock};
use loft_git::worktree as wt;
use loft_protocol::{
    ErrorBody, ErrorCode, Event, ItemBody, Response, Thread, ThreadId, ThreadStatus,
    ThreadWorkspace,
};
use loft_store::now_ms;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

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

impl Core {
    fn load_thread(&self, id: &ThreadId) -> CoreResult<Thread> {
        self.with_store(|s| s.thread(id))?.ok_or_else(not_found)
    }

    fn save_thread(&self, thread: &Thread) -> CoreResult<()> {
        self.commit(vec![Event::ThreadUpdated {
            thread: thread.clone(),
        }])
        .map(|_| ())
    }

    /// Workspace for a new thread from `[worktrees] mode`.
    pub(crate) fn default_workspace(&self, project_root: &str) -> ThreadWorkspace {
        let wants = self.config().worktrees.mode == "worktree";
        if wants && loft_git::repo_root(Path::new(project_root)).is_some() {
            ThreadWorkspace::NewWorktree
        } else {
            ThreadWorkspace::Local
        }
    }

    pub(crate) fn set_thread_workspace(
        &self,
        id: &ThreadId,
        worktree: bool,
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
            ThreadWorkspace::NewWorktree
        } else {
            ThreadWorkspace::Local
        };
        thread.updated_at_ms = now_ms();
        lock(&self.threads).remove(id);
        self.save_thread(&thread)?;
        Ok(Response::Thread { thread })
    }

    /// Before a thread's first message: create its pending worktree, named
    /// after the message, and move the thread into it.
    pub(crate) async fn ensure_workspace(
        self: &Arc<Self>,
        id: &ThreadId,
        text: &str,
    ) -> CoreResult<()> {
        let _guard = self.worktree_lock.lock().await;
        let mut thread = self.load_thread(id)?;
        if thread.workspace != ThreadWorkspace::NewWorktree {
            return Ok(());
        }
        let project = self
            .with_store(|s| s.project(&thread.project_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        let repo = PathBuf::from(&project.root_path);
        let prefix = self.config().worktrees.branch_prefix.clone();
        let dir = self
            .opts()
            .worktrees_dir
            .join(slug_or(&project.name, "project"));
        let slug = slug_or(text, "thread");
        let workspace = blocking(move || {
            let base = wt::current_branch(&repo)
                .or_else(|| {
                    // Detached HEAD: branch from the commit.
                    loft_git::repo_root(&repo).and(Some("HEAD".to_owned()))
                })
                .ok_or_else(|| anyhow::anyhow!("not a git repository"))?;
            let branch = wt::unique_branch(&repo, &prefix, &slug);
            let leaf = branch.rsplit('/').next().unwrap_or(&slug).to_owned();
            let mut path = dir.join(&leaf);
            let mut n = 2;
            while path.exists() {
                path = dir.join(format!("{leaf}-{n}"));
                n += 1;
            }
            wt::add(&repo, &path, &branch, &base)?;
            let copied = wt::copy_env_files(&repo, &path);
            tracing::info!(branch, path = %path.display(), ?copied, "worktree created");
            Ok(ThreadWorkspace::Worktree {
                path: path.to_string_lossy().into_owned(),
                branch,
                base,
            })
        })
        .await?;
        if let ThreadWorkspace::Worktree { path, .. } = &workspace {
            thread.cwd = path.clone();
        }
        thread.workspace = workspace;
        thread.updated_at_ms = now_ms();
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
        let short = &out.commit[..out.commit.len().min(7)];
        let files = if out.files == 1 { "file" } else { "files" };
        Ok(Response::Text {
            text: format!("Merged {} {files} into {base} ({short})", out.files),
        })
    }

    pub(crate) async fn archive_thread(
        self: &Arc<Self>,
        id: &ThreadId,
        force: bool,
    ) -> CoreResult<Response> {
        let mut thread = self.load_thread(id)?;
        // A fork sharing this worktree keeps it alive.
        let shared = self
            .with_store(|s| s.threads())?
            .iter()
            .any(|t| &t.id != id && t.status != ThreadStatus::Archived && t.cwd == thread.cwd);
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
        // Stop the agent session, then the thread's shell.
        let handle = lock(&self.threads).remove(id);
        if let Some(handle) = handle {
            let (tx, rx) = oneshot::channel();
            if handle.send(ThreadCmd::Shutdown { done: tx }).is_ok() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(10), rx).await;
            }
        }
        self.terminals.close_key(&format!("thread:{}", id.as_str()));
        if let ThreadWorkspace::Worktree { path, .. } = &thread.workspace
            && !shared
        {
            let project = self
                .with_store(|s| s.project(&thread.project_id))?
                .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
            let (repo, dir) = (PathBuf::from(project.root_path), PathBuf::from(path));
            blocking(move || {
                if dir.exists() {
                    wt::remove(&repo, &dir, force)?;
                }
                wt::prune(&repo)
            })
            .await?;
        }
        thread.status = ThreadStatus::Archived;
        thread.updated_at_ms = now_ms();
        self.save_thread(&thread)?;
        Ok(Response::Thread { thread })
    }
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
    use super::commit_message;

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
