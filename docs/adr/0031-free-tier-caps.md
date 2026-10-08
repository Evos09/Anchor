# ADR-0031: Free-tier caps: 5-hour window, week, month

- Status: Accepted (Update #1, 2026-10-08)
- Date: 2026-10-08

## Decision

Three caps per free account, counted in **weighted tokens** (input +
output, × the model's `free_tier_weight`), all server-side in
`anchor-proxy` (`src/caps.rs`):

| Cap | Budget | Resets |
|---|---|---|
| Rolling window | 50,000 | 5 hours after the window's first request |
| Weekly | 250,000 | every 7 days from account creation, no carry-over |
| Monthly | 1,000,000 | 00:00 UTC on the 1st |

A request is refused before it is forwarded when the account cannot afford
its input plus the minimum wrap-up output; the refusal names the cap and
its reset time. A request already admitted may finish (the **wrap-up
allowance**, at least 2,048 output tokens); what it uses beyond the
window is charged to the **next** window, so it cannot be used to get
more. Time comes only from the server's clock; client headers are
ignored (tested).

The app shows the three meters (Settings → Models → Free tier).

## Consequences

The numbers are configuration (`[caps]` in `proxy.toml`), to be revisited
against real pool capacity (ADR-0038).
