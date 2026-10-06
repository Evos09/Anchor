# ADR-0008: Editor — CodeMirror 6 vs Monaco (§17 Q8)

- Status: Proposed — pending evaluation
- Date: 2026-10-06

## Observed (npm registry, 2026-10-06)

`@codemirror/view` 6.43.13 (MIT); `monaco-editor` 0.57.0 (MIT);
`@xterm/xterm` 6.0.0 (MIT).

## Evaluation plan

Measure, in each platform WebView (ADR-0002):

- bundle size (gzip);
- 50k-line file: open time, scroll FPS, keystroke latency;
- tree-sitter integration: incremental highlighting fed by a backend parse;
- LSP client integration: a backend-owned LSP process with the editor as a
  thin client.

Leaning towards CodeMirror 6 for its modular bundle and an extension model
that fits a backend-driven parse. The decision is deferred until the
numbers exist.
