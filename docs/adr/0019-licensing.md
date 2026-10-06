# ADR-0019: Licensing of the open-core crates (§13)

- Status: Accepted
- Date: 2026-10-06

All crates in this repository (runtime, safety primitives, and the
backend/mediator that will host the Hub) are licensed **Apache-2.0**. It
was chosen over MIT for its explicit patent grant and patent-retaliation
clause, which suit security-critical code that others will build verifiers
against. The commercial desktop shell (§13.1) will live in a separate
repository.

Dependency licenses are restricted by `deny.toml` to permissive licenses
that were read in full (see `docs/deps/dependency-records.md`).

The hard commitments (no training on user code, no ad tier, no default
telemetry) belong in the product EULA and privacy policy. That legal text is
**not** written here; it needs counsel. The architecture already supports
the commitments: there is no telemetry code, and the runtime only talks to
providers the user configured.
