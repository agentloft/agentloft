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
