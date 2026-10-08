# Provider: OpenRouter (`:free` models)

- **Verdict:** EXCLUDE
- **Terms verified:** 2026-10-08
- **Free pool (path A):** not used

## Why

The resale / competing-service clause, and free endpoints that may train on or publish prompts.

## Sources

- Terms of Service: https://openrouter.ai/terms [SE] (an excerpt showed "Last updated 2023-08-09"; a change tracker reports the resale clause added 2026-07-10; both unverified)
- Limits: https://openrouter.ai/docs/api-reference/limits [SE]
- Provider logging: https://openrouter.ai/docs/guides/privacy/provider-logging [SE]
- Enterprise terms: https://openrouter.ai/terms-of-service-enterprise [SE]

## What the terms say

- **Aggregation:** Prohibited Conduct bars accessing the service "for purposes of reselling API access to Models or otherwise developing a competing service" [SE]; also "repeatedly creating accounts without paying for access" [SE]. Making the service available to end customers appears only in the Enterprise agreement, which bars "renting, reselling, or otherwise allowing third-party access" [SE].
- **Data:** free endpoints have separate settings, including ones that "may train on inputs" and "may publish prompts" (third-party wording); prompts are kept from training "wherever possible, but there are exceptions" [SE].
- **Limits:** 20 RPM on `:free` models; 50 requests/day under $10 of lifetime credit purchases, 1,000/day at or above; account-wide [SE].

## How this was checked

Read on 2026-10-08 from Anchor's build environment, where provider sites
could not be opened directly (WebFetch failed to resolve hosts; the
egress proxy refused CONNECT to provider domains). Quotes marked **[SE]**
are search-result excerpts of the named page, not exact page reads.
Quotes marked **[verbatim]** were read from the page's published source.
Before this provider is reconsidered, someone with normal web access must
re-read every source below and confirm each [SE] quote word for word.

## Gate (Update #1 §5.7)

A provider joins the free pool only with a verdict of INCLUDE: its terms
must permit (1) a vendor pooling free-tier capacity to serve its own
users, and (2) commercial use of the free tier, with no training on user
content. Ambiguous means exclude. `anchor-proxy` refuses to start with an
account whose ADR does not say `Verdict: INCLUDE`
(`crates/anchor-proxy/src/config.rs`).
