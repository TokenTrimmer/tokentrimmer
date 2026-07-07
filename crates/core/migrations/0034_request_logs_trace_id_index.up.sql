-- P2a (learned-compression label-gap closure): a partial index on
-- `request_logs.trace_id` so the per-pair join
-- `quality_verdicts.request_id::text = request_logs.trace_id` (the RUNG 3
-- judge-verdict ↔ capture-pair join) is an index scan, not a seq scan.
--
-- `trace_id` is a NULLABLE TEXT column (migration 0001; not UUID-typed — the
-- join casts the OTHER side, `quality_verdicts.request_id::text`, because
-- casting `trace_id::uuid` throws on non-UUID values, per 0014's note). Most
-- rows carry a trace_id today, but legacy rows + any future null-bearing path
-- keep it nullable, so the index is PARTIAL (`WHERE trace_id IS NOT NULL`) —
-- the join never wants NULL-trace rows anyway, + a partial index is smaller +
-- cheaper to maintain than a full one.
--
-- `org_id` leads the index key: the join is org-scoped (both `quality_verdicts`
-- + `request_body_captures` carry `org_id`, + `request_body_captures`'s
-- `UNIQUE(org_id, trace_id)` is org-scoped — a trace_id is unique within an
-- org, not globally). The org prefix makes the join unambiguous + matches the
-- existing org-scoped access pattern.
--
-- Migration 0014 explicitly flagged this index as a Phase-2 task; migration
-- 0019 deferred it (route-level netting uses `route_id`, not `trace_id`). P2a's
-- per-PAIR join is the first consumer that actually needs the trace_id index.
CREATE INDEX IF NOT EXISTS request_logs_trace_id_idx
  ON request_logs (org_id, trace_id)
  WHERE trace_id IS NOT NULL;
