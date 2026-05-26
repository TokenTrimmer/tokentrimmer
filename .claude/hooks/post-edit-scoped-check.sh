#!/usr/bin/env bash
# post-edit-scoped-check.sh — run cheap scoped checks on the changed file's crate/package.
# The single highest-ROI token saver: avoids whole-workspace cargo runs on every edit.
set -uo pipefail

INPUT=$(cat)
FILE_PATH=$(printf '%s' "${INPUT}" | jq -r '.tool_input.file_path // empty')

if [[ -z "${FILE_PATH}" ]]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(pwd)}"

emit_context() {
  local msg="$1"
  # Truncate very long output (e.g., 200 clippy warnings) to keep context bounded.
  local truncated
  truncated=$(printf '%s' "${msg}" | head -c 4000)
  cat <<EOF
{
  "hookSpecificOutput": {
    "hookEventName": "PostToolUse",
    "additionalContext": "${truncated}"
  }
}
EOF
}

# --- Rust files: find owning crate by walking up to Cargo.toml ---
if [[ "${FILE_PATH}" == *.rs ]]; then
  DIR=$(dirname "${FILE_PATH}")
  CRATE_DIR=""
  while [[ "${DIR}" != "/" && "${DIR}" != "." ]]; do
    if [[ -f "${DIR}/Cargo.toml" ]] && ! grep -q '^\[workspace\]' "${DIR}/Cargo.toml"; then
      CRATE_DIR="${DIR}"
      break
    fi
    DIR=$(dirname "${DIR}")
  done

  if [[ -z "${CRATE_DIR}" ]]; then
    # Edited file isn't inside a crate yet (probably workspace root); skip.
    exit 0
  fi

  CRATE_NAME=$(grep -E '^name\s*=' "${CRATE_DIR}/Cargo.toml" | head -1 | sed -E 's/name\s*=\s*"([^"]+)".*/\1/')
  if [[ -z "${CRATE_NAME}" ]]; then
    exit 0
  fi

  # Run cargo check scoped to the crate. Capture output for feedback.
  CHECK_OUTPUT=$(cargo check -p "${CRATE_NAME}" --message-format short 2>&1 || true)
  CHECK_EXIT=$?

  if [[ "${CHECK_EXIT}" -ne 0 ]] || printf '%s' "${CHECK_OUTPUT}" | grep -qE '^error'; then
    emit_context "## Scoped cargo check failed for crate '${CRATE_NAME}'\n\n\`\`\`\n${CHECK_OUTPUT}\n\`\`\`\n\nFix before next edit. Hook: post-edit-scoped-check.sh"
    exit 0
  fi

  # Optional clippy if quick. Skip on test files (handled below).
  if [[ "${FILE_PATH}" != *"/tests/"* && "${FILE_PATH}" != *"_test.rs" ]]; then
    CLIPPY_OUTPUT=$(cargo clippy -p "${CRATE_NAME}" --message-format short -- -D warnings 2>&1 || true)
    if printf '%s' "${CLIPPY_OUTPUT}" | grep -qE '^(error|warning)'; then
      emit_context "## Clippy issues in '${CRATE_NAME}'\n\n\`\`\`\n${CLIPPY_OUTPUT}\n\`\`\`\nHook: post-edit-scoped-check.sh"
      exit 0
    fi
  fi
  exit 0
fi

# --- Astro/TS files: typecheck the owning pnpm workspace package ---
if [[ "${FILE_PATH}" == *.ts || "${FILE_PATH}" == *.tsx || "${FILE_PATH}" == *.astro ]]; then
  DIR=$(dirname "${FILE_PATH}")
  PKG_DIR=""
  while [[ "${DIR}" != "/" && "${DIR}" != "." ]]; do
    if [[ -f "${DIR}/package.json" ]]; then
      PKG_DIR="${DIR}"
      break
    fi
    DIR=$(dirname "${DIR}")
  done

  if [[ -z "${PKG_DIR}" ]]; then
    exit 0
  fi

  PKG_NAME=$(jq -r '.name // empty' "${PKG_DIR}/package.json")
  if [[ -z "${PKG_NAME}" ]]; then
    exit 0
  fi

  # Only run typecheck if the package defines it.
  if jq -e '.scripts.typecheck // empty' "${PKG_DIR}/package.json" >/dev/null 2>&1; then
    TS_OUTPUT=$(pnpm --filter "${PKG_NAME}" typecheck 2>&1 || true)
    if printf '%s' "${TS_OUTPUT}" | grep -qE 'error TS[0-9]+'; then
      emit_context "## Typecheck failed in '${PKG_NAME}'\n\n\`\`\`\n${TS_OUTPUT}\n\`\`\`\nHook: post-edit-scoped-check.sh"
    fi
  fi
fi

exit 0
