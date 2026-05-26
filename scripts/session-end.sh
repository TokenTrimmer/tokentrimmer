#!/usr/bin/env bash
# session-end.sh — archive the prior HANDOFF, write a new one, update STATE.md,
# update SESSIONS.md index, append rich audit entry.
#
# Run at the end of a session that did substantive work. The session-start hook
# reads HANDOFF.md to give the next session a clean entry point. Past handoffs
# live in .claude/sessions/<timestamp>-<task>.md (never overwritten).
#
# Usage:
#   ./scripts/session-end.sh "<status-line>" [--task <task-name>] [--next "<next-step>"]
set -euo pipefail

cd "$(dirname "$0")/.."

STATUS="${1:-work in progress}"
shift || true

TASK=""
NEXT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --task) TASK="$2"; shift 2 ;;
    --next) NEXT="$2"; shift 2 ;;
    *) shift ;;
  esac
done

TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
TS_SAFE=$(echo "${TS}" | tr ':' '-')      # filename-safe
# CLAUDE_SESSION_ID isn't always exported by Claude Code in script subshells;
# fall back to a stable per-day+pid composite so AUDIT.log entries can be
# correlated across hooks even when the env var is missing.
SESSION="${CLAUDE_SESSION_ID:-$(date -u +%Y%m%d)-${$}}"
BRANCH=$(git branch --show-current 2>/dev/null || echo "no-git")
HEAD=$(git rev-parse --short HEAD 2>/dev/null || echo "no-git")

FILES_CHANGED=$(git diff --name-only 2>/dev/null | head -20 | sed 's/^/- `/;s/$/`/' || echo "(no changes detected)")
SHORTSTAT=$(git diff --shortstat 2>/dev/null || echo "")
RECENT_AUDIT=$(tail -5 .claude/AUDIT.log 2>/dev/null | sed 's/^/    /' || echo "    (none)")

mkdir -p .claude/sessions

# --- 1. Archive the EXISTING HANDOFF.md before overwriting ---
# Filename convention: <iso-timestamp>-<task>.md so sorting is chronological.
if [[ -f .claude/HANDOFF.md ]]; then
  # Pull the task name from the prior handoff if we can; else label "(unknown)".
  PRIOR_TASK=$(grep -oE 'Active task: \`[^\`]+\`' .claude/HANDOFF.md 2>/dev/null | sed 's/Active task: `//;s/`$//' | head -1)
  PRIOR_TASK="${PRIOR_TASK:-unknown}"
  ARCHIVE=".claude/sessions/${TS_SAFE}-${PRIOR_TASK}.md"
  cp .claude/HANDOFF.md "${ARCHIVE}"
  # Append a one-line index entry to SESSIONS.md for easy browsing.
  if [[ ! -f .claude/SESSIONS.md ]]; then
    cat > .claude/SESSIONS.md <<'INDEX'
# Session archive

One line per ended session. The newest is at the bottom. Each line points to a
full HANDOFF snapshot in `.claude/sessions/`. Browse with `make sessions` or
read individual archives directly.

INDEX
  fi
  echo "- ${TS} · task=\`${PRIOR_TASK}\` · branch=\`${BRANCH}\` · diff=\"${SHORTSTAT:-none}\" → [archive](sessions/$(basename "${ARCHIVE}"))" >> .claude/SESSIONS.md
fi

# --- 2. Write fresh HANDOFF.md (the active pointer) ---
cat > .claude/HANDOFF.md <<EOF
# Active session handoff

_Written at ${TS} by session \`${SESSION}\` on branch \`${BRANCH}\` (@ ${HEAD})._

## Status: ${STATUS}

${TASK:+Active task: \`${TASK}\`}

## What happened this session

- Diff: ${SHORTSTAT:-(no git changes)}
- Files touched:
${FILES_CHANGED}

## Next session should

${NEXT:-1. Read .claude/STATE.md and .claude/BACKLOG.md.
2. Pick the highest-priority backlog item.
3. Dispatch onboarding-context-loader, then the matching specialist subagent.}

## Recent audit trail

\`\`\`
${RECENT_AUDIT}
\`\`\`

## Open decisions parked

(none — update if a decision was deferred)
EOF

# --- 3. Update STATE.md (durable pointer) ---
cat > .claude/STATE.md <<EOF
# Autonomous-loop state

Current task: ${TASK:-(idle)}
Last session end: ${TS}
Branch: ${BRANCH} @ ${HEAD}
Status: ${STATUS}

# Past sessions

See \`.claude/SESSIONS.md\` (full index) or \`.claude/sessions/\` (per-session archives).
Most recent 5 from AUDIT.log:

$(tail -5 .claude/AUDIT.log 2>/dev/null || echo "(none)")
EOF

# --- 4. Append rich audit entry ---
{
  echo "${TS}  session=${SESSION}  branch=${BRANCH}  head=${HEAD}  task=\"${TASK}\"  status=\"${STATUS}\"  diff=\"${SHORTSTAT}\""
} >> .claude/AUDIT.log

echo "Handoff written. Prior HANDOFF archived to .claude/sessions/. Next session will read .claude/HANDOFF.md on start."
