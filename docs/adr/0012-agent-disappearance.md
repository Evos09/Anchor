# ADR-0012: Agents that disappear mid-task (§17 Q12)

- Status: Accepted
- Date: 2026-10-06

## Decision

- **Claims are TTL leases.** Defaults: edit 5 min, review 15 min, read
  30 min; any request is capped at 2 h. They are renewable with
  `hub_renew_claim`. Every claim result carries its `lease_expires_at`,
  so the agent knows when to renew. (The spec's warning at 80% of the TTL
  is not pushed yet; a push channel comes with SSE, ADR-0004.)
- **Expiry is swept lazily** at the start of every Hub call:
  - an expired lease becomes `expired`, and its path is claimable again;
  - an agent silent for two heartbeat intervals (default interval 60 s, so 2 min) becomes
    `lost`, its active claims expire, and its `in_progress` tasks return to
    `pending`;
  - any later call from the agent makes it `active` again.
- **Runtimes are kept alive by the backend.** It heartbeats every team
  agent from a ticker thread while the agent's runtime process runs, so a
  slow model turn never makes a live agent lost.
- **No work is discarded automatically.** An agent's isolated directory
  stays until its work is merged. A directory left by an interrupted run
  is snapshotted as `r-<root>/iso-quarantine` before it is replaced.
- **Retiring an agent** (`anchor hub agent retire`, or the end of a team
  run) revokes its claims and returns its in-progress tasks to `pending`.

Tests: `leases_expire_and_silent_agents_are_lost`, and the team e2e
suite (`crates/anchor-backend/tests/team.rs`).
