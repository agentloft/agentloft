//! Git through the `git` CLI (ADR-0008): checkpoints captured into hidden
//! refs without touching the user's index or branches, and diffs between
//! them. All functions are blocking; call them from `spawn_blocking`.

use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub mod worktree;

pub const REF_PREFIX: &str = "refs/agentloft/checkpoints";

/// The empty tree object, used as the base when a repo has no commits.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    cmd
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().context("run git")?;
    if !out.status.success() {
        bail!(
            "git {:?} failed: {}",
            cmd.get_args().collect::<Vec<_>>(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Root of the work tree containing `dir`, if it is inside a git repo.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = run(git(dir).args(["rev-parse", "--show-toplevel"])).ok()?;
    let root = out.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

fn head(dir: &Path) -> Option<String> {
    run(git(dir).args(["rev-parse", "--verify", "-q", "HEAD"]))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Write the current work tree (tracked + untracked, honoring .gitignore)
/// as a tree object via a temporary index. Returns the tree id.
pub fn snapshot_tree(dir: &Path) -> Result<String> {
    let tmp = tempfile_path(dir)?;
    let index = tmp.to_string_lossy().into_owned();
    let result = (|| {
        // Seed the temp index from HEAD so unchanged files are cheap.
        if head(dir).is_some() {
            run(git(dir)
                .env("GIT_INDEX_FILE", &index)
                .args(["read-tree", "HEAD"]))?;
        }
        run(git(dir)
            .env("GIT_INDEX_FILE", &index)
            .args(["add", "-A", "--", "."]))?;
        let tree = run(git(dir).env("GIT_INDEX_FILE", &index).args(["write-tree"]))?;
        Ok(tree.trim().to_owned())
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

fn tempfile_path(dir: &Path) -> Result<PathBuf> {
    let git_dir = run(git(dir).args(["rev-parse", "--absolute-git-dir"]))?;
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(PathBuf::from(git_dir.trim()).join(format!("agentloft-index-{}-{n}", std::process::id())))
}

/// Capture a checkpoint commit of the work tree under `ref_name`
/// (e.g. `refs/agentloft/checkpoints/<thread>/<n>`). Returns the commit id.
pub fn checkpoint(dir: &Path, ref_name: &str, message: &str) -> Result<String> {
    let tree = snapshot_tree(dir)?;
    let mut cmd = git(dir);
    cmd.args(["commit-tree", &tree, "-m", message])
        .env("GIT_AUTHOR_NAME", "Agentloft")
        .env("GIT_AUTHOR_EMAIL", "checkpoints@agentloft.local")
        .env("GIT_COMMITTER_NAME", "Agentloft")
        .env("GIT_COMMITTER_EMAIL", "checkpoints@agentloft.local");
    if let Some(parent) = head(dir) {
        cmd.args(["-p", &parent]);
    }
    let commit = run(&mut cmd)?.trim().to_owned();
    run(git(dir).args(["update-ref", ref_name, &commit]))?;
    Ok(commit)
}

/// Delete all checkpoint refs under `prefix` (e.g. when a thread is deleted).
pub fn delete_refs(dir: &Path, prefix: &str) -> Result<()> {
    let refs = run(git(dir).args(["for-each-ref", "--format=%(refname)", prefix]))?;
    for r in refs.lines().filter(|l| !l.is_empty()) {
        run(git(dir).args(["update-ref", "-d", r]))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Diff {
    pub patch: String,
    pub files: Vec<FileStat>,
}

/// Diff between two tree-ish ids (commits or trees).
pub fn diff(dir: &Path, from: &str, to: &str) -> Result<Diff> {
    let common = ["--no-color", "--no-ext-diff", "-M", "--relative"];
    let patch = run(git(dir).arg("diff").args(common).args([from, to]))?;
    let numstat = run(git(dir)
        .arg("diff")
        .args(common)
        .arg("--numstat")
        .args([from, to]))?;
    Ok(Diff {
        patch,
        files: parse_numstat(&numstat),
    })
}

/// Uncommitted work-tree changes (including untracked files) vs `HEAD`.
pub fn diff_working_tree(dir: &Path) -> Result<Diff> {
    let tree = snapshot_tree(dir)?;
    let base = head(dir).unwrap_or_else(|| EMPTY_TREE.to_owned());
    diff(dir, &base, &tree)
}

/// Diff from a commit to the current work tree.
pub fn diff_to_working_tree(dir: &Path, from: &str) -> Result<Diff> {
    let tree = snapshot_tree(dir)?;
    diff(dir, from, &tree)
}

fn parse_numstat(text: &str) -> Vec<FileStat> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?;
            let removed = parts.next()?;
            let path = parts.next()?.to_owned();
            let binary = added == "-" && removed == "-";
            Some(FileStat {
                path: normalize_rename(&path),
                added: added.parse().unwrap_or(0),
                removed: removed.parse().unwrap_or(0),
                binary,
            })
        })
        .collect()
}

/// `src/{a => b}.rs` / `a => b` → the new path.
fn normalize_rename(path: &str) -> String {
    if let (Some(open), Some(close)) = (path.find('{'), path.find('}'))
        && let Some(arrow) = path[open..close].find(" => ")
    {
        let new = &path[open + arrow + 4..close];
        return format!("{}{}{}", &path[..open], new, &path[close + 1..]).replace("//", "/");
    }
    match path.split_once(" => ") {
        Some((_, new)) => new.to_owned(),
        None => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        run(git(d).args(["init", "-q"])).unwrap();
        std::fs::write(d.join("a.txt"), "one\ntwo\n").unwrap();
        run(git(d).args(["add", "."])).unwrap();
        run(git(d).args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ]))
        .unwrap();
        dir
    }

    #[test]
    fn checkpoints_and_diffs_without_touching_index() {
        let dir = repo();
        let d = dir.path();
        let before = checkpoint(d, "refs/agentloft/checkpoints/t/1", "before").unwrap();
        std::fs::write(d.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(d.join("new.txt"), "hello\n").unwrap();
        std::fs::write(d.join(".gitignore"), "ignored.log\n").unwrap();
        std::fs::write(d.join("ignored.log"), "x\n").unwrap();
        let after = checkpoint(d, "refs/agentloft/checkpoints/t/2", "after").unwrap();

        let diff = diff(d, &before, &after).unwrap();
        let paths: Vec<_> = diff.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"a.txt") && paths.contains(&"new.txt"),
            "{paths:?}"
        );
        assert!(!paths.contains(&"ignored.log"));
        let a = diff.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!((a.added, a.removed), (2, 1));
        assert!(diff.patch.contains("+TWO"));

        // The user's index is untouched: nothing staged.
        let staged = run(git(d).args(["diff", "--cached", "--name-only"])).unwrap();
        assert!(staged.trim().is_empty(), "{staged}");

        let wt = diff_working_tree(d).unwrap();
        assert_eq!(wt.files.len(), 3); // a.txt, new.txt, .gitignore

        delete_refs(d, "refs/agentloft/checkpoints/t").unwrap();
        let left = run(git(d).args(["for-each-ref", REF_PREFIX])).unwrap();
        assert!(left.trim().is_empty());
    }

    #[test]
    fn works_in_repo_without_commits_and_detects_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(repo_root(dir.path()).is_none());
        run(git(dir.path()).args(["init", "-q"])).unwrap();
        std::fs::write(dir.path().join("x"), "1\n").unwrap();
        let wt = diff_working_tree(dir.path()).unwrap();
        assert_eq!(wt.files.len(), 1);
        assert!(repo_root(dir.path()).is_some());
    }

    #[test]
    fn rename_paths() {
        assert_eq!(normalize_rename("src/{a => b}.rs"), "src/b.rs");
        assert_eq!(normalize_rename("old.rs => new.rs"), "new.rs");
        assert_eq!(normalize_rename("src/{ => sub}/x.rs"), "src/sub/x.rs");
    }
}
