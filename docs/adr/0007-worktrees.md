# ADR-0007: Agent isolation and the merge queue (§17 Q7, §7)

- Status: Accepted
- Date: 2026-10-06

## Decision: snapshot-native isolation, not git worktrees

This is the user's decision (2026-10-06) and a deviation from §7's
`git worktree` wording.

Each team agent works in a private directory,
`<state_base>/iso/<workspace-hash>/a<agent-id>/`:

- it is materialized from the workspace's snapshot store, so it includes
  gitignored files such as `.env` and is usable at once;
- it has mode 0700 and lies outside both the workspace and its protected
  state directory;
- it is its own snapshot root (ADR-0006). Objects are shared; the index,
  head ref, journal and `r-<root>/…` refs are its own.

**Why not git.** Running `git` in the trusted backend against an
agent-writable repository lets hooks, fsmonitor, filters and config execute
agent-controlled code, and Windows has no command sandbox yet. The snapshot
engine already provides atomic, verified restore and a three-way merge.
Git branch export is later work.

## Merge queue (`anchor-backend/src/queue.rs`)

Merges are strictly serialized. Every state is a Hub `merge` row
(queued → applying → merged | rejected) and an audit entry.

1. **Claim check**, the hard guarantee behind claims. Every file changed
   between the agent's base and its submission (`diff_files`: new
   directories file by file) must be:
   - within the agent's granted `claim_within`;
   - covered by an edit claim the agent held at some point;
   - not under another agent's active edit claim.

   A change made without the Hub, for example by a command in the agent's
   own directory, is therefore rejected here.
2. **Merge**: `merged = merge3(base, pre, submitted)`, where `pre` is a
   fresh snapshot of main. `merges/<seq>-cand` names `merged` at once, so
   gc can never collect it.
3. **Gate**: the `MergeGate` hook, where Phase 5 plugs in verification.
   Phase 4 ships no gate.
4. **Apply**, after the user's approval (or `--auto-merge`):
   - `restore_expect(main, pre, merged)` is atomic and journaled, and
     refuses if main changed since `pre`. In that case nothing is touched
     and the merge is recomputed, up to 3 times. A merge therefore never
     reverts concurrent edits to main.
   - Refs: `merges/<seq>` (applied) and `merges/<seq>-pre`.
5. **Rejection** carries structured feedback (paths, reason, base and main
   ids), stored on the row and sent to the agent as a `review` message.
6. **Revert** (§7.3) is `merge3(post_k, main, pre_k)`. It undoes exactly
   that merge's changes and keeps everything since. If later changes touch
   the same paths, it refuses and changes nothing. CLI:
   `anchor queue revert --agent NAME | --seq N`.
7. **Crash recovery**: opening the workspace finishes or undoes an
   interrupted restore (Phase 3). The next run then finalizes `applying`
   rows whose result is in place and re-queues the rest
   (`kill_mid_merge_recovers`).

## Process model

The snapshot store keeps its exclusive lock, so the process running
`anchor team run` owns merges. Hub-only CLI commands (`anchor hub …`,
`anchor queue list`) share the Hub database and the multi-process audit
log, and work alongside it. Store commands (`queue revert`, `rollback`)
refuse while it runs.

## Costs

Store operations of all agents are serialized by one lock. An incremental
snapshot of an isolated root costs about what a main snapshot does
(benchmarks/snapshot.md).
