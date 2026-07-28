#!/usr/bin/env bash
# check-doc-versions.sh — assert GETTING_STARTED.md contains the openai version
# strings that are pinned in sdk-python/pyproject.toml and the TypeScript SDK's
# OpenAI peer contract. Exits non-zero with a clear message on drift so
# the CI job fails loudly rather than letting stale docs reach users.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DOC="$REPO_ROOT/GETTING_STARTED.md"
PYPROJECT="$REPO_ROOT/sdk-python/pyproject.toml"
PKG_JSON="$REPO_ROOT/sdk-typescript/package.json"

fail=0

# ── Python pin ────────────────────────────────────────────────────────────────
# Extract the openai specifier from the dependencies array, e.g. "openai>=1.70.0,<3.0.0"
py_pin="$(grep -E '"openai>=' "$PYPROJECT" | sed 's/.*"\(openai[^"]*\)".*/\1/')"
if [[ -z "$py_pin" ]]; then
    echo "ERROR: could not parse openai pin from $PYPROJECT" >&2
    exit 1
fi
if grep -qF "$py_pin" "$DOC"; then
    echo "OK  Python pin '$py_pin' found in GETTING_STARTED.md"
else
    echo "DRIFT  Python pin '$py_pin' is NOT in GETTING_STARTED.md" >&2
    echo "       Update the doc to match $PYPROJECT" >&2
    fail=1
fi

# ── TypeScript pin ────────────────────────────────────────────────────────────
# Parse JSON instead of depending on property order or a nearby `dependencies`
# block. The peer dependency is the SDK's customer-facing compatibility
# contract; the dev dependency must stay identical so CI tests that contract.
ts_pins="$(node -e '
const manifest = require(process.argv[1]);
const peer = manifest.peerDependencies?.openai ?? "";
const dev = manifest.devDependencies?.openai ?? "";
process.stdout.write(`${peer}\n${dev}`);
' "$PKG_JSON")"
ts_pin="$(printf '%s\n' "$ts_pins" | sed -n '1p')"
ts_dev_pin="$(printf '%s\n' "$ts_pins" | sed -n '2p')"
if [[ -z "$ts_pin" ]]; then
    echo "ERROR: could not parse peerDependencies.openai from $PKG_JSON" >&2
    exit 1
fi
if [[ "$ts_dev_pin" != "$ts_pin" ]]; then
    echo "DRIFT  TypeScript dev OpenAI pin '$ts_dev_pin' does not match peer pin '$ts_pin'" >&2
    echo "       Update devDependencies.openai in $PKG_JSON" >&2
    fail=1
fi
if grep -qF "$ts_pin" "$DOC"; then
    echo "OK  TypeScript pin '$ts_pin' found in GETTING_STARTED.md"
else
    echo "DRIFT  TypeScript pin '$ts_pin' is NOT in GETTING_STARTED.md" >&2
    echo "       Update the doc to match $PKG_JSON" >&2
    fail=1
fi

exit "$fail"
