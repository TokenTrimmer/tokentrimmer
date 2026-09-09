# Active session handoff

_Written at 2026-09-09T02:20:08Z by session `20260909-55457` on branch `fix/retrieval-policy-boundary` (@ 76d9f42)._

## Status: Batch 1 merged via public #403/cloud #549/web #62. S01 retrieval and direct-embedding follow-up validated: 1128 core + 32 retrieval + 32 integration tests, 4 DB ignores; Clippy/fmt/Inspect clean.

Active task: `september-review-retrieval-boundary`

## What happened this session

- Diff:  11 files changed, 262 insertions(+), 739 deletions(-)
- Files touched:
- `.claude/CONTEXT_MAP.md`
- `PROJECT_REVIEW.md`
- `crates/core/src/middleware/retrieval.rs`
- `crates/core/src/passes/redaction.rs`
- `crates/core/src/routes/chat.rs`
- `crates/core/src/routes/chat/preparation.rs`
- `crates/core/src/routes/embeddings.rs`
- `crates/core/src/routes/messages.rs`
- `crates/retrieval/src/substitute.rs`
- `docs/routing-rules-guide.md`
- `docs/tt-retrieval-usage.md`

## Next session should

Read ../cloud/docs/reviews/2026-09-05-remediation.md for integration refs and all 59 IDs. Next implementation: S03 explicit durable audit/signer boot/readiness and committed business-event audit delivery; separately retain S01 agent/workflow auxiliary and hosted Pg/L2/fleet acceptance. Retrieval now selects original input before substitution and guards queries, including whole-tag fallback and assistant-origin queries. Direct embeddings redact every batch element. Preserve pre-existing deletion of PROJECT_REVIEW.md. Use CARGO_TARGET_DIR=/tmp/tokentrimmer-review-target CARGO_INCREMENTAL=0; full core unit suite needs ~140 seconds.

## Recent audit trail

```
    2026-06-04T01:38:25Z  session=20260604-81768  branch=fix/public-ci-deploy-gating  model=unknown  shortstat=""  files=[]
    2026-06-04T01:47:19Z  session=20260604-85288  branch=fix/public-ci-deploy-gating  model=unknown  shortstat=""  files=[]
    2026-06-04T01:53:00Z  session=20260604-87567  branch=fix/public-ci-deploy-gating  model=unknown  shortstat=""  files=[]
    2026-06-04T02:09:56Z  session=20260604-93477  branch=feat/v0-cli-foundation  model=unknown  shortstat=" 2 files changed, 4 insertions(+)"  files=[.gitignore,crates/cli/templates/init/.gitignore.append]
    2026-09-09T01:39:52Z  session=20260909-15730  branch=fix/september-review-correctness  head=ec52b98  task="september-review-batch1"  status="September review batch: S07/S08 locally verified; S01 primary routing fixed, pre-handler retrieval privacy still open. Full core/dashboard/web checks passed with explicit DB/UI skips."  diff=" 6 files changed, 100 insertions(+), 250 deletions(-)"
```

## Open decisions parked

(none — update if a decision was deferred)
