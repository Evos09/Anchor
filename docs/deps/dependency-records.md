# Dependency records (spec §0)

Check date: **2026-10-06**. Sources and how each step was performed:

| Step | Method used here |
|---|---|
| 1 Existence | crates.io API (`/api/v1/crates/<name>`); exact name and repository field recorded. Typosquat check: names are the canonical crates referenced by their upstream projects' own docs and by the Rust toolchain ecosystem. |
| 2 Maintenance | crates.io version history: releases in the 90 days before the check date. **GitHub was not reachable** from the build environment (egress policy), so repository commit and issue activity is *not* recorded. It must be filled in before v1.0. |
| 3 License | License files from the exact published crate sources were read. MIT and Apache-2.0 texts were compared against the standard texts (no added clauses). Non-standard files (ring, webpki-roots, globset/ignore) were read in full; see notes. Enforced by `deny.toml` `[licenses]`. |
| 4 Fitness | Our own tests (listed per crate). No README or benchmark claims relied on. |
| 5 Security | `cargo deny check advisories` against the RustSec advisory DB at commit `ef6173cb` (2026-10-03): **no advisories** for the resolved tree (103 crates). OSV and NVD were **not reachable**; re-check before release. `cargo deny check` runs in CI on every push. |

## Direct dependencies

| Crate | Version | Releases in last 90 days | License (read) | Used for | Fitness evidence |
|---|---|---|---|---|---|
| serde / serde_json | 1.0.229 / 1.0.151 | 1 / 1 | MIT OR Apache-2.0 (standard) | protocol, audit, checkpoints (serde_json with `float_roundtrip`, so audit hashes verify; ADR-0016) | `anchor-protocol` tests (unknown fields, versions) |
| toml | 1.1.6 | 4 | MIT OR Apache-2.0 (standard) | policy, roles, workspace config | `policy_rejects_unknown_keys_and_bad_rules` |
| thiserror | 2.0.21 | 3 | MIT OR Apache-2.0 (standard) | error types | compile-time only |
| sha2 | 0.11.0 | 0 (last 2026-03-25) | MIT OR Apache-2.0 (standard) | content hashes, audit chain, snapshot object ids | `audit_chain_detects_tampering`, append verification, `poisoned_store_is_refused` |
| regex | 1.13.1 | 2 | MIT OR Apache-2.0 (standard) | grep parameter (linear time) | `regex_redos_patterns_are_linear`, `regex_size_limit_enforced` |
| globset / ignore | 0.4.20 / 0.4.33 | 2 / 6 | Unlicense OR MIT (standard; COPYING states dual license) | policy globs, workspace walk | policy glob tests, e2e grep/glob |
| rustix | 1.1.5 | 1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT (standard + LLVM exception) | openat2, F_GETPATH, renameat2, Dir, symlinkat, fchmod; snapshot store flock and syncfs | 31 `anchor-fs` tests, 22 `anchor-snapshot` tests |
| libc | 0.2.190 | 6 | MIT OR Apache-2.0 (standard) | setrlimit, unshare, prctl, killpg, landlock syscall | 11 `anchor-exec` tests |
| landlock | 0.4.7 | 2 | MIT OR Apache-2.0 (standard) | runtime self-sandbox, command confinement | `filesystem_confinement_enforced`, `runtime_process_cannot_touch_the_filesystem` |
| ureq | 3.4.2 | 3 | MIT OR Apache-2.0 (standard) | OpenAI-compatible HTTP client | parsing tests only (no live endpoint reachable) |
| unicode-normalization | 0.1.25 | 0 (last 2025-10-30) | MIT OR Apache-2.0 (standard) | macOS name equivalence | macOS-only test (CI) |
| windows-sys | 0.61.2 | 0 (last 2025-10-06) | MIT OR Apache-2.0 | Windows file APIs | type-checked only |
| tempfile (dev) | 3.27.0 | 0 (last 2026-03-11) | MIT OR Apache-2.0 (standard) | tests | — |
| rusqlite (`bundled`) | 0.40.2 | 3 (0.40.0 2026-05-26, 0.40.1 2026-06-06, 0.40.2 2026-08-08) | MIT (standard; read) | Hub database (spec §6.3, ADR-0023) | 13 `anchor-hub` tests (claims race, tamper detection, persistence) |
| libsqlite3-sys | 0.38.2 | 3 (same cadence as rusqlite) | MIT (standard; same text as rusqlite) | compiles the bundled SQLite **3.53.2** amalgamation (`sqlite3/sqlite3.c`; SQLite itself is public domain, per its header) | as above |
| getrandom | 0.4.3 | 1 (2026-06-17) | MIT OR Apache-2.0 (standard) | Hub bearer tokens (32 bytes from the OS) | `only_the_user_registers_agents_and_tokens_authenticate` |

