# ADR-0017: HTTP client for provider adapters (§3.7)

- Status: Accepted
- Date: 2026-10-06

`ureq` 3.x (blocking, rustls, no async runtime), chosen over `reqwest`:

- The runtime loop is sequential (one model call at a time per session), so
  async brings no benefit and pulls in tokio.
- rustls with bundled webpki roots means the Landlock-confined runtime needs
  no filesystem access to a CA store. The policy in `deny.toml` bans
  `openssl-sys`.
- A smaller transitive tree to audit (see `cargo deny list`).

Streaming responses (server-sent events) are not implemented yet.
Revisit if token-streaming UX (Phase 6) needs it.
