# ADR-0016: Audit log format (§5.2(f))

- Status: Accepted
- Date: 2026-10-06

## Decision

`audit.jsonl` in the workspace's state directory (ADR-0022) holds one JSON object per line:
`{seq, ts_ms, kind, data, prev, hash}`. The hash is SHA-256 over the
canonical JSON encoding of `{seq, ts_ms, kind, data, prev}`. serde_json maps
are ordered, so the encoding is canonical. Floats must re-encode exactly
after a read, so serde_json's `float_roundtrip` feature is on; without it,
`verify` rejected valid entries holding values such as cost sums (found in
Phase 4; test `float_values_verify`). Each entry is fsynced. The log is
opened in append mode, and `AuditLog::open` refuses to extend a chain that
does not verify.

Logged kinds include: `decision` (request, matched rule, phase, action),
`prompt_response`, `user_initiated`, `mutation` (decision id,
before/after hashes, guard basis), `command`, `policy_loaded`,
`policy_reloaded`, `policy_reload_failed`, `checkpoint_saved`, `usage`,
`notice`, `pause_resolved`, `rollback_turn`, `user_override`, and
`session_*`. If writing the log fails, the decision fails closed (deny).

Query with `anchor audit query --workspace DIR [--kind K]`; verify with
`anchor audit verify`.

Several processes may append (Phase 4): each append takes an exclusive
lock on `<log>.lock`, verifies and adopts entries appended by others, then
writes, so the chain stays linear. A log that shrank under a handle is
refused. Entries written for a team agent carry an `agent` field.

## Known limitation

A hash chain detects modification, deletion and reordering of entries, but
not, on its own, **truncation of the tail**. Since Phase 4 the Hub database
anchors the log: it records its latest Hub entry, and `anchor hub verify`
detects a log cut back past it (ADR-0023). Truncation after the last Hub
call, and a consistent rewrite of both files by someone who can write the
state directory, remain undetected. A keychain or remote anchor is future
work.
