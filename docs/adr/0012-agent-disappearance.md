# ADR-0012: Agents that disappear mid-task (§17 Q12)

- Status: Proposed (Phase 4)
- Date: 2026-10-06

## Decision (direction)

- Every claim is a TTL lease, renewed by heartbeat. Defaults: edit 5 min,
  review 15 min, read 30 min. The Hub warns the agent at 80% of TTL.
- A missed lease moves the claim to `expired` and makes it claimable. The
  agent's `status` becomes `lost` after two missed heartbeats.
- The worktree of a lost agent is **quarantined, not deleted**: its branch is
  snapshotted, then marked for user review in the merge queue UI. No work is
  discarded automatically.
- In-flight tasks of a lost agent go back to `pending`, with a `status`
  message that links the quarantined branch and the agent's last checkpoint.
