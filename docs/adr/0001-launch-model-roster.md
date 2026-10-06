# ADR-0001: Launch model roster (§17 Q1)

- Status: Proposed — pending evaluation
- Date: 2026-10-06

## Context

The spec forbids trusting marketing or third-party benchmark tables. It
requires that launch models be chosen by measuring them on our own eval
harness (§16.3). The harness exists since Phase 5 (`anchor eval run`,
ADR-0024); measuring candidate models needs API keys, which are pending.

## Decision

1. **No model names in code.** Every model is chosen in
   `config/roles.toml` (`provider`, `model_id`, `family`, prices, context
   window). `anchor-runtime` contains no model identifiers. A test
   (`config_rejects_missing_roles_and_unknown_keys`) keeps the config
   schema closed, so a hardcoded fallback cannot sneak in.
2. **Model families are declared by the user or the curated registry**,
   not inferred from model names. Executor and reviewer families must
   differ, including all fallbacks (`same_family_review_refused`,
   `fallback_into_reviewer_family_refused`).
3. The launch roster is picked when the eval harness can measure, for each
   candidate: task completion rate, reviewer true-positive and
   false-negative rates on the seeded-bug corpus, cost per completed task,
   and wall-clock time. The curated list is versioned data with a license
   field per model (§10). No model ships before its license has been read in
   full.

## Alternatives rejected

- *Pick models from public leaderboards now.* Rejected: §0 and §17 Q1
  explicitly prohibit it.
- *Infer family from the model-id prefix.* Rejected: brittle and
  spoofable. A model id is an arbitrary string chosen by the provider.

## Consequences

Phase 1's "end-to-end against a local model / cloud model" criteria can be
exercised by configuration only. They are **not yet verified** in this
repository: the build environment has no local inference server and no API
keys. The OpenAI-compatible adapter is covered only by response-parsing tests
(`openai_response_parsing_rejects_hostile_tool_calls`).
