# ADR-0002: Desktop shell — Tauri vs Electron (§17 Q2)

- Status: Proposed — pending evaluation
- Date: 2026-10-06

## Context

The security model needs a native backend with kernel-level file APIs
(ADR-0013). Both candidate shells can host such a backend. They differ in
WebView vs bundled Chromium, binary size, and the IPC model.

Observed on 2026-10-06 (crates.io / npm registry APIs):
`tauri` 2.12.1 (published 2026-10-01, repository `tauri-apps/tauri`).
Electron was not evaluated.

## Decision (direction, not yet adopted)

Lean towards **Tauri 2** with the existing Rust backend crates linked in as
a library. The shell's only job is UI; every privileged operation stays
behind `anchor-backend`'s mediator. Tauri is **not** a dependency yet; per
the scope agreed for this push, the backend ships with a CLI harness first.

## Evaluation still required (must pass before acceptance)

On macOS (WKWebView), Windows (WebView2) and Linux (WebKitGTK):

1. Cold start time to an interactive window.
2. Editor rendering: open a 50k-line file, then measure scroll FPS and input
   latency (ties into ADR-0008).
3. Terminal fidelity with xterm.js: ANSI conformance and resize under load.
4. Native dialog quality, and child-process supervision from the backend
   (already covered by `anchor-exec`).
5. A five-step dependency record for `tauri` and its plugin set.

If WebKitGTK performance is insufficient on Linux, that finding goes back
into this ADR before any fallback is chosen.

## Update (Phase 6, 2026-10-06)

The user chose **web UI first, Tauri later** (ADR-0025). The dashboard
is a React bundle (`ui/dist`) served by the backend on loopback. A Tauri
shell will load the same bundle, with the backend linked in, instead of
going over HTTP. The evaluation above still gates Tauri's adoption.
`tauri` cannot be built in the development container (webkit2gtk is
missing), so it is still not a dependency.

## Alternatives

- *Electron*. It would need a native module for every primitive in
  ADR-0013, and its main process is Node with ambient filesystem authority.
  Kept as a fallback only.
