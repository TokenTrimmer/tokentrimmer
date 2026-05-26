#!/usr/bin/env bash
# inject-agents-md-once.sh — inject root AGENTS.md exactly once per session.
# Avoids the "re-load AGENTS.md every turn" anti-pattern; matches our own
# config-agents-md-too-long rule philosophy by minimizing recurring context.
set -euo pipefail

cd "${CLAUDE_PROJECT_DIR:-$(pwd)}"

SESSION_ID="${CLAUDE_SESSION_ID:-unknown}"
SENTINEL="/tmp/tt-session-${SESSION_ID}-agents-loaded"

# Already injected this session? No-op.
if [[ -f "${SENTINEL}" ]]; then
  exit 0
fi

if [[ ! -f "AGENTS.md" ]]; then
  exit 0
fi

touch "${SENTINEL}"

# Emit only the AGENTS.md content (already capped at <4K tokens by pre-edit-guard).
AGENTS_CONTENT=$(cat AGENTS.md)

# JSON-escape the content.
JSON_CONTENT=$(printf '%s' "${AGENTS_CONTENT}" | jq -Rs .)

cat <<EOF
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "## AGENTS.md (injected once per session)\n\n${JSON_CONTENT}"
  }
}
EOF
