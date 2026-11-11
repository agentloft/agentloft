//! Workbench services on the core: scoped file access, diffs from
//! checkpoints, and agent-facing thread queries (ADR-0013).

use crate::{Core, CoreResult, internal};
use loft_protocol::{
    DiffFileStat, DiffResult, DiffScope, DirEntry, ErrorBody, ErrorCode, FileContent, FsRoot,
    ItemBody, ItemStatus, ProjectId, ReadDetail, ThreadId, Turn, TurnStatus,
};
use std::path::{Component, Path, PathBuf};

/// Largest file `ReadFile` returns (the frame limit is 16 MiB after base64).
const MAX_READ: u64 = 10 * 1024 * 1024;
const MAX_DIR_ENTRIES: usize = 5_000;

fn bad(msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, msg)
}

const MAX_INDEXED_FILES: usize = 100_000;

/// All files under `dir`, relative, honoring .gitignore (skips `.git`).
pub(crate) fn walk_files(dir: &Path) -> Vec<String> {
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    walker
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| {
            e.path()
                .strip_prefix(dir)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .take(MAX_INDEXED_FILES)
        .collect()
}

/// Fuzzy-rank `files` against `query` (subsequence match; bonuses for
/// matches in the file name, at segment starts, and in runs). An empty
/// query lists the shortest paths first.
pub(crate) fn fuzzy_find(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let q: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let mut scored: Vec<(i64, &String)> = files
        .iter()
        .filter_map(|f| score(f, &q).map(|s| (s, f)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, f)| f.clone())
        .collect()
}

fn score(path: &str, q: &[char]) -> Option<i64> {
    if q.is_empty() {
        return Some(-(path.len() as i64));
    }
    let lower: Vec<char> = path.to_lowercase().chars().collect();
    let name_start = path
        .rfind('/')
        .map(|i| path[..=i].chars().count())
        .unwrap_or(0);
    let mut score = 0i64;
    let mut qi = 0;
    let mut prev: Option<usize> = None;
    for (i, c) in lower.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            score += 1;
            if i >= name_start {
                score += 3;
            }
            if i == 0 || matches!(lower[i - 1], '/' | '_' | '-' | '.') {
                score += 4;
            }
            if prev == Some(i.wrapping_sub(1)) {
                score += 5;
            }
            prev = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score - (path.len() as i64) / 8)
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
        let (_, path) = self.resolve(root, rel)?;
        self.read_file_at(&path, rel)
    }

    /// A project's icon: the image chosen for it, else the one found in
    /// its folder (`Request::ProjectIcon`).
    pub(crate) fn project_icon(&self, id: &ProjectId) -> CoreResult<FileContent> {
        let project = self
            .with_store(|s| s.project(id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        let icon = project
            .icon
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| loft_config::icons::drawable(p))
            .or_else(|| loft_config::icons::find(Path::new(&project.root_path)))
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "no icon"))?;
        let name = icon.to_string_lossy().into_owned();
        self.read_file_at(&icon, &name)
    }

    /// Read `path` (already checked by the caller), reported under `name`.
    pub(crate) fn read_file_at(&self, path: &Path, name: &str) -> CoreResult<FileContent> {
        use std::io::Read as _;
        let meta = std::fs::metadata(path).map_err(|e| bad(format!("{name}: {e}")))?;
        if meta.is_dir() {
            return Err(bad(format!("{name} is a directory")));
        }
        let file = std::fs::File::open(path).map_err(|e| bad(format!("{name}: {e}")))?;
        let mut bytes = Vec::with_capacity(meta.len().min(MAX_READ) as usize);
        file.take(MAX_READ)
            .read_to_end(&mut bytes)
            .map_err(internal)?;
        let sniff = &bytes[..bytes.len().min(8192)];
        Ok(FileContent {
            rel_path: name.to_owned(),
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
            scope: scope.clone(),
            patch: String::new(),
            files: vec![],
            note: Some(note.to_owned()),
            base: None,
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
            /// A turn's own changes (without what a pull brought in).
            Turn(String, String),
            ToWorkingTree(String),
            /// From where the branch left the remote.
            Origin,
        }
        let plan = match &scope {
            DiffScope::WorkingTree => Plan::WorkingTree,
            DiffScope::Origin => Plan::Origin,
            DiffScope::LastTurn | DiffScope::Turn(_) => {
                let turn = match &scope {
                    DiffScope::Turn(id) => turns.iter().find(|t| &t.id == id),
                    _ => turns.iter().rev().find(|t| t.checkpoint_before.is_some()),
                };
                let Some(before) = turn.and_then(|t| t.checkpoint_before.clone()) else {
                    return Ok(empty("No turns with checkpoints yet."));
                };
                match turn.and_then(|t| t.checkpoint_after.clone()) {
                    Some(after) => Plan::Turn(before, after),
                    None => Plan::ToWorkingTree(before),
                }
            }
            DiffScope::Thread => {
                let Some(first) = turns.iter().find_map(|t| t.checkpoint_before.clone()) else {
                    return Ok(empty("No turns with checkpoints yet."));
                };
                Plan::ToWorkingTree(first)
            }
        };
        let found = tokio::task::spawn_blocking(move || match plan {
            Plan::WorkingTree => loft_git::diff_working_tree(&dir).map(|d| Some((d, None))),
            Plan::Turn(a, b) => loft_git::turn_diff(&dir, &a, &b, true).map(|d| Some((d, None))),
            Plan::ToWorkingTree(a) => {
                loft_git::diff_to_working_tree(&dir, &a).map(|d| Some((d, None)))
            }
            Plan::Origin => match loft_git::origin_base(&dir) {
                Some((base, name)) => {
                    loft_git::diff_to_working_tree(&dir, &base).map(|d| Some((d, Some(name))))
                }
                None => Ok(None),
            },
        })
        .await
        .map_err(internal)?
        .map_err(|e| ErrorBody::new(ErrorCode::Internal, format!("{e:#}")))?;
        let Some((diff, base)) = found else {
            return Ok(empty("This branch has no remote branch to compare with."));
        };
        let files: Vec<DiffFileStat> = diff
            .files
            .into_iter()
            .map(crate::thread::file_stat)
            .collect();
        // A turn from before turns kept their changes: keep them now, so
        // its "changed files" card doesn't have to ask again.
        if let DiffScope::Turn(id) = &scope
            && let Some(mut turn) = turns.into_iter().find(|t| &t.id == id)
            && turn.status.is_terminal()
            && turn.checkpoint_after.is_some()
            && turn.changes.is_none()
        {
            turn.changes = Some(files.clone());
            let _ = self.commit(vec![loft_protocol::Event::TurnUpdated { turn }]);
        }
        Ok(DiffResult {
            scope,
            note: files.is_empty().then(|| "No changes.".to_owned()),
            patch: diff.patch,
            files,
            base,
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

    /// Threads whose conversation mentions `query`: the first matching
    /// message of each, most recently updated thread first.
    pub(crate) fn search_threads(
        &self,
        query: &str,
        limit: usize,
    ) -> CoreResult<Vec<loft_protocol::SearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let (threads, items) =
            self.with_store(|s| Ok((s.threads()?, s.items_mentioning(query)?)))?;
        let mut hits: std::collections::HashMap<ThreadId, loft_protocol::SearchHit> =
            Default::default();
        for item in items {
            let (text, from_user) = match &item.body {
                ItemBody::UserMessage { text, .. } => (text, true),
                ItemBody::AssistantMessage { text } => (text, false),
                _ => continue,
            };
            let Some((start, end)) = find_ignoring_case(text, query) else {
                continue;
            };
            if let Some(hit) = hits.get_mut(&item.thread_id) {
                hit.count += 1;
                continue;
            }
            let (snippet, start, end) = snippet(text, start, end);
            hits.insert(
                item.thread_id.clone(),
                loft_protocol::SearchHit {
                    thread_id: item.thread_id,
                    item_id: item.id,
                    from_user,
                    snippet,
                    start,
                    end,
                    count: 1,
                },
            );
        }
        let mut out: Vec<_> = threads
            .into_iter()
            .filter_map(|t| hits.remove(&t.id).map(|h| (t.updated_at_ms, h)))
            .collect();
        out.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        Ok(out.into_iter().take(limit).map(|(_, h)| h).collect())
    }

    /// Where a thread stands, for the agent orchestrating it.
    pub(crate) fn thread_outcome(&self, id: &ThreadId) -> CoreResult<serde_json::Value> {
        let (thread, turns, items) =
            self.with_store(|s| Ok((s.thread(id)?, s.turns(id)?, s.items(id)?)))?;
        let thread =
            thread.ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        // The latest answer: the last top-level agent message.
        const MAX: usize = 20_000;
        let reply = items
            .iter()
            .rev()
            .filter(|i| i.parent_id.is_none())
            .find_map(|i| match &i.body {
                ItemBody::AssistantMessage { text } if !text.trim().is_empty() => {
                    Some(text.clone())
                }
                _ => None,
            })
            .map(|t| {
                if t.len() > MAX {
                    let mut end = MAX;
                    while !t.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}\n[truncated]", &t[..end])
                } else {
                    t
                }
            });
        let status = match thread.status {
            _ if thread.is_waiting() => "waiting_on_background_work",
            loft_protocol::ThreadStatus::Running => "running",
            loft_protocol::ThreadStatus::WaitingApproval => "waiting_for_approval",
            loft_protocol::ThreadStatus::Error => "error",
            loft_protocol::ThreadStatus::Archived => "archived",
            _ => "idle",
        };
        let last = turns.last();
        let now = loft_store::now_ms();
        let costs: Vec<f64> = turns
            .iter()
            .filter_map(|t| t.usage.as_ref()?.cost_usd)
            .collect();
        let total_cost = (!costs.is_empty()).then(|| round2(costs.iter().sum()));
        let last_activity = items.iter().map(|i| i.updated_at_ms).max();
        Ok(serde_json::json!({
            "thread_id": thread.id,
            "title": thread.title,
            "project_id": thread.project_id,
            "status": status,
            "agent": thread.harness,
            "model": thread.model,
            "effort": thread.effort,
            "permission_mode": thread.permission_mode,
            "cwd": thread.cwd,
            "turns": turns.len(),
            "last_turn": last.map(|t| turn_stats(t, now)),
            "total_cost_usd": total_cost,
            "last_activity_s_ago": last_activity.map(|at| secs(now - at)),
            "spawned_by": thread.parent.as_ref()
                .filter(|p| p.kind == loft_protocol::ForkKind::Spawned)
                .map(|p| p.thread_id.clone()),
            "waiting_on": thread.waiting_on,
            "latest_reply": reply,
        }))
    }

    /// A readable transcript of a thread for another agent.
    pub(crate) fn read_thread_text(&self, id: &ThreadId, detail: ReadDetail) -> CoreResult<String> {
        let (thread, items) = self.with_store(|s| Ok((s.thread(id)?, s.items(id)?)))?;
        let thread =
            thread.ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        let turns = self.with_store(|s| s.turns(id))?;
        let now = loft_store::now_ms();
        let mut out = format!(
            "# {}\nthread_id: {} · harness: {} · cwd: {} · times are UTC, now {}\n\n",
            thread.title,
            thread.id,
            thread.harness,
            thread.cwd,
            utc(now)
        );
        let mut in_turn = None;
        for item in items.iter().filter(|i| i.parent_id.is_none()) {
            if let Some(turn) = item
                .turn_id
                .as_ref()
                .filter(|t| in_turn != Some(*t))
                .and_then(|t| turns.iter().find(|turn| &turn.id == t))
            {
                in_turn = Some(&turn.id);
                out.push_str(&turn_heading(turn, now));
            }
            let at = clock(item.started_at_ms.unwrap_or(item.updated_at_ms));
            match &item.body {
                ItemBody::UserMessage { text, .. } => {
                    out.push_str(&format!("## User · {at}\n{text}\n\n"))
                }
                ItemBody::AssistantMessage { text } if !text.trim().is_empty() => {
                    out.push_str(&format!("## Assistant · {at}\n{text}\n\n"))
                }
                ItemBody::ToolCall {
                    tool,
                    title,
                    output,
                    ..
                } if detail == ReadDetail::Full => {
                    let took = match (item.status, item.started_at_ms) {
                        (ItemStatus::InProgress, Some(start)) => {
                            format!("running {}", span(now - start))
                        }
                        (_, Some(start)) => span(item.updated_at_ms - start),
                        (_, None) => String::new(),
                    };
                    out.push_str(&format!("- tool `{tool}` · {at} · {took}: {title}\n"));
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

#[cfg(test)]
mod find_tests {
    use super::fuzzy_find;

    #[test]
    fn fuzzy_ranks_file_names_first() {
        let files: Vec<String> = [
            "src/main.rs",
            "src/workspace/mod.rs",
            "docs/maintenance.md",
            "crates/loft-ui/src/workspace.rs",
            "README.md",
        ]
        .map(String::from)
        .into();
        let hits = fuzzy_find(&files, "workspace", 3);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.contains("workspace")), "{hits:?}");
        assert_eq!(fuzzy_find(&files, "main", 1), vec!["src/main.rs"]);
        assert_eq!(fuzzy_find(&files, "rdme", 1), vec!["README.md"]);
        assert!(fuzzy_find(&files, "zzz", 5).is_empty());
        assert_eq!(
            fuzzy_find(&files, "", 1),
            vec!["README.md"],
            "shortest first"
        );
    }
}

/// Where `needle` first occurs in `text`, ignoring case: byte offsets into
/// `text`.
pub(crate) fn find_ignoring_case(text: &str, needle: &str) -> Option<(usize, usize)> {
    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return None;
    }
    let hay: Vec<(usize, char)> = text
        .char_indices()
        .flat_map(|(i, c)| c.to_lowercase().map(move |l| (i, l)))
        .collect();
    let at = hay
        .windows(needle.len())
        .position(|w| w.iter().map(|x| x.1).eq(needle.iter().copied()))?;
    let start = hay[at].0;
    let last = hay[at + needle.len() - 1].0;
    let end = last + text[last..].chars().next().map_or(1, char::len_utf8);
    Some((start, end))
}

/// One line of `text` around the match at `start..end`, with the match's
/// position in it: a little context before the match, more after.
pub(crate) fn snippet(text: &str, start: usize, end: usize) -> (String, usize, usize) {
    const BEFORE: usize = 36;
    const AFTER: usize = 140;
    let from = text[..start]
        .char_indices()
        .rev()
        .nth(BEFORE - 1)
        .map_or(0, |(i, _)| i);
    let to = text[end..]
        .char_indices()
        .nth(AFTER)
        .map_or(text.len(), |(i, _)| end + i);
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    let before = flat(&text[from..start]);
    out.push_str(&before);
    // Keep the space between the context and the match.
    if !before.is_empty() && text[..start].ends_with(char::is_whitespace) {
        out.push(' ');
    }
    let hit_start = out.len();
    out.push_str(&text[start..end].replace(['\n', '\r', '\t'], " "));
    let hit_end = out.len();
    if text[end..to].starts_with(char::is_whitespace) {
        out.push(' ');
    }
    out.push_str(&flat(&text[end..to]));
    if to < text.len() {
        out.push('…');
    }
    (out, hit_start, hit_end)
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn finds_ignoring_case() {
        assert_eq!(find_ignoring_case("Hello World", "world"), Some((6, 11)));
        assert_eq!(find_ignoring_case("ÄBC äbc", "äb"), Some((0, 3)));
        assert_eq!(find_ignoring_case("abc", "x"), None);
        assert_eq!(find_ignoring_case("abc", ""), None);
    }

    #[test]
    fn snippet_keeps_the_match_in_place() {
        let text = format!("{}\nthe Needle is here\n\nand more", "x ".repeat(40));
        let (start, end) = find_ignoring_case(&text, "needle").unwrap();
        let (s, a, b) = snippet(&text, start, end);
        assert_eq!(&s[a..b], "Needle");
        assert!(s.starts_with('…'));
        assert!(s.ends_with("is here and more"));
        let (s, a, b) = snippet("needle", 0, 6);
        assert_eq!((s.as_str(), a, b), ("needle", 0, 6));
    }
}

/// A turn's timing, tokens, cost and context, for `thread_status`.
fn turn_stats(t: &Turn, now: i64) -> serde_json::Value {
    let end = t.ended_at_ms.unwrap_or(now);
    let u = t.usage.as_ref();
    serde_json::json!({
        "status": t.status,
        "error": t.error,
        "started_at": utc(t.started_at_ms),
        "ended_at": t.ended_at_ms.map(utc),
        "duration_s": secs(end - t.started_at_ms),
        "tokens": u.map(|u| serde_json::json!({
            "input": u.input_tokens,
            "output": u.output_tokens,
            "cache_read": u.cache_read_tokens,
            "cache_write": u.cache_write_tokens,
        })),
        "cost_usd": u.and_then(|u| u.cost_usd).map(round2),
        "context": t.context.map(|c| serde_json::json!({
            "tokens": c.tokens,
            "window": c.window,
            "percent_full": c.window.filter(|w| *w > 0)
                .map(|w| (c.tokens as f64 * 1000.0 / w as f64).round() / 10.0),
        })),
    })
}

/// `### Turn 2 · 05:16:04 → 06:17:37 (61.5 min) · 95.3M tokens · $44.49 · context 962k of 1M`
fn turn_heading(t: &Turn, now: i64) -> String {
    let mut line = format!("### Turn {} · {}", t.ordinal, clock(t.started_at_ms));
    match t.ended_at_ms {
        Some(end) => line.push_str(&format!(
            " → {} ({})",
            clock(end),
            span(end - t.started_at_ms)
        )),
        None => line.push_str(&format!(" · running for {}", span(now - t.started_at_ms))),
    }
    if t.status != TurnStatus::Running && t.status != TurnStatus::Completed {
        line.push_str(&format!(" · {:?}", t.status).to_lowercase());
    }
    if let Some(u) = &t.usage {
        let all = u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens;
        if all > 0 {
            line.push_str(&format!(" · {} tokens", count(all)));
        }
        if let Some(cost) = u.cost_usd {
            line.push_str(&format!(" · ${cost:.2}"));
        }
    }
    if let Some(c) = t.context {
        line.push_str(&format!(" · context {}", count(c.tokens)));
        if let Some(w) = c.window {
            line.push_str(&format!(" of {}", count(w)));
        }
    }
    line.push_str("\n\n");
    line
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn secs(ms: i64) -> f64 {
    (ms.max(0) as f64 / 100.0).round() / 10.0
}

/// `0.4s`, `12s`, `3.5 min`, `1.2 h`.
fn span(ms: i64) -> String {
    let s = ms.max(0) as f64 / 1000.0;
    if s < 10.0 {
        format!("{s:.1}s")
    } else if s < 120.0 {
        format!("{s:.0}s")
    } else if s < 7200.0 {
        format!("{:.1} min", s / 60.0)
    } else {
        format!("{:.1} h", s / 3600.0)
    }
}

/// `950`, `28k`, `962k`, `1M`, `95.3M`.
fn count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..999_500 => format!("{}k", (n as f64 / 1e3).round()),
        _ => {
            let m = format!("{:.1}", n as f64 / 1e6);
            format!("{}M", m.trim_end_matches(".0"))
        }
    }
}

/// `2026-11-11T06:17:37Z`
fn utc(ms: i64) -> String {
    let (days, secs) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000) / 1000);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// `06:17:37` (UTC)
fn clock(ms: i64) -> String {
    utc(ms)[11..19].to_owned()
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    #[test]
    fn times_and_amounts_read_well() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(1_794_377_857_653), "2026-11-11T06:17:37Z");
        assert_eq!(utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(clock(1_794_377_857_653), "06:17:37");
        assert_eq!(span(400), "0.4s");
        assert_eq!(span(12_300), "12s");
        assert_eq!(span(3_693_304), "61.6 min");
        assert_eq!(count(950), "950");
        assert_eq!(count(28_094), "28k");
        assert_eq!(count(1_000_000), "1M");
        assert_eq!(count(95_022_258), "95M");
        assert_eq!(count(95_322_258), "95.3M");
        assert_eq!(secs(3_693_304), 3693.3);
    }
}
