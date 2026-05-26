#!/usr/bin/env bash
# post-session-reflect.sh — analyze the just-ended session and surface tuning candidates.
#
# Looks for:
#   - Files that were edited and then re-edited within the same session (rework — agent prompt may be too vague)
#   - Failed hook invocations (pre-edit-guard denies → agent didn't read the rules)
#   - Long sessions that produced no commits (agent stuck, scope too broad)
#
# Output: a JSON object printed to stdout + appended to .claude/reflections.jsonl.
# A weekly aggregation (in weekly-review.sh) feeds the loudest signals into BACKLOG.
set -euo pipefail

cd "$(dirname "$0")/.."

SESSION="${CLAUDE_SESSION_ID:-${1:-unknown}}"
mkdir -p .claude

# Files re-edited (heuristic: same file appears more than once in git reflog of this session — hard
# to read without session start SHA, so we approximate by counting unstaged + recent commit changes).
rework_count=0
if [[ -d .git ]]; then
  # Count files appearing twice in `git log --name-only` in the last hour.
  rework_count=$(git log --since="1 hour ago" --name-only --pretty=format: 2>/dev/null \
    | grep -v '^$' | sort | uniq -c | awk '$1 > 1 {n++} END {print n+0}')
fi

# Hook denials this session: pre-edit-guard writes deny reasons to stderr but we can't capture from here.
# Use AUDIT.log as proxy.
hook_denies=0
if [[ -f .claude/AUDIT.log ]]; then
  hook_denies=$(grep -c "permissionDecision.*deny" .claude/AUDIT.log 2>/dev/null || echo 0)
fi

# Stuck-detection: was anything committed?
recent_commits=$(git log --since="2 hours ago" --pretty=oneline 2>/dev/null | wc -l | tr -d ' ')

# Context-map gap heuristic: heavy file changes with no commits often = exploration mode.
# Real signal would require tool-call telemetry; this is a cheap proxy until we have that.
exploration_signal="false"
if [[ "${recent_commits}" -le 1 ]]; then
  files_touched=$(git status --short 2>/dev/null | wc -l | tr -d ' ' || echo 0)
  [[ "${files_touched}" -gt 5 ]] && exploration_signal="true"
fi

# Build reflection JSON.
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
reflection=$(jq -n \
  --arg ts "${TS}" \
  --arg session "${SESSION}" \
  --argjson rework "${rework_count}" \
  --argjson denies "${hook_denies}" \
  --argjson commits "${recent_commits}" \
  --argjson exploration "${exploration_signal}" \
  '{ts: $ts, session: $session, rework_files: $rework, hook_denies: $denies, commits: $commits,
    exploration_likely: $exploration,
    signals: (
      [if $rework > 2 then "rework_pattern" else empty end,
       if $denies > 0 then "hit_pre_edit_guard" else empty end,
       if $commits == 0 then "no_commits" else empty end,
       if $exploration then "context_map_gap_likely" else empty end])
  }')

echo "${reflection}" >> .claude/reflections.jsonl
echo "${reflection}" | jq .
