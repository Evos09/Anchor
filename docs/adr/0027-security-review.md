# ADR-0027: Security review process and audit status

- Status: Accepted. The external audit is pending.
- Date: 2026-10-07

## Context

Spec §14 Phase 7 asks for three things:

- an external audit;
- remediation of what it finds;
- cross-platform sandbox validation.

It is done when no Critical or High finding is outstanding, validation
passes on macOS, Linux and Windows, and accepted risks are documented
for Medium and Low findings.

Only a third party can perform an external audit. The user chose an
internal adversarial review plus a package prepared for the auditor
(2026-10-07).

## Decision

1. **Internal review, round 1.** Five independent reviews, one per
   §11.2 area:
   - the primitives;
   - the threats end to end;
   - the sandbox;
   - the Hub's RPC surface;
   - the dashboard and verify gate.

   Each review starts from a frozen tree. A finding counts as confirmed
   only with a reproduction.
2. **Severity rubric.** Severity is exploitability × impact, measured
   against the spec's guarantees. The rubric is written out in
   `docs/audit/internal-review.md`. Critical means an escape or a
   defeated primitive with no user action.
3. **Remediation rule.**
   - Critical and High: fixed, each with a regression test that fails
     without the fix.
   - Medium and Low: fixed, or recorded in
     `docs/audit/accepted-risks.md`. Each entry states the exposure,
     what still holds, and what would close it.
4. **Round 2.** A second review, limited to the sandbox code written
   during remediation (Windows, macOS, the seccomp signal rule).
5. **Validation suite.** `anchor-sandbox-validation` runs the same
   attacks on all three platforms through the backend's real
   confinement. "Passes on X" means that suite is green in X's CI job.
6. **Audit package.** `docs/audit/README.md` holds:
   - the scope;
   - the trust boundaries;
   - build and test steps;
   - a code map;
   - where to look first.

## Status

- **Internal review.** Done. No Critical or High finding is open.
- **Cross-platform validation.** Passes on Linux, macOS and Windows
  (CI run 37616036608).
- **External audit.** Pending. When it happens, its findings get IDs
  `E-n`. They are triaged with the same rubric and enter the same two
  documents. Phase 7 is fully closed only after that.

## Consequences

- **What an internal review shows.** It is not independent, so it is
  evidence of diligence, not a substitute for the external audit.
- **What CI verifies.** Some code can only be exercised on CI: macOS and
  Windows. The threat model marks a row ✅ only once a CI run has passed
  its test on that platform. The first run green on all three, validation
  suite included, is 37616036608.
- **Windows verify gate.** It keeps failing closed until the rest of W-5
  is fixed (AR-15).
