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
    // `--no-track`: a branch started from `origin/main` must not push there.
    run(git(repo).args([
        "worktree",
        "add",
        "-q",
        "--no-track",
        "-b",
        branch,
        &path,
        base,
    ]))?;
    Ok(())
}

/// A repository's branches, for choosing a worktree's starting point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchList {
    /// Local branch names, most recently committed first.
    pub local: Vec<String>,
    /// `(remote, branch)` remote-tracking branches, most recent first.
    pub remote: Vec<(String, String)>,
    /// The repository's main branch (see [`default_branch`]).
    pub default: Option<String>,
    pub current: Option<String>,
}

pub fn branches(repo: &Path) -> Result<BranchList> {
    let out = run(git(repo).args([
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname)",
        "refs/heads",
        "refs/remotes",
    ]))?;
    let mut list = BranchList::default();
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("refs/heads/") {
            list.local.push(name.to_owned());
        } else if let Some(rest) = line.strip_prefix("refs/remotes/")
            && let Some((remote, name)) = rest.split_once('/')
            && name != "HEAD"
        {
            list.remote.push((remote.to_owned(), name.to_owned()));
        }
    }
    list.default = default_branch(repo);
    list.current = current_branch(repo);
    Ok(list)
}

/// The main branch: what `origin/HEAD` points at, else a local `main` or
/// `master`, else `init.defaultBranch`, else the checked-out branch.
pub fn default_branch(repo: &Path) -> Option<String> {
    let trimmed = |s: String| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
    if let Some(head) = run(git(repo).args([
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ]))
    .ok()
    .and_then(trimmed)
        && let Some(name) = head.strip_prefix("origin/")
    {
        return Some(name.to_owned());
    }
    for name in ["main", "master"] {
        if branch_exists(repo, name) {
            return Some(name.to_owned());
        }
    }
    run(git(repo).args(["config", "init.defaultBranch"]))
        .ok()
        .and_then(trimmed)
        .filter(|b| branch_exists(repo, b))
        .or_else(|| current_branch(repo))
}

/// `git fetch <remote> <branch>`, so a worktree starts from what the remote
/// has now. `env` is the login environment (credentials, SSH agent).
pub fn fetch(
    repo: &Path,
    remote: &str,
    branch: &str,
    env: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let mut cmd = git(repo);
    cmd.envs(env)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(["fetch", "-q", "--no-tags", remote, branch]);
    run(&mut cmd)?;
    Ok(())
}

