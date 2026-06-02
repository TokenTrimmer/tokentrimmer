#!/usr/bin/env bash
# Block edits to files containing common secret patterns. Add lines for
# repo-specific patterns.
set -euo pipefail

FILE="${CLAUDE_EDIT_FILE:-}"
[[ -z "${FILE}" ]] && exit 0
[[ ! -f "${FILE}" ]] && exit 0

if grep -qE 'sk-[a-zA-Z0-9_]{20,}|AIza[0-9A-Za-z_-]{35}|ghp_[a-zA-Z0-9]{36}' "${FILE}"; then
  echo "BLOCKED: ${FILE} contains a high-entropy token. Refuse edit." >&2
  exit 1
fi

if [[ "${FILE}" =~ \.env$|\.env\.local$ ]]; then
  echo "BLOCKED: refusing to edit ${FILE}. Use .env.example or env-specific tooling." >&2
  exit 1
fi

exit 0
