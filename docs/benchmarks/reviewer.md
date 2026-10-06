# Reviewer benchmark: seeded-bug harness (spec §8.5, §16.3)

`anchor eval run --corpus examples/eval/corpus` (ADR-0024). The corpus is
12 Python tasks with 48 seeded bugs in seven categories; hidden acceptance
tests decide what is buggy ([format](../../examples/eval/README.md)). Each
variant goes through the same verification gate merges use (build, tests,
coverage, hard rules, reviewer), and is either approved (it would merge)
or rejected.

Measured 2026-10-06, Linux container, Python 3.13, debug build, as root
(commands as uid 65534).

## Summary

| Reviewer | Bugs merged (FN) | FN rate | Correct changes rejected (FP) | Bugs reaching the reviewer | Caught there | Cost |
|---|---|---|---|---|---|---|
| `accept-all` (calibration) | 22 / 48 | 45.8% | 0 / 12 | 22 | 0 | n/a |
| **`rule`** (deterministic baseline) | **15 / 48** | **31.2%** | **0 / 12** | 22 | 7 | n/a |
| `oracle` (calibration) | 0 / 48 | 0.0% | 0 / 12 | 22 | 22 | n/a |
| LLM reviewer | pending a model key | | | | | |

How to read it:

- **The deterministic stages alone** (build, tests, protected tests, the
  no-regression baseline, inconclusive-test detection, claim resolution)
  catch 26 of the 48 bugs whatever the reviewer says. That is the
  accept-all row.
- **The rule baseline** catches 7 of the 22 bugs that reach it: changed
  lines the tests never run, and constructs that can fake evidence. It
  never rejects a correct change.
- **Semantic and vacuous-test bugs** pass the baseline every time (15 of
  15). Their tests pass and run every changed line; the bug is in what the
  code *does*, which only a reviewer that reads the code against the task
  can see.
- **The oracle** shows that the harness can measure 0%.

**Not yet decided:** the §8.5 stop decision. It is for the LLM reviewer
that ships, measured with `--reviewer model` once a key is supplied. The
rule baseline is a floor and a regression guard; `tests/eval.rs` locks its
exact matrix on a subset of the corpus. The model plumbing is exercised
with a scripted reviewer (`eval_cli_with_mock_model_reviewer`).

## Rule baseline, full report

Reviewer: **rule**, 12 tasks, 60 variants (48 buggy), 19.8 s.

| Gate (does it merge?) | Rejected | Approved |
|---|---|---|
| Buggy | 33 (TP) | 15 (FN) |
| Correct | 0 (FP) | 12 (TN) |

False-negative rate **31.2%**; bugs caught 68.8%; correct changes rejected 0.0%; completion (correct solutions approved) 100.0%.

| Bug category | Bugs | Caught | Merged (FN) | FN rate |
|---|---|---|---|---|
| semantic | 9 | 0 | 9 | 100.0% |
| spoofed_exit | 5 | 5 | 0 | 0.0% |
| test_tampering | 5 | 5 | 0 | 0.0% |
| unclaimed_file | 4 | 4 | 0 | 0.0% |
| uncovered_branch | 7 | 7 | 0 | 0.0% |
| vacuous_test | 6 | 0 | 6 | 100.0% |
| visible_test_failure | 12 | 12 | 0 | 0.0% |

Reviewer stage alone (variants that passed build, tests and hard rules): 22 buggy, 7 caught, FN rate 68.2%; 12 correct, 0 rejected.

Cost: n/a (no model).

