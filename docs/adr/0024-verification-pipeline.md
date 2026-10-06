# ADR-0024: Verification pipeline: deterministic tester, claims, evidence, reviewers, self-repair

- Status: Accepted (the rule baseline is measured; the LLM reviewer's
  false-negative rate is pending a model key)
- Date: 2026-10-06

## Context

Spec §8 and §14 Phase 5 require a planner/executor/tester/reviewer
pipeline. Its hard rules:

- **(a)** The reviewer is never of the executor's model family.
- **(b)** A claim without an artifact reference is rejected automatically,
  and the reviewer checks diff hunks, test artifacts and evidence.
- **(c)** Structured feedback goes back to the executor, with bounded
  retries.
- **(d)** A change merges only when the build passes, the tests pass and
  every claim is approved.

§8.5 requires measuring the reviewer's false-negative rate on a seeded-bug
harness, and stopping to redesign if it is too high.

## Decision

### Roles

| Role | Who | What it may do |
|---|---|---|
| Executor | A runtime session (Phase 4 team agent) | Edits its isolated workspace and ends with **claims** (`submit_claims`) |
| Tester | **The backend**, deterministically | Runs the build, test and coverage commands **the user configured** in `team.toml` `[verify]` on the merge candidate |
| Reviewer | The rule baseline, or a runtime session on the review route | Reads the claims, the diff and the evidence; returns one structured verdict per claim (`submit_verdicts`) |

**The Tester is not a model.** An LLM that chose which commands prove a
change could choose weak ones. Commands come from the user's file, and
the agent can neither see nor change them. A planner session is optional
(`TaskKind::Architecture`) and does not affect the gate.

### Claims are selectors

A claim is `{id, text, files, tests}`. The executor finishes before any
evidence exists, so it cannot cite artifacts. Instead, the backend
resolves each claim's files to `diff:<path>` and `coverage:<path>`
references, and its tests to `test:<id>` references. A claim that names
no changed file, or no test that ran, is rejected before any reviewer
sees it (§8.2(b)): `no_diff_for_claim` / `no_test_for_claim`. Claims are
persisted as Hub evidence when the change is submitted, so a merge
re-checked after a crash still has them.

### The gate (`VerifyGate`, a `MergeGate`)

For each merge candidate (pre-merge main merged with the agent's change):

1. **Changed files and line diffs**, from the pre-merge snapshot to the
   candidate. The diff is Myers, built in-house (`anchor-snapshot::textdiff`,
   spec §0), with line numbers that match Python `trace` coverage.
2. **Hard rules**, whatever the reviewer says:
   - existing files under `protected` (default `tests/**`) may not be
     modified or removed (`tests_tampered`); new test files are fine;
   - no new top-level module may shadow a standard-library module
     (`shadows_stdlib`).
3. **Commands** run on a scratch copy of the candidate through
   `anchor-exec`:
   - no network; writes confined to the scratch directory; limits applied;
   - when the backend runs as root, the commands run as uid 65534 and the
     tree is `lchown`ed to it (never following symlinks), under a 0711
     directory;
   - resource usage is recorded (`wait4`);
   - Python runs as `python3 -I`.
4. **Inconclusive tests.** If the test command exits 0 without unittest's
   own success summary (`Ran N tests … OK`), the result is
   `tests_inconclusive`.
5. **No regressions.** Every test that passed before the merge (the
   baseline is cached by tree) must still pass (`test_regressed`). This
   catches deleted tests, added skips and an emptied suite.
6. **Evidence.** Each command's full output, the review input and the
   decision are stored through `Hub::register_evidence`: backend-only,
   write-once 0400 files that the Hub hashes, outside every sandbox.
7. **Decision** (§8.2(d)): approved only if the build passes, the tests
   pass, no hard rule fired, every claim resolved, and the reviewer
   supports **every** claim. The reviewer runs only if everything before
   it passed. A truncated review input cannot be approved.

### Reviewers

| Reviewer | What it does |
|---|---|
| **Rule baseline** (`RuleReviewer`) | Supports a claim only if every listed file changed, every cited test ran and passed, every changed executable line of its non-test Python files ran under the tests, and no added non-test line uses a construct that can fake evidence (`os._exit`, `sys.exit`, tracer, `builtins`, `__import__`, unittest imports). It checks that the evidence is *consistent*; it cannot tell whether behaviour is *right*. |
| **Model reviewer** | A runtime session on the review route of its own roles file. It gets no workspace tools (ADR-0009): the backend refuses every tool call and ends the session. |

The family rule (§8.2(a)) is checked three times:

- **Family sets.** A route's families are its primary family plus every
  fallback provider's family (`anchor-protocol::roles`, shared by backend
  and runtime).
