# ADR-0030: The model registry and license gating

- Status: Accepted (Update #1, 2026-10-08)
- Date: 2026-10-08

## Context

The free tier must never serve a model whose license forbids commercial
use or gates it on revenue or users; the same family ships under
different licenses at different sizes (Qwen2.5-Coder 3B vs 7B).

## Decision

`config/models.toml` (bundled into the app as
`crates/anchor-backend/assets/models.toml`, kept identical by a test) lists
providers (`id`, `base_url`, `paths`, `family` for path-B providers,
`key_url`, `terms_url`) and models (`license_id`,
`license_commercial_ok`, `license_revenue_gate`, `license_notes`,
`free_tier_weight`, `free_tier_eligible`, `available_on_paths`,
`verified_at`, `verified_by`). `Registry::validate` refuses to load if:

- a free-tier model lacks commercial use, has a revenue gate, has no
  weight, or is not on path A; or a path-A model is not eligible;
- entries naming the same weights (`same_as`) disagree on family or
  license; a provider is unknown; a date is not a date; a BYOK provider
  has no family.

Selection reads only these fields. A test scans the registry and router
sources for name matching (`contains("`, `starts_with("`, regexes) and
fails if any appears.

Licences were recorded on 2026-10-08 from Ollama library pages as quoted
in search results; `verified_by` says so, and each must be confirmed with
`ollama show <tag> --license` before the model is relied on. Non-commercial
(`qwen2.5-coder:3b`, `codestral:22b`) and gated (`llama3.1:8b`) models are
listed for local use with their terms shown, and are never free-tier.

BYOK providers list no models: after a key is added, the app lists the
provider's own models and the user picks one; its family is the
provider's (`openai`, `mistral`, `deepseek`, `gemini`). Prices of models
the registry does not list are unknown and shown as $0 with that caveat.

## Consequences

No model is on path A today, because no provider passes ADR-0032's gate.
