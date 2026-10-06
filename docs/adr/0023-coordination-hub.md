# ADR-0023: Coordination Hub: data model, identity, authorization, audit

- Status: Accepted
- Date: 2026-10-06

## Context

Spec §6 calls for a Hub that every agent talks through (A2S2A), holding
shared state (§6.3) and claims (§6.4), with authentication even locally
(§6.5). Threat 8 says a malicious MCP/A2A peer must not be able to bypass
the permission engine through it.

## Decision

### Storage

`anchor-hub` is a synchronous library over SQLite (rusqlite with the
bundled SQLite 3.53.2), in `<state>/hub.sqlite`.

- **Connection settings**: WAL mode, `foreign_keys`, `synchronous=FULL`
  and a 5 s busy timeout. The file is pre-created with mode 0600 and
  migrations are tracked in `user_version`.
- **Tables**: the §6.3 tables (`project`, `agent`, `claim`, `message`,
  `decision`, `task`, `artifact`), with CHECK constraints on every enum,
  plus:
  - `agent_token` (SHA-256 of each token);
  - `merge` (ADR-0007);
  - `meta`.

### Identity comes from the transport, never from parameters

| Caller | Bound by |
|---|---|
| `Agent(id)`, internal runtime | Its pipe: the backend's mediator is bound to one Hub agent (`HubBinding`) |
| `Agent(id)`, external client | Its bearer token (MCP, A2A) |
| `User` | The CLI |
| `System` | The backend itself (the merge queue) |

Tool arguments are closed structs (`deny_unknown_fields`), so a forged
`from`/`sender` field is an error, not an override.

### Authorization

Grants are given by the user (`Permissions`):

- `claim_within`: the paths the agent may claim, and so change;
- the claim types and message types it may use;
- task, decision and artifact rights;
- `messages_need_approval`.

Capabilities an agent declares grant nothing. Only the user may register or
retire agents, approve or reject held messages, and accept or reject
decisions. §6.5's agent-callable `register_agent` became user-only
(ADR-0021).

### Claims

Claim paths are workspace-relative and normalised:

- `.` is the root; a trailing `/` means a subtree;
- `..`, absolute paths, backslashes, `:` and control characters are
  refused;
- two claims overlap when one covers the other.

Conflicts: edit with edit, edit with review, and edit with an isolated read.
Leases and lost agents are covered in ADR-0012. Claims are enforced twice:

1. **Early.** The mediator refuses a write or edit on a path another agent
   has claimed (`DeniedBy::Claim`) and claims a free path for the agent.
2. **At merge (the guarantee).** The merge queue rejects every changed file
   that the agent did not claim or that lies outside its grant (ADR-0007).

### Messages

- Messages are stored, never only relayed.
- A held message, from an agent with `messages_need_approval` or of a type
  listed in `[hub] approve_message_types`, is invisible to its recipient
  until the user approves it.
- An agent sees only messages to or from itself, plus delivered room
  messages.

### Audit anchoring

Every Hub call appends a `hub_<method>` entry to the workspace's
hash-chained audit log (ADR-0016), refusals included (`hub_denied`,
`hub_unauthenticated`). The entry records the caller, the arguments (message
content only as SHA-256 and length) and the rows touched, with each row's
digest.

- Each row stores the `audit_seq`/`audit_hash` of its latest entry.
- `meta.audit_tail` records the latest Hub entry.
- `anchor hub verify` checks that every row matches its latest entry's
  digest, that every audited row exists, and that the log still contains
  `audit_tail`. Rolled-back rows, edits, insertions, deletions and
  truncation of the log's tail (up to the last Hub call) are therefore
  detected.

**Limit:** an attacker who can write the 0700 state directory can rewrite
both files consistently. This detects partial tampering and accidents. It
is not a defence against the owning user.

### Process model

The audit log is safe for several processes: each append holds a lock on
`<log>.lock` and adopts entries other processes appended. The Hub database
is shared by SQLite.

- `anchor team run` and `anchor hub serve` may run while `anchor hub …`
  commands inspect and approve.
- Snapshot-store commands stay single-process (ADR-0007).

## Alternatives rejected

- **An identity field in tool calls**: spoofable.
- **A separate audit log for the Hub**: two chains cannot anchor each
  other's tails.
- **Postgres or an embedded server**: needs a service for a local tool.

## Verification

- Hub unit tests (`crates/anchor-hub/src/tests.rs`):
  - claims;
  - a 100-way race with one winner;
  - leases;
  - message visibility and holding;
  - permissions;
  - tamper and truncation detection.
- Endpoint and rogue-agent tests: `crates/anchor-hub/tests/endpoints.rs`.
- Team e2e tests: `crates/anchor-backend/tests/team.rs`.
