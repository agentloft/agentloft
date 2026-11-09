//! Per-thread worktrees (ADR-0008, PLAN §5.2): created on a new
//! `agentloft/<slug>` branch, removed on archive, squash-merged back.

use super::{git, run};
use anyhow::{Result, bail};
use std::path::Path;

/// The checked-out branch, or `None` when HEAD is detached / unborn.
pub fn current_branch(dir: &Path) -> Option<String> {
    run(git(dir).args(["symbolic-ref", "--short", "-q", "HEAD"]))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn branch_exists(dir: &Path, branch: &str) -> bool {
    run(git(dir).args([
        "show-ref",
        "--verify",
        "-q",
        &format!("refs/heads/{branch}"),
    ]))
    .is_ok()
}

/// A short, branch-safe slug from free text ("Add a greet() fn!" →
/// "add-a-greet-fn"), at most `max` bytes, cut at a word boundary.
pub fn slugify(text: &str, max: usize) -> String {
    let mut slug = String::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_ascii_lowercase();
        let extra = if slug.is_empty() { 0 } else { 1 };
        if slug.len() + extra + word.len() > max {
            if slug.is_empty() {
                slug.push_str(&word[..max.min(word.len())]);
            }
            break;
        }
        if extra == 1 {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    slug
}

/// `prefix/<slug>`, suffixed `-2`, `-3`, … if the branch already exists.
pub fn unique_branch(repo: &Path, prefix: &str, slug: &str) -> String {
    let slug = if slug.is_empty() { "thread" } else { slug };
    let mut branch = format!("{prefix}/{slug}");
    let mut n = 2;
    while branch_exists(repo, &branch) {
        branch = format!("{prefix}/{slug}-{n}");
        n += 1;
    }
    branch
}

/// `git worktree add -b <branch> <path> <base>`.
pub fn add(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path = path.to_string_lossy();
    run(git(repo).args(["worktree", "add", "-q", "-b", branch, &path, base]))?;
    Ok(())
}

/// Copy untracked files matching `patterns` (paths relative to the repo,
/// `*`/`?` in the last component) from the main checkout into a new
/// worktree — the secrets and local config git doesn't carry.
pub fn copy_untracked(from: &Path, to: &Path, patterns: &[String]) -> Vec<String> {
    let mut copied = Vec::new();
    for pattern in patterns {
        let (dir, name) = match pattern.rsplit_once('/') {
            Some((dir, name)) => (dir, name),
            None => ("", pattern.as_str()),
        };
        if dir.split('/').any(|c| c == "..") {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(from.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            let is_file = entry.file_type().is_ok_and(|t| t.is_file());
            let rel = if dir.is_empty() {
                file.clone()
            } else {
                format!("{dir}/{file}")
            };
            if !is_file || !wildcard(name, &file) || to.join(&rel).exists() {
                continue;
            }
            if let Some(parent) = to.join(&rel).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::copy(entry.path(), to.join(&rel)).is_ok() {
                copied.push(rel);
            }
        }
    }
    copied.sort();
    copied.dedup();
    copied
}

/// Top-level `.env*` files (the default `copy` list).
pub fn copy_env_files(from: &Path, to: &Path) -> Vec<String> {
    copy_untracked(from, to, &[".env*".to_owned()])
}

/// `*` and `?` glob match on one path component.
fn wildcard(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    go(&p, &n)
}

/// Tracked changes or untracked (non-ignored) files present.
pub fn is_dirty(dir: &Path) -> Result<bool> {
    let out = run(git(dir).args(["status", "--porcelain", "-z"]))?;
    Ok(!out.is_empty())
}

/// `git worktree remove [--force] <path>`; the branch is kept.
pub fn remove(repo: &Path, path: &Path, force: bool) -> Result<()> {
    let path = path.to_string_lossy();
    let mut cmd = git(repo);
    cmd.args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    run(cmd.arg(path.as_ref()))?;
    Ok(())
}

pub fn prune(repo: &Path) -> Result<()> {
    run(git(repo).args(["worktree", "prune"]))?;
    Ok(())
}

/// Files that would conflict when merging `branch` into `base`
/// (`git merge-tree --write-tree`, no work tree touched).
pub fn merge_conflicts(repo: &Path, base: &str, branch: &str) -> Result<Vec<String>> {
    let out = git(repo)
        .args(["merge-tree", "--write-tree", "--name-only", base, branch])
        .output()?;
    match out.status.code() {
        Some(0) => Ok(Vec::new()),
        // Conflicts: tree id, then conflicted paths, then a blank line.
        Some(1) => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1)
            .take_while(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()),
        _ => bail!(
            "git merge-tree failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Commit pending work on the worktree's branch (if any), then push it to
/// the base branch's remote (default `origin`) with upstream tracking.
/// `env` is the user's login environment (credential helpers, SSH agent).
/// Returns the remote name.
pub fn commit_and_push(
    worktree: &Path,
    branch: &str,
    base: &str,
    message: &str,
    env: &std::collections::HashMap<String, String>,
) -> Result<String> {
    if is_dirty(worktree)? {
        run(git(worktree).args(["add", "-A"]))?;
        run(git(worktree).args(["commit", "-q", "--no-verify", "-m", message]))?;
    }
    let remotes = run(git(worktree).arg("remote"))?;
    let configured = run(git(worktree).args(["config", &format!("branch.{base}.remote")]))
        .ok()
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty());
    let remote = configured
        .or_else(|| remotes.lines().find(|r| *r == "origin").map(str::to_owned))
        .or_else(|| remotes.lines().next().map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("this repository has no remote to push to"))?;
    let mut cmd = git(worktree);
    cmd.envs(env)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(["push", "-q", "-u", &remote, &format!("{branch}:{branch}")]);
    run(&mut cmd)?;
    Ok(remote)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub commit: String,
    pub files: usize,
}

/// Squash-merge a worktree thread's branch into `base` in the main checkout
/// (PLAN §5.2 "merge back"). Uncommitted work in the worktree is committed
/// to the branch first. Fails without changing anything if `base` isn't
/// checked out in `repo` or the merge would conflict.
pub fn squash_merge(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
    message: &str,
) -> Result<MergeOutcome> {
    if current_branch(repo).as_deref() != Some(base) {
        bail!("check out `{base}` in {} first", repo.display());
    }
    if is_dirty(worktree)? {
        run(git(worktree).args(["add", "-A"]))?;
        run(git(worktree).args(["commit", "-q", "--no-verify", "-m", message]))?;
    }
    let ahead = run(git(repo).args(["rev-list", "--count", &format!("{base}..{branch}")]))?;
    if ahead.trim() == "0" {
        bail!("nothing to merge: `{branch}` has no changes beyond `{base}`");
    }
    let conflicts = merge_conflicts(repo, base, branch)?;
    if !conflicts.is_empty() {
        bail!(
            "merging `{branch}` into `{base}` would conflict in: {}",
            conflicts.join(", ")
        );
    }
    run(git(repo).args(["merge", "--squash", "-q", branch]))?;
    if let Err(err) = run(git(repo).args(["commit", "-q", "-m", message])) {
        // Leave the main checkout as it was.
        let _ = run(git(repo).args(["reset", "-q", "--merge"]));
        return Err(err);
    }
    let commit = run(git(repo).args(["rev-parse", "HEAD"]))?
        .trim()
        .to_owned();
    let files = run(git(repo).args(["diff", "--name-only", "HEAD~1", "HEAD"]))?
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    Ok(MergeOutcome { commit, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_all(d: &Path, msg: &str) {
        run(git(d).args(["add", "-A"])).unwrap();
        run(git(d).args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            msg,
        ]))
        .unwrap();
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        run(git(d).args(["init", "-q", "-b", "main"])).unwrap();
        run(git(d).args(["config", "user.name", "t"])).unwrap();
        run(git(d).args(["config", "user.email", "t@t"])).unwrap();
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        std::fs::write(d.join(".gitignore"), ".env*\n").unwrap();
        commit_all(d, "init");
        dir
    }

    #[test]
    fn commit_and_push_to_bare_remote() {
        let dir = repo();
        let root = dir.path();
        let remote = tempfile::tempdir().unwrap();
        run(git(remote.path()).args(["init", "-q", "--bare"])).unwrap();
        let url = remote.path().to_string_lossy().into_owned();
        run(git(root).args(["remote", "add", "origin", &url])).unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt = wt_parent.path().join("wt");
        add(root, &wt, "agentloft/pr", "main").unwrap();
        std::fs::write(wt.join("new.txt"), "x\n").unwrap();
        let env = std::env::vars().collect();
        assert_eq!(
            commit_and_push(&wt, "agentloft/pr", "main", "Add new", &env).unwrap(),
            "origin"
        );
        let log =
            run(git(remote.path()).args(["log", "--format=%s", "agentloft/pr", "-1"])).unwrap();
        assert_eq!(log.trim(), "Add new");
        assert!(!is_dirty(&wt).unwrap());
    }

    #[test]
    fn copies_matching_untracked_files() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        std::fs::write(from.path().join(".env"), "A=1").unwrap();
        std::fs::write(from.path().join(".env.local"), "B=2").unwrap();
        std::fs::write(from.path().join("README.md"), "x").unwrap();
        std::fs::create_dir_all(from.path().join("config")).unwrap();
        std::fs::write(from.path().join("config/db.local.json"), "{}").unwrap();
        std::fs::write(from.path().join("config/db.json"), "{}").unwrap();
        let copied = copy_untracked(
            from.path(),
            to.path(),
            &[
                ".env*".into(),
                "config/*.local.json".into(),
                "../etc/*".into(),
            ],
        );
        assert_eq!(copied, vec![".env", ".env.local", "config/db.local.json"]);
        assert!(wildcard("a?c*", "abcdef") && !wildcard("*.json", "x.jsonl"));
    }

    #[test]
    fn slugs() {
        assert_eq!(
            slugify("Add a greet() function!", 40),
            "add-a-greet-function"
        );
        assert_eq!(slugify("  Fix: the BUG in parser.rs  ", 12), "fix-the-bug");
        assert_eq!(slugify("日本語", 20), "");
        assert_eq!(slugify("supercalifragilistic", 5), "super");
    }

    #[test]
    fn worktree_lifecycle_and_squash_merge() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join(".env.local"), "SECRET=1\n").unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt = wt_parent.path().join("wt");

        let branch = unique_branch(root, "agentloft", "greet");
        assert_eq!(branch, "agentloft/greet");
        add(root, &wt, &branch, "main").unwrap();
        assert_eq!(current_branch(&wt).as_deref(), Some("agentloft/greet"));
        assert_eq!(
            unique_branch(root, "agentloft", "greet"),
            "agentloft/greet-2"
        );
        assert_eq!(copy_env_files(root, &wt), vec![".env.local".to_owned()]);
        assert!(!is_dirty(&wt).unwrap(), ".env is ignored");

        // Agent work in the worktree (left uncommitted) merges back squashed.
        std::fs::write(wt.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(wt.join("b.txt"), "new\n").unwrap();
        assert!(is_dirty(&wt).unwrap());
        let out = squash_merge(root, &wt, &branch, "main", "Greet").unwrap();
        assert_eq!(out.files, 2);
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "new\n"
        );
        let log = run(git(root).args(["log", "--format=%s", "-1"])).unwrap();
        assert_eq!(log.trim(), "Greet");
        assert!(squash_merge(root, &wt, &branch, "main", "again").is_err());

        // A conflicting change is refused without touching the checkout.
        std::fs::write(wt.join("a.txt"), "worktree\n").unwrap();
        std::fs::write(root.join("a.txt"), "main\n").unwrap();
        commit_all(root, "main edit");
        let err = squash_merge(root, &wt, &branch, "main", "c").unwrap_err();
        assert!(err.to_string().contains("a.txt"), "{err}");
        assert!(!is_dirty(root).unwrap() || !root.join("b.txt").exists());

        // Dirty worktrees need force to remove.
        std::fs::write(wt.join("scratch.txt"), "x").unwrap();
        assert!(remove(root, &wt, false).is_err());
        remove(root, &wt, true).unwrap();
        assert!(!wt.exists());
        prune(root).unwrap();
        assert!(branch_exists(root, &branch), "branch is kept");
    }
}
