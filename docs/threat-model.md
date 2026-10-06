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
| 1 | Symlink escape | Kernel-confined resolution; canonical path from the handle (ADR-0013) | `anchor-fs`: `symlink_escape_refused`, `relative_symlink_escape_refused`, `magic_link_refused` (Linux); e2e attack script | ✅ Linux, macOS · 🟡 Windows (no junction/symlink attack tests yet) |
| 2 | TOCTOU race | Descriptor-based strict reopen, identity check, exchange-and-verify | `symlink_swap_race_never_escapes` (2,000 iterations against a flipping symlink), `parent_swapped_after_resolution_is_refused`, `target_swapped_for_symlink_after_resolution_is_refused`, `compare_and_swap_preconditions`, `remove_requires_delete_approval_and_same_file` | ✅ Linux, macOS · 🟡 Windows (CAS/identity tests pass; no reparse-swap test yet) |
| 3 | Injection via tool parameters | Lexical confinement; glob/regex validation; closed protocol enum; validated model tool calls; Windows verbatim paths normalised | `dotdot_traversal_is_normalised_and_confined`, `absolute_path_injection_is_confined_or_refused`, `verbatim_absolute_params_are_accepted`, `glob_traversal_and_absolute_refused`, `regex_redos_patterns_are_linear`, `openai_response_parsing_rejects_hostile_tool_calls`, protocol `rejects_*` | ✅ Linux, macOS, Windows |
| 4 | Shell injection | argv only, no shell; bare program names resolved against absolute PATH entries; shells require a prompt | `shell_metacharacters_are_inert`, `program_must_be_bare_name`, e2e `echo "; rm -rf / #"` | ✅ Linux, macOS · ⛔ Windows (commands refuse to run) |
| 5 | Credential exfiltration | Policy deny list (ssh, aws, gh, gnupg, keychains, browser profiles, …), including Windows homes; commands confined (Landlock / Seatbelt); runtime cannot read files | `credentials_denied_everywhere`, `credentials_denied_with_backslash_home` (Windows), `filesystem_confinement_enforced` (Linux), `runtime_process_cannot_touch_the_filesystem` (Linux), e2e symlink-to-`.ssh` attack | ✅ Linux · ✅ macOS policy + e2e, 🟡 Seatbelt read-confinement not directly tested · ✅ Windows policy, ⛔ commands refuse |
| 6 | Network egress | Commands default to network-denied: user+net namespace, or a seccomp socket filter where user namespaces are restricted (Ubuntu ≥ 23.10, GitHub runners); macOS Seatbelt; `network = true` requires a prompt | `network_denied_process_has_no_network` (both Linux methods, asserting the mechanism's errno), `network_allowed_process_can_open_inet_sockets` | ✅ Linux · 🟡 macOS (Seatbelt rule applied, no egress test yet) · ⛔ Windows |
| 7 | Resource exhaustion | rlimits (NPROC, AS, CPU, FSIZE, CORE), wall timeout, process-group kill, root drop | `fork_bomb_is_contained`, `process_limit_refuses_excess_forks`, `memory_limit_enforced` (Linux), `wall_timeout_kills`, `output_is_capped` | ✅ Linux · ✅ macOS except memory (RLIMIT_AS unsupported, ADR-0003) · ⛔ Windows |
| 8 | Malicious MCP/A2A peer | Hub RPC authentication and authorisation | — | ⏳ Phase 4 |
| 9 | Snapshot poisoning | Hash-verified blobs; manifest paths re-confined on restore | `crafted_manifest_cannot_escape_on_restore` (interim store) | ✅ interim · ⏳ Phase 3 engine |

## §5 primitive test bullets

| Spec bullet | Test | Status |
|---|---|---|
| 5.1 symlink escape | `symlink_escape_refused` | ✅ Linux, macOS · 🟡 Windows |
| 5.1 symlink-swap race | `symlink_swap_race_never_escapes` + 2 deterministic variants | ✅ Linux, macOS · 🟡 Windows |
| 5.1 hardlink attack | `hardlink_attack_refused` | ✅ Linux, macOS · 🟡 Windows |
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
| 5.5 all | Snapshot engine | ⏳ Phase 3 (interim: ADR-0018) |

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

## Out of scope for this push

External audit (§11.2), seccomp, Windows sandbox, Hub threats (8), and
verification-pipeline threats.
