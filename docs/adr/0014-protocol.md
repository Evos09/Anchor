# ADR-0014: Backend ↔ runtime protocol (§4)

- Status: Accepted
- Date: 2026-10-06

## Decision

- Newline-delimited JSON over the runtime's stdin/stdout; stderr is
  diagnostics only (forwarded to the user).
- Frame: `{"v": 1, "id": n, "msg": {"kind": "...", "payload": {...}}}`.
- `deny_unknown_fields` on every type, a closed `ToolRequest` enum, and a
  bounded frame size (16 MiB). The reader stops at the limit instead of
  buffering an unbounded line. Parameter strings are capped at 4 KiB with no
  NUL; file content is capped at 4 MiB.
- Version mismatch is reported precisely (`VersionMismatch`) before full
  decoding.
- Direction is enforced: the backend kills the runtime if it sends a
  backend-only message (`Welcome`, `Task`, `ToolResult`, …).
- The runtime receives everything it needs (roles config, mock script,
  resume checkpoint) inside the `Task` frame, because it cannot read files.

## Versions

| Version | Phase | Change |
|---|---|---|
| 1 | 1 | Initial protocol |
| 2 | 4 | Hub tools and identity (`HubInfo`, ADR-0023) |
| 3 | 5 | `Done.claims` and the runtime-local `submit_claims` tool; review tasks (`Task.review`, no workspace tools, `submit_verdicts`, `Message::Verdicts`) (ADR-0024) |

A peer of another version is refused at the handshake.

## Alternatives rejected

- *gRPC / Cap'n Proto*: schema tooling we do not need for a two-party,
  same-machine pipe. JSON keeps the protocol inspectable in the audit log.
