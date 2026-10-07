# Internal security review (spec §14 Phase 7)

This is the internal, adversarial half of the Phase 7 audit (ADR-0027).
The **external audit is pending**: only a third party can perform it, and
[README.md](README.md) is the package prepared for them.

## Process

- **Round 1** (at commit `b19ad9d`, before any Phase 7 change). Five
  independent reviews, one per area of §11.2:
  - the §5 primitives (`P-`);
  - the §11 threats end to end (`T-`);
  - the per-platform sandbox (`S-`);
  - the Hub's RPC surface and merge queue (`H-`);
  - the dashboard and verification gate (`U-`, `V-`).

  Each reviewer worked from a frozen tree and wrote a reproduction for
  every finding it marked CONFIRMED (scratch crates, not part of the
  repository).
- **Remediation.** Every Critical and High finding is fixed, each with a
  regression test that fails without the fix. Medium and Low findings
  are either fixed, with a test, or recorded as an accepted risk in
  [accepted-risks.md](accepted-risks.md).
- **Round 2.** A second review, limited to the sandbox code written in
  Phase 7: the Windows AppContainer runner, the macOS Seatbelt profiles,
  the Windows file-system pinning and the seccomp signal rule. See below.

## Severity rubric

Exploitability × impact, measured against the spec's guarantees: the
primitives of §5 and the threats of §11.

| Severity | Meaning |
|---|---|
| Critical | An agent (model output or command) escapes confinement or defeats a primitive with no user action: code execution outside the sandbox, reading credentials, writing outside the workspace, killing the backend. |
| High | A primitive or threat mitigation is defeated under realistic conditions, or a later user action turns the agent's change into an escape (e.g. planting git hooks). |
| Medium | Denial of service against the backend or Hub, integrity loss under specific conditions, or a defence-in-depth layer missing where another layer still holds. |
| Low | A hard-to-exploit gap, information exposure without credentials, or missing auditing. |
| Info | A hardening suggestion or a residual design limit. |

## Round 1 findings and their status

The "Fix" column names the commit; each test there fails without that fix.

### Sandbox (S)

| ID | Sev. | Finding | Status | Fix / test |
|---|---|---|---|---|
| S-1 | Critical | Pathname and abstract `AF_UNIX` sockets reachable from commands (D-Bus `systemd-run`, docker.sock, ssh-agent) and from the runtime | Fixed | `805d5b2`, `fbbbf6b`: seccomp refuses `socket(AF_UNIX)`; Landlock scopes on 6.12+; macOS `(deny default)` refuses unix connects. `unix_sockets_and_signals_outside_the_sandbox_are_unreachable`, `runtime_process_cannot_touch_the_filesystem`, validation suite |
| S-2 | Critical | `setsid`/`setpgid` escaped the process-group kill; daemons outlived commands and held pipes | Fixed | `805d5b2`: command leads its own session; seccomp denies `setsid`/`setpgid`; bounded pipe join. `commands_cannot_escape_the_kill_or_reach_the_terminal` |
| S-3 | Critical | `TIOCSTI` keystroke injection through the inherited terminal; all of `/dev` granted | Fixed | `805d5b2`, `fbbbf6b`: new session (no controlling tty), `/dev` reduced to five nodes, `tcflush` before approval reads. Same test |
| S-4 | Critical | macOS Seatbelt `(allow default)`: Mach services (launchd, Apple Events, keychain daemon), reads outside `$HOME` | Fixed | `805d5b2`, `a1f5bd9`: reads deny-by-default, then the whole profile `(deny default)` with an allowlist. `seatbelt_profile_denies_by_default`, `mach_services_are_unreachable_macos` (CI), `filesystem_confinement_enforced` on macOS |
| S-5 | Medium | A command could signal any same-uid process (`kill -9 -1`, the backend) | Fixed in part; residual accepted | `384feea`: seccomp refuses `kill(-1)` and the backend, its group, session leader and parent; Landlock signal scope on 6.12+; macOS `signal (target same-sandbox)`; Windows lowbox. `protected_processes_cannot_be_signalled`. Residual: AR-1 |
| S-6 | Medium | Runtime self-sandbox failed open | Fixed | `fbbbf6b` (Linux), `a1f5bd9` (macOS): the runtime refuses to run without it |
| S-7 | Medium | Limits are per process (`RLIMIT_FSIZE` per file, AS × NPROC); disk fill; no memory limit on macOS | Fixed in part; residual accepted | `805d5b2`: macOS memory supervisor; `2a92c20`: Windows job-wide limits. Residual: AR-2 |
| S-8 | Medium | `$HOME` exclusion compared paths exactly (`/home`, `~/bin/..`, symlinks); `--exec-read-only` unfiltered | Fixed | `fbbbf6b`: `safe_read_grants`. `grants_exposing_home_root_or_state_are_dropped` |
| S-9 | Low | `pipe` + `fcntl` CLOEXEC race; `/proc` readable | Fixed / accepted | `805d5b2`: `pipe2(O_CLOEXEC)`. `/proc` reads: AR-8 |

