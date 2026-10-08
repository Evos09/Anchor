# Provider: Groq (GroqCloud Free plan)

- **Verdict:** AMBIGUOUS
- **Terms verified:** 2026-10-08
- **Free pool (path A):** not used

## Why

The "functions substantially the same as the APIs" sublicensing ban, and no clause permitting commercial use of the Free plan. Plausible with Groq's written approval: ask. Ambiguous counts as EXCLUDE.

## Sources

- Services Agreement: https://console.groq.com/docs/legal/services-agreement [SE]
- Terms of Sale: https://console.groq.com/docs/terms-of-sale [SE]
- Your data: https://console.groq.com/docs/your-data [SE]
- Rate limits: https://console.groq.com/docs/rate-limits [SE]

## What the terms say

- **Aggregation:** a "Customer Application" may serve "End Users ... who are authorized by Customer to access or use the Cloud Services and AI Model Services through that application" [SE]; but "Customer may not sell, resell, sublicense, transfer, or distribute any of the Cloud Services except as expressly approved by Groq" [SE], and the Terms of Sale bar sublicensing an API for third-party use "in a way that functions substantially the same as the APIs" [SE]. A pass-through proxy arguably does.
- **Commercial use of the Free plan:** no clause found either way.
- **Data:** "By default, Groq does not retain customer data for inference requests" [SE]; no training on customer data [SE].
- **Limits (example):** llama-3.3-70b-versatile on Free: 30 RPM / 1K RPD / 12K TPM / 100K TPD [SE; column mapping inferred]; per organization.

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
