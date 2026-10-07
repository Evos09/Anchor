# Accepted risks (spec §14 Phase 7)

Every Medium and Low finding of the internal review
([internal-review.md](internal-review.md)) that is not fully fixed is
listed here, with what remains exposed, why it is accepted, and what
would close it. None is Critical or High: where a High finding left a
residual (V-1), the residual is narrower and rated on its own.

Each entry names the layer that still holds. Anchor's guarantees are
layered (§5.2(c)): the permission engine decides, the sandbox enforces,
snapshots undo. An accepted risk weakens one layer, never all of them.

| ID | Sev. | From | Platform | Summary |
|---|---|---|---|---|
| AR-1 | Medium | S-5 | Linux < 6.12 | Commands can signal other same-user processes (not Anchor's own) |
| AR-2 | Medium | S-7 | all | Limits are per process or per file: disk fill, aggregate memory |
| AR-3 | Low | S-1 | macOS | With network *allowed*, unix-socket connects are not refused |
| AR-4 | Medium | V-1 | all | Runner-tampering detection is pattern-based |
| AR-5 | Low | T-6 | all | Audit-log volume and runtime-reported cost are not bounded |
| AR-6 | Medium | P-5 | all | A huge number of workspace entries slows or blocks snapshots |
| AR-7 | Medium | — | Windows | The agent runtime (Layer 2) runs unconfined |
| AR-8 | Low | S-9 | Linux | `/proc` is readable: other processes' command lines |
| AR-9 | Low | T-7, T-8 | all | Deletion confirmations unaudited; PATH inside the workspace |
| AR-10 | Low | H-5..H-9, U-2..U-4 | all | Hub and dashboard hygiene items |
| AR-11 | Low | P-9 | all | A torn final audit line marks the log corrupt |
| AR-12 | Low | I-2 | all | One of the two §5.3(a) append-only heuristics is missing |
| AR-13 | Low | round 2 | Windows | AppContainer ACEs persist on workspace and PATH directories |

---

### AR-1: signals to same-user processes on older Linux kernels

- **Exposure.** On kernels before 6.12 (no Landlock signal scope) a
  confined command can `kill` any other process of the same user, except
  the ones the seccomp filter protects: the backend, its process group,
  its session leader and its parent, plus `kill(-1)` and
  `pidfd_send_signal`. An editor or a shell elsewhere in the session can
  be killed.
- **Still holds.** The command cannot ptrace, read memory of, or open
  `/proc/<pid>/{mem,environ}` of those processes (Landlock refuses
  cross-domain ptrace access on every supported kernel). Nothing is read
  or written; this is denial of service only. On 6.12+ the scope refuses
  all of it; on macOS Seatbelt allows `signal (target same-sandbox)`
  only; on Windows the AppContainer token cannot open other processes.
- **Why accepted.** seccomp cannot tell "a process of mine" from another
  pid without a supervisor. A pid namespace would close it but requires
  the user-namespace tier, which GitHub's and Ubuntu's AppArmor policy
  refuses.
- **Would close it.** Linux 6.12+, or a pid namespace where available.

### AR-2: aggregate resource limits

- **Exposure.**
  - `RLIMIT_FSIZE` caps each file, not their sum: a command can fill the
    disk with many files inside the workspace. On Windows there is no
    per-file cap at all (the validation suite reports this row as an
    accepted risk).
  - `RLIMIT_AS` is per process, so the worst case is `AS × NPROC` on
    Linux. macOS polls the process group's resident size every 5 ms,
    so a fast allocation can overshoot briefly before the kill.
- **Still holds.** CPU time, the wall timeout, the process count and the
  group kill bound how long any of it lasts. The Windows Job Object
  limits memory and process count job-wide.
- **Why accepted.** cgroups need delegation that a desktop user rarely
  has; disk quotas need administrator setup.
- **Would close it.** A cgroup v2 leaf (`memory.max`, `pids.max`) where
  delegated; a workspace-size check after each command.

### AR-3: unix sockets on macOS when the network is allowed

- **Exposure.** A command the user explicitly ran with `network = true`
  (a prompt, §11 threat 6) gets `(allow network*)`, which includes
  connecting to pathname unix sockets it can see (Docker, ssh-agent).
- **Still holds.** The default, network-denied, profile refuses every
  socket connect (validation suite: "connect to a unix socket outside").
  File reads remain confined, so socket paths under `$HOME` are not
  even listable.
- **Why accepted.** Seatbelt's `remote unix-socket` filter is
  undocumented; restricting it risks breaking the resolver.
- **Would close it.** `(deny network-outbound (remote unix-socket))`
  with an allowlist for `mDNSResponder`, once tested on CI.

### AR-4: runner-tampering detection is pattern-based (V-1 residual)

- **Exposure.** The verify gate parses a test verdict produced by code
  the candidate controls. Changes that touch the runner, exit paths,
  stderr or import machinery are rejected by name (`rules.rs`), and
  startup hooks, opaque code and native modules are rejected outright.
  An obfuscated equivalent (for example `getattr` on a computed string)
  is not matched.
- **Still holds.** The gate is an extra check, not the merge authority:
  claims, the Hub's merge rules and the user's approval of each merge
  still apply, and every change is snapshotted.
- **Why accepted.** A sound fix needs the runner outside the candidate's
  process: a harness that reads results over a channel the tested code
  cannot write.
- **Would close it.** Run the tests in a confined child and read the
  verdict from a separate, trusted reporter process (planned with the
  verifier plugin interface, ADR-0009).

### AR-5: audit volume and runtime-reported cost

- **Exposure.** A runtime can produce many legitimate tool calls, each
  audited; the cost meter shows token counts the runtime reports.
- **Still holds.** Checkpoints are capped at 8 per session with id
  ownership (T-6 fix); budgets stop a session; the audit chain detects
  tampering, not volume.
- **Would close it.** Per-session audit quotas; provider-side usage
  reconciliation.

### AR-6: snapshot entry-count denial of service

- **Exposure.** A command can create millions of files; each snapshot
  then scans them all, and an over-deep or unreadable directory is
  recorded as untracked (P-5 fix) rather than failing.
- **Still holds.** No snapshot is partial, and untracked entries are
  reported, never silently dropped.
- **Would close it.** An entry-count limit per turn that fails the turn
  with a clear error.

### AR-7: Windows agent runtime unconfined

- **Exposure.** On Windows the `anchor-agent` process (which talks to
  model providers) runs with the user's rights. Its code is Anchor's
  own, but a memory-safety bug reachable from model output would not be
  contained.
- **Still holds.** The runtime has no file tools: every file operation
  goes through the backend's mediator and policy, and every command
  through the AppContainer sandbox. The runtime's own code is safe Rust
  apart from the platform sandbox calls in `sandbox.rs`.
- **Would close it.** Launch the runtime in its own AppContainer with
  only the `internetClient` capability (planned; ADR-0026).

### AR-8: `/proc` readable on Linux

- **Exposure.** A confined command can read other processes' command
  lines and status under `/proc`.
- **Still holds.** `environ`, `mem`, `maps` and fds are ptrace-gated,
  which Landlock refuses across domains. Credentials passed on a command
  line are the user's exposure regardless of Anchor.
- **Would close it.** A pid namespace with a fresh `/proc` mount.

### AR-9: deletion confirmations and PATH inside the workspace (T-7, T-8)

- **Exposure.**
  - The confirmation of a large deletion diff is not itself an audit
    entry (the deletion is), and the set-based count can undercount
    renames.
  - A `PATH` entry inside the workspace resolves binaries the model
    wrote.
  - Frame ids are not checked for monotonicity.
- **Still holds.** The workspace is the model's domain anyway, and such a
  binary still runs confined. Every deletion is snapshotted and audited.

### AR-10: Hub and dashboard hygiene (H-5..H-9, U-2..U-4)

- **H-5.** Each unauthenticated 401 writes an audit entry (log growth).
- **H-6.** Some protocol-level refusals (HTTP 404/405, malformed MCP) are
  not audited.
- **H-7.** `hub_register_artifact` hashes a file before the permission
  check. This is a hash oracle for files the *backend* can read; the
  agent learns only equality.
- **H-8.** The default collaborator grant can reassign another agent's
  task.
- **H-9.** `Content-Length: +N` is accepted, and claim scans are O(n).
- **U-2.** Long polls hold dashboard workers.
- **U-3.** Agent display names can imitate one another in approval
  prompts; ids and roles are shown too.
- **U-4.** The fallback page's inline style is blocked by the CSP.
- **Still holds.** Everything is loopback only, token-authenticated, and
  claims are enforced at merge.

### AR-11: torn final audit line

- **Exposure.** A crash during an audit append leaves the log reported
  as corrupt until a person inspects it.
- **Still holds.** It fails loud: tampering and truncation stay
  detectable, and nothing is silently lost.
- **Would close it.** Truncate a torn tail on open and record that as an
  event.

### AR-12: "user appended repeatedly" append-only heuristic

- **Exposure.** §5.3(a) names two heuristics. Only the content-shape one
  (sticky since the P-1 fix) is implemented. A log the user appends to
  but whose shape does not match stays writable by the agent.
- **Still holds.** Declared append-only paths in policy are enforced;
  snapshots undo any overwrite.

### AR-13: AppContainer ACEs persist (Windows)

- **Exposure.** `anchor-exec` grants the workspace's AppContainer SID
  inheritable access to the workspace and read/execute to absolute PATH
  directories. The ACEs stay after the run, so a later process running
  in *that same* AppContainer (only Anchor creates it) keeps that
  access.
- **Still holds.** The SID is per workspace, it has no capabilities, and
  no other process holds it.
- **Would close it.** `anchor workspace forget` removing the ACEs and
  calling `DeleteAppContainerProfile` (ADR-0026).
