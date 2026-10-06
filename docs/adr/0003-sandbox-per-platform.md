# ADR-0003: Sandbox mechanism per platform (§17 Q3, §11.1)

- Status: Accepted for Linux; Proposed for macOS and Windows
- Date: 2026-10-06

## Context

The sandbox enforces **capability** and the permission engine enforces
**permission** (§5.2(c)). Both are required. Two kinds of process need
confinement: the agent runtime (Layer 2), and commands the agent asks the
backend to run.

## Decision

### Linux (implemented, tested on kernel 6.18)

- **Runtime process**: Landlock self-restriction at startup
  (`anchor-runtime/src/sandbox.rs`). No write access anywhere. Read/execute
  only under `/etc /usr /lib /lib64 /lib32 /nix/store` (needed for DNS, TLS
  and loader). The home directory and the workspace are unreadable.
  Verified by `runtime_process_cannot_touch_the_filesystem`.
- **Agent commands** (`anchor-exec`):
  - argv only, with a cleared environment;
  - `RLIMIT_NPROC / AS / CPU / FSIZE / CORE`, plus a wall-clock timeout and a
    process-group kill;
  - network denial in two tiers, chosen by a one-time probe:
    1. new user + network namespaces (loopback only) where the system
       allows unprivileged user namespaces;
    2. otherwise a seccomp-BPF filter that fails `socket`/`socketpair` for
       every family except `AF_UNIX`, fails `io_uring_setup`, and kills on
       a foreign architecture or the x32 ABI. Ubuntu >= 23.10 (and
       GitHub's runners) restrict unprivileged user namespaces through
       AppArmor; CI run 37414390349 caught this. The method used is
       recorded per command in the audit log (`network_method`);
  - filesystem confinement through a Landlock ruleset built in the parent and
    applied in the child with raw `prctl` + `landlock_restrict_self`
    syscalls (no allocation after fork). The rules are read-write on the
    workspace and read-only on system dirs and absolute `PATH` entries. `/`
    and `$HOME` are never granted.
  - Landlock runs in `HardRequirement` mode: an unsupported kernel makes
    the command **refuse to run**, never run unconfined.
- **Root**: the kernel exempts uid 0 from `RLIMIT_NPROC`, so a backend
  running as root drops agent commands to uid/gid 65534. The dumpable flag
  is restored so that namespace maps can be written.

Chosen over bubblewrap: no extra binary to ship or find on `PATH`; the
rules are expressed in our own code and tested in-tree; Landlock composes
with the namespace approach. Seccomp is used only as the network
fallback, not as a general syscall filter (see Consequences).

### macOS (written, type-checked, not executed)

`sandbox-exec` with a generated Seatbelt profile: `(deny network*)` when
network is denied; writes allowed only under the workspace; reads of
`$HOME` denied except granted roots. Seatbelt is deprecated by Apple but
still present. App Sandbox (needed for App Store distribution) is a separate
decision for the commercial shell.

Resource limits use the same rlimits as Linux, except `RLIMIT_AS`:
macOS rejects lowering it (`EINVAL`, found by CI run 37447560545), so
memory is **not** limited on macOS. A per-process memory cap would need a
supervisor that polls and kills, which is tracked for Phase 7.

### Windows (not implemented — fails closed)

A command that requires network or filesystem confinement returns
`SandboxUnavailable` and is not run. The plan is AppContainer + Job
Objects. WSL2 was rejected as the default because of the heavy dependency.

## Alternatives rejected

- *bubblewrap*: simpler to describe, but an external setuid/userns binary
  with its own CVE history. Could be revisited as a portable fallback.
- *gVisor / containers*: stronger, but too costly for interactive use at
  this stage. Revisit for "untrusted repository" mode.

## Consequences / known gaps

- No general seccomp syscall filter yet (only the network fallback):
  confined commands can still make any other syscall
  Landlock doesn't cover (e.g. `ptrace` of sibling processes under the same
  uid is limited only by Yama). This is tracked for Phase 7.
- With the seccomp tier, `AF_UNIX` sockets stay available, so a command
  could talk to a local daemon socket it can open (e.g. a container
  engine socket). Landlock ABI v6 scoping and a socket-path deny list
  are the follow-ups.
- Fork-bomb containment relies on `RLIMIT_NPROC`, which is per-uid. A user
  already running many processes has less headroom. A cgroup `pids.max` is
  preferable where delegation is available.
- Measured performance cost: not yet measured under realistic load (§17 Q3
  requires it). The `fork_bomb_is_contained` and
  `filesystem_confinement_enforced` tests only show correctness.

## Verification

`cargo test -p anchor-exec` (11 tests, including `fork_bomb_is_contained`,
`process_limit_refuses_excess_forks`, `memory_limit_enforced`,
`network_denied_process_has_no_network`,
`filesystem_confinement_enforced`), and
`runtime_process_cannot_touch_the_filesystem` in `anchor-backend`.
