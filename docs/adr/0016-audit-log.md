# ADR-0016: Audit log format (§5.2(f))

- Status: Accepted
- Date: 2026-10-06

## Decision

`.anchor/audit.jsonl` holds one JSON object per line:
`{seq, ts_ms, kind, data, prev, hash}`. The hash is SHA-256 over the
canonical JSON encoding of `{seq, ts_ms, kind, data, prev}`; serde_json maps
are ordered, so the encoding is canonical. Each entry is fsynced. The log is
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

## Known limitation

A hash chain detects modification, deletion and reordering of entries, but
**not truncation of the tail**. Detecting that needs an external anchor (the
last hash stored elsewhere, e.g. in the Hub database or the OS keychain).
Tracked for Phase 4.
