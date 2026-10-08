# ADR-0033: Code signing, notarization and the updater

- Status: Accepted (Update #1, 2026-10-08)
- Date: 2026-10-08

## Context

The installers are unsigned, so Windows SmartScreen and macOS Gatekeeper
warn. Signing needs paid credentials only the project owner can obtain.

## Decision

The CI desktop job signs **when the secrets exist** and skips otherwise,
then verifies what it signed:

- **macOS:** a Developer ID Application certificate (Apple Developer
  Program, $99/year) and notarization. Secrets: `APPLE_CERTIFICATE`,
  `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`,
  `KEYCHAIN_PASSWORD`, and either `APPLE_API_ISSUER` / `APPLE_API_KEY` /
  `APPLE_API_KEY_P8` or `APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID`.
  Verified with `codesign --verify --deep --strict`, `spctl --assess
  --type execute -vv` and `xcrun stapler validate`.
- **Windows:** Azure Artifact Signing (formerly Trusted Signing; about
  $9.99/month, Basic; individuals only in the US and Canada at the time
  of writing) through `bundle.windows.signCommand`, or an OV/EV
  certificate on a hardware token (about $129–685/year; validity capped
  at 460 days from 2026-03-01). Secrets: `AZURE_CLIENT_ID`,
  `AZURE_CLIENT_SECRET`, `AZURE_TENANT_ID`, `AZURE_SIGNING_ENDPOINT`,
  `AZURE_SIGNING_ACCOUNT`, `AZURE_SIGNING_PROFILE`. Verified with
  `Get-AuthenticodeSignature`. SmartScreen reputation is now per file
  hash; an EV certificate no longer skips the warning.

Prices and eligibility are from search results on 2026-10-08 and must be
re-checked when buying.

**Updater:** Anchor does not use `tauri-plugin-updater`. If it is added:
version ≥ 2.12.0 with `requireSignedVersion: true` (GHSA-p8c6-29q6-cqw7:
an unsigned manifest version allowed downgrades) and a minisign key kept
offline.

## Consequences

Until certificates are bought and added as repository secrets, the
installers stay unsigned and the About page says so.
