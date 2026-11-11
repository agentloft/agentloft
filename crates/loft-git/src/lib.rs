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

/// Make the work tree under `dir` match `commit` again (revert to a
/// checkpoint): files added since are removed, changed or deleted ones are
/// restored. The index and `HEAD` are untouched. Returns the paths changed.
pub fn restore_worktree(dir: &Path, commit: &str) -> Result<Vec<String>> {
    let now = snapshot_tree(dir)?;
    let out = run(git(dir).args([
        "diff",
        "--no-renames",
        "--relative",
        "--name-status",
        "-z",
        commit,
        &now,
    ]))?;
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let mut removed = Vec::new();
    let mut restore = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        match status {
            // Added since the checkpoint: delete it.
            "A" => removed.push(path.to_owned()),
            _ => restore.push(path.to_owned()),
        }
    }
    for path in &removed {
        let file = dir.join(path);
        if std::fs::remove_file(&file).is_ok() {
            // Drop directories the agent created and left empty.
            let mut parent = file.parent();
            while let Some(p) = parent {
                if p == dir || std::fs::remove_dir(p).is_err() {
                    break;
                }
                parent = p.parent();
            }
        }
    }
    if !restore.is_empty() {
        let mut cmd = git(dir);
        cmd.args(["restore", "--worktree", "--source", commit, "--"])
            .args(&restore);
        run(&mut cmd)?;
    }
    let mut changed = removed;
    changed.extend(restore);
    changed.sort();
    Ok(changed)
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

/// Per-file line counts between two tree-ish ids (no patch text).
pub fn diff_stat(dir: &Path, from: &str, to: &str) -> Result<Vec<FileStat>> {
    let numstat = run(git(dir)
        .arg("diff")
        .args([
            "--no-color",
            "--no-ext-diff",
            "-M",
            "--relative",
            "--numstat",
        ])
        .args([from, to]))?;
    Ok(parse_numstat(&numstat))
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
    fn restore_worktree_to_checkpoint() {
        let dir = repo();
        let d = dir.path();
        std::fs::write(d.join("keep.txt"), "user's untracked file\n").unwrap();
        let before = checkpoint(d, "refs/agentloft/checkpoints/t/1-before", "before").unwrap();
        // The agent edits, deletes and creates files.
        std::fs::write(d.join("a.txt"), "changed\n").unwrap();
        std::fs::remove_file(d.join("keep.txt")).unwrap();
        std::fs::create_dir_all(d.join("src/new")).unwrap();
        std::fs::write(d.join("src/new/mod.rs"), "x").unwrap();
        let index_before = run(git(d).args(["diff", "--cached", "--name-only"])).unwrap();
        let changed = restore_worktree(d, &before).unwrap();
        assert_eq!(changed, vec!["a.txt", "keep.txt", "src/new/mod.rs"]);
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("keep.txt")).unwrap(),
            "user's untracked file\n"
        );
        assert!(!d.join("src").exists(), "empty dirs removed");
        assert_eq!(
            run(git(d).args(["diff", "--cached", "--name-only"])).unwrap(),
            index_before
        );
        assert!(
            restore_worktree(d, &before).unwrap().is_empty(),
            "idempotent"
        );
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

/// A new, empty repository at `dir` (created) on branch `main`.
pub fn init_repo(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    run(git(dir).args(["init", "--quiet", "--initial-branch=main"]))?;
    Ok(())
}

/// A README naming the project and, when git knows who's committing, a
/// first commit. Returns whether it committed.
pub fn first_commit(
    dir: &Path,
    name: &str,
    env: &std::collections::HashMap<String, String>,
) -> Result<bool> {
    std::fs::write(dir.join("README.md"), format!("# {name}\n"))
        .with_context(|| format!("write README in {}", dir.display()))?;
    let who = |key: &str| {
        let mut c = git(dir);
        c.envs(env).args(["config", key]);
        run(&mut c).map(|v| !v.trim().is_empty()).unwrap_or(false)
    };
    if !who("user.name") || !who("user.email") {
        return Ok(false);
    }
    let mut add = git(dir);
    add.envs(env).args(["add", "-A"]);
    run(&mut add)?;
    let mut commit = git(dir);
    commit
        .envs(env)
        .args(["commit", "--quiet", "-m", "Initial commit"]);
    run(&mut commit)?;
    Ok(true)
}

