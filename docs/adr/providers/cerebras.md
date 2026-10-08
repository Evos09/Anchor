# Provider: Cerebras Inference

- **Verdict:** EXCLUDE
- **Terms verified:** 2026-10-08
- **Free pool (path A):** not used

## Why

No standing free tier, only an expiring trial credit.

## Sources

- Terms: https://cerebras.ai/terms-of-service (not read)
- Pricing: https://cerebras.ai/pricing (not read; quoted in https://github.com/robhunter/agentdeals/issues/1910)

## What the terms say

- **Free tier:** the pricing page is quoted as "a Developer tier with a free $5 credit to start and no free tier" [SE]; the credit reportedly needs a verified payment method and expires after 30 days [SE].
- **Terms:** not read.

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
