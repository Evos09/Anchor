# Provider: OVHcloud AI Endpoints (anonymous access)

- **Verdict:** AMBIGUOUS
- **Terms verified:** 2026-10-08
- **Free pool (path A):** not used

## Why

The governing conditions were not read, and a per-IP anonymous limit cannot carry a server-side pool (spreading over IPs would be circumvention). Ambiguous counts as EXCLUDE.

## Sources

- Capabilities and limits: https://docs.ovhcloud.com/en/guides/public-cloud/ai-machine-learning/ai-endpoints-capabilities [SE]
- Binding terms: Public Cloud Special Conditions, Appendix 10 (not found or read)

## What the terms say

- **Limits:** anonymous use is 2 requests per minute per IP address and per model [SE].
- **Data:** "Your data is never used to train our models" [SE, marketing page, not contractual].

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
