#!/usr/bin/env bash
# tt-inspect-self.sh — delta-vs-main inspect gate.
#
# Exits non-zero only when the current working tree introduces NEW
# HIGH/CRITICAL findings vs the latest `main`. Pre-existing findings
# (whose `(rule_id, file, line)` already appears in main) do not block.
#
# Identity: a finding is "the same" iff its (rule_id, file, line) tuple
# matches a finding present on main. Confidence drift, message rewording,
# and severity downgrades all keep the identity stable.
#
# Fallbacks (all non-blocking — print and exit 0):
#   - tt binary not built
#   - main branch unreachable (shallow clone, fresh repo)
#   - `jq` not installed
#   - worktree creation fails
set -euo pipefail

cd "$(dirname "$0")/.."

# --- Locate the tt binary (resolved to absolute path so it survives `cd`) ---
TT=""
for path in ./target/release/tt ./target/debug/tt; do
  if [[ -x "$path" ]]; then
    TT=$(cd "$(dirname "$path")" && pwd)/$(basename "$path")
    break
  fi
done
if [[ -z "$TT" ]]; then
  echo "tt binary not built. Build with: cargo build -p tt-cli"
  # Don't block CI before the binary exists in this checkout.
  exit 0
fi

# --- Required tooling ---
if ! command -v jq >/dev/null 2>&1; then
  echo "jq required but not found. Skipping inspect-self gate."
  exit 0
fi

# --- Prep temp paths + cleanup ---
# tt inspect picks Markdown vs JSON output format from the .json suffix, so
# the temp paths must end in .json — mktemp -d gives us a clean dir to
# control filenames inside.
TMPDIR_RUN=$(mktemp -d -t tt-inspect.XXXXXX)
CURR="${TMPDIR_RUN}/curr.json"
BASE="${TMPDIR_RUN}/base.json"
WORKTREE="${TMPDIR_RUN}/main-worktree"
cleanup() {
  git worktree remove --force "$WORKTREE" 2>/dev/null || true
  rm -rf "$TMPDIR_RUN" 2>/dev/null || true
}
trap cleanup EXIT

# --- Inspect current tree ---
# We always succeed on the run itself — fail-on=critical means the binary
# only exits non-zero for CRITICAL, leaving the delta-vs-main logic in
# charge of HIGH gating. JSON shape: top-level array of Finding.
if ! "$TT" inspect . --output "$CURR" --fail-on critical >/dev/null 2>&1; then
  # CRITICAL findings exist — always block, no delta-vs-main exception.
  echo "FAIL: tt inspect reported CRITICAL findings on current tree."
  "$TT" inspect . --fail-on critical || true
  exit 1
fi

# --- Determine baseline ref ---
# Prefer origin/main (matches CI behaviour). Fall back to local main.
# Skip the gate if neither resolves.
BASE_REF=""
if git rev-parse --verify --quiet origin/main >/dev/null 2>&1; then
  BASE_REF="origin/main"
elif git rev-parse --verify --quiet main >/dev/null 2>&1; then
  BASE_REF="main"
else
  echo "No main/origin-main ref available — skipping delta-vs-main gate."
  exit 0
fi

# --- Create main worktree + inspect there ---
if ! git worktree add --detach "$WORKTREE" "$BASE_REF" >/dev/null 2>&1; then
  echo "Could not create worktree from $BASE_REF — skipping delta-vs-main gate."
  exit 0
fi
if ! (cd "$WORKTREE" && "$TT" inspect . --output "$BASE" --fail-on critical >/dev/null 2>&1); then
  echo "tt inspect failed on $BASE_REF worktree — skipping delta-vs-main gate."
  exit 0
fi

# --- Diff: new HIGH/CRITICAL findings that aren't in base ---
# Identity = "{rule_id}|{file}|{line}". Severity case is lowercase per
# inspect-core/src/lib.rs Serde impl. We gate on `high` and `critical`.
NEW=$(jq -s '
  (.[0] // []) as $curr |
  (.[1] // []) as $base |
  ($base
    | map(select(.severity == "high" or .severity == "critical"))
    | map("\(.rule_id)|\(.file)|\(.line)")
  ) as $base_ids |
  $curr
    | map(select(.severity == "high" or .severity == "critical"))
    | map(select((("\(.rule_id)|\(.file)|\(.line)") | IN($base_ids[])) | not))
' "$CURR" "$BASE")

NEW_COUNT=$(printf '%s' "$NEW" | jq 'length')

if [[ "$NEW_COUNT" -gt 0 ]]; then
  echo "FAIL: $NEW_COUNT new HIGH/CRITICAL finding(s) introduced vs $BASE_REF:"
  printf '%s' "$NEW" | jq -r '.[] | "  - \(.severity) \(.rule_id) at \(.file):\(.line)"'
  exit 1
fi

echo "OK: 0 new HIGH/CRITICAL findings vs $BASE_REF."
exit 0