### Maintenance flags

`sha2`, `unicode-normalization`, `windows-sys` and `tempfile` had no
release in the 90 days before the check. Release cadence alone does not
show abandonment: these are mature crates with small surfaces. Repository
activity must still be confirmed once GitHub is reachable (step 2 is
incomplete for every crate, as noted above). Fallbacks if any proves
unmaintained:

- `sha2`: `ring::digest` (already in the tree via rustls).
- `unicode-normalization`: an in-tree NFC table is not attractive; prefer the
  ICU4X normaliser.
- `windows-sys`: `windows` (same upstream).

### Notable transitive dependencies

- **rusqlite's tree** (Phase 4): `hashlink` (MIT OR Apache-2.0),
  `hashbrown`, `foldhash` (Zlib), `fallible-iterator` and
  `fallible-streaming-iterator` (MIT/Apache-2.0), `smallvec`, `bitflags`,
  and the build-time `cc`, `pkg-config`, `vcpkg`, `find-msvc-tools`,
  `shlex`. All permissive. The SQLite C code is compiled from the
  amalgamation vendored in `libsqlite3-sys` (no system library is linked),
  so the SQLite version is pinned by the crate version. `anchor-hub` is
  therefore excluded from CI's cross-target `cargo check` (no C
  cross-compiler); the native macOS and Windows jobs build and test it.

- **ring 0.17.14** (via rustls/ureq). `LICENSE` says new code is ISC
  (`LICENSE-other-bits`, read in full: standard ISC with Brian Smith's
  copyright), code from BoringSSL is under Apache-2.0 (`LICENSE-BoringSSL`),
  and the vendored once_cell polyfill is MIT/Apache-2.0. All permissive and
  compatible with an Apache-2.0 open core and a commercial shell.
- **webpki-roots 1.0.9**: CDLA-Permissive-2.0 (a data license; read in
  full). It permits use, modification and sharing of the data, with no
  copyleft on results.
- **rustls / rustls-webpki / untrusted**: Apache-2.0, ISC and MIT.

## Rejected or not adopted

| Candidate | Reason |
|---|---|
| `hex` 0.4.3 | Last release 2021-03-03 (fails maintenance). Hex encoding is twelve lines in-tree (`anchor_policy::audit::hex`). |
| `reqwest` | See ADR-0017. |
| `nix` | rustix covers the surface with safe APIs (ADR-0013). |
| `clap` | The CLI is a thin harness; hand-rolled parsing avoids a large tree. Revisit for the shipped CLI. |
| `fastcdc` 5.0.0 | Evaluated for Phase 3 (ADR-0006). (1) Exists: crates.io `fastcdc`, repository `github.com/nlfiedler/fastcdc-rs`. (2) Maintained: 5.0.0 on 2026-08-22, 4.0.1 on 2026-04-26. (3) MIT. **Not adopted**: chunk boundaries are part of our on-disk format, and the algorithm (gear hash plus normalized masks) is about 150 lines that our own tests cover fully (`chunker_*`). Owning it removes a format-defining dependency. |
| `zstd` (compression) | Not needed in Phase 3. A future candidate for compressing chunks; it would need a five-step record (and it binds a C library). |
| `rmcp` 3.5.1 | Evaluated for Phase 4 (ADR-0004). (1) Exists: crates.io `rmcp`, published 2026-10-05; the repository `modelcontextprotocol/rust-sdk` is under the MCP organisation and its README calls it the official Rust SDK. (2) Maintained (published the day before the check). (3) Apache-2.0/MIT. Supports MCP 2026-07-28. **Not adopted** (the user's decision, 2026-10-06): it requires tokio, which would make the trusted backend async and add ~15 crates to the audit surface; the stateless 2026-07-28 server subset the Hub needs is small enough to own (`anchor-hub/src/mcp.rs`). |
| third-party A2A crates (`a2a`, `a2a-rs-core`, ...) | No official Rust SDK found; A2A 1.0 (JSON-RPC binding) is implemented from the specification (ADR-0004). |
| `tauri`, editor packages | Phase 6 candidates. Existence only recorded (ADRs 0002, 0008); five-step records pending. |
