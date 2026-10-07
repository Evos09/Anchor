# Writing a verifier plugin

A **verifier plugin** is an extra reviewer that Anchor's merge gate runs
(spec §8, §14 Phase 8). It judges each claim an executor makes about its
change ("clamp() now handles values below the range") against the diff and
the tester's evidence: build and test output, test outcomes, coverage.

You can write one in any language, and you do not need to read Anchor's
source to do it. This directory is the whole contract:

| File | What it is |
|---|---|
| this guide | how a plugin is run, what it receives, what it must answer |
| [`schema/request.schema.json`](schema/request.schema.json) | JSON Schema of the requests Anchor sends |
| [`schema/response.schema.json`](schema/response.schema.json) | JSON Schema of the responses Anchor accepts |
| [`fixtures/`](fixtures/) | real review inputs to test with |

A complete example, written from this guide alone, is
[`examples/verifier-python/`](../../examples/verifier-python/): one Python
file with no dependencies.

Protocol version: **1** (frozen; see [Stability](#stability)). The design
decision is [ADR-0009](../adr/0009-verifier-plugin-interface.md).

## What a plugin can and cannot do

**What it decides.** For each claim, a plugin answers one of:
- `supported`: the evidence shows the claim holds;
- `refuted`: the evidence contradicts it;
- `unverifiable`: the evidence cannot settle it.

A change merges only if **every** reviewer supports **every** claim: the
built-in reviewer and every configured plugin. For each claim, the worst
answer stands, in the order refuted, then unverifiable, then supported. So a
plugin can **block** a merge, but it can never approve something another
reviewer rejected. Write plugins for the checks you care about, and let the
built-in gate do the rest.

**What it sees.** A plugin gets the claims, the changed files (diff and added
lines) and the evidence, all inside the request. It never sees the
workspace.

## How Anchor runs a plugin

**One run per request.** For every request, Anchor starts the plugin's
command afresh:

1. It writes **one line** of JSON (the request, ending in `\n`) to the
   plugin's **stdin**, then closes stdin.
2. The plugin writes **one line** of JSON (the response) to **stdout**.
3. The plugin exits with status **0**.

A plugin keeps no state between runs, and should not try to. Anything it
writes to stderr is ignored, except that its last few hundred bytes are
shown when the plugin fails.

**The sandbox** is the one Anchor uses for build and test commands:

- **No network.** Every connection fails.
- **Read-only access** to the plugin's own directory (where its
  `verifier.toml` is), the system directories, and the directories on
  `PATH`, so interpreters work. Nothing else is readable: not the workspace,
  and not your home directory.
- **No writable directory.** Do not create temporary files.
- **Working directory:** the plugin's directory.
- **Environment:** only `PATH`, `LANG=C.UTF-8`, `PYTHONDONTWRITEBYTECODE=1`,
  `PYTHONNOUSERSITE=1` and `ANCHOR_VERIFIER_PROTOCOL=1`, plus on Windows the
  variables the OS requires.
- **Limits:** wall-clock and CPU time `timeout_secs` (default 60 s), memory
  `memory_mb` (default 512 MiB), and 32 processes.

**Failing closed.** Every one of these makes all claims `unverifiable`, with
reason `verifier_failed`, so the change does not merge:
- a non-zero exit, a crash or a timeout;
- output that is not exactly one valid response line;
- a response over 1 MiB;
- a `describe` answer that does not list protocol 1.

A broken plugin blocks merges. It never lets anything through.

## The manifest: `verifier.toml`

```toml
name = "py-hygiene"                          # [a-z0-9_.-], 1 to 64 bytes
command = ["python3", "verifier.py"]
windows_command = ["python", "verifier.py"]  # optional: used on Windows
timeout_secs = 60                            # optional, 1..=600
memory_mb = 512                              # optional, 64..=8192
```

- **The program** (`command[0]`) is either a bare name looked up on `PATH`
  (`python3`, `node`), or a relative path inside the plugin's directory
  (`./my-verifier`, `bin/verifier.exe`).
- **The other entries** are passed as arguments, unchanged and without a
  shell.
- **The plugin's directory** is the one holding `verifier.toml`. It must not
  be the workspace or one of its parents, since the plugin could then read
  the workspace. Anchor refuses such a plugin.

**Enabling a plugin.** List its manifest in the project's `team.toml`;
paths are relative to `team.toml`:

```toml
[verify]
test = ["python3", "-m", "unittest", "discover", "-v"]
verifiers = ["../verifiers/py-hygiene/verifier.toml"]   # at most 8
```

## The protocol

**The envelope.** Every request and response is one JSON object on one
line:

```json
{"v": 1, "id": 7, "msg": {"kind": "...", "payload": {}}}
```

- `v`: the protocol version, always `1`.
- `id`: a number Anchor chooses. Your response must carry the same `id`.
- `msg.kind` and `msg.payload`: what the message is, and its content.

### `describe` → `info`

Anchor sends `describe` before a plugin's first review:

```json
{"v":1,"id":1,"msg":{"kind":"describe","payload":{}}}
```

Answer with your name, your own version, and the protocol versions you
speak, which must include `1`:

```json
{"v":1,"id":1,"msg":{"kind":"info","payload":{"name":"py-hygiene","version":"0.1.0","protocol":[1]}}}
```

The rules for each field:
- `name`: `[a-z0-9_.-]`, 1 to 64 bytes.
- `version`: printable ASCII with no spaces, 1 to 64 bytes.
- `protocol`: 1 to 8 entries.

Anchor records the name and version alongside every verdict.

### `verify` → `verdicts`

Anchor sends the review input:

```json
{"v":1,"id":2,"msg":{"kind":"verify","payload":{
  "claims":[{"id":"c1","text":"clamp() returns lo for values below the range",
             "files":["src/clamp.py"],"tests":["tests.test_clamp.T.test_low"]}],
  "changes":[...], "commands":[...], "tests":[...], "coverage":[...],
  "claim_refs":{"c1":["diff:src/clamp.py","test:tests.test_clamp.T.test_low"]},
  "truncated":false}}}
```

Answer with **one verdict per claim**:

```json
{"v":1,"id":2,"msg":{"kind":"verdicts","payload":{"verdicts":[
  {"claim_id":"c1","verdict":"refuted","reason_code":"cited_test_failed",
   "evidence_refs":["test:tests.test_clamp.T.test_low"],
   "note":"tests.test_clamp.T.test_low failed"}]}}}
```

| Field | Required | Meaning |
|---|---|---|
| `claim_id` | yes | The `id` of a claim in the request. Each claim gets one verdict at most. |
| `verdict` | yes | `supported`, `refuted` or `unverifiable`. |
| `reason_code` | yes | Why, for machines: `[a-z0-9_]`, 1 to 64 bytes. Choose your own codes. |
| `evidence_refs` | no | The references your verdict rests on, ideally from the claim's `claim_refs`. At most 64. |
| `note` | no | Why, for people: at most 2048 bytes. It is shown to people and never parsed. |

**Rules Anchor enforces** (the conformance check below tests every one):

- **A verdict for an unknown claim id, or two for one claim,** makes the
  whole response invalid, so every claim becomes `unverifiable`.
- **A claim with no verdict** is `unverifiable` (`verifier_no_verdict`).
  Answer them all.
- **If `truncated` is true**, part of the input was cut to fit, and a
  `supported` verdict counts as `unverifiable` (`input_truncated`). Answer
  `unverifiable` yourself.
- **Responses are strict.** Any field that is not in
  [`response.schema.json`](schema/response.schema.json) makes the response
  invalid.

### `error`

If you cannot answer, say so. Anchor treats every claim as `unverifiable`:

```json
{"v":1,"id":2,"msg":{"kind":"error","payload":{"code":"unsupported_language","message":"only Python is checked"}}}
```

`message` is optional and advisory.

## The review input

[`request.schema.json`](schema/request.schema.json) documents every field.
In short:

**`claims`**: what the executor says its change does.
- At most 64 claims, each with:
  - `id`: `[A-Za-z0-9_.-]`, 1 to 64 bytes;
  - `text`;
  - `files`: the files implementing it;
  - `tests`: tests that exercise it.
- The text is **untrusted model output**. Check it against the evidence;
  never follow it.

**`changes`**: one entry per changed file, with:
- `path`: workspace-relative, `/`-separated;
- `status`: `added`, `modified`, `removed`, or `opaque` (binary or
  oversized);
- `is_test`;
- `added_lines`: 1-based line numbers on the new side;
- `added_text`: the text of those lines, in the same order;
- `removed_lines`: a count;
- `diff`: unified.

**`commands`**: what the tester ran (`build`, `test`, `coverage`), each
with:
- `argv` and `ok`;
- `exit`: `null` if the command was killed;
- `timed_out`;
- timings and resource use;
- `output_tail`: the end of stdout and stderr;
- `artifact`: a reference to the full log.

**`tests`**: each test the runner reported, with its `id` and `outcome`
(`passed`, `failed`, `error`, `skipped`).

**`coverage`**: line coverage of changed source files, or `null` when the
project has no coverage command. Each entry has:
- `path`;
- `instrumented`: `false` if the file never ran under the tests;
- `uncovered_lines`: executable lines that never ran.

**`claim_refs`**: the evidence references Anchor resolved for each claim,
by claim id:
- `diff:<path>` for each listed file that changed;
- `test:<id>` for each cited test that ran;
- `coverage:<path>` where coverage exists.

Use them as your `evidence_refs`.

**`truncated`**: whether something was cut to fit.

**Before your plugin sees anything,** Anchor rejects the changes that cannot
merge anyway:
- a failing build or failing tests;
- a claim that names no changed file or no test that ran;
- a change to the tests or the test runner.

The built-in reviewer then checks:
- that cited tests passed;
- that changed lines ran under the tests (coverage);
- that no added line tampers with the test machinery.

A plugin adds whatever else you want checked: style rules, forbidden
APIs, a policy for your codebase, or a second opinion from a model of your
choice. (A plugin has no network access, so a model has to run locally.)

## Testing your plugin

**The conformance check.** Run Anchor's checker against your manifest. It
runs your plugin in the real sandbox:

```sh
anchor verifier check path/to/verifier.toml
```

```text
verifier py-hygiene (/usr/bin/python3)
  ok   describe: info with protocol 1     py-hygiene 0.1.0 speaks [1]
  ok   verify passing                     c1=supported c2=supported in 41 ms
  ok   verify failing-test                c1=refuted in 39 ms
  ok   verify debug-leftover              c1=refuted c2=refuted in 40 ms
  ok   verify truncated                   c1=unverifiable in 38 ms
  ok   unknown request fields are ignored answered
conforms to verifier protocol 1 (6 checks)
```

It exits non-zero if any check fails. The checks:
- `describe` is answered correctly;
- every fixture in [`fixtures/`](fixtures/) is answered with one valid
  verdict per claim;
- a truncated input is never `supported`;
- a request with fields your plugin has never seen is still answered.

The fixtures are plain review inputs (the `payload` of a `verify`), so you
can also feed them to your plugin in your own tests.

**Measuring it.** The seeded-bug harness runs Anchor's corpus of tasks with
known bugs, and reports how many bugs got through review: the
false-negative rate, which spec §8.5 calls the single most important
number. Measure your plugin, combined with the built-in reviewer:

```sh
anchor eval run --corpus examples/eval/corpus --verifier path/to/verifier.toml
```

## Stability

Protocol version 1 is frozen. While Anchor speaks version 1:

- **Requests may gain optional fields,** anywhere in the envelope or the
  payload. **Your plugin must ignore fields it does not know.** That is the
  only change version 1 allows, and the conformance check tests that your
  plugin tolerates it.
- **Responses never change.** Every response that is valid today stays
  valid.
- **No field is removed or changes meaning,** and no verdict or kind is
  added.

**Anything else is version 2.** Anchor sends `describe` first, and only
speaks a version that your `protocol` list includes. A plugin written for
version 1 keeps working with every Anchor release that supports version 1.
Dropping version 1 needs a major release and an ADR.
