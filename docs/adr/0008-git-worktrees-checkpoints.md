# ADR-0008: Git CLI, per-thread worktrees, hidden-ref checkpoints

- **Status:** Proposed
- **Date:** 2026-11-08

## Context
Threads should be able to work in isolated git worktrees, so several agents can edit one repo at once. Every turn's changes must be diffable and revertible without disturbing the user's index or branches.

Zed, Vibex and Ghostex all shell out to the `git` CLI instead of using `git2` or `gix`. Turns can be checkpointed into hidden refs built from a temporary index.

## Decision
**Git access**
- `loft-git` wraps the `git` CLI:
  - porcelain v2 output with `-z`;
  - `GIT_OPTIONAL_LOCKS=0` when polling;
  - timeouts;
  - structured errors.
- Later we may add `gix` for hot read-only paths.

**Worktrees**
- Created with `git worktree add -b agentloft/<thread-slug> ~/.agentloft/worktrees/<project-slug>/<thread-slug> <base>`.
- Then the configured untracked files (`.env*`) are copied in and the project setup script runs in a PTY.
- On archive, the worktree is removed if clean; if dirty, the user confirms first.
- `git worktree prune` runs on startup.

**Checkpoints**
- Captured at the start and end of every turn:
  1. `GIT_INDEX_FILE=<tmp> git add -A`
  2. `write-tree`
  3. `commit-tree -p HEAD`
  4. `update-ref refs/agentloft/checkpoints/<thread-id>/<n>`
- Diffs (turn, cumulative, or between arbitrary checkpoints) are computed between these refs.
- Revert restores a checkpoint's tree into the worktree, then calls the provider's native revert where one exists (Codex `thread/revert`, Claude `rewind_files`).
- Forks branch new worktrees from checkpoint commits ([ADR-0011](0011-threads-forks-provider-pinning.md)).

### Cleaning up merged worktrees (added 2026-11)
- A worktree thread is *landed* when its worktree has nothing uncommitted and its branch's work is in the base: merging the branch into `origin/<base>` (fetched) or `<base>` would change no file (`git merge-tree --write-tree`), so squash and rebase merges count, which `git branch --merged` misses. A branch wholly inside the base counts only if it moved since it was made (its oldest reflog entry), so a thread that did nothing yet isn't landed. A merged pull request whose head is the branch as it is now also counts.
- Looked at when a turn ends (no fetch), after Merge, when its pull request turns merged, and hourly for every worktree thread (one fetch per repository and base).
- `[worktrees] cleanup = "off" | "offer" | "auto"` (default `offer`), `delete_branch` (default on), `delete_remote_branch` (off), `archive_after_cleanup` (off). Archive and delete also drop a landed branch when cleanup is on.
- Cleaning up removes the worktree (never forced) and, per the settings, `git branch -D` its branch, recording the tip so **Restore branch** can bring it back. The thread becomes `ThreadWorkspace::Released`. Its next message makes a new worktree at the **same path**, on a fresh branch from the base. The agents key sessions by folder, so the conversation resumes, and the agent gets a note that files were rebuilt from the base.
- Settings ▸ Worktrees lists leftovers: folders under the worktrees directory and `<prefix>/*` branches that no active thread uses, and removes the ones whose work is in the main branch.

## Consequences
**Benefits**
- Identical behaviour to the user's git: hooks, config, LFS, credential helpers.
- No libgit2 build issues.
- Checkpoints are real commits, so they are cheap, deduplicated, and inspectable with `git log refs/agentloft/...`.

**Costs**
- Process spawn overhead for each git call. It is small, and we will batch and cache.
- Output parsing must be robust.
- Hidden refs accumulate. They are deleted together with their thread.

**Risk**
- Large repos make `git add -A` into a temp index slow. Mitigations:
  - respect `.gitignore`;
  - add size guards;
  - make checkpointing configurable per project.

## Alternatives considered
- **`git2` (libgit2).** C dependency, and it diverges from git CLI behaviour in edge cases.
- **`gix` everywhere.** Write support for worktrees and index is less mature.
- **Copy-based snapshots.** Wasteful.
- **Worktrees inside the repo** (`.agentloft/worktrees`). They pollute the repo and confuse tooling. The product owner chose `~/.agentloft`.

## References
- Ghostex `server/src/typed_operations/worktree.rs`
