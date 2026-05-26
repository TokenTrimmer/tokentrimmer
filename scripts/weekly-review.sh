#!/usr/bin/env bash
# weekly-review.sh — measure the build process and surface improvement signals.
#
# Run every Monday morning (cron-friendly). Produces a markdown report at
# .claude/reviews/<YYYY-WW>.md. Compares to last week and flags drift.
#
# Metrics:
#   - tokens/$ spent this week (from cost-ledger.jsonl)
#   - sessions count and avg cost/session
#   - PRs opened, merged, time-to-merge
#   - inspect-self findings count (regression check)
#   - backlog burn rate (items completed this week vs added)
#   - top 5 most expensive sessions (candidates for subagent prompt tuning)
set -euo pipefail

cd "$(dirname "$0")/.."

mkdir -p .claude/reviews

YEAR_WEEK=$(date -u +%Y-W%V)
WEEK_START=$(date -u -v-7d +%Y-%m-%d 2>/dev/null || date -u --date='-7 days' +%Y-%m-%d)
TODAY=$(date -u +%Y-%m-%d)
REPORT=".claude/reviews/${YEAR_WEEK}.md"

# --- Cost & sessions ---
if [[ -f .claude/cost-ledger.jsonl ]]; then
  weekly_cost=$(jq -s --arg start "${WEEK_START}" \
    '[.[] | select(.date >= $start) | .cost_usd] | add // 0' \
    .claude/cost-ledger.jsonl 2>/dev/null || echo "0")
  session_count=$(jq -s --arg start "${WEEK_START}" \
    '[.[] | select(.date >= $start) | .session] | unique | length' \
    .claude/cost-ledger.jsonl 2>/dev/null || echo "0")
  top_sessions=$(jq -s --arg start "${WEEK_START}" \
    '[.[] | select(.date >= $start)] | group_by(.session) |
     map({session: .[0].session, cost: ([.[] | .cost_usd] | add)}) |
     sort_by(-.cost) | .[0:5] |
     map("\(.session): $\(.cost)") | .[]' \
    .claude/cost-ledger.jsonl 2>/dev/null || echo '"(no ledger entries)"')
else
  weekly_cost="0"
  session_count="0"
  top_sessions='"(no ledger)"'
fi

avg_per_session="0"
if [[ "${session_count}" != "0" ]]; then
  avg_per_session=$(awk -v c="${weekly_cost}" -v s="${session_count}" 'BEGIN { printf "%.2f", c / s }')
fi

# --- PRs (if gh available and authenticated) ---
prs_opened=0
prs_merged=0
if command -v gh >/dev/null 2>&1; then
  prs_opened=$(gh pr list --search "created:>=${WEEK_START}" --state all --json number --jq 'length' 2>/dev/null || echo 0)
  prs_merged=$(gh pr list --search "merged:>=${WEEK_START}" --state merged --json number --jq 'length' 2>/dev/null || echo 0)
fi

# --- Backlog burn ---
completed_this_week=$(grep -c '^- \[x\]' .claude/BACKLOG.md 2>/dev/null || echo 0)
open_count=$(grep -c '^- \[ \]' .claude/BACKLOG.md 2>/dev/null || echo 0)

# --- Inspect self ---
inspect_findings="(not run yet — Week 14+)"
if [[ -x ./target/release/tt || -x ./target/debug/tt ]]; then
  inspect_findings=$(./scripts/tt-inspect-self.sh 2>/dev/null | grep -cE 'high|critical' || echo 0)
fi

# --- Improvement signals ---
SIGNALS=""

# Signal 1: cost-per-session climbing
if awk -v a="${avg_per_session}" 'BEGIN { exit !(a > 2.0) }'; then
  SIGNALS+="- **Cost-per-session is high (\$${avg_per_session}).** Likely candidates: parent context bloat, subagent over-dispatch, or AGENTS.md too long. Inspect top sessions below.\n"
fi

# Signal 2: low PR throughput vs sessions
if [[ "${session_count}" != "0" && "${prs_opened}" != "0" ]]; then
  ratio=$(awk -v s="${session_count}" -v p="${prs_opened}" 'BEGIN { printf "%.1f", s / p }')
  if awk -v r="${ratio}" 'BEGIN { exit !(r > 5) }'; then
    SIGNALS+="- **${session_count} sessions but only ${prs_opened} PRs.** Sessions are not converging to deliverables. Check whether subagent scopes are too broad.\n"
  fi
fi

# Signal 3: backlog growing faster than burning
# (Not implementable without snapshot from last week. TODO when we have history.)

# --- Write report ---
cat > "${REPORT}" <<EOF
# Weekly review — ${YEAR_WEEK} (${WEEK_START} → ${TODAY})

## Cost discipline

- Total LLM spend this week: **\$${weekly_cost}**
- Sessions: ${session_count}
- Avg cost per session: \$${avg_per_session}
- Top 5 most expensive sessions:
\`\`\`
$(echo "${top_sessions}" | jq -r '.' 2>/dev/null || echo "${top_sessions}")
\`\`\`

## Throughput

- PRs opened: ${prs_opened}
- PRs merged: ${prs_merged}
- Backlog items completed (cumulative): ${completed_this_week}
- Backlog items open: ${open_count}

## Dogfood — Inspect on ourselves

- High/critical findings: ${inspect_findings}

## Improvement signals

${SIGNALS:-_No signals fired this week._}

## Actions for next week

(fill in manually after reviewing the report; entries here become BACKLOG items)

- [ ] ...
EOF

echo "Wrote ${REPORT}"
echo
cat "${REPORT}"
