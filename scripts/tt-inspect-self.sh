#!/usr/bin/env bash
# tt-inspect-self.sh — run Inspect against this repo and surface new findings.
# Placeholder until the CLI ships in Week 14; then this becomes a thin wrapper.
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ -x "./target/release/tt" ]]; then
  ./target/release/tt inspect . --fail-on=high
elif [[ -x "./target/debug/tt" ]]; then
  ./target/debug/tt inspect . --fail-on=high
else
  echo "tt binary not built yet. Build with: cargo build -p tt-cli"
  # Until Week 14, exit 0 so the harness check doesn't block all work.
  exit 0
fi
