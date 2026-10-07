# Anchor

A trust-first desktop coding agent: one developers can trust not to lose
their work, leak their credentials, or ship unverified changes.

> **Status: Phases 1–7 are implemented, with these limits:**
>
> - **External audit:** pending; it needs a third party.
> - **Interface:** a loopback web dashboard, with no desktop shell yet. Read
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
> show only the canonical path) → snapshot the workspace (once per turn) → execute on file
> descriptors, consuming a one-shot approval → audit (hash-chained)

| Crate | Role | Spec |
|---|---|---|
| `anchor-protocol` | Versioned wire protocol between backend and runtime | §4 |
| `anchor-fs` | Canonical path resolution and TOCTOU-safe, descriptor-based file operations | §5.1 |
| `anchor-policy` | Structured permission engine, TOML policy, hot reload, hash-chained audit log | §5.2 |
| `anchor-guard` | Append-only guard and session read ledger | §5.3 |
| `anchor-checkpoint` | Context-window tracking, pause at threshold, atomic checkpoints | §5.4 |
| `anchor-exec` | argv-only command runner: rlimits, network namespace, Landlock / Seatbelt | §11 |
| `anchor-snapshot` | Content-addressed, chunked snapshots; journaled, verified restore; diff / merge3 | §5.5 |
| `anchor-runtime` | `anchor-agent`: agent loop, deterministic router, OpenAI-compatible + mock providers | §3.7, §9 |
| `anchor-hub` | Coordination Hub: SQLite shared state, claims, message bus, permissions, audit anchoring; MCP 2026-07-28 and A2A 1.0 endpoints | §6 |
| `anchor-verify` | Verification logic: unittest and coverage parsing, claim resolution, the rule reviewer, the merge decision and feedback | §8 |
| `anchor-backend` | `anchor`: mediator, session supervisor, rollback, isolated workspaces, merge queue, team runner, verification gate, seeded-bug harness, CLI | §3.2, §7, §8 |

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
./target/debug/anchor snapshot list --workspace /tmp/anchor-demo/ws
./target/debug/anchor rollback --workspace /tmp/anchor-demo/ws --turn 3
./target/debug/anchor snapshot verify --workspace /tmp/anchor-demo/ws
```

Every turn that changes anything is snapshotted first. Rollback shows the
changes and asks before applying them. The restore is all-or-nothing:

- it is journaled and verified by rescanning the workspace;
- an interrupted restore is undone the next time the workspace is opened;
- the state just before a rollback is kept, so the rollback itself can be
  undone.

Policy, audit log, checkpoints and snapshots live outside the workspace, so
agent commands can't reach them (ADR-0022). `anchor state-dir --workspace
DIR` prints the location; set `ANCHOR_STATE_DIR` to move it.

### A team of agents (Phase 4)

```sh
mkdir -p /tmp/anchor-team/src && echo "# demo" > /tmp/anchor-team/README.md
./target/debug/anchor team run --workspace /tmp/anchor-team \
    --team examples/team/team.mock.toml --auto-approve --auto-merge
./target/debug/anchor hub messages --workspace /tmp/anchor-team   # the agents' conversation
./target/debug/anchor queue list   --workspace /tmp/anchor-team   # what merged, and why
./target/debug/anchor hub verify   --workspace /tmp/anchor-team   # the Hub matches the audit log
```

Two scripted agents of different model families collaborate:

1. `alpha` claims and writes `src/greet.py`, then asks `beta` for tests.
2. `beta` reads the message, claims `tests/` and writes them.

Each agent works in its own isolated copy of the workspace. Its changes
reach the workspace only through the merge queue, and only for files it
claimed (ADR-0007). External agents connect with
`anchor hub agent add` (prints a token) and `anchor hub serve` (MCP and A2A
on 127.0.0.1, ADR-0004).

### Verified merges and the seeded-bug harness (Phase 5)

[`examples/verify`](examples/verify/README.md) is a team whose scripted
executor ships a bug:

1. Verification rejects it with structured feedback: the failing test,
   the command output and the diff. Verification here means the build,
   test and coverage commands the user put in `[verify]`, run on the merge
   candidate in a sandbox, then a reviewer of another model family.
2. The repair attempt fixes the bug and merges.
3. Every command's output and every decision is stored as Hub evidence
   (`anchor hub artifacts`).

```sh
./target/debug/anchor eval run --corpus examples/eval/corpus             # rule baseline
./target/debug/anchor eval run --corpus examples/eval/corpus --reviewer oracle
```

The harness checks every seeded bug (48, in 12 tasks) with the same gate
and reports how many would merge, overall and per bug category
([results](docs/benchmarks/reviewer.md)).

### The collaboration dashboard (Phase 6)

```sh
npm --prefix ui ci --ignore-scripts && npm --prefix ui run build   # once; then rebuild anchor
cargo build
cp -r examples/ui/workspace /tmp/ui-demo
./target/debug/anchor team run --workspace /tmp/ui-demo --team examples/ui/team.toml --ui
```

Open the printed `http://127.0.0.1:…/#t=…` URL. The token in the URL is
the only credential. [`examples/ui`](examples/ui/README.md) is a scripted
two-model collaboration, and in it you watch:

