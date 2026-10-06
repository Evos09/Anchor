# Threat model and test map (spec §5, §11)

A threat without a passing test is an unmitigated vulnerability. Status
legend:

- ✅ mitigated, with a test passing on Linux in this repository;
- 🟡 implemented and type-checked, but the test runs only on another OS in
  CI and has not been observed passing;
- ⛔ not implemented, fails closed;
- ⏳ later phase.

## §11 threats

| # | Threat | Mitigation | Tests | Status |
|---|---|---|---|---|
| 1 | Symlink escape | Kernel-confined resolution; canonical path from the handle (ADR-0013) | `anchor-fs`: `symlink_escape_refused`, `relative_symlink_escape_refused`, `magic_link_refused`; e2e attack script | ✅ Linux · 🟡 macOS/Windows |
| 2 | TOCTOU race | Descriptor-based strict reopen, identity check, exchange-and-verify | `symlink_swap_race_never_escapes` (2,000 iterations against a flipping symlink), `parent_swapped_after_resolution_is_refused`, `target_swapped_for_symlink_after_resolution_is_refused`, `compare_and_swap_preconditions`, `remove_requires_delete_approval_and_same_file` | ✅ Linux · 🟡 macOS/Windows |
| 3 | Injection via tool parameters | Lexical confinement; glob/regex validation; closed protocol enum; validated model tool calls | `dotdot_traversal_is_normalised_and_confined`, `absolute_path_injection_is_confined_or_refused`, `glob_traversal_and_absolute_refused`, `regex_redos_patterns_are_linear`, `openai_response_parsing_rejects_hostile_tool_calls`, protocol `rejects_*` | ✅ |
| 4 | Shell injection | argv only, no shell; bare program names resolved against absolute PATH entries; shells require a prompt | `shell_metacharacters_are_inert`, `program_must_be_bare_name`, e2e `echo "; rm -rf / #"` | ✅ |
| 5 | Credential exfiltration | Policy deny list (ssh, aws, gh, gnupg, keychains, browser profiles, …); commands confined by Landlock (no `$HOME` access); runtime cannot read files | `credentials_denied_everywhere`, `filesystem_confinement_enforced`, `runtime_process_cannot_touch_the_filesystem`, e2e symlink-to-`.ssh` attack | ✅ Linux · 🟡 macOS (Seatbelt) · ⛔ Windows commands refuse to run |
| 6 | Network egress | Commands default to a fresh network namespace; `network = true` requires a prompt (`prompt_on_network_commands`) | `network_denied_process_has_no_network` | ✅ Linux · 🟡 macOS · ⛔ Windows |
| 7 | Resource exhaustion | rlimits (NPROC, AS, CPU, FSIZE, CORE), wall timeout, process-group kill, root drop | `fork_bomb_is_contained`, `process_limit_refuses_excess_forks`, `memory_limit_enforced`, `wall_timeout_kills`, `output_is_capped` | ✅ Linux · 🟡 macOS · ⛔ Windows |
| 8 | Malicious MCP/A2A peer | Hub RPC authentication and authorisation | — | ⏳ Phase 4 |
| 9 | Snapshot poisoning | Hash-verified blobs; manifest paths re-confined on restore | `crafted_manifest_cannot_escape_on_restore` (interim store) | ✅ interim · ⏳ Phase 3 engine |

## §5 primitive test bullets

| Spec bullet | Test | Status |
|---|---|---|
| 5.1 symlink escape | `symlink_escape_refused` | ✅ |
| 5.1 symlink-swap race | `symlink_swap_race_never_escapes` + 2 deterministic variants | ✅ |
| 5.1 hardlink attack | `hardlink_attack_refused` | ✅ |
| 5.1 `..` traversal | `dotdot_traversal_is_normalised_and_confined` | ✅ |
| 5.1 Unicode normalisation collision | `unicode_normalisation_collision_is_same_file` (macOS) | 🟡 |
| 5.1 case-insensitivity collision | `case_insensitive_collision_is_same_file` (macOS, Windows) | 🟡 |
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
| 5.5 all | Snapshot engine | ⏳ Phase 3 (interim: ADR-0018) |

## Phase 2 "verified by instrumentation"

- *No filesystem mutation bypasses the permission engine*: mutations
  require an `Approved` token that only `anchor-policy` can mint. The e2e
  test checks that `anchor_fs::mutations_performed()` equals the number of
  audited mutations, and that every one traces to an approving decision.
- *The engine never reads natural-language input*: the engine's only input
  type has no free-text field, and a schema test fails if one is added.

## Out of scope for this push

External audit (§11.2), seccomp, Windows sandbox, Hub threats (8), and
verification-pipeline threats.
