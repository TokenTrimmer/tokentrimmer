#!/usr/bin/env bash
# sync-licenses.sh — keep a LICENSE + NOTICE copy in every PUBLISHABLE crate.
#
# WHY
#   `cargo package` only bundles files that live inside the crate's own
#   directory and are tracked by git. A workspace-root LICENSE/NOTICE is NOT
#   included in `cargo publish` tarballs, and cargo refuses to package symlinks
#   that escape the package root. So each publishable crate needs its own real
#   LICENSE + NOTICE file. cargo then auto-includes any file named LICENSE* /
#   NOTICE* in the package — no `include` directive required.
#
#   Rather than hand-maintain 25 copies (drift bait), this script copies the
#   single source-of-truth root LICENSE + NOTICE into every publishable crate.
#   "Publishable" = a workspace member whose `publish` is not `false`
#   (i.e. everything except tt-ts-types). The set is derived live from
#   `cargo metadata`, so adding/removing a crate needs no edit here.
#
# USAGE
#   scripts/sync-licenses.sh            # copy root LICENSE+NOTICE into crates
#   scripts/sync-licenses.sh --check    # verify they're present + up to date
#                                       # (exit 1 on drift; used by CI)
#
# Verify a sample crate ships them:
#   cargo package --list -p tt-shared | grep -E '^(LICENSE|NOTICE)$'

set -euo pipefail

cd "$(dirname "$0")/.."
root="$(pwd)"

check_mode=0
if [ "${1:-}" = "--check" ]; then
  check_mode=1
elif [ -n "${1:-}" ]; then
  echo "usage: $0 [--check]" >&2
  exit 2
fi

for f in LICENSE NOTICE; do
  if [ ! -f "$root/$f" ]; then
    echo "::error::root $f is missing — cannot sync licenses" >&2
    exit 1
  fi
done

# Manifest paths of every publishable workspace member (publish != false).
# `cargo metadata` reports `publish` as null (allowed), an empty array
# (disallowed), or an array of registries; only the empty array means skip.
# `while read` (not `mapfile`) so this also runs on macOS's bundled bash 3.2.
manifests=()
while IFS= read -r line; do
  manifests+=("$line")
done < <(
  cargo metadata --no-deps --format-version 1 \
    | jq -r '.packages[] | select(.publish != []) | .manifest_path'
)

if [ "${#manifests[@]}" -eq 0 ]; then
  echo "::error::no publishable crates found via cargo metadata" >&2
  exit 1
fi

drift=0
synced=0
for manifest in "${manifests[@]}"; do
  crate_dir="$(dirname "$manifest")"
  for f in LICENSE NOTICE; do
    dest="$crate_dir/$f"
    if [ "$check_mode" -eq 1 ]; then
      if ! cmp -s "$root/$f" "$dest"; then
        rel="${crate_dir#"$root"/}"
        echo "::error::$rel/$f is missing or differs from root $f (run scripts/sync-licenses.sh)" >&2
        drift=1
      fi
    else
      cp "$root/$f" "$dest"
      synced=$((synced + 1))
    fi
  done
done

if [ "$check_mode" -eq 1 ]; then
  if [ "$drift" -ne 0 ]; then
    exit 1
  fi
  echo "ok: LICENSE + NOTICE present and current in all ${#manifests[@]} publishable crates"
else
  echo "synced LICENSE + NOTICE into ${#manifests[@]} publishable crates ($synced files)"
fi
