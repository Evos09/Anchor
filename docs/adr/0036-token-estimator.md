# ADR-0036: Estimating tokens before forwarding

- Status: Accepted (Update #1, 2026-10-08)
- Date: 2026-10-08

## Decision

The proxy estimates a request's input as **bytes / 4**, rounded up, over
the JSON of `messages` and `tools` (`crates/anchor-inference/src/estimate.rs`),
and counts streamed output the same way. No tokenizer is shipped: models
on the pool use different tokenizers, and a byte heuristic is
model-independent, cheap, and conservative for code and English. The
estimate only sizes the **reservation**; the charge is the provider's
reported `usage` when it sends one, else the count. The cut-off margin
(20%) absorbs the heuristic's error on output.
