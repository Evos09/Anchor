# ADR-0038: Pool sizing and fair share

- Status: Accepted (Update #1, 2026-10-08)
- Date: 2026-10-08

## Decision

The pool's daily spend is capped (`pool.daily_cap`, weighted tokens), set
from the sum of the cleared providers' documented daily limits, minus
headroom. Once 80% of it is used (`saturation`), each request is held to
a **fair share**: `daily_cap / users active in the last 15 minutes`.
Pool refusals never touch a user's own caps. The values are configuration
in `proxy.toml` and are revisited with real traffic; with no provider
cleared today, there is nothing to size yet.