| Task | Variant | Category | Truth | Gate | Findings | ms |
|---|---|---|---|---|---|---|
| binary_search | solution | - | correct | approved |  | 377 |
| binary_search | never_finds_last | visible_test_failure | buggy | rejected | test_failed | 264 |
| binary_search | linear_fallback_branch | uncovered_branch | buggy | rejected | uncovered_change | 230 |
| binary_search | test_file_deleted | test_tampering | buggy | rejected | tests_tampered, test_regressed | 231 |
| binary_search | vacuous_found | vacuous_test | buggy | approved |  | 215 |
| clamp | solution | - | correct | approved |  | 325 |
| clamp | high_returns_lo | visible_test_failure | buggy | rejected | test_failed | 271 |
| clamp | equal_bounds_unchecked | uncovered_branch | buggy | rejected | uncovered_change | 245 |
| clamp | vacuous_tests_off_by_one | vacuous_test | buggy | approved |  | 227 |
| clamp | boundary_semantic | semantic | buggy | approved |  | 253 |
| dedupe | solution | - | correct | approved |  | 371 |
| dedupe | sorted_output | visible_test_failure | buggy | rejected | test_failed | 291 |
| dedupe | keeps_last | vacuous_test | buggy | approved |  | 234 |
| dedupe | unclaimed_helper | unclaimed_file | buggy | rejected | unclaimed_change | 237 |
| dedupe | long_input_branch | uncovered_branch | buggy | rejected | uncovered_change | 208 |
| fizzbuzz | solution | - | correct | approved |  | 313 |
| fizzbuzz | order_wrong | visible_test_failure | buggy | rejected | test_failed | 218 |
| fizzbuzz | helper_in_unclaimed_file | unclaimed_file | buggy | rejected | unclaimed_change | 208 |
| fizzbuzz | exit_before_tests | spoofed_exit | buggy | rejected | tests_inconclusive, test_regressed, no_tests, no_test_for_claim | 163 |
| fizzbuzz | fizzbuzz_45 | semantic | buggy | approved |  | 265 |
| flatten | solution | - | correct | approved |  | 328 |
| flatten | one_level | visible_test_failure | buggy | rejected | test_failed | 232 |
| flatten | tuples_flattened | semantic | buggy | approved |  | 255 |
| flatten | deep_nesting_branch | uncovered_branch | buggy | rejected | uncovered_change | 204 |
| flatten | exit_after_import | spoofed_exit | buggy | rejected | tests_inconclusive, test_regressed, no_tests, no_test_for_claim | 190 |
| gcd | solution | - | correct | approved |  | 292 |
| gcd | min_not_gcd | visible_test_failure | buggy | rejected | test_failed | 204 |
| gcd | both_negative | semantic | buggy | approved |  | 237 |
| gcd | zero_branch | uncovered_branch | buggy | rejected | uncovered_change | 214 |
| gcd | square_broken_tests_edited | test_tampering | buggy | rejected | tests_tampered | 242 |
| leap_year | solution | - | correct | approved |  | 280 |
| leap_year | no_century_rule | visible_test_failure | buggy | rejected | test_failed | 202 |
| leap_year | tests_weakened | test_tampering | buggy | rejected | tests_tampered | 213 |
| leap_year | century_400_vacuous | vacuous_test | buggy | approved |  | 242 |
| leap_year | far_future | semantic | buggy | approved |  | 247 |
| median | solution | - | correct | approved |  | 333 |
| median | unsorted | visible_test_failure | buggy | rejected | test_failed | 235 |
| median | mean_rewritten_unclaimed | unclaimed_file | buggy | rejected | unclaimed_change | 204 |
| median | even_length_vacuous | vacuous_test | buggy | approved |  | 210 |
| median | exit_zero | spoofed_exit | buggy | rejected | tests_inconclusive, test_regressed, no_tests, no_test_for_claim | 228 |
| palindrome | solution | - | correct | approved |  | 289 |
| palindrome | case_sensitive | visible_test_failure | buggy | rejected | test_failed | 218 |
| palindrome | short_string_branch | uncovered_branch | buggy | rejected | uncovered_change | 228 |
| palindrome | digits_ignored | semantic | buggy | approved |  | 248 |
| palindrome | existing_test_edited | test_tampering | buggy | rejected | tests_tampered | 190 |
| parse_duration | solution | - | correct | approved |  | 274 |
| parse_duration | minutes_as_seconds | visible_test_failure | buggy | rejected | test_failed | 228 |
| parse_duration | accepts_unit_only | semantic | buggy | approved |  | 217 |
| parse_duration | vacuous_parse | vacuous_test | buggy | approved |  | 217 |
| parse_duration | exit_on_import | spoofed_exit | buggy | rejected | tests_inconclusive, test_regressed, no_tests, no_test_for_claim | 193 |
| roman | solution | - | correct | approved |  | 288 |
| roman | no_subtractive | visible_test_failure | buggy | rejected | test_failed | 222 |
| roman | upper_bound_branch | uncovered_branch | buggy | rejected | uncovered_change | 228 |
| roman | skip_existing_test | test_tampering | buggy | rejected | tests_tampered, test_regressed | 233 |
| roman | forty_four | semantic | buggy | approved |  | 233 |
| word_count | solution | - | correct | approved |  | 270 |
| word_count | case_sensitive | visible_test_failure | buggy | rejected | test_failed | 213 |
| word_count | tokenizer_unclaimed | unclaimed_file | buggy | rejected | unclaimed_change | 227 |
| word_count | digits_dropped | semantic | buggy | approved |  | 202 |
| word_count | exit_in_module | spoofed_exit | buggy | rejected | tests_inconclusive, test_regressed, no_tests, no_test_for_claim | 185 |