/// Whether `rev` names a commit (`main`, `origin/main`, a sha).
pub fn rev_exists(repo: &Path, rev: &str) -> bool {
    run(git(repo).args(["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])).is_ok()
}

/// `git worktree add <path> <branch>` for an existing branch (unarchive).
pub fn add_existing(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path = path.to_string_lossy();
    run(git(repo).args(["worktree", "add", "-q", &path, branch]))?;
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

/// The tree a merge of `branch` into `base` would produce, and the files
/// that would conflict.
fn merge_tree(repo: &Path, base: &str, branch: &str) -> Result<(String, Vec<String>)> {
    let out = git(repo)
        .args(["merge-tree", "--write-tree", "--name-only", base, branch])
        .output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let tree = lines.next().unwrap_or_default().to_owned();
    match out.status.code() {
        Some(0) => Ok((tree, Vec::new())),
        // Conflicts: tree id, then conflicted paths, then a blank line.
        Some(1) => Ok((
            tree,
            lines
                .take_while(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
        )),
        _ => bail!(
            "git merge-tree failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Commit pending work on the worktree's branch (if any), then push it to
/// the base branch's remote (default `origin`) with upstream tracking.
/// `env` is the user's login environment (credential helpers, SSH agent).
/// With `gh`, HTTPS pushes sign in through the GitHub CLI, as `gh pr create`
/// does, instead of a credential helper that may have nothing stored.
/// Fails before pushing when the branch changes no files beyond `base`.
/// Returns the remote name.
pub fn commit_and_push(
    worktree: &Path,
    branch: &str,
    base: &str,
    message: &str,
    env: &std::collections::HashMap<String, String>,
    gh: Option<&Path>,
) -> Result<String> {
    if is_dirty(worktree)? {
        run(git(worktree).args(["add", "-A"]))?;
        run(git(worktree).args(["commit", "-q", "--no-verify", "-m", message]))?;
    }
    let fork_point = run(git(worktree).args(["merge-base", base, branch]))?;
    if git(worktree)
        .args(["diff", "--quiet", fork_point.trim(), branch])
        .status()?
        .success()
    {
        bail!("nothing to open a pull request for: `{branch}` changes no files on `{base}`");
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
        .env("LC_ALL", "C");
    if let Some(gh) = gh {
        let quoted = gh.to_string_lossy().replace('\'', "'\\''");
        cmd.args(["-c", "credential.helper="])
            .arg("-c")
            .arg(format!("credential.helper=!'{quoted}' auth git-credential"));
    }
    cmd.args(["push", "-q", "-u", &remote, &format!("{branch}:{branch}")]);
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
    let (tree, conflicts) = merge_tree(repo, base, branch)?;
    if !conflicts.is_empty() {
        bail!(
            "merging `{branch}` into `{base}` would conflict in: {}",
            conflicts.join(", ")
        );
    }
    // Commits ahead can still change nothing: an empty checkpoint, or work
    // already on `base` by another route.
    let base_tree = run(git(repo).args(["rev-parse", &format!("{base}^{{tree}}")]))?;
    if tree == base_tree.trim() {
        bail!("nothing to merge: `{branch}` changes no files on `{base}`");
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
    fn worktree_from_a_fetched_remote_branch() {
        // `upstream` has a commit the local `main` hasn't seen.
        let dir = repo();
        let root = dir.path();
        let remote = tempfile::tempdir().unwrap();
        run(git(remote.path()).args(["init", "-q", "--bare", "-b", "main"])).unwrap();
        let url = remote.path().to_string_lossy().into_owned();
        run(git(root).args(["remote", "add", "origin", &url])).unwrap();
        run(git(root).args(["push", "-q", "origin", "main"])).unwrap();
        let other = tempfile::tempdir().unwrap();
        run(git(other.path()).args(["clone", "-q", &url, "."])).unwrap();
        std::fs::write(other.path().join("remote.txt"), "new\n").unwrap();
        commit_all(other.path(), "remote work");
        run(git(other.path()).args(["push", "-q", "origin", "main"])).unwrap();
        run(git(root).args(["branch", "-q", "local-only"])).unwrap();

        let env = std::env::vars().collect();
        fetch(root, "origin", "main", &env).unwrap();
        let list = branches(root).unwrap();
        assert_eq!(list.default.as_deref(), Some("main"));
        assert_eq!(list.current.as_deref(), Some("main"));
        assert!(list.local.contains(&"local-only".to_owned()));
        assert!(
            list.remote
                .contains(&("origin".to_owned(), "main".to_owned()))
        );
        assert!(rev_exists(root, "origin/main"));
        assert!(!rev_exists(root, "origin/nope"));

        let wt_parent = tempfile::tempdir().unwrap();
        let wt = wt_parent.path().join("wt");
        add(root, &wt, "agentloft/x", "origin/main").unwrap();
        assert!(
            wt.join("remote.txt").exists(),
            "started from the fetched commit"
        );
        // No upstream: pushing must not target origin/main.
        assert!(run(git(&wt).args(["rev-parse", "--abbrev-ref", "@{upstream}"])).is_err());
    }

    #[test]
    fn commit_and_push_to_bare_remote() {
        let dir = repo();
        let root = dir.path();
        let remote = tempfile::tempdir().unwrap();
        run(git(remote.path()).args(["init", "-q", "--bare", "-b", "main"])).unwrap();
        let url = remote.path().to_string_lossy().into_owned();
        run(git(root).args(["remote", "add", "origin", &url])).unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt = wt_parent.path().join("wt");
        add(root, &wt, "agentloft/pr", "main").unwrap();
        std::fs::write(wt.join("new.txt"), "x\n").unwrap();
        let env = std::env::vars().collect();
        assert_eq!(
            commit_and_push(&wt, "agentloft/pr", "main", "Add new", &env, None).unwrap(),
            "origin"
        );
        let log =
            run(git(remote.path()).args(["log", "--format=%s", "agentloft/pr", "-1"])).unwrap();
        assert_eq!(log.trim(), "Add new");
        assert!(!is_dirty(&wt).unwrap());

        // A branch with only empty commits is refused before anything is pushed.
        add(
            root,
            &wt_parent.path().join("wt2"),
            "agentloft/empty",
            "main",
        )
        .unwrap();
        let wt2 = wt_parent.path().join("wt2");
        run(git(&wt2).args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "checkpoint",
        ]))
        .unwrap();
        let err = commit_and_push(&wt2, "agentloft/empty", "main", "x", &env, None).unwrap_err();
        assert!(err.to_string().contains("changes no files"), "{err}");
        assert!(
            run(git(remote.path()).args(["rev-parse", "--verify", "-q", "agentloft/empty"]))
                .is_err()
        );
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

        // Commits that change nothing beyond `main` are refused by name.
        run(git(&wt).args(["merge", "-q", "main"])).unwrap();
        run(git(&wt).args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "checkpoint",
        ]))
        .unwrap();
        let err = squash_merge(root, &wt, &branch, "main", "empty").unwrap_err();
        assert!(err.to_string().contains("changes no files"), "{err}");

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
