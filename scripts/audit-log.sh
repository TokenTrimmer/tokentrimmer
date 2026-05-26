#!/usr/bin/env bash
# audit-log.sh — manually append an audit line for events that aren't covered
# by hooks (e.g. external infra changes).
set -euo pipefail

cd "$(dirname "$0")/.."

EVENT="${1:-}"
DETAIL="${2:-}"
if [[ -z "${EVENT}" ]]; then
  echo "usage: audit-log.sh <event> [detail]"
  exit 1
fi

mkdir -p .claude
touch .claude/AUDIT.log

TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
USER=$(whoami)
echo "${TS}  manual  event=${EVENT}  detail=\"${DETAIL}\"  user=${USER}" >> .claude/AUDIT.log
