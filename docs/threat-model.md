# Threat model and test map (spec §5, §11)

A threat without a passing test is an unmitigated vulnerability. Status
legend:

- ✅ mitigated, with a test passing in blocking CI on the named platforms
  (first all-green run: 37448154072);
- 🟡 implemented, but no test exercises it on that platform yet;
- ⛔ not implemented, fails closed;
- ⏳ later phase.

## §11 threats

| # | Threat | Mitigation | Tests | Status |
|---|---|---|---|---|
| 1 | Symlink escape | Kernel-confined resolution; canonical path from the handle (ADR-0013) | `anchor-fs`: `symlink_escape_refused`, `relative_symlink_escape_refused`, `magic_link_refused` (Linux); e2e attack script | ✅ Linux, macOS, Windows (pinned chains; junction tests `junction_escapes_are_refused`, `junction_swapped_after_resolution_is_caught`, `open_workspace_root_cannot_be_renamed`, CI 37616036608) |
| 2 | TOCTOU race | Descriptor-based strict reopen, identity check, exchange-and-verify | `symlink_swap_race_never_escapes` (2,000 iterations against a flipping symlink), `parent_swapped_after_resolution_is_refused`, `target_swapped_for_symlink_after_resolution_is_refused`, `compare_and_swap_preconditions`, `remove_requires_delete_approval_and_same_file` | ✅ Linux, macOS, Windows (CAS/identity tests; reparse-swap test on Windows, CI 37616036608) |
| 3 | Injection via tool parameters | Lexical confinement; glob/regex validation; closed protocol enum; validated model tool calls; Windows verbatim paths normalised | `dotdot_traversal_is_normalised_and_confined`, `absolute_path_injection_is_confined_or_refused`, `verbatim_absolute_params_are_accepted`, `glob_traversal_and_absolute_refused`, `regex_redos_patterns_are_linear`, `openai_response_parsing_rejects_hostile_tool_calls`, protocol `rejects_*` | ✅ Linux, macOS, Windows |
| 4 | Shell injection | argv only, no shell; bare program names resolved against absolute PATH entries; shells require a prompt | `shell_metacharacters_are_inert`, `program_must_be_bare_name`, e2e `echo "; rm -rf / #"` | ✅ Linux, macOS, Windows (AppContainer; `CommandLineToArgvW`-exact quoting `command_lines_round_trip_through_argv_quoting`, batch files refused `confined_batch_files_are_refused`; ADR-0026, CI 37616036608) |
| 5 | Credential exfiltration | Policy deny list (ssh, aws, gh, gnupg, keychains, browser profiles, …), including Windows homes; commands confined (Landlock / Seatbelt); runtime cannot read files | `credentials_denied_everywhere`, `credentials_denied_with_backslash_home` (Windows), `filesystem_confinement_enforced` (Linux), `runtime_process_cannot_touch_the_filesystem` (Linux), e2e symlink-to-`.ssh` attack | ✅ Linux · ✅ macOS (policy, e2e, `(deny default)` Seatbelt, runtime `sandbox_init`) · ✅ Windows (policy; AppContainer grants `confined_commands_reach_the_workspace_only` and the validation suite) · CI 37616036608 · ✅ Windows runtime AppContainer (`runtime_process_cannot_touch_the_filesystem_windows`, CI 37675535141; unconfined with a loopback provider: AR-7) |
| 6 | Network egress | Commands default to network-denied: user+net namespace, or a seccomp socket filter where user namespaces are restricted (Ubuntu ≥ 23.10, GitHub runners); macOS Seatbelt; `network = true` requires a prompt | `network_denied_process_has_no_network` (both Linux methods, asserting the mechanism's errno), `network_allowed_process_can_open_inet_sockets` | ✅ Linux · ✅ macOS (egress test, validation suite) · ✅ Windows (AppContainer without capabilities, `confined_commands_have_no_network`, validation suite) · CI 37616036608 |
| 7 | Resource exhaustion | rlimits (NPROC, AS, CPU, FSIZE, CORE), wall timeout, process-group kill, root drop | `fork_bomb_is_contained`, `process_limit_refuses_excess_forks`, `memory_limit_enforced` (Linux), `wall_timeout_kills`, `output_is_capped` | ✅ Linux · ✅ macOS (incl. memory supervisor) · ✅ Windows (Job Object: `confined_memory_is_limited`, `confined_commands_are_killed_at_the_deadline`, validation suite; no per-file size limit, AR-2) · CI 37616036608 · aggregate limits: AR-2 |
| 8 | Malicious MCP/A2A peer | Identity from the transport only (pipe binding or per-agent bearer token); user-granted permissions; closed argument structs; loopback-only listener with Host/Origin checks and size/time limits; claims enforced at merge; every call audited, refusals included (ADR-0023, ADR-0004, ADR-0007) | `rogue_agent_cannot_bypass_the_hub` (missing, invalid and retired tokens; a bad Host or an Origin header; claims outside the grant or of an ungranted type; releasing another agent's claim; user-only operations (approve, accept, register), which are not exposed as tools; forged identity fields over MCP; reading others' messages; each refused, DB unchanged, audited; an A2A metadata sender is ignored: identity is the token), `hostile_http_is_refused`, `claims_outside_grants_are_refused_and_audited`, `only_the_user_registers_agents_and_tokens_authenticate`, `unclaimed_changes_are_rejected_at_merge` (a change made without the Hub never reaches main), `claims_prevent_conflicts` (an internal write to another agent's claim: `DeniedBy::Claim`), `verify_detects_database_tampering`, `verify_detects_audit_truncation` | ✅ Linux, macOS, Windows (run 37474281956) |
| 9 | Snapshot poisoning | The store lives outside the workspace, beyond every command sandbox (ADR-0022). Type-tagged, hash-verified objects; strict canonical decoding refuses escaping names and setuid bits; restore is approval-gated, descriptor-based, never follows symlinks and runs a pre-flight. Dedup re-verifies existing objects and heals damaged ones (ADR-0006) | `command_cannot_touch_state_dir`, `crafted_snapshot_cannot_escape_on_restore`, `poisoned_store_is_refused`, `crafted_tree_names_are_refused`, `symlink_planted_in_store_is_refused` | ✅ Linux, macOS, Windows (store tests; command confinement CI 37616036608) |

## §5 primitive test bullets

| Spec bullet | Test | Status |
|---|---|---|
| 5.1 symlink escape | `symlink_escape_refused`, `junction_escapes_are_refused` (Windows) | ✅ Linux, macOS, Windows |
| 5.1 symlink-swap race | `symlink_swap_race_never_escapes` + 2 deterministic variants; `junction_swapped_after_resolution_is_caught` (Windows) | ✅ Linux, macOS, Windows |
| 5.1 hardlink attack | `hardlink_attack_refused` | ✅ Linux, macOS · ✅ Windows (CI 37667610242) |
| 5.1 `..` traversal | `dotdot_traversal_is_normalised_and_confined` | ✅ |
| 5.1 Unicode normalisation collision | `unicode_normalisation_collision_is_same_file` (macOS) | ✅ macOS |
| 5.1 case-insensitivity collision | `case_insensitive_collision_is_same_file` (macOS, Windows) | ✅ macOS, Windows |
| 5.1 absolute path injection | `absolute_path_injection_is_confined_or_refused` | ✅ |
| 5.2 every policy branch | `every_match_condition_is_exercised`, `deny_overrides_earlier_allow`, `first_match_wins_among_non_deny`, `capability_floor_*`, … | ✅ |
| 5.2 `../../../../etc/passwd` param | `dotdot_traversal_…`, `capability_floor_rejects_relative_target`, e2e | ✅ |
| 5.2 policy change mid-session | `hot_reload_takes_effect_on_next_request`, `invalid_reload_keeps_last_good_policy` | ✅ |
| 5.2 no rule → default | `unmatched_request_falls_to_default` | ✅ |
| 5.2 never reads natural language (instrumented) | `request_schema_has_no_natural_language_fields` (schema has no free-text field) | ✅ |
| 5.2(f) decisions logged, queryable | `every_decision_is_logged_with_structured_context`, `audit_chain_detects_tampering`, `anchor audit query` | ✅ |
| 5.3 write to append-only refused | `write_to_append_only_refused`, e2e | ✅ |
| 5.3 edit-append byte-verified | `append_only_edits`, `append_verified_preserves_prefix` | ✅ |
| 5.3 edit modifying a byte refused | `append_only_edits`, e2e `replace` on CHANGELOG | ✅ |
| 5.3 write to unread file refused | `write_without_read_refused`, `overwrite_of_unread_file_requires_diff_confirmation` | ✅ |
| 5.3 write after read succeeds | `write_after_full_read_succeeds` | ✅ |
| 5.3 override persists | `override_persists_across_sessions` | ✅ |
| 5.4 pause at 80% | `tracker_pauses_at_threshold`, `agent_pauses_at_context_threshold`, e2e `context_threshold_pauses_and_fresh_session_resumes` | ✅ |
| 5.4 checkpoint has required fields | `checkpoint_contains_required_fields` | ✅ |
| 5.4 fresh session from checkpoint | `fresh_session_seeded_from_checkpoint_resumes`, e2e | ✅ |
| 5.4 kill mid-turn leaves valid checkpoint | `kill_mid_write_leaves_valid_checkpoint` (SIGKILL during 2 MiB writes), e2e `kill_mid_turn_leaves_valid_checkpoint_and_resumes` | ✅ |
| 5.5(a) snapshot before every mutating turn | mediator `ensure_turn_snapshot`; e2e `end_to_end_session_enforces_every_primitive` (snapshot audit entries), `rollback_after_command_restores_gitignored_and_dotgit` | ✅ Linux, macOS |
| 5.5 100k files within target | `examples/bench.rs`, CI `snapshot-bench` (Linux gate: 2 s); [benchmark](benchmarks/snapshot.md) | ✅ Linux (CI runner: 0.40 s / 0.67 s; container: 0.77 s / 1.10 s) · ❌ macOS runner (2.53 s / 2.59 s; scan-bound) · ❌ Windows runner (3.95 s / 5.56 s; scan-bound, Defender) |
| 5.5 kill mid-snapshot → no partial snapshot | `kill_mid_snapshot_leaves_no_partial_snapshot` (child process aborted after 50 objects published), e2e `kill_mid_turn_…` (store `verify` after SIGKILL) | ✅ Linux, macOS, Windows |
| 5.5 kill mid-restore → workspace pre-restore | `kill_mid_restore_recovers_to_pre_restore_state` (child aborted after 4 operations; `recover()` on next open), `failed_restore_reverts_to_pre_restore_state` | ✅ Linux, macOS, Windows |
| 5.5 symlink loop doesn't hang | `scan_never_follows_symlinks_or_loops` (anchor-fs), `symlink_loops_snapshot_and_restore` | ✅ Linux, macOS |
| 5.5 revert restores exactly | `revert_restores_exactly` (bytes, modes, symlinks, empty dirs, type changes; rescan tree id equals the original), e2e exact-state rollback | ✅ Linux, macOS · Windows: bytes and directories only (no modes or symlinks) |
| 5.5 crafted file cannot escape the store | `crafted_tree_names_are_refused`, `poisoned_store_is_refused`, `symlink_planted_in_store_is_refused`, e2e `crafted_snapshot_cannot_escape_on_restore` | ✅ |
| 5.5(c) dedup / unchanged files not re-stored | `unchanged_files_are_neither_reread_nor_restored`, `chunker_*`, `index_catches_backdated_and_replaced_files` | ✅ |
| 5.5(d) concurrent change never clobbered | `concurrent_change_is_detected_and_preserved`, `damaged_objects_are_healed_from_the_workspace` | ✅ |
| 5.5(f) merge-queue substrate | `diff_merge3_and_materialize` | ✅ |

## §16.2 Hub integration and §16.4 concurrency (Phase 4)

| Requirement | Test | Status |
|---|---|---|
| Two agents of different model families collaborate | `two_agents_of_different_families_collaborate` (families from the roles files; beta works from alpha's merged code and reads alpha's message; exact final tree; Hub and audit verify) | ✅ |
| File claims prevent conflicts (two agents, one file) | `claims_prevent_conflicts` (parallel real runtimes: exactly one write, the other gets a conflict and `DeniedBy::Claim`), `concurrent_claims_on_one_file_have_exactly_one_winner` (100 threads), `claim_conflict_rules` | ✅ |
| All messages persist and are auditable | `messages_persist_across_restart`, `messages_are_persisted_visible_only_to_their_parties_and_holdable`, `anchor hub log`, `anchor hub verify` | ✅ |
| §16.4 adjacent files, isolated, merged without corruption | `concurrent_isolated_agents_merge_adjacent_files` (parallel; exact tree; store verify; then one agent's merge reverted, by agent) | ✅ |
| Merge conflicts return structured feedback | `merge_conflict_returns_structured_feedback`, `merge_gate_failure_rejects` | ✅ |
| A merge never reverts concurrent edits | `merge_never_reverts_concurrent_main_edits` | ✅ |
| Kill mid-merge recovers | `kill_mid_merge_recovers` (child `anchor team run` aborted after 2 apply operations) | ✅ |
| Claims are checked per file | `new_directories_are_checked_file_by_file` | ✅ |
| MCP and A2A conformance | `mcp_discover_list_and_call_conform_to_2026_07_28`, `mcp_protocol_errors`, `a2a_card_send_and_tasks` | ✅ |

## §8 verification pipeline (Phase 5)

Tests are in `crates/anchor-backend/tests/verify.rs` and `tests/eval.rs`.
They use real commands (Python unittest and trace) in the sandbox. As root
(the development container), commands run as uid 65534. Design: ADR-0024.

| Requirement / threat | Test | Status |
|---|---|---|
| The pipeline refuses to start if executor and reviewer share a family | `pipeline_refuses_same_family`: same primary family, or overlap only through a fallback | ✅ Linux, macOS (run 37496045693) |
| | `reviewer_falling_back_into_executor_family_is_refused` | ✅ Linux, macOS (run 37496045693) |
| | `verdicts_from_an_executor_family_are_refused`: a session that fell back mid-review | ✅ Linux, macOS (run 37496045693) |
| A failing build does not merge | `failing_build_does_not_merge` | ✅ Linux, macOS (run 37496045693) |
| Failing tests do not merge | `failing_tests_do_not_merge`, with structured feedback (failing test, hunks) | ✅ Linux, macOS (run 37496045693) |
| A failed review does not merge | `refuted_claim_does_not_merge` (mock model reviewer) | ✅ Linux, macOS (run 37496045693) |
| | `uncovered_change_is_not_supported` (rule reviewer) | ✅ Linux, macOS (run 37496045693) |
| A claim without evidence is rejected automatically (§8.2(b)) | `claim_without_evidence_rejected_automatically` | ✅ Linux, macOS (run 37496045693) |
| Test tampering | `tampered_tests_rejected`: protected tests edited | ✅ Linux, macOS (run 37496045693) |
| | `regressed_test_rejected`: a passing test deleted, with no protected paths | ✅ Linux, macOS (run 37496045693) |
| Spoofed success (exit 0 before the tests can fail) | `spoofed_exit_rejected` (`tests_inconclusive`) | ✅ Linux, macOS (run 37496045693) |
| Stdlib shadowing (`unittest.py` in the workspace) | `stdlib_shadowing_rejected` | ✅ Linux, macOS (run 37496045693) |
| Verification commands are confined | `verification_commands_cannot_write_outside_scratch`; `verified_change_merges_with_evidence` (the test log records `fs_confined` and the network-isolation method) | ✅ Linux, macOS (run 37496045693) |
| Evidence is durable, hashed and backend-only | `verified_change_merges_with_evidence`, `evidence_is_backend_only_hashed_and_immutable` | ✅ Linux, macOS (run 37496045693) |
| §8.2(c) self-repair with feedback, bounded | `self_repair_merges_on_second_attempt` (the feedback is in the repair instruction) | ✅ Linux, macOS (run 37496045693) |
| | `retries_are_bounded`, `user_decline_is_not_retried` | ✅ Linux, macOS (run 37496045693) |
| What is applied is what was verified | `drift_reverifies_candidate`: main edited mid-check; the applied tree is the one verified last | ✅ Linux, macOS (run 37496045693) |
| | `crash_after_gate_reverifies_on_recover`: a child `anchor team run` killed mid-apply; re-verified before applying | ✅ Linux, macOS (run 37496045693) |
| Verification never stalls other agents | `slow_gate_does_not_block_other_agents`: the store stays available during a 1.5 s check | ✅ Linux, macOS (run 37496045693) |
| Unverified merges are explicit | `unverified_team_needs_explicit_flag`; `merge_unverified` audited | ✅ Linux, macOS (run 37496045693) |
| The gate on Windows (AppContainer; per-command profiles deleted, W-5; coverage written into a directory the command creates, read only if it is a real directory) | the gate tests above, now run on Windows too (`verified_change_merges_with_evidence` … `verification_commands_cannot_write_outside_scratch`); `released_profiles_are_deleted` | ✅ Windows (CI 37623168183) |
| §8.5 reviewer false-negative rate measured | `seeded_bug_harness_measures_reviewers`: accept-all lets through exactly what reaches review, oracle 0%, rule baseline matrix locked | ✅ Linux, macOS (run 37496045693) |
| | `corpus_ground_truth_holds`, `eval_cli_with_mock_model_reviewer`; results in the [benchmark](benchmarks/reviewer.md) | ✅ Linux, macOS (run 37496045693) |

**Residual risks** (ADR-0024):

- code under test can still forge results in its own process, beyond
  the checks above;
- coverage measures execution, not correctness: vacuous tests and
  semantic bugs pass the rule baseline (measured: FN 100% in both
  categories);
- configured commands read workspace files an agent can change, unless
  those files are protected.

**LLM reviewer:** its false-negative rate is not yet measured, pending a
model key.

## §12 dashboard (Phase 6)

The dashboard is a loopback web server with authority over the user's
approvals (ADR-0025). Its tests are in
`crates/anchor-backend/tests/ui.rs` (HTTP, raw requests) and
`ui/e2e/dashboard.spec.ts` (Chromium).

| Threat / requirement | Mitigation | Test | Status |
|---|---|---|---|
| A malicious web page drives the dashboard (CSRF) | Token required on every `/api/` call, as a header, never a cookie. A foreign `Origin` or `Sec-Fetch-Site: cross-site` gets 403. Writes need `application/json`, which no simple cross-site form can send | `dashboard_is_token_gated_same_origin_and_hardened` | ✅ Linux, macOS, Windows (CI 37551637739) |
| DNS rebinding | Exact `Host` check (`127.0.0.1:<port>` / `localhost:<port>`); a rebound name is refused even with the token | same test (`evil.example` Host → 403) | ✅ Linux, macOS, Windows (CI 37551637739) |
| Token leakage (URL, history, `Referer`, logs) | The token is in the URL fragment (never sent); the page removes it from the address bar and history; `Referrer-Policy: no-referrer`; the token is never in a request line | same test (URL shape); e2e `not.toHaveURL(/#t=/)` | ✅ Linux, macOS, Windows; e2e on Linux (CI 37551637739) |
| Another local user or process | Loopback bind; 256-bit per-launch token from the OS RNG; constant-time comparison. `--ui-url-file` is written 0600. *Residual:* any process running as the same user can read that process's memory and environment anyway | same test (missing, wrong and short tokens → 401) | ✅ Linux, macOS, Windows (CI 37551637739) |
| XSS from agent-written content | React renders text only (no `dangerouslySetInnerHTML`); `Content-Security-Policy: default-src 'none'; script-src 'self'` (no inline script); framing denied | same test (headers); content rendered as text in the e2e | ✅ Linux, macOS, Windows; e2e on Linux (CI 37551637739) |
| Path traversal to files outside the bundle | Static files come from an embedded table only; there is no filesystem lookup | same test (`/../Cargo.toml`, `%2e%2e`) | ✅ Linux, macOS, Windows (CI 37551637739) |
| A forged or replayed approval | Approvals are answered by id, with a choice from the offered set, once. Every answer is audited (`ui_action`, `approval_resolved via ui`), then goes through `prompt_response`. Rollback needs a one-time nonce from a preview, and is refused while a run holds the workspace | `two_model_collaboration_through_the_dashboard` (mid-run rollback → 409; nonce reuse → 409) | ✅ Linux, macOS (CI 37551637739; the test is unix-only) |
| Every inter-agent interaction is visible and auditable | Every audit entry is shown (unknown kinds generically), verified against the hash chain as it is read; every Hub row is reachable; agent actions are attributed | `two_model_collaboration_through_the_dashboard` (feed = audit log, seq for seq; Hub messages, claims, decisions and merges equal the API's; every session or agent-caller entry attributed) | ✅ Linux, macOS (CI 37551637739; the test is unix-only) |
| Failures are never hidden (§12.3) | Rejections with findings and evidence; conflicts in red; `fails_on_main` ("caught later"); a broken audit chain or a cost mismatch gives a red banner | `two_model_collaboration_through_the_dashboard`, `test_only_change_failing_is_flagged_as_caught_later`, e2e | ✅ Linux, macOS; e2e on Linux (CI 37551637739) |
| The cost meter is accurate | Each call is recomputed from the roles files' prices and compared with the runtime's report and the session's own totals; any mismatch is listed | `two_model_collaboration_through_the_dashboard` (per agent, session and total = tokens × prices exactly, review sessions included); `cost_is_recomputed_from_prices_and_mismatches_are_reported` (a lying runtime is flagged) | ✅ Linux, macOS (CI 37551637739; the test is unix-only) |
| The user's polling floods the audit log | User views read through `Hub::view_*` and do no audit writes. An unauthenticated request is *not* audited (a web page could otherwise spam the log) | `user_views_see_everything_and_write_no_audit_entries`, the hardening test (`audit.head()` unchanged) | ✅ Linux, macOS, Windows (CI 37551637739) |

## Phase 7: security review and sandbox validation

The internal review, its fixes and the accepted risks are in
[audit/](audit/README.md) (ADR-0027). The cross-platform validation suite
(`crates/anchor-sandbox-validation`, `every_attack_is_stopped`) runs
each attack through the backend's real command confinement. First green
on all three platforms in CI run 37616036608:

| Attack | Linux (both tiers) | macOS | Windows |
|---|---|---|---|
| TCP to an external address; to a loopback listener | stopped | stopped | stopped |
| Read a file outside the workspace; a fake `~/.ssh` key; list outside | stopped | stopped | stopped |
| Write outside the workspace | stopped | stopped | stopped |
| Unbounded process creation | stopped | stopped | stopped |
| Allocate past the memory limit; spin the CPU | stopped | stopped | stopped |
| Write a file past the size limit | stopped | stopped | not enforced (AR-2) |
| Inherit the caller's environment | stopped | stopped | stopped |
| Unix socket outside (network denied, and allowed); signal the backend | stopped | stopped | n/a |
| `ptrace`, `bpf`, `keyctl`, `mount`, `unshare` | stopped | n/a | n/a |

Each CI job appends its platform's table to the job summary.

| Review finding (Critical/High) | Regression test | Status |
|---|---|---|
| S-1 unix sockets reachable | `unix_sockets_and_signals_outside_the_sandbox_are_unreachable` | ✅ Linux (CI 37616036608); macOS by the validation suite's unix-socket row; n/a on Windows |
| S-2 `setsid` escapes the kill; S-3 `TIOCSTI` | `commands_cannot_escape_the_kill_or_reach_the_terminal` | ✅ Linux (CI 37616036608) |
| S-4 macOS `(allow default)` | `seatbelt_profile_denies_by_default`, `mach_services_are_unreachable_macos` | ✅ macOS (CI 37616036608) |
| T-1 git hooks planted by commands | `commands_cannot_plant_git_hooks_or_config` | ✅ Linux, macOS (CI 37616036608) |
| P-1 append-only reshaping; P-2 restore skipped | `heuristic_detection_is_sticky`, `tracked_files_replaced_by_untracked_entries_are_restored` | ✅ Linux (CI 37616036608) |
| V-1 verdict forged by runner tampering; V-2 startup hooks | `runner_tampering_rejected`, `startup_hooks_rejected_and_home_is_empty` | ✅ Linux (CI 37616036608); residual AR-4 |
| Round 2: W-1 workspace ACLs rewritable; F-1 pins did not block renames | ACE mask and `SET_ACCESS`; `open_workspace_root_cannot_be_renamed` | ✅ Windows (CI 37616036608) |
| Round 2: M-2 unix sockets with network; M-5 keychains; M-1 `/System/Volumes/Data` (not reproduced: Seatbelt resolves firmlinks) | `seatbelt_closes_unix_sockets_data_volume_and_keychains_macos` | ✅ macOS (CI 37616036608) |
| Round 2: K-1 backend threads signalled | `protected_processes_cannot_be_signalled` (tkill to a thread) | ✅ Linux (CI 37616036608) |

## Phase 8: verifier plugins (ADR-0009)

Plugins are third-party programs that judge each claim at the merge gate.
A plugin is untrusted the way a command is: it runs in the command sandbox,
and its answer can only add rejections.

| Property | Tests | Status |
|---|---|---|
| A plugin cannot approve what the built-in reviewer rejects; it can block what it approves | `combined_reviewers_take_the_worst_verdict`, `a_refuting_plugin_blocks_the_merge` | 🟡 CI pending (Linux local) |
| A plugin that crashes, hangs or answers badly fails closed | `a_crashing_plugin_fails_closed`, `a_hanging_plugin_times_out_and_fails_closed`, `verifier::tests` (protocol rules) | 🟡 CI pending (Linux local) |
| A plugin can read neither the workspace nor the network | `plugins_are_sandboxed_and_support_merges` (a loopback listener and a workspace file, with a positive read control) | 🟡 CI pending (Linux local) |
| The published contract matches the code | `anchor-protocol/tests/verifier_schema.rs`, `fixtures_are_valid_review_inputs`, `the_example_plugin_conforms` | 🟡 CI pending (Linux local) |

Residual: a plugin sees the change it reviews (AR-17).

## Phase 2 "verified by instrumentation"

- *No filesystem mutation bypasses the permission engine*: mutations
  require an `Approved` token that only `anchor-policy` can mint. The e2e
  test checks that `anchor_fs::mutations_performed()` equals the number of
  audited mutations, and that every one traces to an approving decision.
- *The engine never reads natural-language input*: the engine's only input
  type has no free-text field, and a schema test fails if one is added.

## Platform history

- Run 37414390349: Linux and macOS commands failed closed (user
  namespaces restricted; macOS `RLIMIT_AS`). On Windows, verbatim paths
  were wrongly refused.
- Run 37447560545: Linux green after the seccomp fallback. The Windows
  credential-glob bug was found in the policy tests.
- Run 37448154072: all jobs green; macOS and Windows made blocking.
- Run 37450990863: state directory moved out of the workspace (ADR-0022);
  green on all platforms.
- Run 37453554617 (Phase 3 integration): one Linux test race (two `Store`
  handles in one test) and three Windows failures. The worst was a real
  bug: FILETIME was not converted to the Unix epoch, so every file looked
  racy and was re-hashed on every snapshot.
- Run 37456108695: all test jobs green on Linux, macOS and Windows. The
  Linux snapshot benchmark passed the 2 s gate. The macOS and Windows
  benchmarks (reported only) missed it.
- Run 37458112407: all jobs green on Linux, macOS and Windows.
- Run 37470753364 (Phase 4): Linux and macOS green. On Windows,
  `two_agents_of_different_families_collaborate` failed because of a test
  bug: the CRLF checkout broke the test's multi-line roles replacement, so
  both agents stayed in one family.
- Run 37471666248: with the CRLF fix, a second Windows test bug remained.
  The test derived the isolated-workspace directory from the raw path,
  not the canonical `\\?\` root the runner uses.
- Runs 37472772722 and 37474281956 (attempts 1–2): no runner was
  allocated, because the account's included Actions minutes were used up.
  CI now cancels superseded runs, and the macOS and Windows benchmarks run
  only on demand.
- Run 37474281956 (attempt 3): all jobs green on Linux, macOS and Windows;
  the Phase 4 done-when tests and the Threat 8 suite pass on all three.
- Run 37496045693 (Phase 5, attempt 2): all jobs green on Linux, macOS
  and Windows. The §8 done-when tests pass on Linux and macOS, and the
  Windows gate fails closed. The Linux job ran the seeded-bug harness on
  the whole corpus.
- Run 37521970692 (Phase 6, attempt 2): Linux green, including the
  frontend build, `npm audit` and the Playwright dashboard test; macOS
  green. Windows clippy failed: helpers used only by the unix-only
  dashboard test were unused there.
- Run 37550399552: Windows green. macOS failed `hardlink_attack_refused`
  intermittently: `F_GETPATH` may report a hardlinked file's *outside*
  name, so resolution refused it as escaping before the hardlink check
  (fail-closed either way). The refusal now names the hardlink, and the
  test accepts both outcomes. `symlink_swap_race_never_escapes` was made
  robust on loaded machines at the same time.
- Run 37551637739 (Phase 6): all jobs green on Linux, macOS and Windows. The §12
  dashboard tests pass on all three (the two-model collaboration test on
  Linux and macOS), and the Playwright test passes in Chromium on Linux.

## Out of scope for this push

The external audit (§11.2; pending a third party, see
[audit/README.md](audit/README.md)), the Windows runtime with a loopback
provider (AR-7),
and the LLM reviewer's measured false-negative rate (pending a model
key).
