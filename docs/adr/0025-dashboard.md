# ADR-0025: The collaboration dashboard (web UI first, Tauri later)

- Status: Accepted (the "unfamiliar user" criterion awaits a human
  sign-off; see `docs/ui-walkthrough.md`)
- Date: 2026-10-06

## Context

Spec §14 Phase 6 asks for the agent panel of §12.2: roster, file claim
map, message stream, decision log, merge queue, cost meter and snapshot
timeline. It is done when:

- a user unfamiliar with the tool can follow a two-model collaboration
  from the UI alone;
- every inter-agent interaction is visible and auditable;
- the cost meter is accurate.

§12.3 adds that the UI never hides a failure.

The desktop shell (ADR-0002, Tauri) cannot be built in the development
container: webkit2gtk is missing. The editor and terminal (§12.1,
ADR-0008) are a separate body of work.

## Decision

**User decisions (2026-10-06):**

1. **A web UI now, wrapped by Tauri later.** The backend serves the
   dashboard on loopback; a later Tauri shell loads the same `ui/dist`.
2. **React + TypeScript + Vite,** with every npm dependency vetted
   (`docs/deps/dependency-records.md`).
3. **Dashboard only.** The editor and terminal stay deferred.

### Where it runs

The dashboard runs inside the process that owns the state.

| Command | What it does |
|---|---|
| `anchor team run … --ui` | Serves it live. That process holds the snapshot store, the Hub and the approval prompts, so the dashboard answers gates as they happen. After the run it keeps serving and holds the workspace, which enables one-click rollback. |
| `anchor ui --workspace DIR` | Opens a finished workspace. If a team run holds the snapshot store, the timeline says so instead of showing stale data. |

### Transport and authentication

The Hub's hardened HTTP core is reused as a `Service`: the same size and
time limits, worker pool and backlog, and loopback only. The dashboard's
policy differs from the Hub's:

| Check | Dashboard | Hub |
|---|---|---|
| `Host` | exactly `127.0.0.1:<port>` or `localhost:<port>` | the same |
| `Origin` | allowed only if it is this exact origin | any `Origin` refused |
| `Sec-Fetch-Site` | only `same-origin` or `none` | — |
| Methods | `GET` and `POST` | `POST` only |
| Credential | a per-launch 256-bit token (`getrandom`) | per-agent bearer tokens |

The token handling:

- **Delivery.** The token is printed in the URL **fragment**
  (`#t=…`). Browsers never send a fragment to a server or in
  `Referer`.
- **Storage and use.** The page moves it to `sessionStorage`, removes
  it from the address bar and history, and sends it as
  `Authorization: Bearer` on every `/api/` call.
- **No cookies.** A forged cross-site request therefore carries no
  credential.
- **Comparison.** The token is compared in constant time.

Static files come from a table embedded at build time
(`crates/anchor-backend/build.rs`). There is no filesystem lookup, so a
path cannot escape. Every response carries:

- `Content-Security-Policy: default-src 'none'; script-src 'self'; …;
  frame-ancestors 'none'` (no inline script or style);
- `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`,
  `Cross-Origin-Opener-Policy` and `Cross-Origin-Resource-Policy`;
- `nosniff` and `no-store` (from the Hub's core).

Agent-written text is rendered only as React text: there is no
`dangerouslySetInnerHTML`.

**Live updates use a long poll** (`/api/events?after=N&wait=1`, at most
8 s) rather than SSE. It fits the one-request-per-connection core
unchanged.

### Data: a fold over the audit log

The audit log is the source of truth.

**Reading the log.** A verifying `Tailer` reads new complete lines and
checks each one extends the hash chain. It waits out a half-written last
line. A break stops the feed and shows a red banner.

**Turning entries into events** (`ui/model.rs`):

- each entry becomes one event with a sentence generated from a fixed
  template, never by a model;
- unknown kinds become a generic "kind + JSON" event, so **nothing is
  dropped**;
- bookkeeping (heartbeats, allowed reads, per-call usage) is marked
  `trace` and folded away by default;
- failures are never folded.

**Attribution.** To attribute every event, the backend now tags entries:

- every mediator and session entry gets `session_id`, and `agent` comes
  through the engine;
- claims and verdicts are logged as objects, so the tags apply to them
  too;
- sessions log `turn_started` / `turn_finished`, which drive the roster's
  "thinking" status;
- Hub entries are attributed through their `caller`.

**Approvals.** With `--ui`, a broker turns every terminal prompt into an
approval:

1. it logs `approval_pending`;
2. the dashboard answers it;
3. the broker logs `approval_resolved` (`via: "ui"`);
4. the answer then takes the usual `prompt_response` path.

`Interaction::split` gives each agent its own handle, so one agent waiting
on the user never stalls another.

**The cost meter recomputes** every reported call cost from the prices in
the user's own roles files. Those prices are now logged in `team_started`
for each agent and for the model reviewer. The meter flags two kinds of
mismatch:

- the runtime's reported cost against your prices;
- each session's summary against the sum of its calls.

So a runtime that misreports cannot hide behind the meter. Review
sessions are counted under the agent `reviewer`.

**The user's views are not audited.** Hub reads made by agents stay
audited, because they are agent actions. The dashboard reads through
`Hub::view_*`, which write nothing: these are not inter-agent
interactions, and polling would flood the log. Every dashboard
**action** is audited as `ui_action`, then goes through the same path as
the CLI:

- Hub user calls for held messages and decisions;
- `user_initiated` rollback, which is two-step (preview, then a one-time
  nonce) and refused while a run holds the workspace.

**Failures are first-class.**

- Gate rejections appear with their findings and stored evidence.
- The merge queue shows the change and the evidence side by side.
- A refused claim or a merge conflict turns the claim map red.
- The gate adds a `fails_on_main` finding: a test-only change whose tests
  fail exposes a bug already merged ("caught later", §12.3).

### Frontend

`ui/` is built with Vite, React 19 and TypeScript in strict mode:

- **No component, chart or state libraries.** The panels are
  hand-written CSS and SVG.
- **Shipped code.** The bundle contains only react, react-dom and
  scheduler.
- **Installs.** `npm ci --ignore-scripts`, with exact versions pinned in
  the lockfile.
- **Without Node.** `cargo build` still works; the binary embeds a
  "frontend not built" page instead.

## Consequences and limits

- **Agent turn snapshots** live only as long as the agent's isolated
  workspace. They show in the timeline during a run; afterwards the merge
  snapshots, the tagged `snapshot` entries in the story, and the merge
  diffs say which agent changed what.
- **Decisions have no task link** in the Hub's model. The "task" filter
  selects the agent working on a team task or an assigned Hub task.
- **"Expected merge time"** is the queue position × the median gate time
  observed so far. It shows "—" until a merge has completed.
- **Slash commands that start sessions** (`/plan`, `/implement`,
  `/review`) print the CLI command. Starting sessions from the UI comes
  with the editor.
- **The "unfamiliar user" criterion** is checked in two ways:
  - automatically, in part: the e2e test asserts the narrated sequence;
  - by a human: the sign-off in `docs/ui-walkthrough.md`.
