# ADR-0028: The desktop app, a thin Tauri window over `anchor app`

- Status: Accepted (Phase 9, 2026-10-08)
- Date: 2026-10-08
- Amends: ADR-0002 (desktop shell), ADR-0025 (dashboard: "Tauri later")

## Context

Until Phase 9, Anchor was a command-line program plus a browser dashboard
for team runs. The user tried it on Windows and asked for a desktop app
where coding happens. Their decisions (2026-10-07):

1. **An installable Tauri app**, with real installers (`.msi`, `.dmg`,
   `.deb`).
2. **Version 1 is chat-style tasks:** open a project folder, type a task,
   watch the agent, approve or deny each action in the window, and undo
   any turn. The editor, terminal and settings screen come later
   (ADR-0008 still governs the editor).

ADR-0002 leaned towards Tauri with the backend **linked in as a library**,
and ADR-0025 towards loading the dashboard bundle over IPC rather than
HTTP. Building it showed a better split.

## Decision

**The shell is a window and nothing else.** `desktop/` starts the bundled
`anchor app` backend as a child process and opens one window on the URL
it prints, `http://127.0.0.1:<port>/#t=<token>`. The page is the same React
bundle the dashboard uses (`ui/`); in app mode it renders the app view
(`ui/src/panels/Workspace.tsx`).

**Why not link the backend in:**

- **No new trust boundary.** The page talks to the backend over the
  dashboard's loopback HTTP surface (ADR-0025): token-gated, same-origin
  only, strict CSP, every action audited. That surface is in the audit
  pack. A Tauri IPC layer would be a second RPC surface with its own
  permission system to get right.
- **The origins line up.** The webview's origin is `http://127.0.0.1:<port>`
  itself, so the Host/Origin checks and the fragment-token flow work
  unchanged.
- **Testable without a webview.** `anchor app` runs in any browser, so CI
  drives the app with Playwright (`ui/e2e/app.spec.ts`) and over HTTP
  (`crates/anchor-backend/tests/app.rs`) on every platform. Only the shell
  needs a webview toolkit.
- **Contributors are unaffected.** `desktop/` is its own Cargo workspace
  with its own lockfile and `deny.toml`. `cargo build --workspace` and the
  quickstart never touch Tauri or WebKitGTK.
- **Layer 1 is unchanged.** The trusted process is still `anchor`; the
  shell holds no authority.

**The shell's hardening:**

- **No IPC.** No Tauri commands, no plugins, no capabilities (the
  generated capability set is empty), `withGlobalTauri: false`. Tauri
  refuses every command from a remote origin that no capability names (its
  own `remote_context_denied` test); here no capability exists at all.
  Tested: `the_shell_grants_the_page_nothing`.
- **Pinned navigation.** The window accepts only
  `http://127.0.0.1:<port>/#t=<64 hex>` from the backend, navigates only
  within that origin, and refuses new windows. Tested:
  `only_the_loopback_app_url_is_accepted`,
  `navigation_stays_on_the_backend_origin`.
- **No orphaned backend.** The shell stops the backend when the window
  closes. If the shell dies without that chance, the backend's stdin pipe
  closes and it exits (`anchor app --exit-with-stdin`). Tested:
  `the_app_exits_when_the_shell_goes_away`, and by hand: SIGKILL of the
  installed shell left no backend running.
- **The URL hand-over** (it carries the token) goes through a fresh
  owner-only temporary directory and the existing owner-only URL file.

**The app's backend (`anchor app`, `ui::app`):**

- **One project, one task at a time.** A task borrows the project's
  mediator on a worker thread and hands it back afterwards, so the
  dashboard's two-step rollback works between tasks: **Undo** is a restore
  of the snapshot taken before that turn's first change.
- **Stop** kills the runtime (`session::Cancel`) and refuses the approval
  it was waiting on (`Broker::cancel_pending`). The session ends as failed,
  "cancelled by the user".
- **The folder picker** (`/api/app/dirs`) lists directory names only, never
  file names or contents, and only behind the token.
- **The models file** is checked before use: it must parse, and the
  reviewer's family must differ from the executor's. The default is two
  local Ollama models (`examples/local/roles.ollama.toml`), written to the
  state directory on first run.
- **Every action is audited** (`ui_action`: `open_project`, `start_task`,
  `cancel_task`, `set_models`), like the dashboard's.

**Distribution.** CI builds unsigned installers on every push: `.deb` and
`.rpm` on Linux, `.app` and `.dmg` on macOS, `.msi` and NSIS `.exe` on
Windows. It runs the packaged app's `--smoke` check (start the backend, ask
it for the overview with the token) and uploads the installers as
artifacts. Nothing is released (ADR-0021). Code signing needs the owner's
certificates, so Windows SmartScreen and macOS Gatekeeper will warn.

## ADR-0002's gates

| Gate | Outcome |
|---|---|
| 1. Cold start | Measured by `--smoke` in CI on each OS (backend up and serving); 50 ms to 151 ms on Linux here. |
| 2. Editor rendering | Moved to ADR-0008: there is no editor in this version. |
| 3. Terminal fidelity | Moved to ADR-0008: there is no terminal. |
| 4. Native dialogs, child supervision | No native dialogs are used (the folder picker is part of the page). Child supervision is described above. |
| 5. Dependency record | `docs/deps/dependency-records.md`, "Desktop shell". |

## Consequences

- **The shell is new code outside the audit pack.** It holds no
  authority, but the external audit should include `desktop/src/main.rs`
  and the app endpoints (`crates/anchor-backend/src/ui/app.rs`).
- **The tray, menus, deep links, auto-update and native file dialogs** are
  out of scope. Each would add a plugin, and with it IPC capability, so
  each needs its own decision.
- **Tauri's tree brings named exceptions** in `desktop/deny.toml`: five
  MPL-2.0 crates and six unmaintained notices, none a vulnerability
  (dependency records).
- **Windows loopback providers.** The default Ollama setup is a loopback
  provider, so on Windows the agent runtime runs unconfined, warned and
  audited (AR-7). Agent commands are still sandboxed.

## Alternatives

- **Backend linked into the shell, IPC instead of HTTP** (ADR-0002's
  direction). Rejected for the reasons above: a second RPC surface, and no
  way to test the app without a webview.
- **A browser tab only** (`anchor app` alone). This works, and it is how
  the app is tested, but it is not an installable app (user decision 1).
- **Electron.** Still rejected (ADR-0002): its main process has ambient
  filesystem authority.
