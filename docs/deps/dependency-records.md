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
| windows-sys | 0.61.2 | 0 (last 2025-10-06) | MIT OR Apache-2.0 | Windows file APIs (`anchor-fs`: handle-based resolution, pinned chains, `FileIdBothDirectoryInfo`); Windows command sandbox (`anchor-exec`, Phase 7, ADR-0026). No new crate: only features of the vetted version (see below) | `anchor-fs` and `anchor-exec` Windows tests, the validation suite (Windows CI) |
| tempfile (dev) | 3.27.0 | 0 (last 2026-03-11) | MIT OR Apache-2.0 (standard) | tests | — |
| rusqlite (`bundled`) | 0.40.2 | 3 (0.40.0 2026-05-26, 0.40.1 2026-06-06, 0.40.2 2026-08-08) | MIT (standard; read) | Hub database (spec §6.3, ADR-0023) | 13 `anchor-hub` tests (claims race, tamper detection, persistence) |
| libsqlite3-sys | 0.38.2 | 3 (same cadence as rusqlite) | MIT (standard; same text as rusqlite) | compiles the bundled SQLite **3.53.2** amalgamation (`sqlite3/sqlite3.c`; SQLite itself is public domain, per its header) | as above |
| getrandom | 0.4.3 | 1 (2026-06-17) | MIT OR Apache-2.0 (standard) | Hub bearer tokens (32 bytes from the OS) | `only_the_user_registers_agents_and_tokens_authenticate` |

### Phase 7: `windows-sys` features (2026-10-07)

No new crate was added in Phase 7. `anchor-exec` now enables these
features of the already-vetted `windows-sys` 0.61.2:

- `Win32_Foundation`
- `Win32_Security`
- `Win32_Security_Authorization`: ACL grants
- `Win32_Security_Isolation`: AppContainer profiles
- `Win32_Storage_FileSystem`
- `Win32_System_JobObjects`
- `Win32_System_Pipes`
- `Win32_System_Threading`: `CreateProcessW` and process attributes

Features select bindings from the same crate, which has the same license
and source. `cargo deny check` covers the tree unchanged.

`anchor-runtime` uses `Win32_Foundation`, `Win32_Security` and
`Win32_System_Threading` of the same crate, to read its own token
(`TokenIsAppContainer`) on Windows.

The new workspace crate `anchor-sandbox-validation` (`publish = false`)
uses only `libc`, plus `anchor-exec` and `tempfile` for tests, all
already vetted.

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

## Frontend (npm, Phase 6)

The dashboard (`ui/`, ADR-0025) is the project's first npm code. Checked
2026-10-06 against the npm registry (`npm view`, `npm audit`). The steps
were performed as follows:

1. **Existence:** registry entry, exact scoped name, and the upstream
   repository field. Typosquat check: these are the names the React,
   Vite, TypeScript and Playwright projects document.
2. **Maintenance:** each package was published within the last 7 days,
   and every one has 1 to 7 maintainers. GitHub activity was not
   reachable, as for the crates.
3. **License:** read from each published `package.json` and `LICENSE`.
4. **Fitness:** our own tests: `tests/ui.rs` (API) and
   `ui/e2e/dashboard.spec.ts` (the bundle in Chromium).
5. **Security:** `npm audit` reports 0 vulnerabilities across the 28
   installed packages. CI runs `npm audit --audit-level=high` on every
   push.

Installs always use `npm ci --ignore-scripts`: no package's lifecycle
script runs. Versions are pinned exactly (`--save-exact`) and the
lockfile is committed.

| Package | Version | Ships in the bundle? | License | Used for |
|---|---|---|---|---|
| react | 19.3.0 | yes | MIT | UI rendering (text only; no raw HTML) |
| react-dom | 19.3.0 | yes (brings `scheduler` 0.28.0, MIT) | MIT | DOM renderer |
| vite | 8.3.3 | no (build) | MIT | bundler (`rolldown` 1.2.12, `lightningcss` 1.33.0, `postcss`, `tinyglobby`, `picomatch`) |
| @vitejs/plugin-react | 6.1.2 | no (build) | MIT | JSX transform |
| typescript | 7.0.2 | no (build) | Apache-2.0 | type checking (`tsc --noEmit`, strict) |
| @types/react, @types/react-dom | 19.3.0 | no (types) | MIT | type definitions |
| @playwright/test | 1.63.0 | no (test) | Apache-2.0 | the e2e test (`playwright`, `playwright-core`) |

