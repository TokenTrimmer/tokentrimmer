#!/usr/bin/env bash
# cost-ledger.sh — append a cost-ledger entry.
# Called by hooks or by the autonomous loop when an AI-assisted session ends.
set -euo pipefail

cd "$(dirname "$0")/.."

SESSION="${1:-${CLAUDE_SESSION_ID:-unknown}}"
COST_USD="${2:-0}"
MODEL="${3:-${CLAUDE_MODEL:-unknown}}"

mkdir -p .claude
touch .claude/cost-ledger.jsonl

TODAY=$(date -u +%Y-%m-%d)
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)

jq -n \
  --arg date "${TODAY}" \
  --arg ts "${TS}" \
  --arg session "${SESSION}" \
  --arg model "${MODEL}" \
  --argjson cost "${COST_USD}" \
  '{date: $date, ts: $ts, session: $session, model: $model, cost_usd: $cost}' \
  >> .claude/cost-ledger.jsonl
