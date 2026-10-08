# Provider: NVIDIA API Catalog (build.nvidia.com)

- **Verdict:** EXCLUDE
- **Terms verified:** 2026-10-08
- **Free pool (path A):** not used

## Why

An evaluation trial, not for production use; serving a product's free users is production use.

## Sources

- NVIDIA API Trial Terms of Service (PDF): https://assets.ngc.nvidia.com/products/api-catalog/legal/NVIDIA%20API%20Trial%20Terms%20of%20Service.pdf [SE; download blocked]
- Forum: https://forums.developer.nvidia.com/t/clarification-on-trial-api-use/334275 [SE]

## What the terms say

- **Nature:** "this trial service is governed by the NVIDIA API Trial Terms of Service" [SE]; "for trial purposes only and not for production use" [SE, via a summary].
- **Limits:** about 40 RPM (forum reports; no official page found).

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
