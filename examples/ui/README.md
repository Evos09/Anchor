# A two-model collaboration, watched in the dashboard

A scripted team: no API keys, deterministic. It exercises every panel of
the dashboard (spec §12.2, ADR-0025).

```sh
npm --prefix ui ci --ignore-scripts && npm --prefix ui run build
cargo build
cp -r examples/ui/workspace /tmp/ui-demo
./target/debug/anchor team run --workspace /tmp/ui-demo --team examples/ui/team.toml --ui
```

Open the URL it prints. Answer the approval gates as they appear (left
column). When the run ends, the dashboard stays open until you press
Ctrl-C. Restore from the snapshot timeline is enabled from that point.

## What happens

| Who | Family | What it does | Where you see it |
|---|---|---|---|
| alpha | A (`demo-alpha-a`, $3 / $15 per Mtok) | Claims `calc.py` and `tests/`, and proposes a decision ("clamp() returns the nearest bound"). Then it writes `clamp()` with a bug (the high bound returns `lo`) and tests | roster, claim map, decision log |
| verification | — | Build, tests and coverage in the sandbox: `test_high` fails, so merge #1 is **rejected** | story (red), merge queue (diff and evidence side by side) |
| alpha, attempt 2 | A | Gets the structured feedback, fixes the bug, and asks beta for edge-case tests | story ("another attempt"), message stream |
| reviewer | C (`demo-reviewer-c`, $0.50 / $2) | Supports alpha's claim; merge #2 is applied after your approval | merge queue, cost meter (reviewer row) |
| beta | B (`demo-beta-b`, $1 / $5) | Tries to claim `calc.py` (**refused**: outside its area), writes `tests/test_edges.py`, and reports back. Its messages are held for your approval | claim map (red), message stream (held) |
| reviewer | C | Supports beta's claim; merge #3 is applied | merge queue |

The cost meter should show these totals, computed exactly from the token
counts in the scripts and the prices in the roles files:

| Who | Tokens in / out | Cost |
|---|---|---|
| alpha | 9,000 / 680 | $0.0372 |
| beta | 4,200 / 240 | $0.0054 |
| reviewer | 2,400 / 100 | $0.0014 |
| **Total** | | **$0.0440** |

Agent ids assume a fresh Hub, so each run needs a fresh copy of
`workspace/`.
