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
| §7 worktrees | "git worktree" per agent | Snapshot-native isolated directories and a snapshot merge queue (user decision, ADR-0007) | Running `git` in the trusted backend on agent-writable repositories |
| §6.5 `register_agent` | Listed among agent-callable Hub methods | User-only: an agent cannot mint identities or grants (ADR-0023) | Agents registering peers |
| §6.5 local transport | "Unix socket / named pipe" | Loopback TCP with per-agent bearer tokens, Host and Origin checks, because MCP Streamable HTTP and A2A bind to HTTP (ADR-0004) | A socket that standard MCP/A2A clients cannot reach |
| §6.4 claim enforcement | Advisory or enforced | Enforced twice: the mediator refuses writes to others' claims, and the merge queue rejects any unclaimed change (ADR-0007) | Advisory claims only |
| §8.2(a) "model family" in teams | Where a team agent's family comes from | From its roles file's agentic route (provider family or role override), never from the team file | Declaring the family in `team.toml` |
| §8 Tester | "Tester" listed among the model roles | The backend runs the commands the user configured in `[verify]`, deterministically; no model chooses what proves a change (ADR-0024) | An LLM tester choosing its own commands |
| §8.2(b) "artifact reference" | The executor finishes before evidence exists | Claims are selectors (files, tests) that the backend resolves to `diff:`/`test:`/`coverage:` references; unresolvable claims are rejected before review (ADR-0024) | Executors citing artifact ids they cannot know |
| §8.2(d) on Windows | Commands cannot be sandboxed (ADR-0003) | The verification gate rejects every merge (`unsupported_platform`): fail closed | Running verification unsandboxed |
| §8.5 "too high: STOP" | Which reviewer it applies to, and the threshold | It applies to the LLM reviewer the product ships. The rule baseline's rate is reported, not gated. `anchor eval run --max-fn-rate` is the stop switch; the threshold is set with the measured LLM rate (pending a model key) | Gating on the rule baseline, which cannot catch semantic bugs by construction |
