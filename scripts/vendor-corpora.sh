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
# <ref> MUST be a full 40-hex commit SHA (not a branch or HEAD) so the fetch is
# reproducible and auditable; the script asserts the checked-out commit equals it.
#
# [glob] selects which files to take. A bare pattern (e.g. '*.py') matches files
# at ANY depth via `find -name`. A pattern containing '/' (e.g. 'examples/*.py')
# is a `find -path` pattern — note `*` crosses '/' in -path, so 'examples/*.py'
# also matches 'examples/sub/foo.py'. Default: '*.py'.
#
# Example:
#   ./scripts/vendor-corpora.sh openai-cookbook \
#       https://github.com/openai/openai-cookbook <40-hex-sha> 'examples/*.py' 8
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ $# -lt 3 ]]; then
  sed -n '2,34p' "$0"
  exit 2
fi

NAME="$1"
URL="$2"
REF="$3"
GLOB="${4:-*.py}"
MAX_FILES="${5:-8}"

command -v git >/dev/null || { echo "git required" >&2; exit 2; }

# Require a pinned full SHA — a branch name or HEAD would resolve to a moving
# tip at fetch time, making the vendored slice non-reproducible.
if [[ ! "$REF" =~ ^[0-9a-f]{40}$ ]]; then
  echo "REF must be a full 40-hex commit SHA (got '${REF}'); pin a commit, not a branch/HEAD." >&2
  exit 2
fi

DEST="corpora/vendor/${NAME}"
TMP=$(mktemp -d -t vendor-corpora.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

echo "Cloning ${URL} @ ${REF} …" >&2
git clone --quiet --depth 1 --filter=blob:none "$URL" "$TMP/repo"
git -C "$TMP/repo" fetch --quiet --depth 1 origin "$REF"
git -C "$TMP/repo" checkout --quiet FETCH_HEAD
SHA=$(git -C "$TMP/repo" rev-parse HEAD)
# Fail loud if the resolved commit isn't the pinned SHA (moved/rewritten tip).
if [[ "$SHA" != "$REF" ]]; then
  echo "checked-out HEAD ${SHA} != requested REF ${REF}" >&2
  exit 1
fi

mkdir -p "$DEST"
# Capture the upstream licence (try common filenames).
for lic in LICENSE LICENSE.md LICENSE.txt COPYING; do
  if [[ -f "$TMP/repo/$lic" ]]; then
    cp "$TMP/repo/$lic" "$DEST/LICENSE"
    break
  fi
done

# Copy up to MAX_FILES matching files, preserving basenames.
# `find` emits repo-relative paths (e.g. ./examples/foo.py); the copy must
# resolve them against the clone dir, not the caller's CWD.
# A bare glob ('*.py') matches every depth via -name (including top-level files,
# which `-path './**/*.py'` would skip); a path glob ('examples/*.py') uses -path.
if [[ "$GLOB" == */* ]]; then
  find_match=(-path "./$GLOB")
else
  find_match=(-name "$GLOB")
fi
count=0
while IFS= read -r -d '' f; do
  [[ "$count" -ge "$MAX_FILES" ]] && break
  cp "$TMP/repo/$f" "$DEST/$(basename "$f")"
  count=$((count + 1))
done < <(cd "$TMP/repo" && find . -type f "${find_match[@]}" -print0 2>/dev/null)

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
elif [[ "$count" -lt "$MAX_FILES" ]]; then
  echo "note: matched ${count} file(s) (< MAX_FILES=${MAX_FILES}); verify the glob captured the intended set." >&2
fi
echo "Review the licence in ${DEST}/LICENSE, then: git add ${DEST}" >&2
