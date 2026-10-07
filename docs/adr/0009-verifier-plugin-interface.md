# ADR-0009: Verifier plugin interface (§17 Q9)

- Status: Accepted. Protocol version 1 is frozen (Phase 8).
- Date: 2026-10-06, revised 2026-10-07 (Phase 8)

## Context

Spec §17 Q9 asks for a verifier plugin interface that is stable before
open-sourcing, "because the community will build against it". Phase 8's
"done when" requires that:
- the interface is documented and stable;
- a third party can build a plugin without reading Anchor's source.

Phase 5 built the in-process review types (`anchor-protocol::verify`,
ADR-0024) and two reviewers: the deterministic rule reviewer and the model
reviewer. They share the `Reviewer` trait.

## Decision

**A verifier plugin is a separate program, never linked into Anchor.**
- **Its manifest** is a `verifier.toml` with the name, the command (an
  optional Windows variant), the time limit and the memory limit.
- **Enabling it:** a project lists the manifest under `[verify] verifiers`,
  at most 8.

**One run per request.** For each request Anchor starts the plugin:
1. It writes one JSON line to the plugin's stdin and closes it.
2. The plugin writes one JSON line to stdout and exits 0.

**The protocol** (version 1). The envelope is
`{"v":1,"id":N,"msg":{"kind":K,"payload":P}}`:
- `describe` is answered with `info` (`name`, `version`, `protocol` list);
- `verify`, whose payload is the existing `ReviewInput`, is answered with
  `verdicts` (one `ClaimVerdict` per claim);
- either may be answered with `error`.

**Sandbox.** A plugin runs in the command sandbox (ADR-0003, ADR-0026):
- no network;
- read-only access to its own directory and the system and `PATH`
  directories, and nothing else;
- no writable directory;
- a cleared environment;
- time, CPU, memory and process limits.

A plugin directory that holds the workspace is refused. On Windows each
run's AppContainer profile is released.

**Statelessness.** Each request is a fresh run. A plugin cannot carry state
from one candidate to the next (the V-2 class), and a hung plugin costs one
time limit.

**Combining verdicts.** A plugin is an extra gate (user decision,
2026-10-07). `anchor_verify::Combined` takes the worst verdict per claim
from the built-in reviewer and every plugin, in the order refuted,
unverifiable, supported. So a plugin can block a merge but never approve
one.

**Fail closed.** These all make every claim `unverifiable`
(`verifier_failed`):
- a crash, a non-zero exit, a timeout or a sandbox failure;
- not exactly one valid response line, or a response over 1 MiB;
- a wrong `id` or version;
- a verdict for an unknown claim, or two verdicts for one claim;
- an `error` response;
- a `describe` answer that does not list protocol 1.

Two further rules:
- A claim the plugin leaves out is `unverifiable`.
- A `supported` verdict over a truncated input counts as `unverifiable`.

**Free text is advisory.** `note` and `error.message` are shown to people
and never parsed for a decision (as in the draft).

**The published contract** is `docs/verifier/`:
- a guide;
- JSON Schemas (2020-12) of request and response;
- fixtures.

`anchor-protocol/tests/verifier_schema.rs` keeps them and the code in step,
with a dependency-free validator for the schema subset used:
- every request Anchor sends, and every response it accepts, validates
  exactly, field for field;
- the schema and the code refuse the same bad responses.

**Tooling for plugin authors:**
- `anchor verifier check MANIFEST` runs the conformance checks in the real
  sandbox;
- `anchor eval run --verifier MANIFEST` measures a plugin on the seeded-bug
  corpus.

**Stability policy:**
- **Version 1 is frozen.** Within it, requests may gain optional fields,
  and plugins must ignore unknown fields (the conformance check tests
  this). Responses never change.
- **Anything else is version 2,** which is negotiated through `describe`.
- **Dropping version 1** needs a major release and a new ADR.

## Why freeze now

The draft deferred the freeze until the LLM reviewer had been measured,
against at least two independent implementations. Both reasons are
addressed:

- **The interface does not depend on how good any reviewer is.** The LLM
  measurement (pending a model key) decides the launch roster and the §8.5
  stop threshold (ADR-0001, ADR-0021). It does not decide the shape of the
  wire format: a better reviewer answers the same `ReviewInput` with the
  same verdicts.
- **There are now two independent implementations of the out-of-process
  protocol:**
  - Anchor's side, in Rust;
  - the example plugin, in Python with the standard library only
    (`examples/verifier-python/`), written from `docs/verifier/` alone.

  Behind the same review types there are also two in-process reviewers:
  the rule reviewer and the model reviewer.

## Alternatives rejected

- **Dynamic libraries / an in-process ABI.** A plugin bug or a malicious
  plugin would run with the backend's authority (§4). This was rejected in
  the draft and stays rejected.
- **A long-lived plugin process.** That would mean state across
  candidates, a supervisor, and a harder sandbox story. It could be
  revisited as a version 2 option if start-up cost matters.
- **Plugins replacing the reviewer.** A plugin could then approve what the
  built-in gate rejects. The user chose an extra gate instead.
- **Workspace access for plugins.** A plugin sees what the reviewer sees:
  the diff and the evidence. Read access would make every plugin a
  potential exfiltration path.
- **A schema-validation dependency in the test.** The schemas use a small
  subset, which a validator of about 150 lines covers. That avoids a new
  dependency (§0).

## Consequences

- **Plugin authors** need only `docs/verifier/`. Phase 8's done-when test
  for this is `the_example_plugin_conforms`.
- **CI** runs the conformance check on Linux, macOS and Windows.
- **A plugin sees the diff and the evidence by design,** so plugins are
  trusted with the change's content. See AR-17 in
  `docs/audit/accepted-risks.md`.
- **Each plugin run starts a process.** For a Python plugin that is about
  40 ms on Linux (the conformance output records the times).

## Verification

- `anchor-protocol` unit tests (`verifier::tests`) cover every response
  rule.
- `anchor-protocol/tests/verifier_schema.rs`: the schemas match the code.
- `anchor-verify` `combined_reviewers_take_the_worst_verdict`.
- `anchor-exec` `stdin_bytes_reach_the_command` (Unix) and
  `confined_commands_read_their_stdin` (Windows).
- `anchor-backend`:
  - `tests/verifier.rs`: the example conforms; a hanging plugin times out
    and fails closed;
  - `tests/verify.rs`:
    - a refuting plugin blocks a merge;
    - a crashing plugin fails closed;
    - a probe plugin can neither read the workspace nor reach a loopback
      listener, and the change merges.
