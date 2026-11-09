//! Forks (ADR-0011): same harness → the provider's native fork; another
//! harness → a deterministic handoff digest offered as the first message.
//! By default the fork gets its own worktree from the parent's current
//! state, so conversation and code start from the same point.

use crate::worktrees::{blocking, slug_or};
use crate::{Core, CoreResult, handoff};
use loft_git::worktree as wt;
use loft_protocol::{
    DiffScope, ErrorBody, ErrorCode, Event, ForkKind, HarnessId, Response, Thread, ThreadId,
    ThreadParent, ThreadStatus, ThreadWorkspace,
};
use loft_store::now_ms;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) fn harness_display(h: &HarnessId) -> String {
    match h.as_str() {
        HarnessId::CLAUDE => "Claude Code".into(),
        HarnessId::CODEX => "Codex".into(),
        HarnessId::OPENCODE => "OpenCode".into(),
        HarnessId::GEMINI => "Gemini".into(),
        other => {
            let mut c = other.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
    }
}

impl Core {
    pub(crate) async fn fork_thread(
        self: &Arc<Self>,
        parent_id: &ThreadId,
        harness: Option<HarnessId>,
        share_workspace: bool,
        at_turn: Option<loft_protocol::TurnId>,
    ) -> CoreResult<Response> {
        let parent = self
            .with_store(|s| s.thread(parent_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        let harness = harness.unwrap_or_else(|| parent.harness.clone());
        let Some(h) = self.harness(&harness) else {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("unknown harness `{harness}`"),
            ));
        };
        let (turns, items) = self.with_store(|s| Ok((s.turns(parent_id)?, s.items(parent_id)?)))?;
        // Forking from an earlier turn: the conversation and files as they
        // were when it ended.
        let at = match &at_turn {
            Some(id) => Some(
                turns
                    .iter()
                    .find(|t| &t.id == id)
                    .cloned()
                    .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "turn not found"))?,
            ),
            None => None,
        };
        let at = at.filter(|t| turns.last().map(|l| &l.id) != Some(&t.id));
        let items = match &at {
            Some(at) => {
                let kept: std::collections::HashSet<_> = turns
                    .iter()
                    .filter(|t| t.ordinal <= at.ordinal)
                    .map(|t| t.id.clone())
                    .collect();
                items
                    .into_iter()
                    .filter(|i| i.turn_id.as_ref().is_none_or(|t| kept.contains(t)))
                    .collect()
            }
            None => items,
        };
        let same = harness == parent.harness;
        // A native fork from an earlier turn needs the agent's marker for it.
        let native = same
            && parent.native_session_id.is_some()
            && h.capabilities().native_fork
            && at.as_ref().is_none_or(|t| t.native_turn.is_some());
        let draft = if native || items.is_empty() {
            None
        } else {
            let files = match &at {
                None => self
                    .diff(parent_id, DiffScope::Thread)
                    .await
                    .map(|d| d.files)
                    .unwrap_or_default(),
                Some(at) => {
                    let first = turns.iter().find_map(|t| t.checkpoint_before.clone());
                    match (first, at.checkpoint_after.clone()) {
                        (Some(from), Some(to)) => {
                            let dir = PathBuf::from(&parent.cwd);
                            blocking(move || loft_git::diff(&dir, &from, &to))
                                .await
                                .map(|d| {
                                    d.files
                                        .into_iter()
                                        .map(|f| loft_protocol::DiffFileStat {
                                            path: f.path,
                                            added: f.added,
                                            removed: f.removed,
                                            binary: f.binary,
                                        })
                                        .collect()
                                })
                                .unwrap_or_default()
                        }
                        _ => Vec::new(),
                    }
                }
            };
            Some(handoff::digest(
                &parent,
                &harness_display(&parent.harness),
                &items,
                &files,
            ))
        };

        let id = ThreadId::new();
        let (cwd, workspace) = if share_workspace {
            (parent.cwd.clone(), parent.workspace.clone())
        } else {
            let start = at.as_ref().and_then(|t| t.checkpoint_after.clone());
            self.fork_workspace(&parent, &id, start).await?
        };
        let now = now_ms();
        let thread = Thread {
            id,
            project_id: parent.project_id.clone(),
            title: parent.title.clone(),
            harness,
            cwd,
            model: same.then(|| parent.model.clone()).flatten(),
            permission_mode: same.then_some(parent.permission_mode).flatten(),
            effort: same.then(|| parent.effort.clone()).flatten(),
            native_session_id: None,
            status: ThreadStatus::Idle,
            created_at_ms: now,
            updated_at_ms: now,
            workspace,
            parent: Some(ThreadParent {
                thread_id: parent.id.clone(),
                turn_id: at
                    .as_ref()
                    .map(|t| t.id.clone())
                    .or_else(|| turns.last().map(|t| t.id.clone())),
                kind: if native {
                    ForkKind::Native
                } else {
                    ForkKind::Handoff
                },
            }),
            slash_commands: if same {
                parent.slash_commands.clone()
            } else {
                Vec::new()
            },
            command_info: if same {
                parent.command_info.clone()
            } else {
                Vec::new()
            },
            started_at_ms: None,
            draft: None,
        };
        self.commit(vec![Event::ThreadCreated {
            thread: thread.clone(),
        }])?;
        Ok(Response::Forked { thread, draft })
    }

    /// A new worktree from a checkpoint of the parent's current state (or
    /// from `start`, a turn's checkpoint), or the parent's workspace when
    /// the project isn't a git repo.
    async fn fork_workspace(
        &self,
        parent: &Thread,
        child: &ThreadId,
        start: Option<String>,
    ) -> CoreResult<(String, ThreadWorkspace)> {
        let project = self
            .with_store(|s| s.project(&parent.project_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        let repo = PathBuf::from(&project.root_path);
        if loft_git::repo_root(&repo).is_none() || !Path::new(&parent.cwd).exists() {
            return Ok((parent.cwd.clone(), parent.workspace.clone()));
        }
        let parent_cwd = PathBuf::from(&parent.cwd);
        let base = match &parent.workspace {
            ThreadWorkspace::Worktree { base, .. } => Some(base.clone()),
            _ => None,
        };
        let stem = match &parent.workspace {
            ThreadWorkspace::Worktree { branch, .. } => {
                branch.rsplit('/').next().unwrap_or("thread").to_owned()
            }
            _ => slug_or(&parent.title, "thread"),
        };
        let prefix = self.config().worktrees.branch_prefix.clone();
        let dir = self
            .opts()
            .worktrees_dir
            .join(slug_or(&project.name, "project"));
        let ref_name = format!("{}/{}/fork", loft_git::REF_PREFIX, child);
        let project_cfg = loft_config::ProjectConfig::load(&repo);
        if let Some(cmd) = project_cfg
            .worktree
            .setup
            .clone()
            .filter(|c| !c.trim().is_empty())
        {
            crate::lock(&self.pending_setup).insert(child.clone(), cmd);
        }
        let workspace = blocking(move || {
            let base = base
                .or_else(|| wt::current_branch(&repo))
                .unwrap_or_else(|| "HEAD".to_owned());
            // Uncommitted work comes along as the fork-point commit.
            let commit = match start {
                Some(commit) => commit,
                None => loft_git::checkpoint(&parent_cwd, &ref_name, "Agentloft fork point")?,
            };
            let branch = wt::unique_branch(&repo, &prefix, &format!("{stem}-fork"));
            let leaf = branch.rsplit('/').next().unwrap_or("fork").to_owned();
            let mut path = dir.join(&leaf);
            let mut n = 2;
            while path.exists() {
                path = dir.join(format!("{leaf}-{n}"));
                n += 1;
            }
            wt::add(&repo, &path, &branch, &commit)?;
            wt::copy_untracked(&repo, &path, &project_cfg.worktree.copy);
            Ok(ThreadWorkspace::Worktree {
                path: path.to_string_lossy().into_owned(),
                branch,
                base,
            })
        })
        .await?;
        let cwd = match &workspace {
            ThreadWorkspace::Worktree { path, .. } => path.clone(),
            _ => parent.cwd.clone(),
        };
        Ok((cwd, workspace))
    }
}
