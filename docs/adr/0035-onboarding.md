# ADR-0035: Onboarding and the first failure

- Status: Accepted (Update #1, 2026-10-08)
- Date: 2026-10-08

## Decision

**First run** asks for nothing: no account, no key, no email. The new-task
screen offers, in order, **"Run locally — nothing leaves your machine"**
(Ollama, found automatically), **"Use your own API key"**, and **"Start
free — no signup"** only when a free tier is actually offered. A first
task can be small; if nothing can run it, it gets the three-way prompt and
keeps its text (ADR-0029).

**First failure** (once, the first time a task fails) explains what
Anchor gives the user at that moment, as features: **Undo** (every
changing turn has a snapshot; "Undo from here"), **the Reviewer** (a
second model of another family checks the work), and **evidence** (checks
actually run and recorded; the tamper-evident Audit log). It is
remembered per viewer in browser storage only.
