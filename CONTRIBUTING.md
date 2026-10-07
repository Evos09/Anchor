# Contributing to Anchor

Thank you for helping. Anchor is a coding agent whose whole product is
trust: it must not lose your work, leak your credentials, or ship unverified
changes. Every contribution is judged by the spec's one-sentence test:

> Does this make the agent more trustworthy, or does it just make it more
> capable?

A change that only adds capability while weakening durability, security or
verifiability will not be merged, however useful it is.

## Quickstart

**Prerequisites:**

- **Rust:** the stable toolchain. `rust-toolchain.toml` pins the channel and
  components, so `rustup` installs them on first use. The minimum supported
  version is 1.89.
- **Python 3:** the verification gate's tests, the seeded-bug corpus and the
  example verifier plugin run Python projects. The Windows runner's
  interpreter is `python`.
- **Node 22 (optional):** builds the dashboard bundle (`ui/`). Without it the
  dashboard serves a notice page, and everything else works.
- **Git.**

From a fresh clone, run:

```sh
./scripts/quickstart.sh
```

On Windows, run it from Git Bash. CI runs this exact script on a new clone
of every push (the "Contributor quickstart (fresh clone)" job), so if it
fails for you on a supported platform, that is a bug: please report it.

The script does three things:

```sh
# 1. the dashboard bundle (skipped without npm, or with ANCHOR_SKIP_UI=1)
(cd ui && npm ci --ignore-scripts && npm run build)
# 2. what every pull request must pass
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# 3. the reference verifier plugin conforms to the published contract
cargo run -q -p anchor-backend --bin anchor -- \
  verifier check examples/verifier-python/verifier.toml
```

## Where things are

The [README](README.md) has the architecture (three isolated layers) and a
table of crates. In short:

| Area | Crates | Spec |
|---|---|---|
| Safety primitives | `anchor-fs`, `anchor-policy`, `anchor-guard`, `anchor-checkpoint`, `anchor-snapshot` | §5 |
| Sandbox | `anchor-exec`, `anchor-seccomp`, `anchor-sandbox-validation` | §11.1 |
| Wire protocols | `anchor-protocol` (runtime protocol, review types, verifier plugin protocol) | §4, §8 |
| Agent runtime (Layer 2) | `anchor-runtime` | §4 |
| Coordination Hub | `anchor-hub` | §6 |
| Backend, CLI, merge gate | `anchor-backend` | §3.2, §7, §8 |
| Dashboard | `ui/` (React + TypeScript), served by `anchor-backend` | §12 |

Design decisions are in [`docs/adr/`](docs/adr/0000-adr-process.md). The
threat model, with each threat's tests and per-platform status, is
[`docs/threat-model.md`](docs/threat-model.md). The audit pack is
[`docs/audit/`](docs/audit/README.md).

## Tests

`cargo test --workspace` runs everything that runs on your platform:
- unit tests;
- the attack suites (§5, §11);
- the cross-platform sandbox validation suite;
- the verification gate end to end;
- the Hub.

**Where the platform-specific code is tested.** Some tests run only on the
platform whose code they cover:
- the Landlock and seccomp tests on Linux;
- the Seatbelt tests on macOS;
- the AppContainer tests on Windows.

CI runs all three, and a change to platform-specific code is only verified
once the matching CI job passes. You can type-check other platforms from
Linux:

```sh
rustup target add x86_64-pc-windows-gnu aarch64-apple-darwin
cargo check --tests --target x86_64-pc-windows-gnu -p anchor-exec -p anchor-fs
```

`anchor-runtime`, `anchor-hub` and `anchor-backend` need a C cross-compiler
for that, so CI builds them natively.

Other runs:

| Run | Command |
|---|---|
| The seccomp-only sandbox tier (Linux without user namespaces) | `ANCHOR_EXEC_TEST_DISABLE_USERNS=1 cargo test -p anchor-exec -p anchor-sandbox-validation -p anchor-seccomp` |
| The seeded-bug harness (§8.5) | `cargo run -p anchor-backend --bin anchor -- eval run --corpus examples/eval/corpus` |
| The dashboard end to end (Chromium) | `cargo build --workspace --bins`, then in `ui/`: `npx playwright install chromium && npx playwright test` |
| The snapshot benchmark (§5.5) | `cargo run --release -p anchor-snapshot --example bench -- --files 100000` |

