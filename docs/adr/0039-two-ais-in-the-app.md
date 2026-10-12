# ADR-0039: Two AIs in the app

- Status: Accepted (2026-10-12)
- Amends: ADR-0028 (desktop app), ADR-0029 (inference paths)

## Context

The app ran one model per task. It showed "reviewed by <model>", but no
review ran: the session report was discarded. Users asked for two AIs that
work on the project together, back and forth.

## Decision

A task runs in one of three modes, saved as `collab` in `app.json` (default
`pair`) and chosen per task with `mode` on `POST /api/app/task` or a
`/team` prefix:

1. **Builder + checker (`pair`).** The builder is the executor route. After
   it finishes, the backend diffs the project against a snapshot taken at
   the start of the task (a `pair/start` ref, so gc keeps it) and builds a
   review input from the gate's own code (`changed_files`, `fit`): the
   builder's claims, or one claim standing for the whole change, each tied
   to its files' diffs. A model reviewer (`verify::ModelReviewer`, the
   Phase 5 review session: no tools) judges it. If every claim is
   `supported`, the task is done. Otherwise the original task, the
   reviewer's notes and the cited diffs go back to the builder in a new
   session, up to `pair_rounds` reviews (default 2, 1-5). The verify gate's
   rules for tests do not apply: in the app there is no test configuration,
   and the reviewer judges the claims directly. Audit: `pair_review`,
   `pair_reply`, `pair_error`, `pair_skipped`.
2. **Two builders (`team`).** The Phase 4 team runner with two agents in a
   parallel schedule, each in an isolated copy, coordinating through the
   Hub; `NoGate` and `auto_merge: false`, so each merge into the project is
   an approval the user confirms. Routed: builder A runs the plan's
   executor and builder B the plan's second model (another family). Hub
   names are per task (`builder-a-<stamp>`), because a project's Hub keeps
   retired agents' names.
3. **One AI (`single`).** As before.

The family rule holds in every mode: the models file and per-task plans are
checked before a task starts; `ModelReviewer::new` refuses a review route
that shares a builder's family; the review session's routing notices are
re-checked after it ran.

Stop cancels everything: `session::Cancel` tracks every runtime it started
(two builders run at once), review sessions and team sessions run
cancellably (`TeamOptions.cancel`; the CLI passes `None`).

Scripted demos and tests: `--review-mock-script` (one per review, the last
repeats), `--repair-mock-script` (builder rounds 2+), `--team-mock-script`
(builder B). A scripted demo with no scripted reviewer runs as one AI.

## Consequences

- The "reviewed by" label is now true, and the reviewer's verdicts appear in
  the conversation as a second speaker.
- A builder + checker task costs up to `2 × rounds` sessions instead of one.
- The app does not open the Hub in its dashboard sources: two-builder
  coordination shows in the conversation through the audit log.
