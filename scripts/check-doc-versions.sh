#!/usr/bin/env bash
# check-doc-versions.sh — assert GETTING_STARTED.md contains the openai version
# strings that are pinned in sdk-python/pyproject.toml and
# sdk-typescript/package.json. Exits non-zero with a clear message on drift so
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
# Extract the openai value from package.json, e.g. "^6.44.0". openai is a
# peerDependency (not a bundled dependency), so match the versioned entry
# `"openai": "<spec>"` in whatever section it lives in (peer/dev) rather than
# only the line after `"dependencies"` — the bare `"openai"` keywords entry has
# no colon+version and is skipped by the `":` anchor.
ts_pin="$(grep -E '"openai": *"' "$PKG_JSON" | head -1 | sed 's/.*"openai": *"\([^"]*\)".*/\1/')"
if [[ -z "$ts_pin" ]]; then
    echo "ERROR: could not parse openai pin from $PKG_JSON" >&2
    exit 1
fi
if grep -qF "$ts_pin" "$DOC"; then
    echo "OK  TypeScript pin '$ts_pin' found in GETTING_STARTED.md"
else
    echo "DRIFT  TypeScript pin '$ts_pin' is NOT in GETTING_STARTED.md" >&2
    echo "       Update the doc to match $PKG_JSON" >&2
    fail=1
fi

exit "$fail"