### Threats end to end (T)

| ID | Sev. | Finding | Status | Fix / test |
|---|---|---|---|---|
| T-1 | High | Commands could plant `.git/hooks` or `.git/config`, which the user's own git then runs unconfined; `.git/**` could reach main through a merge | Fixed | `7bbc464`: `gitguard` reverts git-internal changes after each command; merges refuse `.git` paths. `commands_cannot_plant_git_hooks_or_config`, `git_internals_revoked_claims_and_retired_agents_never_merge` |
| T-2 | High | Background processes outlived `run()` | Fixed | = S-2 |
| T-3 | Medium | Control characters (ESC, OSC 52, bidi) in model-chosen text reached the terminal | Fixed | `fbbbf6b`: `terminal_safe` on every prompt and notice; control characters refused in path parameters. `terminal_tests`, `rejects_control_characters_in_names` |
| T-4 | Medium | Runtime-chosen turn numbers made `rollback --turn N` restore the wrong snapshot | Fixed | `92ce0df`: turn numbers must increase. `runtime_cannot_reuse_turn_numbers` |
| T-5 | Medium | Runtime stderr buffered without bound (1.18 GB RSS) | Fixed | `92ce0df`: 4 KiB lines, 256-line queue. `runtime_stderr_is_bounded` |
| T-6 | Medium | Unlimited checkpoints; ids overwritable across sessions; audit flooding; runtime-reported cost | Fixed in part; residual accepted | `aa66902`: id ownership, 8 per session. `ids_are_not_stolen_and_pruning_keeps_the_newest`. Residual: AR-5 |
| T-7 | Low | Deletion-diff confirmation not audited; set-based diff undercounts | Accepted | AR-9 |
| T-8 | Low | PATH entry inside the workspace resolves model-written binaries; frame ids unchecked | Accepted | AR-9 |

### Hub and merge queue (H)

| ID | Sev. | Finding | Status | Fix / test |
|---|---|---|---|---|
| H-1 | Medium | Slowloris: 40 idle sockets starved authenticated agents | Fixed | `aa66902`: reader threads apart from workers; total write deadline. `idle_connections_do_not_starve_requests` |
| H-2 | Medium | Merges honoured revoked claims and retired agents | Fixed | `7bbc464`. `git_internals_revoked_claims_and_retired_agents_never_merge` |
| H-3 | Medium | Claims checked once, before a long gate and approval | Fixed | `92ce0df`: rechecked right before applying. `claims_are_rechecked_before_apply` |
| H-4 | Medium | Claim paths compared byte-wise (case, Windows aliases) | Fixed | `92ce0df`: case-folded overlap; Windows aliases refused. `overlap_and_cover_matrix`, `hostile_claim_paths_are_refused` |
| H-5..H-9 | Low/Info | Unauthenticated 401s each write an audit entry; some refusals unaudited; artifact hashing before the permission check; collaborator task reassignment; `Content-Length: +N` | Accepted | AR-10 |

### Primitives (P)

