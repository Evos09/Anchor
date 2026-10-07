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
  - The container SID gets inheritable ACEs. On `read_write` it gets
    "Modify": read, write, execute and delete. It does not get
    `WRITE_DAC` or `WRITE_OWNER`, so a command cannot rewrite the
    workspace's ACLs (round 2 finding W-1).
  - `read_only` entries (absolute PATH directories) get read and execute
    for a custom capability, `anchorReadExecute`
    (`DeriveCapabilitySidsFromName`), which every Anchor container
    carries. It conveys no network access. Granting a directory
    propagates the ACE through its whole tree, which takes minutes for a
    toolchain. With a per-workspace SID that happened again for every
    workspace, and parallel grants with different SIDs could overwrite
    each other (CI run 37613963524: a lost grant left python without its
    DLLs). A shared SID is granted once per machine. Grants are also
    serialized within the process.
  - Drive roots and the user profile are never granted.
  - An ACE is written (`SET_ACCESS`) unless one with exactly that mask
    and inheritance exists, so repeated runs neither grow the ACL nor
    keep a broader grant.
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
- **Programs.** Only `.exe` and `.com` run, judged on the real path:
  Windows drops trailing dots, so `x.bat.` is a batch file. `cmd.exe`
  re-parses a batch file's arguments (the BatBadBut class), which would
  reopen threat 4. Program names with `:` (drive-relative paths, streams)
  or a trailing dot or space are refused at resolution.
- **Environment.** Cleared and set explicitly. Windows needs a few
  variables, so they are copied from the backend when the command does
  not set them:
  - `SystemRoot`, `SystemDrive` and `windir`, for DLL loading;
  - `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `TEMP`, `TMP` and
    `ProgramData`. Process creation rewrites these to the container's
    own folders, and fails with error 203 when they are missing (CI run
    37611621725).

  They are paths, not secrets, and the container cannot read the
  directories they name.

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
