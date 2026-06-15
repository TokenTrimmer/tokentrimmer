-- ============================================================================
-- OPTIONAL, MANUALLY-APPLIED template — per-large-org partial HNSW index for L2.
--
-- THIS FILE IS NOT A MIGRATION. The embedded migrator
-- (`tt_core::db::MIGRATOR = sqlx::migrate!("./migrations")`) only scans
-- `crates/core/migrations/`, so this template under `docs/templates/` is NEVER
-- auto-applied. Apply it by hand, per heavy tenant, only after reading
-- `docs/l2-hnsw-recall-budget.md` and deciding the revisit threshold is met.
--
-- WHY: the default design uses ONE shared index
-- `cache_entries_embedding_hnsw` (m=16, ef_construction=64) and filters tenants
-- at query time with `WHERE org_id = $1`. The HNSW walk ranks by vector
-- distance and only THEN discards other tenants' rows, so a few high-volume
-- orgs can dilute every org's candidate list (hence `hnsw.ef_search = 100`,
-- raised from pgvector's 40). A PARTIAL index scoped to one big org gives that
-- org an undiluted graph: its lookups need no inflated ef_search and stop
-- competing with everyone else's candidates.
--
-- WHEN to apply (see the doc for the full budget):
--   * a single org holds a large, disproportionate share of live `cache_entries`
--     rows (e.g. >~25% of the table, or >~100k live vectors), AND
--   * raising `TT_L2_EF_SEARCH` to hold recall is pushing query latency / Neon
--     compute RAM past your budget.
-- Do this for the FEW heavy tenants only — one partial index per such org.
-- Do NOT add one per tenant (index bloat + write amplification).
--
-- COST / RISK:
--   * Each partial HNSW index is built + maintained independently → extra RAM
--     and write amplification on INSERT/DELETE for that org's rows.
--   * `CREATE INDEX CONCURRENTLY` avoids a long write lock but CANNOT run inside
--     a transaction block and takes longer. Run it off-peak; verify with
--     `\d+ cache_entries` and `EXPLAIN (ANALYZE, BUFFERS)` on that org's lookup.
--   * This is ADDITIVE: the shared index stays as the fallback for every other
--     org. Postgres' planner picks the partial index for matching `org_id = …`.
--
-- HOW (replace the placeholder UUID; one statement per heavy org):
-- ============================================================================

-- 1. Inspect candidate orgs by live-row share before deciding:
--    SELECT org_id, count(*) AS live_rows
--      FROM cache_entries
--     WHERE expires_at > now()
--     GROUP BY org_id
--     ORDER BY live_rows DESC
--     LIMIT 20;

-- 2. Create the per-org partial index. CONCURRENTLY = no long write lock; it
--    must run OUTSIDE a transaction. Name it after the org for easy rollback.
CREATE INDEX CONCURRENTLY IF NOT EXISTS
  cache_entries_embedding_hnsw_org_REPLACE_ME
  ON cache_entries
  USING hnsw (embedding vector_cosine_ops)
  WITH (m = 16, ef_construction = 64)
  WHERE org_id = '00000000-0000-0000-0000-000000000000';  -- <- the heavy org_id

-- 3. (Optional) For lookups that hit a partial index you can LOWER the per-query
--    ef_search back toward pgvector's default, since there is no cross-tenant
--    dilution in that index. This is a per-session GUC, applied by the caller;
--    keep the global TT_L2_EF_SEARCH for the shared index.
--    Example (psql):  SET LOCAL hnsw.ef_search = 40;

-- 4. Rollback (also CONCURRENTLY, also outside a transaction):
--    DROP INDEX CONCURRENTLY IF EXISTS cache_entries_embedding_hnsw_org_REPLACE_ME;
