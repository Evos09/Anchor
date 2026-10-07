# ADR-0003: Sandbox mechanism per platform (§17 Q3, §11.1)

- Status: Accepted for Linux, macOS and Windows (Phase 7; macOS and
  Windows pending their first green CI run with the validation suite)
- Date: 2026-10-06, revised 2026-10-07 (Phase 7)

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
  and loader). The home directory and the workspace are unreadable. The
  Landlock ABI v6 scopes (abstract unix sockets, signals) apply where the
  kernel has them, and the shared seccomp filter (below) refuses unix
  sockets and the syscall denylist. If either cannot be enforced the
  runtime refuses to run (review finding S-6).
  Verified by `runtime_process_cannot_touch_the_filesystem`.
- **Agent commands** (`anchor-exec`):
  - argv only, with a cleared environment;
  - `RLIMIT_NPROC / AS / CPU / FSIZE / CORE`, plus a wall-clock timeout and a
    process-group kill;
  - the command leads its own session (no controlling terminal, so no
    `TIOCSTI`), and only five `/dev` nodes are granted;
  - a general seccomp filter (`crates/anchor-seccomp`) in both tiers,
    installed last before `exec`: `ptrace`, `process_vm_*`, mounts,
    kernel modules and kexec, `bpf`, `perf_event_open`, `userfaultfd`,
    keyrings, `open_by_handle_at`, swap, reboot, `setns`, namespace
    flags on `unshare`/`clone`/`clone3`, `setsid`/`setpgid`,
    `socket(AF_UNIX)`, `io_uring`, and signals to the backend, its
    process group, session leader and parent (plus `kill(-1)`) all fail
    with `EPERM`. No filter, no run;
  - Landlock ABI v6 scopes (abstract unix sockets, signals) where the
    kernel supports them, best effort;
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
with the namespace approach.

### macOS (Phase 7)

- **Agent commands**: `sandbox-exec` with a generated Seatbelt profile
  that starts from `(deny default)` (review finding S-4: the earlier
  `(allow default)` left Mach services and most reads open). Allowed:
  fork, signals and process info within the same sandbox, sysctl and
  preference reads, a short list of Mach services (libinfo, notification
  center, logging, cfprefsd), the standard `/dev` nodes, exec and reads
  only under the granted roots, writes only under the workspace. With
  the network denied, `(deny network*)`; allowed, network plus the
  resolver services (AR-3).
- **Memory**: macOS rejects `RLIMIT_AS`, so `run()` polls the process
  group's resident size every 5 ms and kills it over the limit, reporting
  `ExitKind::MemoryExceeded` (AR-2 for the overshoot window).
- **Runtime process**: applies `MACOS_PROFILE` to itself with
  `sandbox_init`: system reads only, no writes, no exec, outbound IP and
  the resolver only. It refuses to run without it.

Seatbelt is deprecated by Apple but still present. App Sandbox (needed
for App Store distribution) is a separate decision for the commercial
shell.

### Windows (Phase 7, ADR-0026)

Confined commands run in a per-workspace AppContainer (no capabilities
with the network denied) inside a Job Object attached at creation, with
explicit ACL grants for the workspace and PATH directories, an explicit
handle list, `CommandLineToArgvW`-exact quoting and batch files refused.
The agent runtime is not yet confined on Windows (AR-7). WSL2 was
rejected as the default because of the heavy dependency.

## Alternatives rejected

- *bubblewrap*: simpler to describe, but an external setuid/userns binary
  with its own CVE history. Could be revisited as a portable fallback.
- *gVisor / containers*: stronger, but too costly for interactive use at
  this stage. Revisit for "untrusted repository" mode.

## Consequences / known gaps

The residual risks are recorded, with what still holds, in
`docs/audit/accepted-risks.md`:

- AR-1: before Linux 6.12 a command can signal other same-user processes
  (not Anchor's own).
- AR-2: limits are per process or per file (disk fill; no per-file cap
  on Windows). A cgroup `pids.max`/`memory.max` is preferable where
  delegation is available.
- AR-3: macOS unix sockets when the network is allowed.
- AR-7: Windows runtime unconfined. AR-8: Linux `/proc` command lines.
  AR-13: Windows ACE persistence.
- Measured performance cost: not yet measured under realistic load (§17 Q3
  requires it).

## Verification

`cargo test -p anchor-exec` (including `fork_bomb_is_contained`,
`memory_limit_enforced`, `network_denied_process_has_no_network`,
`filesystem_confinement_enforced`,
`commands_cannot_escape_the_kill_or_reach_the_terminal`,
`protected_processes_cannot_be_signalled`, the macOS
`seatbelt_profile_denies_by_default`/`mach_services_are_unreachable_macos`
and the Windows `tests_windows`), `runtime_process_cannot_touch_the_filesystem`
in `anchor-backend` (Linux, macOS), and the cross-platform validation
suite `cargo test -p anchor-sandbox-validation`, run on all three CI
platforms (ADR-0027).
