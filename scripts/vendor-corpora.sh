#!/usr/bin/env bash
# vendor-corpora.sh — vendor a slice of a permissively-licensed OSS repo into
# the Inspect FP corpus (`corpora/vendor/<name>/`).
#
# Real-world code is the strongest false-positive signal: if our rules fire on
# idiomatic upstream LLM code, that's a true FP. This script makes vendoring
# such samples reproducible and license-clean. It is intentionally GENERIC —
# you pass the repo, the pinned ref, and which files to take — so it never bit
# rots against a hardcoded upstream layout. See `corpora/SOURCES.md` for the
# curated source list to feed it.
#
# Requires network (it clones). Vendored files + the upstream LICENSE land
# under corpora/vendor/<name>/; review the licence, then `git add` them.
#
# Usage:
#   ./scripts/vendor-corpora.sh <name> <git-url> <ref> [glob] [max-files]
#
# Example:
#   ./scripts/vendor-corpora.sh openai-cookbook \
#       https://github.com/openai/openai-cookbook <pinned-sha> 'examples/*.py' 8
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ $# -lt 3 ]]; then
  sed -n '2,28p' "$0"
  exit 2
fi

NAME="$1"
URL="$2"
REF="$3"
GLOB="${4:-**/*.py}"
MAX_FILES="${5:-8}"

command -v git >/dev/null || { echo "git required" >&2; exit 2; }

DEST="corpora/vendor/${NAME}"
TMP=$(mktemp -d -t vendor-corpora.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

echo "Cloning ${URL} @ ${REF} …" >&2
git clone --quiet --depth 1 --filter=blob:none "$URL" "$TMP/repo"
git -C "$TMP/repo" fetch --quiet --depth 1 origin "$REF"
git -C "$TMP/repo" checkout --quiet FETCH_HEAD
SHA=$(git -C "$TMP/repo" rev-parse HEAD)

mkdir -p "$DEST"
# Capture the upstream licence (try common filenames).
for lic in LICENSE LICENSE.md LICENSE.txt COPYING; do
  if [[ -f "$TMP/repo/$lic" ]]; then
    cp "$TMP/repo/$lic" "$DEST/LICENSE"
    break
  fi
done

# Copy up to MAX_FILES matching files, preserving basenames.
count=0
while IFS= read -r -d '' f; do
  [[ "$count" -ge "$MAX_FILES" ]] && break
  cp "$f" "$DEST/$(basename "$f")"
  count=$((count + 1))
done < <(cd "$TMP/repo" && find . -path "./$GLOB" -type f -print0 2>/dev/null)

# Record provenance for reproducibility + audit.
cat > "$DEST/.source" <<EOF
name = "${NAME}"
url = "${URL}"
ref = "${REF}"
commit = "${SHA}"
glob = "${GLOB}"
files = ${count}
EOF

echo "Vendored ${count} file(s) + LICENSE into ${DEST} (commit ${SHA})." >&2
if [[ "$count" -eq 0 ]]; then
  echo "WARNING: glob '${GLOB}' matched nothing — check the path against the repo." >&2
fi
echo "Review the licence in ${DEST}/LICENSE, then: git add ${DEST}" >&2