**Licenses across the whole installed tree:** MIT, Apache-2.0, ISC,
BSD-3-Clause and MPL-2.0. The MPL-2.0 package is `lightningcss`, used at
build time only; none of its code ships in the bundle.

**Native build binaries** (`@rolldown/binding-*`, `lightningcss-*`,
`@typescript/typescript-*`) are per-platform optional packages from the
same upstreams. Their install scripts are never run.

**Not adopted:** component libraries, chart libraries, state managers
and a router. The panels are hand-written CSS and SVG, which keeps the
shipped JavaScript to React. The browser's own `fetch` is the only HTTP
client.

## Desktop shell (Phase 9, ADR-0028)

Check date: **2026-10-08**. The shell (`desktop/`) is outside the main
workspace, with its own `Cargo.lock` and `desktop/deny.toml`; nothing in the
main tree depends on it. It holds no authority: no Tauri commands, plugins or
capabilities (ADR-0028).

| Crate | Version | Released | License (read) | Used for | Fitness evidence |
|---|---|---|---|---|---|
| tauri | 2.11.6 | 2026-09-19 (newest 2.x at least two weeks old; 2.12.0 and 2.12.1 are newer) | Apache-2.0 OR MIT (`LICENSE_APACHE-2.0`, `LICENSE_MIT`: standard) | the window and its webview (WebView2, WKWebView, WebKitGTK) | `desktop` tests; the packaged app's `--smoke` on each OS in CI; the window rendered the app under Xvfb (2026-10-08) |
| tauri-build | 2.6.3 | 2026-06-17 (the version `tauri` 2.11.6 names) | Apache-2.0 OR MIT (standard) | build script: config, icons, sidecars | builds |
| tauri-cli (tool, not linked) | 2.11.5 | 2026-09-19 (same release train) | Apache-2.0 OR MIT (standard) | `cargo tauri build`: the installers; installed `--locked` in CI | the CI desktop job; locally the .deb, .rpm and AppImage were built, and the .deb installed and ran |
| serde_json (dev) | as resolved | — | already recorded above | the config test | — |

1. **Existence.** These are the canonical crates of tauri-apps/tauri (its own
   docs and templates name them); the `tauri-*` subcrates resolved by the
   lockfile are all from the 2.11 release train (`tauri-runtime` 2.11.3,
   `tauri-runtime-wry` 2.11.4, `tauri-utils` 2.9.3, `tauri-macros` and
   `tauri-codegen` 2.6.3).
2. **Maintenance.** Three 2.x releases in the 90 days before the check
   (2.11.6, 2.12.0, 2.12.1) and an active 3.0 alpha line. As above, GitHub
   activity was not reachable from the build environment.
3. **License.** The tree is checked by `desktop/deny.toml`: the main allow
   list, plus **MPL-2.0 for five crates by name**, each used unmodified with
   its source on crates.io (MPL-2.0 is file-level copyleft; the text was
   read): `cssparser`, `cssparser-macros`, `dtoa-short`, `selectors` (HTML
   processing in `tauri-utils`, through `dom_query`) and `option-ext`
   (through `dirs`).
4. **Fitness.** Built and run on Linux here (WebKitGTK 2.52): the window
   loads the app from the bundled backend; `--smoke` passes from the
   installed .deb; killing the shell stops the backend (`--exit-with-stdin`).
   macOS and Windows: the CI desktop job (build, tests, bundle, `--smoke`).
5. **Security.** `cargo deny check` on `desktop/`: no vulnerability
   advisories. **Six unmaintained notices are accepted by ID**, none a
   vulnerability and none avoidable within Tauri 2:
   `RUSTSEC-2024-0370` (`proc-macro-error`, compile time only, under the
   GTK 3 bindings on Linux) and `RUSTSEC-2025-0075`, `-0080`, `-0081`,
   `-0098`, `-0100` (the `unic-*` Unicode tables, through `urlpattern` in
   `tauri-utils`). The tree: 430 crates in the lockfile across platforms;
   362 resolved for Linux, 299 for Windows, 295 for macOS.

