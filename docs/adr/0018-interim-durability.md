# ADR-0018: Interim durability before the snapshot engine

- Status: Accepted (temporary; superseded when ADR-0006 lands)
- Date: 2026-10-06

## Context

Constraint 4 ("every mutating turn is snapshotted first") is
non-negotiable, but the full snapshot engine is Phase 3.

## Decision

`anchor-backend/src/preimage.rs`:

- Before any file mutation, the target's pre-image (or "absent") is stored
  in a content-addressed blob store, and a per-turn manifest is written
  atomically. The first record per path per turn wins.
- Before a `run_command`, the **whole workspace** is captured. Files are
  read through the descriptor-based path. If the workspace exceeds 50,000
  files or 2 GiB, the command is **refused**: no durability, no command.
- Records made earlier in the same turn take precedence over the
  whole-workspace capture. This bug was found by the e2e test and fixed.
- `anchor rollback --turn N` undoes turns ≥ N newest-first. Each restore is
  a user-initiated, audited, approval-consuming write through `anchor-fs`.
  Blob hashes are verified, and manifest paths are re-confined (Threat 9).

## Limits

- Hashing every file before each command is O(workspace size), far from
  the Phase 3 target.
- A restore interrupted halfway leaves a partially restored workspace;
  per-file atomicity holds, whole-restore atomicity does not. The Phase 3
  engine adds a restore journal (§5.5(d)).
