# ADR-0026: The Windows command sandbox (AppContainer + Job Object)

- Status: Accepted, pending its first green Windows CI run
- Date: 2026-10-07

## Context

Until Phase 7, `anchor-exec` refused every confined command on Windows
(`SandboxUnavailable`). Threats 4–7 therefore read "⛔ Windows", and the
verify gate failed closed there. Spec §11.1 requires a sandbox on every
platform. The user chose AppContainer plus Job Objects (2026-10-07).
WSL2 had already been rejected (ADR-0003).

## Decision

Every command with `Network::Deny` or a file-system confinement runs
through `crates/anchor-exec/src/windows.rs`.

### AppContainer

- **Profile.** There is one profile per workspace, named
  `anchor.<FNV-1a of the first read/write root>`. It is created with
  `CreateAppContainerProfile`, or derived when it already exists.
- **Capabilities.**
  - With the network denied there are none: no internet, no private
    network and no loopback.
  - With the network allowed (a prompt) it gets `internetClient` only.
- **File access.**
  - The container SID gets inheritable ACEs: full access on
    `read_write`, and read and execute on `read_only` entries (absolute
    PATH directories).
  - An ACE is added only when no explicit one already grants that
    access, so repeated runs do not grow the ACL.
  - System directories are readable through ALL APPLICATION PACKAGES, as
    for any AppContainer.
  - The user's profile, credentials, other workspaces and other
    processes stay out of reach.

### Job Object

- **Attachment.** The job is created first and attached at process
  creation through `PROC_THREAD_ATTRIBUTE_JOB_LIST`. No instruction runs
  outside the job.
- **Limits.**
  - Active processes: `max_processes`.
  - Job-wide memory: `address_space_bytes`.
  - Job CPU time: `cpu_seconds`.
  - Kill-on-close and die-on-unhandled-exception.
  - No breakaway.
- **UI restrictions.** Desktop, clipboard, global atoms, handles, system
  parameters, display settings and exit-Windows.
- **Accounting.** The wall timeout calls `TerminateJobObject`, which
  kills the whole tree. Job accounting supplies `Rusage`. A peak at or
  above 95% of the memory limit reports `ExitKind::MemoryExceeded`.

### Process creation

- **API.** `CreateProcessW` with `STARTUPINFOEXW`, carrying
  `SECURITY_CAPABILITIES`, `JOB_LIST` and `HANDLE_LIST`. Only the three
  stdio pipe ends are inherited.
- **Arguments.** The command line is built with the inverse of
  `CommandLineToArgvW`, so backslashes and quotes round-trip.
- **Batch files.** `.bat` and `.cmd` are refused: `cmd.exe` re-parses
  their arguments (the BatBadBut class), which would reopen threat 4.
- **Environment.** Cleared and set explicitly. Only `SystemRoot` is
  added, because Windows DLL loading needs it.

## Consequences

- **Threats 4–7.** They get a Windows mechanism. The validation suite
  (`crates/anchor-sandbox-validation`) and `anchor-exec`'s `tests_windows`
  exercise it on CI. Until that run is green, the verify gate keeps
  failing closed on Windows.
- **File size.** There is no per-file size limit on Windows (AR-2).
- **ACE persistence.** The ACEs persist after the run (AR-13). Cleanup
  belongs to a future `anchor workspace forget`: remove the ACEs, then
  `DeleteAppContainerProfile`.
- **Runtime.** The agent runtime (Layer 2) is not yet confined on Windows
  (AR-7). The plan is a second AppContainer with only `internetClient`
  and no file grants.
- **Junction swaps.** `anchor-fs` pins directory chains on Windows, so a
  junction swapped mid-operation cannot redirect the backend's own file
  operations (review finding P-3). This matters more now that Windows
  commands can run.

## Alternatives rejected

- **Restricted tokens, without an AppContainer.** They do not isolate
  the network, and a write-restricted token still reads the whole
  profile.
- **Windows Sandbox and Hyper-V containers.** Too heavy per command, and
  not present on Home editions.
- **WSL2.** Rejected in ADR-0003.