The Linux AppImage bundler downloads unpinned helper tools
(`linuxdeploy`, its plugins) at build time, so CI builds only `.deb` and
`.rpm` on Linux. On Windows the bundler downloads pinned WiX 3 and NSIS
releases and verifies them against hashes compiled into `tauri-bundler`
2.9.4 (WiX: SHA-256; NSIS: SHA-1 only, a weaker check, recorded here). The
macOS bundler uses the runner's own tools.

## Update #1: inference paths (2026-10-08)

New direct dependency: **`keyring = "=4.2.0"`** (the OS keychain for BYOK
keys and the free-tier token, ADR-0029). Pinned exactly. New workspace
crates `anchor-inference` and `anchor-proxy` use only already-vetted
crates (serde, serde_json, toml, thiserror, sha2, getrandom, rusqlite,
ureq); `anchor-backend` now also uses `ureq` (vetted) for the Ollama
probe, the key-verify call and the free-tier client.

| Crate | Version | Released (crates.io) | Releases in last 90 days | License (Cargo.toml) | Used for |
|---|---|---|---|---|---|
| keyring | 4.2.0 | 2026-08-29 | 4 (4.1.4 … 4.2.0) | MIT OR Apache-2.0 | `Entry::new/set_password/get_password/delete_credential` (default `v1` feature) |
| keyring-core | 1.0.0 | 2026-04-21 | 0 (stable 1.0 API) | MIT OR Apache-2.0 | keyring's store interface |
| apple-native-keyring-store | 1.0.2 | 2026-08-06 | 2 | MIT OR Apache-2.0 | macOS Keychain (via `security-framework` 3.7.0, MIT OR Apache-2.0) |
| windows-native-keyring-store | 1.1.0 | 2026-05-24 | 0 (last 2026-05-24, 137 days) | MIT OR Apache-2.0 | Windows Credential Manager (via the vetted `windows-sys`) |
| zbus-secret-service-keyring-store | 1.0.1 | 2026-08-15 | 1 | MIT OR Apache-2.0 | Linux Secret Service over D-Bus |
| secret-service | 5.2.0 | 2026-08-29 | 1 | MIT OR Apache-2.0 | Secret Service protocol (pure-Rust crypto: `aes`, `cbc`, `hkdf`, `num-bigint`; no OpenSSL) |
| zbus | 5.19.0 | 2026-08-09 | 3 | MIT | D-Bus (pure Rust, no libdbus); brings the `async-*` / `futures-*` / `zvariant` family as transitive crates |

- **Existence / typosquatting:** `keyring` is the crate of the
  `open-source-cooperative/keyring-rs` repository (crates.io `repository`
  field); the store crates are published by the same project and named in
  keyring's own manifest.
- **Maintenance:** `windows-native-keyring-store`'s last release is 137 days
  old, over the 90-day bar; it is part of the same actively released
  project (keyring 4.2.0 on 2026-08-29) and depends on the vetted
  `windows-sys`. Accepted with that note; re-check before v1.0.
- **Security:** `cargo deny check` (advisories, bans, licenses, sources)
  passes for the whole resolved tree on 2026-10-08, including every new
  transitive crate. Repository activity was not checked (GitHub not
  reachable from the build environment), as for earlier records.
- **Fitness:** `inference::keys::tests` (memory store; the real keychain
  round trip with `ANCHOR_TEST_KEYCHAIN=1` on the macOS and Windows CI
  runners; a clear error where no keychain exists, as on headless Linux);
  `secret_env_never_prints_values`.
- **Linux note:** a running Secret Service (GNOME Keyring, KWallet) is
  required to store keys; without one the app says so and BYOK is
  unavailable, local and free paths still work.
- **CI-only tool, `artifact-signing-cli =0.11.0`** (MIT; crates.io, repository
  `levminer/trusted-signing-cli`, the tool Tauri's Windows signing guide
  names; released 2026-05-17, 144 days before the check, over the 90-day
  bar). Installed only in the Windows desktop job, and only when the Azure
  signing secrets exist (ADR-0033); it never ships in Anchor. Its
  maintenance must be re-checked before the secrets are added.
- **Not adopted:** `tauri-plugin-updater` (no auto-update yet; ADR-0033
  records the ≥ 2.12.0 / `requireSignedVersion` requirement if added).

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
| editor packages | Deferred (ADR-0008). `tauri` was adopted in Phase 9: see "Desktop shell". |
