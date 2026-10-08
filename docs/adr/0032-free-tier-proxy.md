# ADR-0032: The free-tier proxy and pool manager

- Status: Accepted (Update #1, 2026-10-08)
- Date: 2026-10-08

## Decision

`crates/anchor-proxy` (open source, binary `anchor-proxy`) is an
OpenAI-compatible endpoint (`/v1/chat/completions`, `/v1/models`) plus
`/v1/anchor/register` and `/v1/anchor/usage`. It listens on loopback
behind a TLS-terminating reverse proxy, checks `Host`, and refuses any
`Origin`.

**Counting** (§4): estimate input from the request bytes (ADR-0036),
reserve input + `max_tokens` against the account's caps and the pool,
forward with `max_tokens` set, always stream from the provider with
`stream_options.include_usage`, count output as it arrives, cut the
stream when output passes the reservation by the margin (20%), settle on
the provider's reported usage (or the count), and return one assembled
completion. Client streaming is refused.

**Privacy claim** (published; Update #1 §4): *"The proxy reads each
request in memory to estimate token usage before forwarding. It does not
write request or response content to disk, does not log it, does not
include it in error reports, and does not retain it after the request
completes. The only persisted record is metadata: user ID, timestamp,
provider, model, input tokens, output tokens, HTTP status, latency."* The
`log` table has exactly those columns (plus the path); the store's
methods take no content; a test scans the database files for a request's
text.

**Pool** (`src/pool.rs`): **exactly one account per provider** (a second
account to stretch a free tier breaks most terms; config validation
refuses it). Each key tracks four sliding windows (RPM, RPD, TPM, TPD);
the least-loaded usable key is chosen. Cooldowns: 429 → `Retry-After` or
10 s; quota exhausted → 1 h; 5xx → 5 s doubling; 401/403 → disabled.
Failing over between providers serving the same weights (`same_as`).
A pool-wide daily cap, and once the pool is 80% used, a fair share of
`daily_cap / active users`. When the pool is exhausted the user gets the
conversion prompt: add your own key, run locally, or wait.

**Terms gate** (§5.7): an account is accepted only if its provider ADR
(`docs/adr/providers/`) says `Verdict: INCLUDE` and `Terms verified:
YYYY-MM-DD`. As of 2026-10-08 **no provider passes**
(`docs/adr/providers/README.md`), so the shipped example config has no
accounts and the app ships with the free tier off. A test keeps the
shipped ADRs and the gate in step.

## Consequences

Deploying the proxy (host, TLS, database backups) and registering
provider accounts are operator tasks outside this repository.
