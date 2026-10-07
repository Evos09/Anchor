# Audit package (spec §11.2, §14 Phase 7)

This directory is the package for Anchor's external security audit, and
the record of the internal review that preceded it.

| Document | Contents |
|---|---|
| this file | Scope, architecture and trust boundaries, how to build and test, code map, priorities |
| [internal-review.md](internal-review.md) | The internal adversarial review: process, severity rubric, every finding and its fix |
| [accepted-risks.md](accepted-risks.md) | Medium and Low findings not fully fixed: exposure, what still holds, what would close them |
| [../threat-model.md](../threat-model.md) | Every §11 threat and §5 primitive mapped to its mitigation and test, per platform |
| ADR-0003, ADR-0026, ADR-0027 | The sandbox per platform, the Windows sandbox, the review process |

## Status

| Item | Status |
|---|---|
| Internal review, round 1 (five areas) | Done. No Critical or High finding open |
| Internal review, round 2 (Phase 7 sandbox code) | See [internal-review.md](internal-review.md#round-2) |
| Cross-platform sandbox validation | Passes on Linux (both network tiers), macOS and Windows: CI run 37616036608; latest all-green run 37675535141, with the Windows runtime in an AppContainer. Each job's report is in its summary |
| **External audit** | **Pending: needs a third party.** This package is prepared for them |

## Scope (§11.2)

1. **The five primitives (§5)**:
   - confined file access, in `anchor-fs`;
   - permission and policy, in `anchor-policy`;
   - append-only protection, in `anchor-guard`;
   - checkpoints, in `anchor-checkpoint`;
   - snapshots and rollback, in `anchor-snapshot`.
2. **The threat model (§11)**: threats 1–9, end to end through the
   backend.
3. **The sandbox on each platform (§11.1)**:
   - **Linux**: Landlock, plus namespaces or seccomp.
   - **macOS**: Seatbelt plus a memory supervisor.
   - **Windows**: AppContainer plus a Job Object.

   Each platform covers both the agent commands and the runtime's
   self-sandbox.
4. **The Hub's RPC surface (§6.5)**:
   - HTTP, MCP and A2A on loopback;
   - the merge queue;
   - the verification gate;
   - the dashboard's API.

Out of scope: the model providers, the user's own git, and the OS
kernel.

## Architecture and trust boundaries

```
 user ── terminal / dashboard (loopback, per-launch token)
   │
 anchor (backend, trusted) ── policy, mediator, snapshots, Hub, merge queue
   │  frames over a pipe (anchor-protocol)          │ spawns, confined
   ▼                                                ▼
 anchor-agent (runtime, Layer 2)            agent commands (anchor-exec)
   self-sandboxed: no files,                  workspace rw, system ro,
   network to providers only                  no network by default, limits
   │
 model provider (untrusted output)
```

The trust boundaries, each enforced in code rather than by convention:

- **Model output → runtime.** Tool calls are parsed into closed types.
  Nothing the model writes is executed by the runtime.
- **Runtime → backend.** Every file or command request is a protocol
  frame. The mediator checks it against policy, prompts when the policy
  requires it, and audits it. The runtime itself cannot open files: its
  sandbox refuses, and a clippy rule forbids the APIs.
- **Backend → command.** Commands run by argv only, with no shell, a
  cleared environment and kernel-enforced confinement. A command that
  cannot be confined does not run.
- **Agents → Hub.** Each agent authenticates with its own bearer token
  or pipe binding. Claims are checked again at merge time.
- **Candidate code → verify gate.** Candidate code is untrusted. It runs
  confined with an empty home directory, and hard rules reject changes
  that touch test runners or startup hooks.
- **Snapshot store.** It lives outside the workspace and is
  hash-verified on read (ADR-0022).

## Building and testing

These steps need Rust stable (MSRV 1.89). The dashboard also needs
Node 22 for `ui/`.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# The seccomp network tier (what Ubuntu >= 23.10 and CI runners get):
ANCHOR_EXEC_TEST_DISABLE_USERNS=1 cargo test -p anchor-exec -p anchor-sandbox-validation -p anchor-seccomp
# Cross-platform sandbox validation alone; writes target/sandbox-report.md:
cargo test -p anchor-sandbox-validation
cargo deny check
```

The validation suite (`crates/anchor-sandbox-validation`) runs one probe
binary per attack through the backend's real command confinement. It
covers:

- network egress, both external and to loopback;
- reading outside the workspace and a fake `~/.ssh` key;
- listing and writing outside the workspace;
- a process flood, memory, CPU and file-size limits;
- an inherited environment;
- unix sockets and signalling the backend;
- on Linux, `ptrace`, `bpf`, `keyctl`, `mount` and `unshare`.

CI runs it on Linux, macOS and Windows and appends each platform's table
to the job summary.

Run the end-to-end attack script and the dashboard e2e as CI does: see
`.github/workflows/ci.yml`.

## Code map

| Area | Files |
|---|---|
| Confined file access | `crates/anchor-fs/src/lib.rs`, `sys/{unix,windows}.rs` (descriptor or handle-based resolution, pinned chains on Windows) |
| Policy and audit | `crates/anchor-policy/src/{policy,params,audit}.rs`, `config/policy.default.toml` |
| Append-only, checkpoints | `crates/anchor-guard`, `crates/anchor-checkpoint` |
| Snapshots and restore | `crates/anchor-snapshot/src/{store,engine,object,restore}.rs`, `crates/anchor-fs/src/scan.rs` |
| Mediator and session | `crates/anchor-backend/src/{mediator,session,interact,gitguard}.rs` |
| Command sandbox | `crates/anchor-exec/src/lib.rs` (Linux, macOS), `windows.rs`; `crates/anchor-seccomp` (BPF assembler and filters) |
| Runtime self-sandbox | `crates/anchor-runtime/src/sandbox.rs` |
| Hub | `crates/anchor-hub/src/{http,mcp,a2a,hub,db,path}.rs` |
| Merge queue, verify gate | `crates/anchor-backend/src/{queue,verify}.rs`, `crates/anchor-verify/src/rules.rs` |
| Dashboard | `crates/anchor-backend/src/ui/`, `ui/` |
| Validation suite | `crates/anchor-sandbox-validation` |

## Where an auditor should look first

1. **The Windows sandbox** (`anchor-exec/src/windows.rs`, ADR-0026). It
   is the newest code, and only CI has run it. Check:
   - the AppContainer ACL grants;
   - the `HANDLE_LIST` and `JOB_LIST` attributes;
   - command-line quoting and the refusal of batch files.
2. **The macOS Seatbelt profiles** (`macos_profile` in `anchor-exec`,
   `MACOS_PROFILE` in the runtime). Seatbelt is undocumented, so check
   that the allowlist cannot be widened by a path the model chooses.
3. **The seccomp filters** (`anchor-seccomp`). They are hand-assembled
   BPF, so check:
   - the jump offsets;
   - the architecture check;
   - the argument tests for `clone`, `clone3`, `unshare`, `socket` and
     `kill`.
4. **Windows `anchor-fs` pinning** (`sys/windows.rs`). Check for races
   between a pinned chain and the final open.
5. **The verify gate's residual** (AR-4). Can a candidate forge a
   passing verdict without matching `rules.rs`?
6. **The Hub's HTTP parser** (`anchor-hub/src/http.rs`). Check request
   smuggling and resource exhaustion.
