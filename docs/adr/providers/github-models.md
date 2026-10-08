# Provider: GitHub Models (free usage)

- **Verdict:** EXCLUDE
- **Terms verified:** 2026-10-08
- **Free pool (path A):** not used

## Why

Framed as experimentation, possibly under non-production pre-release terms.

## Sources

- https://docs.github.com/en/github-models/use-github-models/prototyping-with-ai-models [SE]
- GitHub Pre-release License Terms, from `github/docs` (branch `main`) [verbatim]

## What the terms say

- Free limits "are intended to help you get started with experimentation" [SE].
- Pre-release terms: "GitHub grants you a limited right to use a non-production instance of the software for evaluation and testing ... may not use the pre-release software in any active production environment, including any use to process live customer data" [verbatim] (whether these still govern GitHub Models is unconfirmed).

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
