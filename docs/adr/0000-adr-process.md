# ADR-0000: Architecture Decision Records

- Status: Accepted
- Date: 2026-10-06

## Context

The master spec (§0, §17) requires that every dependency, ambiguous
requirement, and open question be decided in writing, with rejected
alternatives, before implementation. It also says that nothing it names is
confirmed fact.

## Decision

Each decision is a numbered Markdown file in `docs/adr/` with these
sections: Status, Date, Context, Decision, Alternatives rejected,
Consequences, and (where relevant) Verification. Statuses:

- **Proposed — pending evaluation**: the direction is stated, but the evidence
  the spec demands (benchmarks, hardware, external specs) has not been
  gathered. Nothing that depends on it may ship.
- **Accepted**: implemented and verified as described in the ADR's
  Verification section.
- **Superseded by ADR-NNNN**.

ADRs state what was **observed** (with date and source), and keep it separate
from what is **assumed**. When a source could not be reached from the build
environment, the ADR says so instead of filling the gap from memory.

Third-party dependencies also get a five-step record (existence,
maintenance, license, fitness, security) in
[`docs/deps/dependency-records.md`](../deps/dependency-records.md).

## Template

```markdown
# ADR-NNNN: Title
- Status:
- Date:
## Context
## Decision
## Alternatives rejected
## Consequences
## Verification
```

## Index

| ADR | Topic | Status |
|---|---|---|
| 0001 | Launch model roster (§17 Q1) | Proposed — pending evaluation |
| 0002 | Desktop shell: Tauri vs Electron (§17 Q2) | Proposed — pending evaluation |
| 0003 | Sandbox mechanism per platform (§17 Q3) | Accepted (Linux) / Proposed (macOS, Windows) |
| 0004 | MCP and A2A spec versions and SDKs (§17 Q4) | Proposed — pending evaluation |
| 0005 | LLM router: build vs integrate (§17 Q5) | Accepted (build); cost benefit pending |
| 0006 | Snapshot engine design (§17 Q6) | Proposed |
| 0007 | Worktree management (§17 Q7) | Proposed |
| 0008 | Editor: CodeMirror 6 vs Monaco (§17 Q8) | Proposed — pending evaluation |
| 0009 | Verifier plugin interface (§17 Q9) | Proposed (draft) |
| 0010 | Append-only marking and heuristics (§17 Q10) | Accepted |
| 0011 | Policy rule conflict resolution (§17 Q11) | Accepted |
| 0012 | Agents that disappear mid-task (§17 Q12) | Proposed |
| 0013 | Canonical path resolution and TOCTOU design | Accepted (Linux) |
| 0014 | Backend ↔ runtime protocol | Accepted |
| 0015 | Policy file format | Accepted |
| 0016 | Audit log format | Accepted |
| 0017 | HTTP client for providers | Accepted |
| 0018 | Interim durability before the snapshot engine | Accepted (temporary) |
| 0019 | Licensing of the open-core crates | Accepted |
| 0020 | Enforcing "the runtime has no OS authority" | Accepted |
| 0021 | Spec interpretations | Accepted |
