# ADR-0015: Policy file format (§5.2(d))

- Status: Accepted
- Date: 2026-10-06

TOML (`toml` crate). Reasons: comments, no implicit typing surprises
(unlike YAML's `no`/`on`), arrays of tables map directly to ordered rules,
and the Rust ecosystem's parser is strict. The schema is closed
(`deny_unknown_fields`), so a typo such as `targt_within` is an error, not a
silently ignored condition. Versioned with `version = 1`.

See `config/policy.default.toml` and ADR-0011 for semantics.
