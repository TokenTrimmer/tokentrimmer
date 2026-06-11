#!/usr/bin/env bash
# ci-local.sh — local mirror of CI. Run before pushing.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --all -- --check

echo "==> cargo clippy --workspace"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> cargo test --workspace"
cargo test --workspace --no-fail-fast

echo "==> cargo deny check"
if command -v cargo-deny >/dev/null 2>&1; then
  cargo deny check licenses advisories bans sources
else
  echo "(skipping: install with 'cargo install cargo-deny')"
fi

echo "==> per-crate LICENSE + NOTICE"
./scripts/sync-licenses.sh --check

echo "==> THIRD-PARTY-LICENSES up to date"
if command -v cargo-about >/dev/null 2>&1; then
  ./scripts/gen-third-party-licenses.sh --check
else
  echo "(skipping: install with 'cargo install --locked cargo-about --features cli')"
fi

echo "==> cargo audit"
if command -v cargo-audit >/dev/null 2>&1; then
  cargo audit
else
  echo "(skipping: install with 'cargo install cargo-audit')"
fi

echo "==> tt inspect ."
./scripts/tt-inspect-self.sh

echo "==> check-p0-bugs"
if command -v gh >/dev/null 2>&1; then
  ./scripts/check-p0-bugs.sh
else
  echo "(skipping: gh CLI not installed)"
fi

echo
echo "All local CI gates passed."
