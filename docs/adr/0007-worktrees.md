# ADR-0007: Worktree management (§17 Q7, §7)

- Status: Proposed (Phase 4)
- Date: 2026-10-06

## Decision (direction)

Build a thin lifecycle manager over the `git` CLI, invoked through
`anchor-exec` (argv only, confined, audited), rather than linking a git
library:

1. `git fetch` (if a remote exists; this needs network approval).
2. `git worktree add -b anchor/<agent>/<task> <path> <base-ref>`.
3. Environment setup comes from a declarative per-workspace recipe: files
   to copy (e.g. `.env`) and commands to run. Each command goes through the
   permission engine like any other.
4. Teardown: `git worktree remove --force` plus deleting the branch, after
   the merge queue has either integrated or archived the branch.

Snapshots compose per worktree: each worktree is its own snapshot root, and
a merge is recorded as a snapshot of main (§7.3).

## Alternatives

- `gix`/`git2` libraries. Their worktree support needs its own five-step
  evaluation, and the CLI is the reference behaviour users already trust.
