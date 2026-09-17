# Active session handoff

_Written at 2026-09-17 by session on branch `main` (public @ 0b403443, cloud @ d478a43a)._

## Status: Governance refresh batches 64–68 — the 156-command cloud sweep went from **130 pass / 26 fail** to **153 pass / 3 fail**. Public PR #453 (merged `0b403443`) repaired a real `main` test failure class: the S02 `Streaming` capability guard began suppressing `stream:true` routes on mocks that did not declare `Capability::Streaming`, turning five `tt-core` binaries red. Cloud PRs #663–#667 advanced the engine pin to `0b403443`, re-minted every pin-coupled record, migrated the R07 workload-policy form back to shared controls, and fixed a real Firefox-only WCAG-AA color-contrast violation in the workflow canvas. No deployment or launch approval.

Active task: `governance-refresh-batches-64-68`

## What happened this session

- Public #453 `0b403443`: `test: declare Streaming capability on streaming dispatch mocks` — 5 red `tt-core` binaries fixed; `cargo test -p tt-core --tests` green across 99 binaries.
- Cloud #663 (batch 64): governance evidence refresh + R07 `WorkloadPoliciesController` shared-control migration (zero native controls again).
- Cloud #664 (batch 65): advanced the public pin `fb60e78 → 0b403443`; re-minted definition/fusion/route-preview-replay/route-transformation/gateway-cache-purge/reconciliation evidence.
- Cloud #665 (batch 66): stood up a live local stack (disposable PostgreSQL 18 + pgvector, production dashboard build, loopback fixtures, all three engines) and re-ran chat-drawer 3/3, interaction-safety 9/9, workflow-graph 3/3, target-size 3/3, route-activation-health 3/3 + 2/2 DB. **Found + fixed a real Firefox-only WCAG-AA color-contrast defect** (`--tt-text-faint` 2.54:1 → `--tt-text-muted`).
- Cloud #666 (batch 67): completed the fresh performance run batch 54 required (`--project=perf` 7/7); observed values + source hashes re-minted.
- Cloud #667 (batch 68): recorded the visual-baseline assessment — 9/10 extended chromium baselines differ from the reviewed 2026-08-03 shots due to legitimate product changes; a deliberate reviewed `--update-snapshots` pass is required (not blind-refreshed).

## Next session should

1. Do a deliberate, reviewed visual-baseline re-shoot for `accessibility-mobile-state` + `accessibility-nonchromium-visual-performance` (9 PNG diffs already characterised in `cloud/docs/reviews/2026-09-13-open-items.md`).
2. Re-run the whole-release `supply-chain` record at the current revision (full workspace).
3. Continue the dependency-ordered source queue and C05 D1. Keep root totals at 3 local complete / 42 partial / 11 external / 3 open.

## Recent audit trail

```
    2026-09-14T21:09:06Z  session=20260914-18206  branch=main  head=370e0d8  task="feature-slices-r02-r03-r05-r07-c05"  status="Nine feature slices: #642 R03 draft lifecycle, #643/#647 R05 canary client+panel, #644/#648 R07 workload client+UI, #645 R05 measurement allowance, #646 R02 trace expiry, #449/#450 C05 MoneyMicros type + run-budget adoption. Tri-repo pins advanced to public 370e0d8. Verified: public workspace all-targets clean, tt-shared 174/0, cloud tt-api 1,199 lib/0, dashboard 3,259/89, web 231. No deployment or launch approval."  diff=""
    2026-09-15T00:33:57Z  session=20260915-19545  branch=main  head=c1f69a0  task="c05-d2-numeric-money-migration"  status="C05 stage 2 / D2 landed: migration 0053 migrates the six durable reservation money columns to NUMERIC(12,6) with ROUND backfill, preserved checks, restored DEFAULT 0; budget_reservation decodes via ::float8; a migrations guard test prevents float reintroduction. The DB-gated test caught a real dropped-default defect during development. Merged public #451 c1f69a0; pins advanced (cloud #651, web #75). Verified: public tt-core lib 1,132/0 + DB-gated budget suites, cloud 1,954/0 against the migrated schema, web 231. No deployment or launch approval."  diff=""
    2026-09-15T00:55:51Z  session=20260915-36271  branch=main  head=fb60e78  task="c05-d4-signed-money"  status="C05 D4 consolidation: added tt-shared SignedMoneyMicros (i64 micro-USD, biased constructors + from_usd_round + positive_magnitude) (public #452) and moved Cloud showback off its local (usd*1e6).round() onto it (Cloud #652), so the signed-money conversion rule lives in one place; a non-finite value now fails closed. Pins advanced (cloud #652, web #76). Verified: tt-shared 175/0, cloud tt-api 1,199/0, web 231, immutable-pin check. No deployment or launch approval."  diff=""
    2026-09-15T01:27:07Z  session=20260915-60609  branch=main  head=ecafa22  task="c05-d4-complete"  status="C05 D4 completed: the last production micros copy (run_receipt_savings) now delegates to tt-shared SignedMoneyMicros; all three Cloud receipt/showback conversions share one rule. Confirmed the existing --features pdf cross-surface population-parity test (digest/PDF/run-receipt/monthly-report) still passes after the D2 NUMERIC money migration. Merged Cloud #653. Verified: cloud tt-api run_receipt 10/0 and the parity test 1/0. No deployment or launch approval."  diff=""
    2026-09-15T01:31:38Z  session=20260915-65354  branch=main  head=eb036bf  task="data-retention-disclosure"  status="Closed a customer-facing honesty gap: the /data retention schedule omitted the value-free decision-trace ledger (90d, migration 0130) and the opt-in body capture (1-30d per-row); both now disclosed with a source contract pinning the schedule to the code. Merged Cloud #654. Verified: Dashboard 3,262/89, typecheck 0/0/0, build budgets. No deployment or launch approval."  diff=""
```

## Open decisions parked

(none — update if a decision was deferred)