| ID | Sev. | Finding | Status | Fix / test |
|---|---|---|---|---|
| P-1 | High | Append-only heuristic recomputed from current content: an agent could reshape a log, then overwrite it | Fixed | `4b1bc71`: detection is sticky and persisted. `heuristic_detection_is_sticky` |
| P-2 | High | Restore skipped a tracked file an agent had replaced with a hardlink, FIFO or oversized file, yet reported success | Fixed | `4b1bc71`. `tracked_files_replaced_by_untracked_entries_are_restored` |
| P-3 | Medium (High with Windows commands) | Windows operations path-based after verification: junction swap of an intermediate directory | Fixed | `bcbd669`: pinned directory chains, handle-based delete and listing. `junction_escapes_are_refused`, `junction_swapped_after_resolution_is_caught`, `open_workspace_root_cannot_be_renamed` (Windows CI) |
| P-4 | Medium | Text diff used quadratic memory (20k lines aborted) | Fixed | `fc3bee9`. `text_diff_of_large_rewrites_is_bounded` |
| P-5 | Medium | An unreadable or too-deep directory failed every snapshot and so every rollback | Fixed | `fc3bee9`: recorded as untracked. `unscannable_directories_do_not_block_snapshots`. Entry-count limit: AR-6 |
| P-6 | Medium | `write()` accepted any mutation approval | Fixed | `4b1bc71`. `write_requires_the_matching_operation` |
| P-7 | Medium | Credential deny list expanded `~` from a non-canonical home; no glob escaping; missing defaults | Fixed | `fc3bee9`. `credentials_denied_everywhere`, `home_with_glob_syntax_is_escaped` |
| P-8 | Low | Overwrite kept setuid/setgid bits | Fixed | `4b1bc71` |
| P-9 | Low | A torn final audit line marks the log corrupt | Accepted | AR-11 |

### Verification gate and dashboard (V, U)

| ID | Sev. | Finding | Status | Fix / test |
|---|---|---|---|---|
| V-1 | High | The test verdict is parsed from output the candidate's code controls (runner patching, forged summaries) | Fixed in part; residual accepted | `08a2c67`: hard rules reject changes touching the runner, exit, stderr or import machinery. `runner_tampering_rejected`, `runner_tampering_is_found`. Residual: AR-4 |
| V-2 | Medium-High | Python startup hooks: `.pth`, `sitecustomize`, user site under `HOME` set to the candidate tree | Fixed | `08a2c67`. `startup_hooks_rejected_and_home_is_empty` |
| V-3 | Medium | Coverage files read through symlinks or FIFOs, without a cap | Fixed | `08a2c67`. `untrusted_reads_refuse_links_fifos_and_large_files` |
| V-4 | Low-Medium | Evidence the Hub failed to record did not stop the merge | Fixed | `08a2c67` |
| U-1 | Medium | `--ui-url-file` followed symlinks and reused readable files | Fixed | `92ce0df`. `secret_file_replaces_links_and_is_owner_only` |
| U-2..U-4 | Low/Info | Long polls hold dashboard workers; display-name impersonation in prompts; CSP blocks the fallback page's style | Accepted | AR-10 |

### Info (I)

- **I-1.**
  - `**/.git/**` does not match a `.git` file. It is covered for commands by `gitguard` and for merges by the `.git` segment rule.
  - macOS `open_follow` opens before the path check (no read follows it).
  - Windows name folding approximates NTFS upcase (ADR-0013).
- **I-2.**
  - The "user appended repeatedly" append-only heuristic (§5.3(a)) is not implemented: AR-12.
  - On Linux, names ending in `(deleted)` are refused.

## Round 2

Scope: the sandbox code written during Phase 7. Three independent reviews:

- the Windows command runner (`W-`);
- the Windows `anchor-fs` pinning (`F-`);
- the macOS Seatbelt profiles and the seccomp signal rule (`M-`, `K-`).

