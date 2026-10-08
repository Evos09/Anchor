# Anchor Privacy Policy — DRAFT

> **DRAFT — not reviewed by a lawyer.** Written by the developers to describe
> what the software does with data. It must be drafted or reviewed by a
> qualified lawyer (including for GDPR / UK GDPR) before it is published or
> relied on (ADR-0034).

## Short version

- **No telemetry.** The app sends no usage data, analytics or crash reports.
- **Local models:** your code never leaves your computer.
- **Your own key:** your code goes from your computer straight to the
  provider you picked, under its privacy terms. Anchor never sees it or the
  key. Keys are kept in your operating system's keychain.
- **Free tier:** your request passes through Anchor's proxy.

## The free-tier proxy

The proxy reads each request in memory to estimate token usage before
forwarding. It does not write request or response content to disk, does
not log it, does not include it in error reports, and does not retain it
after the request completes. The only persisted record is metadata: user
ID, timestamp, provider, model, input tokens, output tokens, HTTP status,
latency.

The "user ID" is a random identifier created when the app first starts the
free tier. The proxy also stores a one-way hash of the app's random install
identifier and of its access token, and the counters for the three usage
limits. It does not store IP addresses, names, emails or device details.

The proxy forwards requests only to providers whose terms say they do not
train on the content. [Lawyer: providers as sub-processors, international
transfers, retention period for metadata, legal basis, data-subject rights
and how to exercise them, contact / DPO.]

## On your computer

The app stores, in its state folder: recent project paths, your settings
(no keys), each project's audit log and snapshots. Deleting the state
folder removes them. Keys are removed from the keychain when you remove
them in Settings.