- **At team start.** The team refuses to start if any executor's families
  meet the reviewer's.
- **On each review.** The gate refuses verdicts if the session's routing
  or fallback notices name a provider in an executor's family.

### Gate without the lock, re-verify on drift

Verification can take minutes, so the gate runs without the snapshot
store's lock. It receives a `GateContext` (seq, attempt, agent, base,
submission, pre, candidate, changes, claims) and takes the lock only to
export trees.

The apply step refuses if the main workspace changed since `pre`. In that
case the merge is recomputed and the **new candidate is verified again**;
an identical tree is not re-checked. What is applied is always the tree
verified last. After 3 rounds the merge is rejected with `drift`.

The user's approval prompt comes after the gate and shows its verdict.

### Typed rejections and self-repair

Rejections carry a `RejectKind`: `claims`, `conflict`, `gate`, `drift`,
`declined` or `apply`, plus the pre-merge snapshot and the candidate.

**Retries.** Only `gate` and `conflict` rejections are retried, at most
`max_attempts` submissions per agent (default 3).

**The repair attempt:**

1. The agent's isolated workspace is rebuilt from the rejected candidate
   (its change on top of current main), counted from that merge's `pre`.
2. A fresh session gets the original instruction plus the rendered
   feedback (§8.2(c)): findings, failing tests, failed command output,
   rejected claims and the relevant hunks.

The agent's claims are freed, and its workspace is torn down, only once
the outcome is final.

### Unverified merges are explicit

`anchor team run` refuses a team file without `[verify]` unless given
`--unverified`. Merges through a gate that verifies nothing are audited
as `merge_unverified`.

### Windows fails closed

Agent commands have no sandbox on Windows (ADR-0003), so the gate rejects
every merge there (`unsupported_platform`).

### The seeded-bug harness (§8.5, §16.3)

**Corpus.** `examples/eval/corpus` has 12 Python tasks and 48 seeded bugs
in seven categories. Hidden acceptance tests decide the ground truth.

**`anchor eval run`:**

- refuses a corpus whose ground truth does not hold;
- checks every variant with the **same** `VerifyGate`;
- reports the confusion matrix, the false-negative rate overall and per
  category, the reviewer stage alone, completion, time and cost;
- turns the §8.5 stop rule into an exit status with `--max-fn-rate`.

**Calibration reviewers** test the harness itself:

- `accept-all`: what the deterministic stages alone catch;
- `oracle`: must reach 0% false negatives.

**Results**, full corpus ([benchmark](../benchmarks/reviewer.md)):

| Reviewer | Bugs merged | Correct changes rejected |
|---|---|---|
| Rule baseline | 15 of 48 (FN 31.2%) | 0 |
| Accept-all | 22 of 48 | — |
| Oracle | 0 | — |

Every *semantic* and *vacuous-test* bug passes the rule baseline: it
checks evidence, not intent. Those categories are what a model reviewer
must add. The §8.5 stop rule applies to the LLM reviewer, whose rate is
measured once a model key is available. Until then it is pending, and so
are ADR-0001's roster choice and the freeze of ADR-0009.

## Threats and residual risks

- **Spoofing from code in the same process.** Test results come from a
  process that also runs the code under test. That code can print fake
  results, exit early, patch unittest or skip tests.
  - *Countered by:*
    - the exit status is authoritative;
    - a zero exit without unittest's own summary is inconclusive;
    - the no-regression baseline;
    - protected test files;
    - `python3 -I` plus the stdlib-shadowing rule;
    - suspicious constructs refused by the rule reviewer.
  - *Residual:* code that tampers more cleverly with the interpreter at
    import time. A separate tester process that never imports the code
    under test (the hidden-test approach the harness uses) is the
    long-term answer.
- **Coverage is execution, not correctness.** Vacuous tests and semantic
  bugs pass every deterministic check: measured, and left to the
  reviewer.
- **Workspace content that commands read.** Build files (`setup.cfg`,
  `conftest.py`, `pyproject.toml`) are workspace content, so an agent can
  change what a configured command does. Mitigations:
  - protect those paths;
  - use an argv that does not read them (`-I`, explicit modules).
- **Commands run the agent's code.** They do so only inside the same
  sandbox as agent commands (threats 4–7). Writes outside the scratch
  directory fail (`verification_commands_cannot_write_outside_scratch`).

## Consequences

- Phase 5 merges are only as strong as the configured commands and the
  reviewer. The harness makes the reviewer's weakness a number, and the
  number is reported per category.
- `team.toml` gains `[verify]`. Protocol v3 adds claims and verdicts
  (ADR-0014). The Hub gains backend-only evidence.
