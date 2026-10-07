# The desktop app, scripted

A demo of `anchor app` (the desktop app's backend, ADR-0028) with a scripted
model, so it needs no API key and no local model server:

```sh
cp -r examples/app/workspace /tmp/app-demo
anchor app --roles examples/app/roles.mock.toml --mock-script examples/app/script.json
```

Open the printed URL, choose **Open folder**, pick `/tmp/app-demo`, and send
any task. The scripted agent reads `hello.py`, then asks to edit it and to
write a `README.md`. Approve or deny each, then try **Undo** on the turn.
Every task replays the same script. The browser test
(`ui/e2e/app.spec.ts`) and `crates/anchor-backend/tests/app.rs` drive this
same demo.
