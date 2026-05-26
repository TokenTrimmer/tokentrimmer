#!/usr/bin/env bash
# audit-line.sh — append one line to .claude/AUDIT.log on session end.
# Seeds the "auditable AI assistance" promise from day one.
set -euo pipefail

cd "${CLAUDE_PROJECT_DIR:-$(pwd)}"

mkdir -p .claude
touch .claude/AUDIT.log

TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
# CLAUDE_SESSION_ID isn't always exported to script subshells. Fall back to a
# stable per-day+pid composite so AUDIT.log entries are correlatable.
SESSION="${CLAUDE_SESSION_ID:-$(date -u +%Y%m%d)-${$}}"
MODEL="${CLAUDE_MODEL:-unknown}"
BRANCH=$(git branch --show-current 2>/dev/null || echo "no-git")

# Try to summarize files changed in this session. Fall back gracefully.
SHORTSTAT=$(git diff --shortstat 2>/dev/null || echo "")
FILES_CHANGED=$(git diff --name-only 2>/dev/null | tr '\n' ',' | sed 's/,$//' || echo "")

# Cost ledger entry is appended by cost-cap-check.sh; we just write the audit line here.
LINE="${TS}  session=${SESSION}  branch=${BRANCH}  model=${MODEL}  shortstat=\"${SHORTSTAT}\"  files=[${FILES_CHANGED}]"

echo "${LINE}" >> .claude/AUDIT.log
exit 0
