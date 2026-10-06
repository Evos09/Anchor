# ADR-0011: Policy rule conflict resolution (§17 Q11)

- Status: Accepted
- Date: 2026-10-06

## Decision

Evaluation is total and has four phases:

1. **Capability floor** (built in, not editable). It denies: any target
   inside `<workspace>/.anchor` for agent tools; any target inside the
   backend's state directory for every tool (ADR-0022); non-absolute
   targets or workspace roots; mutations without a target.
2. **Deny overrides**: if *any* `deny` rule matches, the request is denied,
   wherever that rule sits in the file.
3. **First match**: otherwise the first matching `allow`/`prompt` rule in
   file order wins.
4. **Default**: otherwise `[default].action`.

Within a rule, all `match` conditions must hold (AND). An empty list in a
condition is a parse error (it would otherwise match nothing, silently).
Unknown keys anywhere are a parse error. Rule names must be unique.

`target_matches` globs let `*` cross directory separators, so
`~/.ssh/*` also covers `~/.ssh/keys/id_rsa`. Matching is case-insensitive on
macOS and Windows.

## Alternatives rejected

- *Pure first match*: a broad early `allow` would silently disable a later
  credential `deny`. Auditing would then require reading the whole file in
  order.
- *Numeric priorities*: harder to predict from reading the file. Deny
  overrides gives the property users expect ("deny means deny").

## Verification

`deny_overrides_earlier_allow`, `first_match_wins_among_non_deny`,
`unmatched_request_falls_to_default`, `capability_floor_*`,
`policy_rejects_unknown_keys_and_bad_rules`.
