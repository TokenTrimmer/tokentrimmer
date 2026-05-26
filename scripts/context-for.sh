#!/usr/bin/env bash
# context-for.sh — find minimum context for a topic.
#
# Lookup order (cheapest first):
#   1. CONTEXT_MAP.md keyword match (curated)
#   2. DECISIONS.md keyword match (ADR log)
#   3. INDEX.md keyword match (auto-generated)
#   4. Scoped grep over crates/ and docs/
#
# Output: ranked list of `path:line` pointers per hit.
# Used by the onboarding-context-loader subagent and by humans.
set -euo pipefail

cd "$(dirname "$0")/.."

TOPIC="${1:-}"
if [[ -z "${TOPIC}" ]]; then
  echo "usage: context-for.sh <topic-or-keyword>"
  echo "examples:"
  echo "  context-for.sh anthropic"
  echo "  context-for.sh cache_control"
  echo "  context-for.sh 'audit log'"
  exit 1
fi

found_anything=0

# Reusable grep with context.
grep_in() {
  local path="$1"
  local before="${2:-1}"
  local after="${3:-1}"
  [[ -e "$path" ]] || return 0
  grep -i -n -B "$before" -A "$after" --color=never -- "$TOPIC" "$path" 2>/dev/null || true
}

# --- 1. CONTEXT_MAP.md ---
hits=$(grep_in .claude/CONTEXT_MAP.md 1 1)
if [[ -n "${hits}" ]]; then
  echo "## CONTEXT_MAP.md (curated)"
  echo
  printf '%s\n' "${hits}" | sed 's/^/    /'
  echo
  found_anything=1
fi

# --- 2. DECISIONS.md ---
hits=$(grep_in .claude/DECISIONS.md 2 4 | head -40)
if [[ -n "${hits}" ]]; then
  echo "## DECISIONS.md (ADR log)"
  echo
  printf '%s\n' "${hits}" | sed 's/^/    /'
  echo
  found_anything=1
fi

# --- 3. INDEX.md ---
hits=$(grep_in .claude/INDEX.md 0 0 | head -10)
if [[ -n "${hits}" ]]; then
  echo "## INDEX.md (auto-generated)"
  echo
  printf '%s\n' "${hits}" | sed 's/^/    /'
  echo
  found_anything=1
fi

# --- 4. Scoped grep over code/docs ---
echo "## Code/doc matches (grep -rn)"
echo
SEARCH_ROOTS=("crates" "docs" "AGENTS.md" "README.md")
existing_roots=()
for r in "${SEARCH_ROOTS[@]}"; do [[ -e "$r" ]] && existing_roots+=("$r"); done

if [[ ${#existing_roots[@]} -gt 0 ]]; then
  code_hits=$(grep -rin --include='*.rs' --include='*.md' --include='*.toml' \
    --exclude-dir=target --exclude-dir=node_modules \
    -- "$TOPIC" "${existing_roots[@]}" 2>/dev/null | head -30 || true)
  if [[ -n "${code_hits}" ]]; then
    printf '%s\n' "${code_hits}"
    found_anything=1
  else
    echo "_(no matches)_"
  fi
fi

echo
if [[ "${found_anything}" -eq 0 ]]; then
  echo "---"
  echo "**No matches anywhere.** Either:"
  echo "  - The topic is genuinely new — add an entry to \`.claude/CONTEXT_MAP.md\` when you figure it out."
  echo "  - Try a different keyword (e.g. exact symbol name or file path)."
  echo "  - Run \`./scripts/context-map.sh\` to refresh INDEX.md if the repo changed recently."
fi