**Rules for tests:**

- **Never skip, ignore or weaken a test to get green.** A failing test is a
  finding: fix the cause.
- **Every security fix comes with a regression test** that fails without the
  fix.
- **A threat without a passing test is an unmitigated vulnerability**
  (§11). A new mitigation adds a row to the threat model, with its test.

## Making a change

1. **For anything beyond a small fix, open an issue first.** For a design
   decision, write an ADR (below).
2. **Keep the change focused.** One concern per pull request.
3. **Run `./scripts/quickstart.sh`,** or at least fmt, clippy and the
   affected crates' tests.
4. **Update the docs the change affects:** the README, the threat model,
   the ADRs, `docs/verifier/`.
5. **Explain what changed and why** in the pull request, and how you
   verified it, including the platforms where it was verified.

### Code style

- **Formatting and lints:** `cargo fmt`, and `cargo clippy -D warnings` on
  every target.
  - The workspace lints reject `unwrap()` outside tests, so use `?` or a
    reasoned `expect`.
  - `crates/anchor-runtime/clippy.toml` bans filesystem and process APIs in
    the agent runtime, because Layer 2 has no OS authority (§4).
- **`unsafe`:** only where an OS API requires it, each block with a
  `// SAFETY:` comment.
- **Comments:** say why, not what.
- **Errors:** fail closed. When a check cannot be made, refuse; never
  continue with a warning (§5.1(e)).

### Architecture decisions (ADRs)

**When to write one.** Every dependency, ambiguous requirement and open
question is decided in writing before it is implemented (spec §0, §17):
see [ADR-0000](docs/adr/0000-adr-process.md) for the template and statuses.
If your change interprets the spec, add or update an ADR, and say which
alternatives you rejected.

**What it records.** An ADR keeps what was **observed**, with its date and
source, apart from what is **assumed**.

### Dependencies

**What a new dependency needs.** Before a crate or npm package is added, it
needs a five-step record in
[`docs/deps/dependency-records.md`](docs/deps/dependency-records.md):
1. **Existence:** the canonical source; check for typosquats.
2. **Maintenance:** activity in the last 90 days, with the date of the
   check.
3. **License:** read in full. It must be compatible with Apache-2.0 and
   allowed by `deny.toml`.
4. **Fitness:** tested against our requirements on every target platform.
5. **Security:** advisories, and the transitive tree.

**Enabling a feature** of a vetted crate is recorded the same way.

**The bar.** `cargo deny check` runs in CI. A dependency that cannot pass
all five steps is not added: build the piece yourself, or choose another
dependency.

### Commits

- Write a short imperative subject line. The body says what changed and
  why, and which findings or spec sections it addresses.
- Start a docs-only commit's subject with `docs:`, or add `[skip ci]` if
  it cannot affect any build.
- Contributions are licensed under Apache-2.0 (see `LICENSE`), the
  project's license.

## Writing a verifier plugin

You do not need to change Anchor to add a review check. A verifier plugin
is a separate program, in any language, that Anchor runs sandboxed at the
merge gate. The full contract is [`docs/verifier/`](docs/verifier/README.md),
and the example is [`examples/verifier-python/`](examples/verifier-python/).

## Security

Please do **not** open public issues for vulnerabilities. Follow
[SECURITY.md](SECURITY.md).

## What is open

The open parts of Anchor are licensed under Apache-2.0, per spec §13.1:
- the agent runtime;
- the verification pipeline and the verifier plugin interface;
- the five safety primitives;
- the Coordination Hub;
- the MCP and A2A layers.

Some parts are commercial and are not in this repository:
- the polished desktop shell;
- team features;
- sync and SSO.

**Commitments that do not change** (ADR-0019):
- no training on user code;
- no ad-supported tier;
- no telemetry by default.

A contribution that would weaken any of them will not be accepted.
