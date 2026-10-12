# ADR-0040: The app grows up: models, updates, tests, follow-ups

- Status: Accepted (2026-10-12)
- Amends: ADR-0028 (desktop app), ADR-0029 (inference paths), ADR-0033
  (updater), ADR-0039 (two AIs)

## Context

0.11.0 users still had to leave the app for four things:

- getting local models (`ollama pull` in a terminal);
- updating (finding a CI run's artifact, which expires after 90 days);
- knowing whether an "approved" change breaks the project (the checker read
  diffs only);
- saying "now also do X" (every message started a new task).

## Decision

1. **Models download in the app.** `POST /api/app/pull {model}` streams
   Ollama's own `POST /api/pull` on the loopback port (NDJSON progress, no
   proxy, no time limit except to connect). Progress is in
   `GET /api/app/pulls`; `POST /api/app/pull_cancel` drops the connection,
   which stops the pull.
   - Only models the registry lists for Ollama can be pulled
     (`inference::pull::allowed`). The app never fetches an arbitrary name,
     and every model it offers has its license recorded.
   - Settings → Models shows a builder and a checker of two families
     (`qwen2.5-coder:7b`, `llama3.1:8b`). The registry has no RAM field, so
     the pair is fixed.
   - Without Ollama, the card links to its download page.

2. **Updates.**
   - CI's `release` job publishes one GitHub Release per desktop version.
     It runs on a green push to the branch users download from, when tag
     `v<version>` does not exist yet. It attaches all three platforms'
     installers, with that version's `CHANGELOG.md` section as the notes.
   - The desktop shell passes its version (`ANCHOR_APP_VERSION`).
     `GET /api/app/update` asks GitHub's `releases/latest` at most every six
     hours, only while Settings → General → *Check for updates* is on (the
     default). The request carries nothing about the user or their projects.
   - A newer, non-draft, non-prerelease release whose page is on the
     allow-list shows a banner with a Download button.
   - There is still **no auto-install** (ADR-0033): the installers are
     unsigned, and installing unsigned code by itself would make the update
     channel an attack path. The user downloads and runs the installer.
   - `POST /api/app/open_url` opens the default browser, because the window
     never navigates away from the app. It only opens the Releases page and
     Ollama's download page. Other URLs are refused (403). The URL is
     restricted to plain characters, with no `%` or `..`, and is passed as
     one argument with no shell (`rundll32 url.dll,FileProtocolHandler`,
     `open`, `xdg-open`).

3. **The checker runs the project's tests.**
   - Each project can have a test command, saved in `app.json` (`tests`)
     and set in Settings → Projects or from a one-time prompt.
   - Anchor suggests one from the project's files: `cargo test`,
     `npm test`, `go test ./...`, or Python's pytest or unittest. It never
     runs a suggestion the user has not confirmed.
   - Runs reuse the verify gate's runner, factored out as
     `verify::run_command`:
     - sandboxed, with no network;
     - an exact environment and an empty read-only `HOME`;
     - the gate's read-only view;
     - on a scratch **export** of a snapshot, never the live project.
   - Builder + checker runs the tests on the starting snapshot (the
     baseline) and after each round. The run goes into the review input
     (`commands`, plus parsed `tests` for unittest).
   - A change is approved only when the reviewer supports every claim
     **and** the tests hold. "Hold" means they pass, or they failed at the
     start and nothing fails now that did not fail then.
   - Otherwise the builder's next instruction includes the end of the test
     output (at most 4 KiB). Audit: `pair_tests`, with `tests_ok` on
     `pair_review`.
   - One AI and Two builders run the tests once afterwards, for
     information only. Two builders do not use the verify gate: its rules
     reject claims that cite no tests, which app tasks rarely do.

4. **Follow-up messages.**
   - When a finished task is open, the task box replies to it
     (`POST /api/app/task {follow_up: true}`).
   - The builder's conversation resumes from its last checkpoint (one is
     saved at every turn), with the reply as the next user message.
     `Task.continue_with` is a new optional protocol field, so protocol v3
     is unchanged for older peers.
   - The reply reuses the same models: the plan is kept in memory only,
     because its environment holds keys. After a restart the reply is
     routed again.
   - When the checkpoint is gone, the reply starts a fresh session whose
     instruction summarizes what was asked and answered. The last task is
     kept in `app.json` with no secrets.
   - The checker reviews the reply's own change. A reply to a two-builder
     task runs as builder + checker.
   - Audit: `follow_up_task {text, of, resumed}`. The page shows the reply
     in the same thread, and the history keeps one entry per task.

## Consequences

- No new dependencies: ureq, std and existing crates.
- The update check is the app's first request to a server not chosen per
  task. It is opt-out, rate-limited and documented on the settings page.
- Test commands are the user's own and confirmed, but they run on every
  round. A slow suite slows each round; the time limit defaults to
  300 s (1-1800).
