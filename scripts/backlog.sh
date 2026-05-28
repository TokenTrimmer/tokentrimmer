#!/usr/bin/env bash
# backlog.sh — CLI for .claude/BACKLOG.md and GitHub Issues sync.
#
# Subcommands:
#   list                       show open P0/P1 items
#   take                       print the next item (P0 > P1 > P2 > P3) and its task-id
#   add <P> <task-id> <agent> <desc> [est] add a new item
#   done <task-id>             mark item completed
#   sync                       sync open items to GitHub Issues with `autopilot` label
set -euo pipefail

cd "$(dirname "$0")/.."

BACKLOG=".claude/BACKLOG.md"
[[ -f "${BACKLOG}" ]] || { echo "no .claude/BACKLOG.md"; exit 1; }

cmd="${1:-list}"
shift || true

case "${cmd}" in
  list)
    echo "P0/P1 open items:"
    grep -E '^\- \[ \] \[P[01]\]' "${BACKLOG}" || echo "  (none)"
    ;;

  take)
    # Highest priority open item that is NOT tagged [BLOCKED ...].
    line=""
    for prio in P0 P1 P2 P3; do
      line=$(grep -E "^\- \[ \] \[${prio}\]" "${BACKLOG}" | grep -Ev '\[BLOCKED|\[DEFERRED|\[NEEDS-SPEC|\[NEEDS-PLAN' | head -1 || true)
      [[ -n "${line}" ]] && break
    done
    if [[ -z "${line}" ]]; then
      echo "no open (unblocked) items"
      exit 1
    fi
    # Extract task-id (between second and third [...] pair).
    task_id=$(echo "${line}" | sed -E 's/^\- \[ \] \[P[0-3]\] \[([a-zA-Z0-9-]+)\].*/\1/')
    echo "task_id=${task_id}"
    echo "line=${line}"
    ;;

  add)
    prio="${1:-}"; task_id="${2:-}"; agent="${3:-}"; desc="${4:-}"; est="${5:-?}"
    if [[ -z "${prio}" || -z "${task_id}" || -z "${agent}" || -z "${desc}" ]]; then
      echo "usage: backlog.sh add <P0|P1|P2|P3> <task-id> <agent> <description> [estimate]"
      exit 1
    fi
    new_line="- [ ] [${prio}] [${task_id}] ${agent}: ${desc} (est: \$${est})"
    # Append above the "## Completed" section, or at end if none.
    if grep -q '^## Completed' "${BACKLOG}"; then
      awk -v line="${new_line}" '/^## Completed/ {print line; print ""} {print}' "${BACKLOG}" > "${BACKLOG}.tmp"
      mv "${BACKLOG}.tmp" "${BACKLOG}"
    else
      echo "${new_line}" >> "${BACKLOG}"
    fi
    echo "added: ${new_line}"
    ;;

  done)
    task_id="${1:-}"
    [[ -z "${task_id}" ]] && { echo "usage: backlog.sh done <task-id>"; exit 1; }
    # Flip [ ] to [x] for the line containing this task-id.
    sed -i.bak -E "s/^(\- )\[ \](.*\[${task_id}\])/\1[x]\2/" "${BACKLOG}"
    rm -f "${BACKLOG}.bak"
    echo "marked done: ${task_id}"
    ;;

  sync)
    # Sync open items to GitHub Issues with `autopilot` label.
    # Idempotent: existing issues identified by [task-id] in the title.
    if ! command -v gh >/dev/null 2>&1; then
      echo "gh CLI required; install or 'brew install gh'"
      exit 1
    fi
    existing=$(gh issue list --label autopilot --state open --limit 100 --json title --jq '.[].title' 2>/dev/null || echo "")
    grep -E '^\- \[ \] \[P[0-3]\]' "${BACKLOG}" | grep -Ev '\[BLOCKED|\[DEFERRED|\[NEEDS-SPEC|\[NEEDS-PLAN' | while IFS= read -r line; do
      task_id=$(echo "${line}" | sed -E 's/^\- \[ \] \[P[0-3]\] \[([a-zA-Z0-9-]+)\].*/\1/')
      desc=$(echo "${line}" | sed -E 's/^\- \[ \] \[P[0-3]\] \[[a-zA-Z0-9-]+\] [a-z-]+: (.*) \(est:.*/\1/')
      title="[${task_id}] ${desc}"
      if printf '%s\n' "${existing}" | grep -qF "[${task_id}]"; then
        echo "skip (exists): ${title}"
      else
        gh issue create --label autopilot --title "${title}" --body "Source: \`.claude/BACKLOG.md\` line:\n\n\`\`\`\n${line}\n\`\`\`" >/dev/null
        echo "created: ${title}"
      fi
    done
    ;;

  *)
    echo "unknown subcommand: ${cmd}"
    echo "usage: backlog.sh {list|take|add|done|sync}"
    exit 1
    ;;
esac
