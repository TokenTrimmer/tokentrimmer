#!/usr/bin/env bash
# cost-cap-check.sh — circuit breaker for autonomous loops.
# Drops .claude/PAUSED if daily or weekly cost cap is exceeded.
set -euo pipefail

cd "${CLAUDE_PROJECT_DIR:-$(pwd)}"

LEDGER=".claude/cost-ledger.jsonl"
BUDGET=".claude/budget.toml"

# If budget config is missing, do nothing (opt-in feature).
if [[ ! -f "${BUDGET}" ]]; then
  exit 0
fi

# Defaults; can be overridden in budget.toml as plain shell-readable values.
DAILY_CAP_USD=25
WEEKLY_CAP_USD=100
if [[ -f "${BUDGET}" ]]; then
  # Crude TOML parse — keys must be unquoted floats/ints on their own line.
  DC=$(grep -E '^daily_cap_usd' "${BUDGET}" | sed -E 's/.*=\s*([0-9.]+).*/\1/' || true)
  WC=$(grep -E '^weekly_cap_usd' "${BUDGET}" | sed -E 's/.*=\s*([0-9.]+).*/\1/' || true)
  [[ -n "${DC}" ]] && DAILY_CAP_USD="${DC}"
  [[ -n "${WC}" ]] && WEEKLY_CAP_USD="${WC}"
fi

if [[ ! -f "${LEDGER}" ]]; then
  exit 0
fi

# Sum costs for today and this week using jq.
TODAY=$(date -u +%Y-%m-%d)
WEEK_START=$(date -u -v-7d +%Y-%m-%d 2>/dev/null || date -u --date='-7 days' +%Y-%m-%d)

DAILY_SUM=$(jq -s --arg today "${TODAY}" '[.[] | select(.date == $today) | .cost_usd] | add // 0' "${LEDGER}" 2>/dev/null || echo "0")
WEEKLY_SUM=$(jq -s --arg start "${WEEK_START}" '[.[] | select(.date >= $start) | .cost_usd] | add // 0' "${LEDGER}" 2>/dev/null || echo "0")

PAUSE_REASON=""
if awk -v a="${DAILY_SUM}" -v b="${DAILY_CAP_USD}" 'BEGIN { exit !(a > b) }'; then
  PAUSE_REASON="Daily cost \$${DAILY_SUM} exceeded cap \$${DAILY_CAP_USD}"
elif awk -v a="${WEEKLY_SUM}" -v b="${WEEKLY_CAP_USD}" 'BEGIN { exit !(a > b) }'; then
  PAUSE_REASON="Weekly cost \$${WEEKLY_SUM} exceeded cap \$${WEEKLY_CAP_USD}"
fi

if [[ -n "${PAUSE_REASON}" ]]; then
  cat > .claude/PAUSED <<EOF
Autonomous loop paused at $(date -u +%Y-%m-%dT%H:%M:%SZ)
Reason: ${PAUSE_REASON}

To resume:
  1. Investigate cost spike: tail .claude/cost-ledger.jsonl
  2. Decide whether to raise caps in .claude/budget.toml or address the root cause
  3. Delete this file: rm .claude/PAUSED
EOF
fi

exit 0
