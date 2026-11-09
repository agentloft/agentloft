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

/// Copy top-level untracked `.env*` files (secrets the agent's build needs
/// but git doesn't carry) from the main checkout into a new worktree.
pub fn copy_env_files(from: &Path, to: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Vec::new();
    };
    let mut copied = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_file = entry.file_type().is_ok_and(|t| t.is_file());
        if !is_file || !name.starts_with(".env") || to.join(&name).exists() {
            continue;
        }
        if std::fs::copy(entry.path(), to.join(&name)).is_ok() {
            copied.push(name);
        }
    }
    copied.sort();
    copied
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
