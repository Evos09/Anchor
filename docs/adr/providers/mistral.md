# Provider: Mistral AI (Experiment free plan)

- **Verdict:** EXCLUDE
- **Terms verified:** 2026-10-08
- **Free pool (path A):** not used

## Why

Trains on free-plan data by default, and bars making API keys available to third parties. (Mistral remains available as a BYOK provider, path B.)

## Sources

- Commercial Terms: https://legal.mistral.ai/terms/commercial-terms-of-service [SE]
- Consumer terms: https://legal.mistral.ai/terms/row-consumer-terms [SE]
- Help center (training): https://help.mistral.ai/en/articles/347614-can-i-opt-out-of-my-input-or-output-data-being-used-for-training [SE]

## What the terms say

- **Aggregation:** "You may not make your account credentials (including any API keys) available to third parties, ... or resell or lease access to your account or API keys" [SE]; multiple accounts "to bypass rate limits" are prohibited [SE].
- **Data:** the Experiment (free) plan's inputs and outputs may be used for training unless the user opts out [SE].

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
