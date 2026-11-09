//! Workbench services on the core: scoped file access, diffs from
//! checkpoints, and agent-facing thread queries (ADR-0013).

use crate::{Core, CoreResult, internal};
use loft_protocol::{
    DiffFileStat, DiffResult, DiffScope, DirEntry, ErrorBody, ErrorCode, FileContent, FsRoot,
    ItemBody, ProjectId, ReadDetail, ThreadId, TurnStatus,
};
use std::path::{Component, Path, PathBuf};

/// Largest file `ReadFile` returns (the frame limit is 16 MiB after base64).
const MAX_READ: u64 = 10 * 1024 * 1024;
const MAX_DIR_ENTRIES: usize = 5_000;

fn bad(msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, msg)
}

impl Core {
    /// Absolute directory for a root.
    pub fn root_dir(&self, root: &FsRoot) -> CoreResult<PathBuf> {
        let path = match root {
            FsRoot::Project(id) => {
                self.with_store(|s| s.project(id))?
                    .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?
                    .root_path
            }
            FsRoot::Thread(id) => {
                self.with_store(|s| s.thread(id))?
                    .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?
                    .cwd
            }
        };
        Ok(PathBuf::from(path))
    }

    /// Resolve `rel` under `root`, refusing anything that escapes it.
    fn resolve(&self, root: &FsRoot, rel: &str) -> CoreResult<(PathBuf, PathBuf)> {
        let base = self.root_dir(root)?;
        let rel = Path::new(rel.trim_start_matches('/'));
        if rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(bad("path must stay inside the project"));
        }
        let full = base.join(rel);
        // Symlinks may point outside; canonicalize and re-check.
        if let (Ok(c_full), Ok(c_base)) = (full.canonicalize(), base.canonicalize())
            && !c_full.starts_with(&c_base)
        {
            return Err(bad("path resolves outside the project"));
        }
        Ok((base, full))
    }

    pub(crate) fn list_dir(&self, root: &FsRoot, rel: &str) -> CoreResult<Vec<DirEntry>> {
        let (base, dir) = self.resolve(root, rel)?;
        let walker = ignore::WalkBuilder::new(&dir)
            .max_depth(Some(1))
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .parents(true)
            .filter_entry(|e| e.file_name() != ".git")
            .build();
        let mut entries = Vec::new();
        for entry in walker.flatten().skip(1).take(MAX_DIR_ENTRIES) {
            let path = entry.path();
            let meta = entry.metadata().ok();
            let rel_path = path
                .strip_prefix(&base)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                rel_path,
                is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
                size: meta.map(|m| m.len()).unwrap_or(0),
            });
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    pub(crate) fn read_file(&self, root: &FsRoot, rel: &str) -> CoreResult<FileContent> {
        use std::io::Read as _;
        let (_, path) = self.resolve(root, rel)?;
        let meta = std::fs::metadata(&path).map_err(|e| bad(format!("{rel}: {e}")))?;
        if meta.is_dir() {
            return Err(bad(format!("{rel} is a directory")));
        }
        let file = std::fs::File::open(&path).map_err(|e| bad(format!("{rel}: {e}")))?;
        let mut bytes = Vec::with_capacity(meta.len().min(MAX_READ) as usize);
        file.take(MAX_READ)
            .read_to_end(&mut bytes)
            .map_err(internal)?;
        let sniff = &bytes[..bytes.len().min(8192)];
        Ok(FileContent {
            rel_path: rel.to_owned(),
            abs_path: path.to_string_lossy().into_owned(),
            size: meta.len(),
            binary: sniff.contains(&0),
            truncated: meta.len() > MAX_READ,
            bytes: bytes.into(),
        })
    }

    pub(crate) async fn diff(
        &self,
        thread_id: &ThreadId,
        scope: DiffScope,
    ) -> CoreResult<DiffResult> {
        let dir = self.root_dir(&FsRoot::Thread(thread_id.clone()))?;
        let turns = self.with_store(|s| s.turns(thread_id))?;
        let empty = |note: &str| DiffResult {
            scope,
            patch: String::new(),
            files: vec![],
            note: Some(note.to_owned()),
        };
        let repo = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || loft_git::repo_root(&dir))
                .await
                .map_err(internal)?
        };
        if repo.is_none() {
            return Ok(empty(
                "This project is not a git repository, so changes can't be tracked.",
            ));
        }
        enum Plan {
            WorkingTree,
            Between(String, String),
            ToWorkingTree(String),
        }
        let plan = match scope {
            DiffScope::WorkingTree => Plan::WorkingTree,
            DiffScope::LastTurn => {
                let Some(turn) = turns.iter().rev().find(|t| t.checkpoint_before.is_some()) else {
                    return Ok(empty("No turns with checkpoints yet."));
                };
                let before = turn.checkpoint_before.clone().unwrap_or_default();
                match (&turn.checkpoint_after, turn.status) {
                    (Some(after), _) => Plan::Between(before, after.clone()),
                    (None, TurnStatus::Running) => Plan::ToWorkingTree(before),
                    (None, _) => Plan::ToWorkingTree(before),
                }
            }
            DiffScope::Thread => {
                let Some(first) = turns.iter().find_map(|t| t.checkpoint_before.clone()) else {
                    return Ok(empty("No turns with checkpoints yet."));
                };
                Plan::ToWorkingTree(first)
            }
        };
        let diff = tokio::task::spawn_blocking(move || match plan {
            Plan::WorkingTree => loft_git::diff_working_tree(&dir),
            Plan::Between(a, b) => loft_git::diff(&dir, &a, &b),
            Plan::ToWorkingTree(a) => loft_git::diff_to_working_tree(&dir, &a),
        })
        .await
        .map_err(internal)?
        .map_err(|e| ErrorBody::new(ErrorCode::Internal, format!("{e:#}")))?;
        Ok(DiffResult {
            scope,
            note: diff.files.is_empty().then(|| "No changes.".to_owned()),
            patch: diff.patch,
            files: diff
                .files
                .into_iter()
                .map(|f| DiffFileStat {
                    path: f.path,
                    added: f.added,
                    removed: f.removed,
                    binary: f.binary,
                })
                .collect(),
        })
    }

    pub(crate) fn list_threads_json(
        &self,
        query: Option<&str>,
        project: Option<&ProjectId>,
    ) -> CoreResult<serde_json::Value> {
        let (threads, projects) = self.with_store(|s| Ok((s.threads()?, s.projects()?)))?;
        let q = query.map(str::to_lowercase);
        let rows: Vec<serde_json::Value> = threads
            .iter()
            .filter(|t| project.is_none_or(|p| &t.project_id == p))
            .filter(|t| {
                q.as_ref()
                    .is_none_or(|q| t.title.to_lowercase().contains(q))
            })
            .take(200)
            .map(|t| {
                let project = projects
                    .iter()
                    .find(|p| p.id == t.project_id)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                serde_json::json!({
                    "thread_id": t.id, "title": t.title, "project": project,
                    "harness": t.harness, "status": t.status, "updated_at_ms": t.updated_at_ms,
                })
            })
            .collect();
        Ok(serde_json::Value::Array(rows))
    }

    /// A readable transcript of a thread for another agent.
    pub(crate) fn read_thread_text(&self, id: &ThreadId, detail: ReadDetail) -> CoreResult<String> {
        let (thread, items) = self.with_store(|s| Ok((s.thread(id)?, s.items(id)?)))?;
        let thread =
            thread.ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        let mut out = format!(
            "# {}\nthread_id: {} · harness: {} · cwd: {}\n\n",
            thread.title, thread.id, thread.harness, thread.cwd
        );
        for item in items.iter().filter(|i| i.parent_id.is_none()) {
            match &item.body {
                ItemBody::UserMessage { text, .. } => out.push_str(&format!("## User\n{text}\n\n")),
                ItemBody::AssistantMessage { text } if !text.trim().is_empty() => {
                    out.push_str(&format!("## Assistant\n{text}\n\n"))
                }
                ItemBody::ToolCall {
                    tool,
                    title,
                    output,
                    ..
                } if detail == ReadDetail::Full => {
                    out.push_str(&format!("- tool `{tool}`: {title}\n"));
                    if let Some(o) = output.as_ref().filter(|o| !o.trim().is_empty()) {
                        let short: String = o.chars().take(600).collect();
                        out.push_str(&format!("  output: {}\n", short.replace('\n', "\n  ")));
                    }
                }
                ItemBody::Error { text } => out.push_str(&format!("> error: {text}\n\n")),
                _ => {}
            }
        }
        const MAX: usize = 60_000;
        if out.len() > MAX {
            let mut cut = out.len() - MAX;
            while !out.is_char_boundary(cut) {
                cut += 1;
            }
            out = format!("[… {cut} earlier characters omitted …]\n{}", &out[cut..]);
        }
        Ok(out)
    }
}