The macOS and Windows findings come from reading the code against the
platform's documented behaviour. Only CI runs that code. Each fix below
has a test that runs on that platform's CI job. Every one of those tests,
and the validation suite, passed on Linux, macOS and Windows in CI run
37616036608 (commit 0f9c0c7). Getting there took fixes found only on CI:
the AppContainer environment (error 203), workspace teardown order on
Windows, `RLIMIT_NPROC` headroom on macOS, and read-only grants moved to
a shared capability (which also removed W-5's read-only churn; see AR-15).
W-5 is fixed: the verify gate deletes each command's AppContainer profile
after it runs, and the Windows gate runs (`released_profiles_are_deleted`,
the gate tests in `crates/anchor-backend/tests/verify.rs`; green on all
three platforms in CI run 37623168183).

| ID | Sev. | Finding | Status | Fix / test |
|---|---|---|---|---|
| W-1 | High | The container got `FILE_ALL_ACCESS` on the workspace. That includes `WRITE_DAC`/`WRITE_OWNER`, so a command could rewrite ACLs, lock the user out of files (defeating `gitguard` and restores), or drop its own grant | Fixed | "Modify" rights only (no ACL or owner changes), set with `SET_ACCESS`, which replaces a broader ACE. The ACE check requires the exact mask and inheritance (W-10). Drive roots and the user profile are never granted |
| F-1 | High | Pins opened with attribute access only. Windows does not enforce share modes on such opens, so a pinned directory could still be renamed (P-3 not actually fixed) | Fixed | Pins and the root hold `FILE_LIST_DIRECTORY`. `open_workspace_root_cannot_be_renamed` (Windows CI) |
| M-1 | High (not reproduced) | A `/System` grant (system read list and runtime profile) was thought to cover `/System/Volumes/Data`, a second path to every home directory | Hardened | macOS CI run 37608878226 showed that Seatbelt resolves the firmlinked path to its canonical one, so the data-volume path gives no more access than the canonical path. The narrowing to `/System/Library` and the final deny of `/System/Volumes/Data` are kept as hardening. `seatbelt_closes_unix_sockets_data_volume_and_keychains_macos` checks that an ungranted directory stays closed through that path |
| M-2 | High | With the network allowed, `(allow network*)` let commands connect to any unix socket (docker.sock, ssh-agent) | Fixed | IP and the resolver socket only, as on Linux. Same macOS test; validation suite row "unix socket outside, network allowed". Closes AR-3 |
| K-1 | High | The signal rule protected pids but not thread ids, and a fatal signal to any backend thread kills the backend (kernels before 6.12) | Fixed in part; residual accepted | The backend's and its parent's thread ids, taken at spawn, are protected as compact ranges. The BPF program now uses local returns, so it can exceed 255 instructions. `protected_processes_cannot_be_signalled` (tkill to a thread), `pid_ranges_are_merged_and_bounded`. Threads started later: AR-1 |
| K-2 | Medium | The runtime shared the backend's process group, so `kill(0)` from it reached the backend | Fixed | The runtime gets its own process group. Its filter also protects the backend's group |
| K-3 | Medium | `fcntl(F_SETOWN*)` and `ioctl(FIOSETOWN, SIOCSPGRP)` let the kernel deliver `SIGIO` to any same-user process | Fixed | Refused with `EPERM`. Same seccomp test |
| M-3 | Medium | An empty write list made `(allow file-write*)` with no filter, which allows every write | Fixed | Allows are emitted only with paths. `seatbelt_profile_denies_by_default` |
| M-5 | Low | Keychains were read-denied only, and the home directory was taken from `$HOME` | Fixed | Reads and writes denied; home from `getpwuid_r` |
| M-6 | Low | A path that does not exist yet was not canonicalised; non-UTF-8 paths were quoted lossily | Fixed | Resolved through the deepest existing ancestor; a non-UTF-8 path gets no grant |
| W-2 | Medium | Program names like `C:python.exe` (drive-relative) or with a trailing dot passed the bare-name check | Fixed | `:` refused everywhere; a trailing dot or space refused on Windows. `program_must_be_bare_name` |
| W-3 | Medium | The batch-file check read the literal extension (`x.bat.` slipped through) | Fixed | The real path's extension must be `.exe` or `.com` |
| W-8 | Low | `limit * 95` could overflow | Fixed | Saturating |
| W-9 | Low | Environment names or values with `=`/NUL could smuggle in variables | Fixed | Refused on every platform. `malformed_environment_is_refused` |
| W-12 | Low | The error code was read after `TerminateJobObject`, and a failed `GetExitCodeProcess` read as exit 0 | Fixed | |
| F-2..F-13, W-4, W-6, W-7, W-11, W-13, W-14, M-4, M-7..M-9, K-4 | Medium–Info | Windows `anchor-fs` and runner hardening; macOS preference reads and executable roots | Accepted | AR-14, AR-15, AR-16 |

## Result

- **Critical and High findings:** none is open, in either round. Each is fixed with a regression test. Where a residual remains (S-5, V-1, K-1), it is narrower than the original finding, it is Medium at most, and it is recorded in [accepted-risks.md](accepted-risks.md).
- **CI:** the macOS and Windows parts are verified there (see [README.md](README.md#status)).
