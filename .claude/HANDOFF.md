# Active session handoff

_Written at 2026-09-15T02:33:32Z by session `20260915-77258` on branch `main` (@ 152db40)._

## Status: Two honesty/observability fixes: the /data retention schedule now discloses the decision-trace ledger (90d) and opt-in body capture (1-30d) with a source contract pinning schedule to code (Cloud #654); the retention sweep log now emits the trace/activation/funnel deletion counts it was hiding (Cloud #655). Verified on a FRESH disposable DB: tt-api lib 1,232/0, retention 9/0, route_decision_trace 4/0, dashboard 3,262/89. No deployment or launch approval.

Active task: `honesty-and-observability-fixes`

## What happened this session

- Diff: (no git changes)
- Files touched:


## Next session should

Continue the dependency-ordered source queue and C05 D1. Keep root totals at 3 local complete / 42 partial / 11 external / 3 open.

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
