# Scripts & corpora hardening (batch 5a) — Design

**Status:** approved (design + scope, 2026-06-08)
**Date:** 2026-06-08
**Slice:** Audit-remediation, public repo, `scripts/` + `corpora/`. Locally-verifiable script/corpora hardening + docs. The two CI-gate findings (FP-rate gate #1, shellcheck #4) are split into a separate validated slice (5b).

## Scope (user-approved)
5a: #2 (vendor ref-pin), #3 (corpora docs), #5 (vendor glob), #6 (backlog sed escaping), #4-partial (refresh-pricing.py pure-helper unit test). 5b (separate): #1 FP-rate CI gate + #4 shellcheck job.

## Fixes

### #6 — `backlog.sh done` interpolates task-id into a sed regex unescaped (`scripts/backlog.sh:61-64`)
`sed -i.bak -E "s/^(\- )\[ \](.*\[${task_id}\])/\1[x]\2/"` injects `$task_id` raw into the ERE. A task-id with `/`, `.`, `[`, `*` etc. breaks or mis-matches.
**Fix:** validate the task-id against the canonical charset (the `sync` branch already uses `[a-zA-Z0-9-]+`) before the sed; reject otherwise. With the charset limited to alnum + hyphen, no ERE-special character can reach the pattern, closing the injection without fragile escaping.
```bash
if [[ ! "${task_id}" =~ ^[A-Za-z0-9-]+$ ]]; then
  echo "invalid task-id '${task_id}' (expected [A-Za-z0-9-]+)" >&2; exit 1
fi
```

### #5 — `vendor-corpora.sh` glob uses `find -path` semantics that miss top-level files (`:32,64`)
Default `GLOB='**/*.py'` → `find . -path './**/*.py'`; `*` crosses `/`, so `**` collapses to `*` and the leading-segment requirement skips top-level files (`./foo.py`).
**Fix:** dispatch on whether the glob contains a `/`. A bare pattern (`*.py`) uses `-name` (matches every depth incl. top-level); a path pattern (`examples/*.py`) keeps `-path` (documented as `find -path` semantics, not shell-glob). Change the default to `*.py`. Warn when fewer than `MAX_FILES` matched so partial/incorrect globs are visible.
```bash
GLOB="${4:-*.py}"
...
if [[ "$GLOB" == */* ]]; then find_match=(-path "./$GLOB"); else find_match=(-name "$GLOB"); fi
... find . -type f "${find_match[@]}" -print0 ...
# after copy loop:
if [[ "$count" -eq 0 ]]; then echo "WARNING: glob '${GLOB}' matched nothing …" >&2
elif [[ "$count" -lt "$MAX_FILES" ]]; then echo "note: matched ${count} file(s) (< MAX_FILES=${MAX_FILES}); verify the glob captured the intended set." >&2; fi
```
(Doc the `-path` semantics for slash-globs in the usage header.)

### #2 — Vendored slices recorded `ref="HEAD"`, defeating the pinned-SHA control (`vendor-corpora.sh:31,41-45`; 4 `.source` files)
The script accepted `REF=HEAD` and fetched the moving default-branch tip — non-reproducible. All four committed `.source` files record `ref = "HEAD"` (though each also records the resolved `commit = <sha>`).
**Fix (script):** reject `REF == HEAD`; require a 40-hex SHA; after checkout assert `git rev-parse HEAD == REF` so a moved/rewritten tip fails loud.
```bash
if [[ ! "$REF" =~ ^[0-9a-f]{40}$ ]]; then
  echo "REF must be a full 40-hex commit SHA (got '${REF}'); pin a commit, not a branch/HEAD." >&2; exit 2
fi
... checkout ...
SHA=$(git -C "$TMP/repo" rev-parse HEAD)
if [[ "$SHA" != "$REF" ]]; then echo "checked-out HEAD ${SHA} != requested REF ${REF}" >&2; exit 1; fi
```
**Fix (metadata backfill, no network):** the committed slices are already at the recorded `commit` SHA, so set each `.source`'s `ref = "<commit-sha>"` (the value already present in `commit =`). This realizes the "pinned" intent for the committed provenance without re-fetching external code.

### #3 — Corpora README/SOURCES contradict the committed state (`corpora/README.md`, `corpora/SOURCES.md`)
README says samples are authored-in-repo and vendoring "needs network, a follow-up"; SOURCES says `vendor/<name>` is "Empty until vendored … isn't committed by default." Both false: `corpora/vendor/` holds 4 committed upstream slices (openai-python, openai-cookbook, anthropic-sdk-python, vercel-ai) with LICENSE + `.source`.
**Fix:** README — keep the authored-samples description for `corpora/samples/`, but correct the claim that vendoring hasn't happened; point to `vendor/` being populated. SOURCES — mark `vendor/` as populated, list the four vendored sources with their pinned commit SHAs and captured licences, keep the two-tier framing and the `langchain` row as still-suggested/not-yet-vendored.

### #4-partial — no-network unit test for `refresh-pricing.py` pure helpers (`scripts/test_refresh_pricing.py`, new)
`per_m` (USD/token→USD/1M, with `None`/`""`/`"0"` handling) and `latest_entries` (most-recent entry per (provider,model) from a TOML catalog) are pure. Add a test mirroring `scripts/test_refresh_models.py` (plain `assert` + `__main__`), exercising:
- `per_m`: `None→None`, `""→None`, `"0"→0.0`, `0→0.0`, `"0.000002"→2.0`, `"bad"→None`.
- `latest_entries`: a temp TOML with two `effective_at` rows for one (provider,model) returns only the newest; distinct models both returned, sorted.

## Out of scope (→ 5b)
- #1 FP-rate gate CI job; #4 shellcheck CI job. (Validated-locally follow-up.)
- Re-vendoring upstream code over the network (the metadata backfill suffices; no external fetch performed).

## Testing
- `bash -n` parse-check all edited scripts; targeted behavioral checks: backlog.sh rejects a bad task-id and still flips a good one; vendor-corpora.sh `find` dispatch matches top-level + nested for a bare glob (dry harness, no network).
- `python3 scripts/test_refresh_pricing.py` passes.
- No Rust touched → no cargo gates needed; `cargo fmt`/clippy unaffected.
