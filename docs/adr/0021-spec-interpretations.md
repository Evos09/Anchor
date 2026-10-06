# ADR-0021: Interpretations of ambiguous requirements

- Status: Accepted
- Date: 2026-10-06

| Spec | Ambiguity | Interpretation | Rejected alternative |
|---|---|---|---|
| §5.1(d) | `RESOLVE_NO_SYMLINKS` at resolution would forbid all in-workspace symlinks | BENEATH-follow at resolution; NO_SYMLINKS for every later operation (ADR-0013) | NO_SYMLINKS everywhere (breaks normal repos) |
| §5.2(a) `blast_radius_count` | Undefined for commands and searches | Files touched by the operation: 1 for file tools; number of files scanned for glob/grep; `u32::MAX` for commands (unbounded) | 0 / omitted for unknown |
| §5.2(e) format strings | No tool takes one | No format-string parameters exist in the tool surface; documented in `anchor_policy::params` | — |
| §5.3(c) "entire file was read" | Truncated reads | Only an untruncated `read_file` whose content hash still matches counts | Counting partial reads |
| §5.3(c) Edit | §5.3(c) speaks of Write only | `edit_file` replace also requires a full read (or a confirmed diff); appends to append-only files do not | Allowing blind replaces |
| §5.4(b) "pause" | Who decides | The runtime pauses after the turn's checkpoint; the backend asks the user for one of the three options; the choice is audited | Backend-side context tracking (it never sees prompts) |
| §5.5(a) snapshot granularity | "workspace or claimed subtrees" | Phase 2: per-file pre-images for file tools, whole workspace before commands (ADR-0018) | Skipping snapshots until Phase 3 |
| §8.2(a) "model family" | No definition | Declared per provider (or role override) in config; case-insensitive comparison; fallbacks included | Inferring from model-id strings |
| §11 Threat 7 root | `RLIMIT_NPROC` ignored for uid 0 | Drop commands to uid 65534 when the backend is root | Running commands as root |