- the roster (thinking, waiting, blocked, done);
- every approval gate, which you answer in the page;
- a rejected change with its diff and test evidence side by side;
- the repair;
- a refused claim, in red on the claim map;
- a held message and a proposed decision;
- the cost meter, recomputed from your own prices;
- the snapshot timeline, with a one-click restore.

`anchor ui --workspace DIR` opens the same dashboard after a run.
[`docs/ui-walkthrough.md`](docs/ui-walkthrough.md) tours the panels with
screenshots.

For real models, copy `config/roles.example.toml`. Point it at a local
OpenAI-compatible server (`anchor detect-local`) and/or a hosted API with
your own key (`api_key_env`).

## Tests

```sh
cargo test --workspace                       # 242 tests on Linux, including the attack suites
npm --prefix ui run build && (cd ui && npx playwright test)   # the dashboard, end to end
cargo run --release -p anchor-snapshot --example bench   # 100k-file snapshot benchmark
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
- Phase 3 durability, covering every §5.5 test bullet:
  - **Engine:** content-addressed, FastCDC-chunked snapshots taken before
    each mutating turn.
  - **Speed:** on Linux, incremental snapshots of 100k files take 0.4–0.8 s
    with no changes and 0.7–1.1 s with 100 files changed
    ([benchmark](docs/benchmarks/snapshot.md); CI gates Linux at 2 s).
    The macOS and Windows CI runners do not meet the 2 s target yet
    (2.5 s and 4.0 s with no changes; both scan-bound).
  - **Rollback:** atomic, journaled restore; kill-mid-snapshot and
    kill-mid-restore are tested in child processes.
  - **Merge-queue groundwork:** diff, path-level three-way merge and
    checkout into an empty workspace (ADR-0006).
- State directory outside the workspace, beyond every command sandbox
  (ADR-0022).
- Phase 4, the Coordination Hub (ADR-0004, 0007, 0012, 0023):
  - **Endpoints:** MCP 2026-07-28 and A2A 1.0, implemented from the
    specifications.
  - **Shared state:** the §6.3 schema in SQLite, with leased claims and a
    persistent message bus that can hold messages for approval.
  - **Isolation:** snapshot-native isolated workspaces and a serialized
    merge queue that rejects unclaimed changes and never reverts
    concurrent edits.
  - **Audit:** every Hub call is audited and cross-checked by
    `anchor hub verify`.
  - **Done-when tests** (`crates/anchor-backend/tests/team.rs`):
    - two families collaborating;
    - two agents claiming one file;
    - adjacent files merged from parallel isolated agents, then one
      reverted;
    - kill mid-merge;
    - messages persisting across restart;
    - the Threat 8 rogue-agent suite.

- Phase 5, the verification pipeline (ADR-0024).
  - **Deterministic Tester:** build, test and coverage commands from the
    user's `[verify]`, sandboxed.
  - **Claims and evidence:** claims resolved to evidence, and claims
    without evidence rejected automatically.
  - **Hard rules:** protected tests, no test regressions, and spoofed-exit
    and stdlib-shadowing detection.
  - **Reviewers:** the rule baseline, or a model reviewer of another
    family. The family check runs at start and again on what the review
    session actually ran on.
  - **Evidence:** Hub evidence for every check.
  - **Self-repair:** bounded, with structured feedback.
  - **Gate behaviour:** runs without the store lock, re-verifies when main
    drifts. On Windows it runs in AppContainers (ADR-0026).
  - **Seeded-bug harness:** the rule baseline lets 15 of 48 bugs through
    (FN 31.2%: every semantic and vacuous-test bug) and rejects no correct
    change ([benchmark](docs/benchmarks/reviewer.md)).
  - **Done-when tests:** `crates/anchor-backend/tests/verify.rs` and
    `tests/eval.rs`.

- Phase 6, the collaboration dashboard (ADR-0025). Green on Linux, macOS
  and Windows in CI run 37551637739, including the Playwright browser test
  (Linux).
  - **Serving:** `anchor team run --ui` (live, answering approvals) and
    `anchor ui` (after a run). Loopback only, with a per-launch token in
    the URL fragment, exact Host and same-origin checks, and a strict CSP.
  - **Panels** (§12.2): roster, approvals, "what happened" (every audit
    entry, narrated), message stream, claim map, decision log, merge
    queue with the diff and evidence side by side, cost meter, snapshot
    timeline with restore, and the audit log. Plus the §12.4 slash
    commands.
  - **Every interaction visible:** every audit entry reaches the
    dashboard, verified against the hash chain as it is read. Agent
    actions are attributed: entries now carry their agent and session.
  - **Failures shown:** rejections, conflicts, a broken audit chain, cost
    mismatches, and bugs "caught later" (`fails_on_main`).
  - **Accurate cost:** every call is recomputed from the prices in your
    roles files and checked against the runtime's report and each
    session's totals.
  - **Done-when tests:** `crates/anchor-backend/tests/ui.rs` (over HTTP)
    and `ui/e2e/dashboard.spec.ts` (Chromium). The "a newcomer
    understands it" criterion also needs your sign-off in
    [`docs/ui-walkthrough.md`](docs/ui-walkthrough.md).

- Phase 7, security review and sandbox validation (ADR-0027,
  [`docs/audit/`](docs/audit/README.md)).
  - **Internal review:** five area reviews, then a second round on the
    new sandbox code. Every Critical and High finding is fixed, each with
    a regression test. The remaining Medium and Low findings are in
    [accepted-risks.md](docs/audit/accepted-risks.md).
  - **Linux:** a general seccomp filter in both tiers covers `ptrace`,
    mounts, `bpf`, keyrings, namespaces, `setsid`, unix sockets and
    signals to Anchor's own processes. Landlock scopes apply on 6.12+.
    Commands get their own session and no terminal.
  - **macOS:** Seatbelt is now `(deny default)`, and a memory supervisor
    is added. The runtime confines itself with `sandbox_init`.
  - **Windows:** agent commands run in an AppContainer inside a Job
    Object (ADR-0026), and the verify gate runs there too. The agent
    runtime runs in its own AppContainer with network access only
    (unless a role uses a loopback provider, AR-7). `anchor-fs` pins
    directory chains against junction swaps.
  - **Validation:** `crates/anchor-sandbox-validation` runs the same
    attacks on every platform. Every one is stopped on Linux (both
    tiers), macOS and Windows (first in CI run 37616036608; latest
    all-green run 37675535141).
  - **External audit: pending (third party).** The package for the
    auditor is [`docs/audit/README.md`](docs/audit/README.md).

**Known platform gaps:**

- **Windows runtime:** the agent runtime runs in an AppContainer, but
  unconfined (warned and audited) when a role uses a loopback provider,
  which no AppContainer can reach (AR-7).
- The OpenAI-compatible adapter has not been run against a live endpoint:
  there was no local server or API key in the build environment.

**Phase 4 limits:**

- Claims are hinted (the mediator) and enforced at merge, per file.
  Merges are path-level, so two edits to one file conflict.
- The MCP and A2A endpoints are request/response only: no SSE,
  subscriptions, streaming or push notifications.
- Agent commands in isolated workspaces need the backend to run as a
  normal user. As root, commands run as `nobody` and cannot enter the
  0700 isolated directory.

**Phase 5 limits:**

- **LLM reviewer not measured.** Its false-negative rate, the §8.5 stop
  decision, the launch roster (ADR-0001) and the freeze of the verifier
  interface (ADR-0009) all wait for a model key. Only the plumbing is
  tested, with scripted reviewers.
- **Python only.** The corpus and the rule reviewer's coverage parsing are
  for Python (unittest, trace). Other languages can configure their own
  commands, but get no line coverage.
- **Results can be forged.** Test results come from the process that runs
  the code under test, so that code can forge them beyond the checks above
  (ADR-0024, residual risks).
- **Pipeline completion not measured.** Completion with real executors
  (§16.3) needs a model; in review-only mode, every correct solution is
  approved.

**Phase 6 limits:**

- **No desktop shell, editor or terminal yet.** The dashboard is a web
  page; the Tauri shell (ADR-0002) and the editor and terminal (§12.1,
  ADR-0008) are deferred. `/plan`, `/implement` and `/review` print the
  CLI command to run.
- **Agent turn snapshots** are visible only while the agent's isolated
  workspace exists. After a run, the merge snapshots and the narrated
  story say which agent changed what.
- **Linux only so far.** The browser test runs only in the Linux CI job.
  macOS and Windows build without the bundle (a notice page) and run the
  API tests.

**Not started:**

- The external audit (needs a third party).

The architecture decisions, including those still pending evaluation, are
in [`docs/adr/`](docs/adr/0000-adr-process.md). Dependency vetting is in
[`docs/deps/dependency-records.md`](docs/deps/dependency-records.md).

## Commitments

Local-first. No training on user code. No telemetry: the code contains
none, and any future telemetry will be opt-in only and show exactly what it
sends. No ad-supported tier. See ADR-0019.

## License

Apache-2.0 (see `LICENSE` and ADR-0019).
