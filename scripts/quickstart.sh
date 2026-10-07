#!/usr/bin/env bash
# The contributor quickstart from CONTRIBUTING.md. CI runs this file, as is,
# on a fresh clone (the "Contributor quickstart" job), so the documented
# steps are the ones known to work.
set -euo pipefail
cd "$(dirname "$0")/.."

# 1. The dashboard bundle, embedded by the Rust build (needs Node 22).
#    Optional: without it the dashboard serves a notice page instead.
if [ "${ANCHOR_SKIP_UI:-0}" != 1 ] && command -v npm >/dev/null 2>&1; then
  (cd ui && npm ci --ignore-scripts && npm run build)
fi

# 2. What every pull request must pass.
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 3. The reference verifier plugin conforms to the published contract.
cargo run -q -p anchor-backend --bin anchor -- \
  verifier check examples/verifier-python/verifier.toml

echo "quickstart: all good"
