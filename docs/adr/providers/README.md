# Provider terms ADRs (Update #1 §5.7)

One file per provider considered for Anchor's free tier (path A). Each
records whether the provider's terms let a vendor pool free-tier capacity
for its users and use it commercially, and when that was checked.
`anchor-proxy` will only run an account whose file says
`Verdict: INCLUDE` with a `Terms verified:` date.

**As of 2026-10-08 no provider is INCLUDE**, so the free tier is not
offered. The two worth asking in writing are Cloudflare (§2.2(a)) and
Groq (sublicensing clause).

| Provider | Verdict |
|---|---|
| [Cloudflare Workers AI](cloudflare.md) | AMBIGUOUS → excluded |
| [Groq](groq.md) | AMBIGUOUS → excluded |
| [OVHcloud AI Endpoints](ovhcloud.md) | AMBIGUOUS → excluded |
| [SambaNova](sambanova.md) | AMBIGUOUS → excluded |
| [OpenRouter](openrouter.md) | EXCLUDE |
| [Cerebras](cerebras.md) | EXCLUDE |
| [NVIDIA API Catalog](nvidia.md) | EXCLUDE |
| [Gemini API (unpaid)](gemini.md) | EXCLUDE |
| [Mistral Experiment](mistral.md) | EXCLUDE |
| [Cohere trial](cohere.md) | EXCLUDE |
| [GitHub Models](github-models.md) | EXCLUDE |
