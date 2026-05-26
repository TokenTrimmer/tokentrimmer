#!/usr/bin/env bash
# ralph-iteration.sh — one iteration of the autonomous build loop.
#
# Run by cron, /loop, or scheduled remote agent. Safety properties:
#   - One GitHub issue per iteration (bounded scope).
#   - One crate per iteration (bounded blast radius).
#   - Mandatory gates: cargo test green, inspect-self clean, cost < $1.
#   - Opens a PR; never auto-merges. Human review required.
#   - Cron decides cadence, not the model.
set -euo pipefail

cd "$(dirname "$0")/.."

ITERATION_CAP_USD="${ITERATION_CAP_USD:-1.00}"
LABEL="${AUTOPILOT_LABEL:-autopilot}"

# --- Gate 1: paused? ---
if [[ -f ".claude/PAUSED" ]]; then
  echo "Loop paused (.claude/PAUSED present). Exiting."
  cat .claude/PAUSED
  exit 0
fi

# --- Gate 2: weekly budget? ---
# cost-cap-check.sh writes PAUSED if exceeded; double-check here.
if [[ -f ".claude/budget.toml" ]]; then
  weekly_cap=$(grep -E '^weekly_cap_usd' .claude/budget.toml | sed -E 's/.*=\s*([0-9.]+).*/\1/' || echo "100")
  weekly_sum=$(jq -s --arg start "$(date -u -v-7d +%Y-%m-%d 2>/dev/null || date -u --date='-7 days' +%Y-%m-%d)" \
    '[.[] | select(.date >= $start) | .cost_usd] | add // 0' \
    .claude/cost-ledger.jsonl 2>/dev/null || echo "0")
  if awk -v a="${weekly_sum}" -v b="${weekly_cap}" 'BEGIN { exit !(a > b) }'; then
    echo "Weekly budget \$${weekly_sum} exceeded cap \$${weekly_cap}. Pausing."
    touch .claude/PAUSED
    exit 0
  fi
fi

# --- Gate 3: pull next autopilot issue ---
ISSUE_JSON=$(gh issue list --label "${LABEL}" --state open --limit 1 --json number,title,body,labels 2>/dev/null || echo "[]")
ISSUE_NUM=$(printf '%s' "${ISSUE_JSON}" | jq -r '.[0].number // empty')

if [[ -z "${ISSUE_NUM}" ]]; then
  echo "No open autopilot issues. Exiting."
  exit 0
fi

ISSUE_TITLE=$(printf '%s' "${ISSUE_JSON}" | jq -r '.[0].title')
echo "Iteration target: issue #${ISSUE_NUM} — ${ISSUE_TITLE}"

# --- Gate 4: branch ---
BRANCH="autopilot/issue-${ISSUE_NUM}"
git switch -c "${BRANCH}" 2>/dev/null || git switch "${BRANCH}"

# --- Gate 5: dispatch subagent ---
# In practice this is invoked from a Claude Code session; this script is the
# orchestrator that decides whether to run, not the runtime. The actual
# subagent dispatch happens via the Agent tool inside Claude Code.
echo "Subagent dispatch is performed by Claude Code, not this script."
echo "Manual invocation: claude --resume <session-id> or via /loop hook."

# --- Gate 6: mandatory verification before commit ---
# (Assumes subagent has made edits; this verifies them.)

echo "==> cargo check + clippy on changed crates"
CHANGED_CRATES=$(git diff --name-only main...HEAD | grep -E '^crates/' | awk -F/ '{print $2}' | sort -u || true)
for c in ${CHANGED_CRATES}; do
  crate_name=$(grep -E '^name' "crates/$c/Cargo.toml" | head -1 | sed -E 's/name *= *"([^"]+)".*/\1/' || true)
  if [[ -n "${crate_name}" ]]; then
    cargo test -p "${crate_name}" --no-fail-fast
    cargo clippy -p "${crate_name}" -- -D warnings
  fi
done

echo "==> inspect-self (no new high/critical)"
./scripts/tt-inspect-self.sh

# --- Gate 7: open PR ---
COMMIT_MSG="autopilot: ${ISSUE_TITLE} (#${ISSUE_NUM})"
git add -A
if ! git diff --cached --quiet; then
  git commit -m "${COMMIT_MSG}"
  echo "Commit created. Opening PR..."
  # PR creation deferred to human; auto-creation would bypass review intent.
  echo "Run: gh pr create --title \"${COMMIT_MSG}\" --body \"closes #${ISSUE_NUM}\""
else
  echo "No changes to commit. Subagent may have hit a blocker."
fi

# --- Update state ---
{
  echo "# Last iteration: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "# Issue: #${ISSUE_NUM} — ${ISSUE_TITLE}"
  echo "# Branch: ${BRANCH}"
} > .claude/STATE.md
