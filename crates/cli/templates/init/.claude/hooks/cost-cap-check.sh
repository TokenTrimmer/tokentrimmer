#!/usr/bin/env bash
# Check daily/weekly Claude API spend against budget.toml. Pauses the
# autonomous loop if exceeded.
set -euo pipefail

BUDGET=".claude/budget.toml"
LEDGER=".claude/cost-ledger.jsonl"
[[ -f "${BUDGET}" ]] || exit 0
[[ -f "${LEDGER}" ]] || exit 0

DC=$(grep -E '^daily_cap_usd' "${BUDGET}" | sed -E 's/.*=\s*([0-9.]+).*/\1/' || echo "10")
WC=$(grep -E '^weekly_cap_usd' "${BUDGET}" | sed -E 's/.*=\s*([0-9.]+).*/\1/' || echo "50")

D_START=$(date -u +%Y-%m-%d)
W_START=$(date -u -v-7d +%Y-%m-%d 2>/dev/null || date -u --date='-7 days' +%Y-%m-%d)

DS=$(jq -s --arg s "${D_START}" '[.[] | select(.date >= $s) | .cost_usd] | add // 0' "${LEDGER}")
WS=$(jq -s --arg s "${W_START}" '[.[] | select(.date >= $s) | .cost_usd] | add // 0' "${LEDGER}")

if awk -v a="${DS}" -v b="${DC}" 'BEGIN { exit !(a > b) }'; then
  echo "Daily cap exceeded: \$${DS} > \$${DC}. Pausing." > .claude/PAUSED
  exit 1
fi
if awk -v a="${WS}" -v b="${WC}" 'BEGIN { exit !(a > b) }'; then
  echo "Weekly cap exceeded: \$${WS} > \$${WC}. Pausing." > .claude/PAUSED
  exit 1
fi
