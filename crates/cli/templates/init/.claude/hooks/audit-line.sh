#!/usr/bin/env bash
# Append a one-line audit entry per session to .claude/AUDIT.log
set -euo pipefail

LOG=".claude/AUDIT.log"
[[ -f "${LOG}" ]] || touch "${LOG}"
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
HEAD=$(git rev-parse --short HEAD 2>/dev/null || echo "no-git")
SESSION="${CLAUDE_SESSION_ID:-unknown}"
echo "${TS}  session=${SESSION}  head=${HEAD}" >> "${LOG}"
