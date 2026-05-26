#!/usr/bin/env bash
# session-start.sh — inject minimal repo context + active HANDOFF.md.
#
# Hard budget: under 2000 tokens of additionalContext. Tree never loaded.
# If HANDOFF.md exists and has a non-idle status, it takes priority.
set -euo pipefail

cd "${CLAUDE_PROJECT_DIR:-$(pwd)}"

PAUSED_NOTICE=""
if [[ -f ".claude/PAUSED" ]]; then
  PAUSED_NOTICE="\n\n**AUTONOMOUS-LOOP PAUSED**: cost cap exceeded. Read \`.claude/PAUSED\` for details."
fi

BRANCH=$(git branch --show-current 2>/dev/null || echo "(no git)")
HEAD=$(git rev-parse --short HEAD 2>/dev/null || echo "(no commits)")
STATUS=$(git status --short 2>/dev/null | head -15 || echo "")
[[ -z "${STATUS}" ]] && STATUS="(clean working tree)"

HANDOFF_BLOCK=""
if [[ -f ".claude/HANDOFF.md" ]]; then
  HANDOFF_SHORT=$(head -40 .claude/HANDOFF.md)
  HANDOFF_BLOCK="\n\n## Active handoff (from previous session)\n\n\`\`\`\n${HANDOFF_SHORT}\n\`\`\`"
fi

STATE_BLOCK=""
if [[ -f ".claude/STATE.md" ]]; then
  STATE_SHORT=$(head -10 .claude/STATE.md)
  STATE_BLOCK="\n\n## Current pointer\n\n\`\`\`\n${STATE_SHORT}\n\`\`\`"
fi

BACKLOG_BLOCK=""
if [[ -f ".claude/BACKLOG.md" ]]; then
  # Show only the top 3 P0/P1 items, never the whole backlog.
  TOP_ITEMS=$(grep -E '^\- \[P[01]\]' .claude/BACKLOG.md 2>/dev/null | head -3 || echo "")
  if [[ -n "${TOP_ITEMS}" ]]; then
    BACKLOG_BLOCK="\n\n## Top backlog (P0/P1 only — see .claude/BACKLOG.md for full list)\n\n${TOP_ITEMS}"
  fi
fi

# Build the additionalContext. jq -Rs handles JSON escaping cleanly.
CONTEXT=$(cat <<EOF
## Session context

- Branch: \`${BRANCH}\` @ \`${HEAD}\`
- Working tree:
\`\`\`
${STATUS}
\`\`\`
${STATE_BLOCK}${HANDOFF_BLOCK}${BACKLOG_BLOCK}

AGENTS.md will be injected on first user prompt. Use scoped \`cargo check -p <crate>\`. When ending a substantive session, run \`./scripts/session-end.sh "<status>" --task "<task>" --next "<step>"\`.${PAUSED_NOTICE}
EOF
)

# Emit hook output JSON.
printf '%s' "${CONTEXT}" | jq -Rs '{hookSpecificOutput: {hookEventName: "SessionStart", additionalContext: .}}'
