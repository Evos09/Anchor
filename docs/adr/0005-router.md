# ADR-0005: LLM router — build vs integrate (§17 Q5, §9)

- Status: Accepted (build). The cost-control benefit is still unquantified.
- Date: 2026-10-06

## Decision

Build. The router (`anchor-runtime/src/router.rs`) is a precomputed table
from task kind (mechanical / architecture / agentic / review) to a
configured role. That makes routing deterministic and constant-time. The
measured cost on the build machine is well under a microsecond per decision
(`router_is_deterministic_and_fast` asserts < 100 µs as a regression
guard).

Provider fallback is per role (`fallback = [...]`). Every fallback emits a
`provider_fallback` notice to the user and the audit log, so there is no
silent downgrade. Fallbacks may not cross into the reviewer's model family
(checked at config time).

Cost accounting uses user-configured per-million-token prices. The runtime
reports a `Usage` message per call; the backend shows cumulative spend and
audits it.

## Alternatives rejected

- *Integrate a third-party LLM gateway*: adds a network hop or a large
  dependency, and moves routing decisions out of our audit trail.

## Pending

Quantify the cost-control benefit on our own workload once the eval harness
exists (§16.3). Also pending: the pre-operation projected-cost estimate
(§9.5) and the per-agent breakdown once multiple agents exist (Phase 4).