/// A private GitHub repository named `name` for the repo at `dir`, added
/// as `origin` (and pushed when it has commits), through `gh`.
pub fn publish_github(
    dir: &Path,
    name: &str,
    push: bool,
    env: &std::collections::HashMap<String, String>,
) -> Result<()> {
    if !which("gh", env) {
        bail!("the GitHub CLI (gh) isn't installed");
    }
    let mut cmd = Command::new("gh");
    cmd.args(["repo", "create", name, "--private", "--source", "."])
        .args(["--remote", "origin"]);
    if push {
        cmd.arg("--push");
    }
    cmd.current_dir(dir)
        .envs(env)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null());
    let out = cmd.output().context("run gh")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// The GitHub account `gh` is signed in to, if any.
pub fn github_login(env: &std::collections::HashMap<String, String>) -> Option<String> {
    if !which("gh", env) {
        return None;
    }
    let out = Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .envs(env)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let login = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !login.is_empty()).then_some(login)
}

/// Clone `url` into `dest` (which must not exist yet). `env` is the login
/// environment (credentials, SSH agent); GitHub `owner/repo` goes through
/// `gh` when it's installed (private repos), else plain https.
pub fn clone_repo(
    source: &CloneSource,
    dest: &Path,
    env: &std::collections::HashMap<String, String>,
) -> Result<()> {
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    let parent = dest.parent().context("no parent folder")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut cmd = match source {
        CloneSource::GitHub(repo) if which("gh", env) => {
            let mut c = Command::new("gh");
            c.args(["repo", "clone", repo]).arg(dest);
            c
        }
        CloneSource::GitHub(repo) => {
            let mut c = Command::new("git");
            c.args([
                "clone",
                "--quiet",
                &format!("https://github.com/{repo}.git"),
            ])
            .arg(dest);
            c
        }
        CloneSource::Url(url) => {
            let mut c = Command::new("git");
            c.args(["clone", "--quiet", url]).arg(dest);
            c
        }
    };
    cmd.current_dir(parent)
        .envs(env)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    let out = cmd.output().context("run clone")?;
    if !out.status.success() {
        bail!(
            "clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Where to clone from.
pub enum CloneSource {
    Url(String),
    /// `owner/repo`.
    GitHub(String),
}

impl CloneSource {
    /// The folder name a clone gets: the repo's name.
    pub fn name(&self) -> String {
        let s = match self {
            CloneSource::Url(u) | CloneSource::GitHub(u) => u.trim_end_matches('/'),
        };
        let last = s.rsplit(['/', ':']).next().unwrap_or(s);
        last.strip_suffix(".git").unwrap_or(last).to_owned()
    }
}

fn which(bin: &str, env: &std::collections::HashMap<String, String>) -> bool {
    let path = env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    std::env::split_paths(&path).any(|d| d.join(bin).is_file())
}

#[cfg(test)]
mod source_tests {
    use super::CloneSource;

    #[test]
    fn clone_folders_are_named_after_the_repo() {
        assert_eq!(
            CloneSource::Url("https://github.com/a/b.git".into()).name(),
            "b"
        );
        assert_eq!(
            CloneSource::Url("git@github.com:a/tool.git".into()).name(),
            "tool"
        );
        assert_eq!(
            CloneSource::Url("https://x.org/repo/".into()).name(),
            "repo"
        );
        assert_eq!(
            CloneSource::GitHub("zed-industries/zed".into()).name(),
            "zed"
        );
    }
}
