# ADR-0037: The free tier's device identifier

- Status: Accepted (Update #1, 2026-10-08)
- Date: 2026-10-08

## Decision

Registering needs no signup. The app generates a **random 32-hex-digit
install id** (OS randomness), stored in `app.json`; it describes nothing
about the machine (no hardware ids, MAC, hostname or user name). The
proxy stores only its SHA-256, and issues a random access token (stored
by the app in the keychain, by the proxy as a SHA-256). Registering again
from the same install rotates the token and keeps the same account and
caps, so reinstalling the app does not reset limits unless `app.json` is
deleted. Registration is throttled globally in memory; IP addresses are
not stored.

## Consequences

A determined user can create new installs to get new caps; the pool-wide
cap and fair share (ADR-0038) bound the damage. Stronger identity would
need an account, which onboarding deliberately avoids.
