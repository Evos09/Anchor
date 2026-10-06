# Verified team run (demo)

```sh
cargo build
W=$(mktemp -d) && cp -r examples/verify/workspace/. "$W"
export ANCHOR_STATE_DIR=$(mktemp -d)
./target/debug/anchor team run --workspace "$W" --team examples/verify/team.toml \
    --auto-approve --auto-merge
./target/debug/anchor hub artifacts --workspace "$W"
./target/debug/anchor hub verify --workspace "$W"
./target/debug/anchor audit verify --workspace "$W"
```

The scripted executor (`alpha.json`) makes `clamp` return `lo` above the
range. Verification runs the user's commands from `[verify]` on the merge
candidate in a sandbox: `test_high` fails, so the merge is rejected and the
agent gets the failing test, the command output and the diff. The repair
attempt (`alpha-fix.json`) fixes it; build, tests, coverage and the
reviewer pass, and it merges. Every command's output, the review input
and the decision are Hub evidence (`anchor hub artifacts`, `anchor hub
show PATH`).
