# Provider: Google Gemini API (unpaid quota)

- **Verdict:** EXCLUDE
- **Terms verified:** 2026-10-08
- **Free pool (path A):** not used

## Why

Unpaid prompts are used for training and read by human reviewers, which breaks Anchor's no-training promise; EEA/CH/UK users require Paid Services. (Gemini remains available as a BYOK provider, path B, under the user's own paid terms.)

## Sources

- Gemini API Additional Terms: https://ai.google.dev/gemini-api/terms [SE]

## What the terms say

- **Data:** "Google uses the content you submit to the Services and any generated responses to provide, improve, and develop Google products and services and machine learning technologies" [SE]; "Human reviewers may read, annotate, and process your API input and output" [SE] (unpaid Services).
- **Region:** "you may use only Paid Services when making API Clients available to users in the European Economic Area, Switzerland, or the United Kingdom" [SE].
- **Aggregation:** no explicit ban found; API Clients are allowed [SE].

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
