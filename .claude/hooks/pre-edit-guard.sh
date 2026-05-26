#!/usr/bin/env bash
# pre-edit-guard.sh — enforce repo rules at edit time (cheaper than discovering at CI time).
# Dogfoods our own Inspect rules:
#   - config-agents-md-too-long: blocks AGENTS.md edits pushing past ~4K tokens
#   - config-agents-md-contains-secrets: blocks any file with secret patterns
#   - lib-anthropic-sdk-no-cache-control: blocks long Anthropic system= without cache_control
#   - prompt-bloated-system: blocks oversized Rust source files
set -euo pipefail

# Read tool input from stdin (Claude Code passes JSON).
INPUT=$(cat)
FILE_PATH=$(printf '%s' "${INPUT}" | jq -r '.tool_input.file_path // empty')
CONTENT=$(printf '%s' "${INPUT}" | jq -r '.tool_input.content // .tool_input.new_string // empty')

# No path or content? Let it through (Edit with replace_all, etc.).
if [[ -z "${FILE_PATH}" || -z "${CONTENT}" ]]; then
  exit 0
fi

deny() {
  local reason="$1"
  cat <<EOF
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "${reason}"
  }
}
EOF
  exit 0
}

# --- Rule 1: file size on .rs files (prompt-bloated-system analog) ---
if [[ "${FILE_PATH}" == *.rs ]]; then
  LINE_COUNT=$(printf '%s' "${CONTENT}" | wc -l | tr -d ' ')
  if [[ "${LINE_COUNT}" -gt 800 ]]; then
    deny "File exceeds 800 lines (${LINE_COUNT}). Split the module — our own prompt-bloated-system equivalent for source files. Hook: pre-edit-guard.sh"
  fi
fi

# --- Rule 2: AGENTS.md token budget (config-agents-md-too-long) ---
if [[ "$(basename "${FILE_PATH}")" == "AGENTS.md" || "$(basename "${FILE_PATH}")" == "CLAUDE.md" ]]; then
  # Rough token estimate: 4 chars/token. 4000 tokens ≈ 16000 chars.
  CHAR_COUNT=$(printf '%s' "${CONTENT}" | wc -c | tr -d ' ')
  if [[ "${CHAR_COUNT}" -gt 16000 ]]; then
    deny "AGENTS.md/CLAUDE.md would exceed 4K-token budget (estimated ${CHAR_COUNT} chars, ~$((CHAR_COUNT / 4)) tokens). Move detail into per-crate AGENTS.md or docs/. Hook: pre-edit-guard.sh enforcing our own config-agents-md-too-long rule."
  fi
fi

# --- Rule 3: secret patterns (config-agents-md-contains-secrets, generalized) ---
# Anthropic, OpenAI, Stripe, AWS, generic high-entropy near "key"/"secret"/"token".
SECRET_PATTERNS=(
  'sk-ant-[A-Za-z0-9_-]{32,}'
  'sk-[A-Za-z0-9]{20,}'
  'sk_live_[A-Za-z0-9]{20,}'
  'sk_test_[A-Za-z0-9]{20,}'
  'AKIA[0-9A-Z]{16}'
  'AIza[0-9A-Za-z_-]{35}'
  'ghp_[A-Za-z0-9]{36,}'
  'github_pat_[A-Za-z0-9_]{82,}'
  'xox[baprs]-[A-Za-z0-9-]{10,}'
  'tt_live_[A-Za-z0-9]{20,}'
)

# Allow obvious placeholders/examples.
SAFE_PLACEHOLDERS='(EXAMPLE|PLACEHOLDER|REDACTED|YOUR_KEY|XXXXXXXX|\.\.\.|\<key\>|abc123)'

for pattern in "${SECRET_PATTERNS[@]}"; do
  if printf '%s' "${CONTENT}" | grep -Eq "${pattern}"; then
    MATCH=$(printf '%s' "${CONTENT}" | grep -Eo "${pattern}" | head -1)
    if ! printf '%s' "${MATCH}" | grep -Eqi "${SAFE_PLACEHOLDERS}"; then
      deny "Potential secret detected in ${FILE_PATH} (pattern matched: ${pattern}). If this is intentional (e.g. a fixture), prefix with EXAMPLE_/PLACEHOLDER_ or use a safe placeholder. Hook: pre-edit-guard.sh enforcing our own config-agents-md-contains-secrets rule."
    fi
  fi
done

# --- Rule 4: lib-anthropic-sdk-no-cache-control ---
# Anthropic messages.create with a long literal system= but no cache_control nearby.
if printf '%s' "${CONTENT}" | grep -qE 'messages\.create|client\.messages\.create'; then
  # Look for system="..." with > 200 chars and no cache_control in surrounding 30 lines.
  SYSTEM_LONG=$(printf '%s' "${CONTENT}" | grep -cE 'system\s*=\s*["'\''][^"'\'']{200,}' || true)
  if [[ "${SYSTEM_LONG}" -gt 0 ]]; then
    if ! printf '%s' "${CONTENT}" | grep -q 'cache_control'; then
      deny "Anthropic messages.create() with long literal system= but no cache_control. Add 'cache_control': {'type': 'ephemeral'} on system block to enable 90% cache discount. Hook: pre-edit-guard.sh enforcing our own lib-anthropic-sdk-no-cache-control rule."
    fi
  fi
fi

# All checks passed.
exit 0
