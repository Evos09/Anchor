# ADR-0029: Three inference paths and the per-task router

- Status: Accepted (Update #1, 2026-10-08)
- Date: 2026-10-08
- Amends: ADR-0005 (router), ADR-0028 (desktop app)

## Context

Update #1 asks that a new user can run Anchor without an account or a
key, and that people who have a key or a local GPU can use them. Before
it, the app ran every task on one hand-written models (roles) file.

## Decision

Three paths, all behind the runtime's existing OpenAI-compatible adapter:

- **A — free tier:** Anchor's capped pool, through `anchor-proxy`
  (ADR-0032). The app holds an anonymous access token in the keychain.
- **B — your own key (BYOK):** straight from this computer to the
  provider. The key lives in the OS keychain (`keyring` 4.2.0: macOS
  Keychain, Windows Credential Manager, Linux Secret Service) and leaves
  it only for (1) one verify call to that provider's `GET /models` when
  added, and (2) the environment of the runtime process that calls that
  provider (`SessionConfig.extra_env`, redacted in `Debug`). It is never
  logged, audited, written to `app.json`, or sent to Anchor. Removing a
  key deletes the keychain entry and checks it is gone.
- **C — local:** Ollama, probed on `127.0.0.1:<port>` (`/api/version`,
  `/api/tags`; port configurable, default 11434). The page says "Local
  inference active" when it answers.

**Router** (`crates/anchor-inference/src/router.rs`), per task:
a project **pin** wins, then a one-task override (`/local`, `/byok`,
`/free` at the start of the task), then the user's **preference**
(Automatic, Local first, My key first, Free only) or a custom order.
Automatic sends short or mechanical tasks to C → A → B and complex ones
(long, or refactor/implement/migrate…) to B → A → C. A path is
*available* if it can actually serve: Ollama answers with a usable model,
a key is stored with a model chosen, the free tier is started and not
capped. When nothing is available the task gets a **prompt** listing the
three ways forward, never an error (`{ok:false, prompt}`), and the task
text is kept.

The backend (`crates/anchor-backend/src/inference/`) writes the
runtime's models file per task (`plan.rs`): an executor on the chosen
path and a reviewer of a **different model family** (the §9.1 rule is
unchanged: the runtime and the verify gate still enforce it). The
reviewer may come from another available path, except when the user
confined the task (pin, override, or Free only). A task is audited
with its route (path, models, families, skipped paths); never with a key.

`anchor app --roles FILE` keeps the old behaviour (a "Models file"
source in Settings → Models → Advanced).

## Consequences

- Fallback happens per task, before it starts. A path failing mid-task
  surfaces the provider's own message (`ProviderError::Refused`), e.g. a
  free-tier cap with its reset time.
- On Windows a loopback provider (Ollama) still runs the runtime outside
  its AppContainer, warned and audited (ADR-0026, AR-7).
- Linux without a running Secret Service cannot store keys; the page
  says so plainly ("no system keychain is available").
