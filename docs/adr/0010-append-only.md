# ADR-0010: Append-only marking and heuristic detection (§17 Q10, §5.3)

- Status: Accepted
- Date: 2026-10-06

## Decision

Detection order (`anchor-guard`):

1. **Explicit**: workspace-relative globs in `.anchor/workspace.toml`
   (`append_only = ["logs/*.md"]`).
2. **Marker**: the string `anchor:append-only` within the first 4 KiB.
3. **Heuristics**:
   - file stem (case-insensitive, extension ignored) in
     `DEVLOG, CHANGELOG, CHANGES, HISTORY, JOURNAL`;
   - **dated entries**: plain-text extensions only (`md, txt, rst, org, log,
     adoc` or none), with ≥ 3 lines starting with an ISO date (optionally
     after `#`, `-`, `*` or `[`), and dated lines ≥ 50% of
     heading-or-dated lines.
4. **Override**: `anchor mark-not-append-only PATH` persists
   `not_append_only` in the workspace config. The status is surfaced as
   `Overridden(reason)`, never silently as "no". Explicit marks and markers
   beat overrides.

Enforcement: `write_file` on an append-only file is always refused. An edit
must be a pure suffix append, verified by SHA-256 and byte comparison before
and after the write (`anchor-fs::append_verified`).

For other files: a write or edit of an existing file needs a full read in
this session whose content hash still matches, **or** the user's
confirmation of an explicit deletion diff. A truncated read never counts as
a full read.

## Acceptable false-positive rate

Target: **≤ 2%** of heuristic-flagged files overridden by users. A false
positive costs one click; a false negative can cost history. The rate is
measurable from the persisted overrides versus flagged files once telemetry
is opted in (local counters only).

Known false-positive guards are tested
(`heuristic_avoids_common_false_positives`): a README with dated release
headings among many others, a date-keyed CSV, and dated comments in code.

## Deferred

The "files the user has manually appended to repeatedly" heuristic needs
per-file history, which comes from the snapshot engine (ADR-0006). It is not
implemented yet.
