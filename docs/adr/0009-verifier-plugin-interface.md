# ADR-0009: Verifier plugin interface (§17 Q9)

- Status: Proposed (draft; must be stable before open-sourcing, Phase 8)
- Date: 2026-10-06

## Draft

A verifier is a **separate process** that speaks versioned NDJSON, following
the same conventions as ADR-0014. It never links into Anchor:

```text
anchor -> verifier: {"v":1,"id":1,"msg":{"kind":"verify","payload":{
    "claims":[{"id":"c1","text":"handles expired tokens"}],
    "diff_artifact":"artifact://…", "evidence":["artifact://…"]}}}
verifier -> anchor: {"v":1,"id":1,"msg":{"kind":"verdict","payload":{
    "results":[{"claim":"c1","verdict":"rejected",
                "evidence_refs":["artifact://…"],"reason_code":"no_test_exercises_claim"}]}}}
```

Rules:

- A claim without at least one evidence reference is rejected automatically
  (§8.2(b)).
- Verifiers get artifacts read-only, through the Hub; they have no workspace
  access.
- Verdicts are structured (enum + references). Free text is advisory and is
  never parsed for decisions.

## Pending

Freeze this interface after the seeded-bug harness (§8.5) has been run
against at least two independent verifier implementations.
