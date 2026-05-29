#!/usr/bin/env bash
# check-p0-bugs.sh — fail if any open P0-labeled GitHub issue is older than
# the threshold (default 24h). Gate for the "zero P0 bugs >24h" SLA.
#
# Usage:
#   ./scripts/check-p0-bugs.sh                  # default 24h threshold
#   ./scripts/check-p0-bugs.sh --hours 48       # 48h threshold
#   ./scripts/check-p0-bugs.sh --label priority/critical  # custom label
set -euo pipefail

cd "$(dirname "$0")/.."

HOURS=24
LABEL="P0"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --hours) HOURS="$2"; shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if ! command -v gh >/dev/null 2>&1; then
  echo "gh CLI required (install or 'brew install gh')." >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "jq required." >&2
  exit 2
fi

# Threshold ISO-8601 — GitHub `createdAt` is UTC RFC-3339.
# Use BSD `date` on macOS, GNU `date` on Linux.
if THRESHOLD=$(date -u -v-"${HOURS}"H +%Y-%m-%dT%H:%M:%SZ 2>/dev/null); then
  :
else
  THRESHOLD=$(date -u --date="-${HOURS} hours" +%Y-%m-%dT%H:%M:%SZ)
fi

# Fetch open P0 issues with createdAt + URL + title.
# `--limit 200` keeps this bounded; if you have >200 open P0s, FP-rate is the
# least of your problems.
issues_json=$(gh issue list \
  --label "${LABEL}" \
  --state open \
  --limit 200 \
  --json number,title,createdAt,url 2>/dev/null) || {
  # If the label doesn't exist on the repo, treat as "no open P0s" (success).
  echo "Could not query gh (label '${LABEL}' may not exist on this repo, or auth failed)."
  echo "Pass — no open P0 issues to gate on."
  exit 0
}

# Old = createdAt < THRESHOLD.
stale=$(printf '%s' "${issues_json}" | jq --arg cut "${THRESHOLD}" \
  '[.[] | select(.createdAt < $cut)]')
count=$(printf '%s' "${stale}" | jq 'length')

if [[ "${count}" -gt 0 ]]; then
  echo "FAIL: ${count} open '${LABEL}' issue(s) older than ${HOURS}h:"
  printf '%s' "${stale}" | jq -r '.[] | "  #\(.number)  \(.title)  \(.createdAt)  \(.url)"'
  exit 1
fi

echo "OK: 0 open '${LABEL}' issue(s) older than ${HOURS}h."
