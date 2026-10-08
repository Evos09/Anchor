# Anchor Terms of Service — DRAFT

> **DRAFT — not reviewed by a lawyer.** This text was written by the
> developers to describe how Anchor works. It is not a legal agreement, must
> not be published or shown to users as one, and must be drafted or reviewed
> by a qualified lawyer first (ADR-0034).

## 1. What Anchor is

Anchor is open-source software (Apache-2.0) that runs a coding agent on
your computer. It asks before changing files or running commands, keeps a
snapshot of every turn so it can be undone, and keeps a tamper-evident
audit log.

## 2. Three ways to run models

1. **Local models** (Ollama): run entirely on your computer.
2. **Your own API key:** requests go from your computer straight to the
   provider you chose, under that provider's terms and billing. Anchor never
   receives your key.
3. **The free tier** (when offered): requests go through Anchor's proxy to a
   provider Anchor has an account with. It is capped (5-hour, weekly and
   monthly limits) and offered with no account and no guarantee of
   availability.

## 3. Using the free tier

- No signup: the app registers a random install identifier.
- Limits are enforced by the proxy and may change.
- Not allowed: getting around the limits (for example by scripting new
  installs), reselling or sharing access, using it to build a competing
  service, or any use the underlying provider's or model's acceptable-use
  policy forbids. [Lawyer: flow-down of each provider's and model license's
  acceptable-use terms.]
- We may suspend the free tier, or an install's access, at any time.

## 4. Your code and your responsibility

You choose which folders Anchor opens and approve its actions. You are
responsible for what you run and for having the right to send code to the
model you choose. [Lawyer: warranties disclaimer, limitation of liability,
indemnity, governing law, age limits (some providers forbid users under
18), export controls, changes to these terms.]

## 5. Contact

[Lawyer / operator: legal entity, address, contact.]
