#!/usr/bin/env bash
# gen-third-party-licenses.sh — regenerate THIRD-PARTY-LICENSES with cargo-about.
#
# WHY
#   The `tt` binary statically links its third-party Rust dependencies. Most of
#   those (MIT, BSD, Apache-2.0, ...) require their license text + copyright be
#   reproduced in distributions. This collects every linked crate's license and
#   emits a single THIRD-PARTY-LICENSES file that the Dockerfile copies into the
#   GHCR image, so the distributed binary carries the required attribution.
#
# USAGE
#   scripts/gen-third-party-licenses.sh            # regenerate the file
#   scripts/gen-third-party-licenses.sh --check    # fail if it would change
#                                                  # (drift guard for CI)
#
# Scope: the `tt-cli` binary crate, target x86_64-unknown-linux-gnu (the GHCR
# image arch — set in about.toml). `--fail` makes an unaccepted/unresolvable
# license a hard error, a second guard alongside `cargo deny check licenses`.

set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v cargo-about >/dev/null 2>&1; then
  echo "::error::cargo-about not installed — run: cargo install --locked cargo-about --features cli" >&2
  exit 1
fi

out="THIRD-PARTY-LICENSES"
check_mode=0
if [ "${1:-}" = "--check" ]; then
  check_mode=1
elif [ -n "${1:-}" ]; then
  echo "usage: $0 [--check]" >&2
  exit 2
fi

# Scope the dependency graph to the binary that actually ships.
gen() {
  cargo about generate \
    --fail \
    --manifest-path crates/cli/Cargo.toml \
    --config about.toml \
    about.hbs
}

if [ "$check_mode" -eq 1 ]; then
  tmp="$(mktemp)"
  trap 'rm -f "$tmp"' EXIT
  gen >"$tmp"
  if ! cmp -s "$tmp" "$out"; then
    echo "::error::$out is stale — run scripts/gen-third-party-licenses.sh and commit the result" >&2
    exit 1
  fi
  echo "ok: $out is up to date"
else
  gen >"$out"
  echo "wrote $out ($(wc -l <"$out" | tr -d ' ') lines)"
fi
