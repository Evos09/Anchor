# Provider: Cloudflare Workers AI (Workers Free allocation)

- **Verdict:** AMBIGUOUS
- **Terms verified:** 2026-10-08
- **Free pool (path A):** not used

## Why

The no-training commitment is verified, but §2.2(a) ("loan ... access to the Services to any third party") is unresolved for a pooled proxy, and the allocation is tiny. Ask Cloudflare in writing. Ambiguous counts as EXCLUDE.

## Sources

- Pricing, limits, data usage: the source files of https://developers.cloudflare.com/workers-ai/platform/pricing/, …/limits/ and …/data-usage/ in `cloudflare/cloudflare-docs` (branch `production`) [verbatim, retrieved 2026-10-08]
- Self-Serve Subscription Agreement: https://www.cloudflare.com/terms/ [SE]

## What the terms say

- **Free tier:** "Our free allocation allows anyone to use a total of 10,000 Neurons per day at no charge." [verbatim]
- **Data:** "Cloudflare does not use your Customer Content to (1) train any AI models made available on Workers AI or (2) improve any Cloudflare or third-party services, and would not do so unless we received your explicit consent." [verbatim]
- **Aggregation:** unless permitted in writing, a customer may not "(a) rent, lease, loan, export, or sell access to the Services to any third party, or sign up for the Services on behalf of a third party" (§2.2) [SE]; the licence is "non-transferable, and non-sublicensable (except to Affiliates)" (§2.1) [SE].
- **Limits:** Text Generation "300 requests per minute, unless the model requires the Workers Paid plan"; "All limits reset daily at 00:00 UTC." [verbatim]
- **Capacity:** at the published neuron rates for llama-3.3-70b-fp8-fast, 10,000 neurons/day is about 375K input or 49K output tokens per day for the whole pool.

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
