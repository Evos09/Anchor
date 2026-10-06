# Anchor

A trust-first desktop coding agent: one developers can trust not to lose
their work, leak their credentials, or ship unverified changes.

> **Status: first push — ADRs, the Phase 1 skeleton, and the Phase 2 safety
> core, backend-only.** No desktop UI yet. Read
> [What is and isn't done](#what-is-and-isnt-done) before relying on
> anything here.

## Architecture

Three isolated layers (spec §4):

```
 ┌──────────────────────────────┐   NDJSON over stdio    ┌──────────────────────────┐   HTTPS   ┌───────────────┐
 │ Layer 1: anchor (backend)    │ ◀────────────────────▶ │ Layer 2: anchor-agent    │ ────────▶ │ Layer 3:      │
 │ sole OS authority            │   versioned, closed    │ agent loop, router,      │           │ model         │
 │ path resolution · policy ·   │   schema, size-capped  │ provider adapters        │           │ providers     │
 │ guard · durability · sandbox │                        │ NO fs / shell authority  │           │ (untrusted)   │
 └──────────────────────────────┘                        └──────────────────────────┘           └───────────────┘
```

Every tool call the agent makes goes through this pipeline in the backend:

> validate parameters → resolve the canonical path (kernel-confined) → check
> the append-only guard and read ledger → structured policy decision (prompts
> show only the canonical path) → capture a pre-image → execute on file
> descriptors, consuming a one-shot approval → audit (hash-chained)

| Crate | Role | Spec |
|---|---|---|
| `anchor-protocol` | Versioned wire protocol between backend and runtime | §4 |
| `anchor-fs` | Canonical path resolution and TOCTOU-safe, descriptor-based file operations | §5.1 |
| `anchor-policy` | Structured permission engine, TOML policy, hot reload, hash-chained audit log | §5.2 |
| `anchor-guard` | Append-only guard and session read ledger | §5.3 |
| `anchor-checkpoint` | Context-window tracking, pause at threshold, atomic checkpoints | §5.4 |
| `anchor-exec` | argv-only command runner: rlimits, network namespace, Landlock / Seatbelt | §11 |
| `anchor-runtime` | `anchor-agent`: agent loop, deterministic router, OpenAI-compatible + mock providers | §3.7, §9 |
| `anchor-backend` | `anchor`: mediator, session supervisor, interim durability, rollback, CLI | §3.2 |

## Try it

```sh
cargo build
./examples/demo/setup.sh /tmp/anchor-demo        # prints the workspace path
./target/debug/anchor run --workspace /tmp/anchor-demo/ws \
    --task "Make the greeting mention anchor and log it" \
    --roles examples/demo/roles.mock.toml \
    --mock-script examples/demo/script.json
```

The scripted "model" tries:

- path traversal;
- a symlink to a fake `~/.ssh`;
- rewriting an append-only changelog;
- overwriting a file it never read;
- disabling the policy.

Then it does real work. Each refusal is shown, every prompt shows the
canonical path, and the session ends with a cost summary. Afterwards:

```sh
./target/debug/anchor audit verify --workspace /tmp/anchor-demo/ws
./target/debug/anchor audit query  --workspace /tmp/anchor-demo/ws --kind decision
./target/debug/anchor checkpoint list --workspace /tmp/anchor-demo/ws
./target/debug/anchor rollback --workspace /tmp/anchor-demo/ws --turn 3
```

For real models, copy `config/roles.example.toml`. Point it at a local
OpenAI-compatible server (`anchor detect-local`) and/or a hosted API with
your own key (`api_key_env`).

## Tests

```sh
cargo test --workspace                       # 115 tests on Linux, including the attack suites
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check                             # advisories, licenses, bans, sources
```

[`docs/threat-model.md`](docs/threat-model.md) maps every §5 test bullet and
every §11 threat to its test and its per-platform status.

## What is and isn't done

**Done and verified in blocking CI on Linux, macOS and Windows**
(per-platform detail in [`docs/threat-model.md`](docs/threat-model.md)):

- §5.1–5.4 with their attack suites. The symlink and hardlink attacks run
  on Linux and macOS; case and Unicode collisions on macOS and Windows.
- Threats 1–7 and 9 (interim store) on Linux. On stock Ubuntu ≥ 23.10,
  where user namespaces are restricted, network denial falls back to a
  seccomp socket filter. macOS covers the same threats except memory limits
  and a direct egress test.
- Phase 2's instrumentation criteria.
- The runtime as a separate, Landlock-confined process (Linux).
- The versioned, validated protocol.
- Kill-mid-turn recovery.

**Known platform gaps:**

- Windows: agent commands refuse to run (no AppContainer sandbox yet), and
  there are no junction/reparse-point attack tests yet.
- macOS: memory is not limited (`RLIMIT_AS` is unsupported), and Seatbelt
  network/read confinement has no direct test yet.
- The OpenAI-compatible adapter has not been run against a live endpoint:
  there was no local server or API key in the build environment.

**Not started (later phases):**

- Snapshot engine (Phase 3; the interim pre-image store is ADR-0018).
- Coordination Hub, MCP/A2A, worktrees (Phase 4).
- Verification pipeline and seeded-bug harness (Phase 5).
- Desktop UI (Phase 6).
- General seccomp syscall filter, Windows sandbox, external audit (Phase 7).

The architecture decisions, including those still pending evaluation, are
in [`docs/adr/`](docs/adr/0000-adr-process.md). Dependency vetting is in
[`docs/deps/dependency-records.md`](docs/deps/dependency-records.md).

## Commitments

Local-first. No training on user code. No telemetry: the code contains
none, and any future telemetry will be opt-in only and show exactly what it
sends. No ad-supported tier. See ADR-0019.

## License

Apache-2.0 (see `LICENSE` and ADR-0019).
