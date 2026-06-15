# L2 semantic cache — HNSW recall / `ef_search` budget

> Scope: the **L2 semantic cache** (`crates/cache/src/l2.rs`, `PostgresL2Cache`)
> backed by pgvector HNSW. The retrieval store (`crates/retrieval/src/store/postgres.rs`)
> uses the same single-index + `org_id` post-filter shape and the same default,
> so the reasoning here applies there too.

## TL;DR

- L2 uses **one shared HNSW index** `cache_entries_embedding_hnsw`
  (`m = 16, ef_construction = 64`, migration `0002_cache_entries.up.sql`) and
  isolates tenants with a **query-time** `WHERE org_id = $1` filter
  (`l2.rs` `lookup`).
- Because the filter is applied *after* the graph walk ranks by vector distance,
  other tenants' near vectors dilute each org's candidate list. To keep recall,
  we raise `hnsw.ef_search` to **100** (pgvector's default is 40) per lookup via
  `SET LOCAL` inside the lookup transaction.
- `ef_search` is now tunable at runtime via the **`TT_L2_EF_SEARCH`** env var
  (default 100) — no recompile. See [Tuning](#tuning).
- This is fine at today's tenant count. **Revisit** when a few orgs dominate L2
  volume (thresholds below); the mitigation is per-large-org **partial** HNSW
  indexes — template at `docs/templates/l2-per-org-hnsw-index.sql` (manual, not
  auto-applied). **No live drop/rebuild of the shared index is planned.**

## The three-way tradeoff: recall ↔ latency ↔ RAM

`ef_search` is the size of the dynamic candidate list HNSW keeps while walking
the graph at query time.

| Knob | Lower `ef_search` (→ 40) | Higher `ef_search` (→ 100+) |
|---|---|---|
| **Recall** | Worse — true nearest neighbour can fall outside the list, especially once the `org_id` filter discards rows | Better — wider list survives the post-filter |
| **Latency** | Faster — fewer distance comparisons per query | Slower — more comparisons; roughly linear in `ef_search` |
| **RAM (Neon compute)** | Lower working set per query | Higher transient working set; on Neon, compute RAM is the scaling cost and what scale-to-zero / autoscale bills against |

`ef_search` is a **per-query** GUC (`SET LOCAL hnsw.ef_search`), so it scopes to
the lookup transaction and does not change the on-disk index. `m` and
`ef_construction` are **build-time** index parameters and changing them requires
rebuilding the index (out of scope here).

## Why `ef_search = 100` today (the post-filter dilution problem)

The privacy/cost design keeps **one** index over all tenants' vectors rather than
one index per org. Lookups isolate tenants with `WHERE org_id = $1`. pgvector's
HNSW walk, however, ranks candidates by **vector distance first** and only then
does Postgres apply the `org_id` predicate. So under multi-tenant load:

1. The query org's own nearest neighbour competes against *every other tenant's*
   near vectors for the `ef_search`-sized candidate slots.
2. With `ef_search = 40`, the list can fill with other orgs' vectors that are
   then discarded by the filter — and the querying org's real match never makes
   the list. Result: a **false cache miss** (recall drop), which silently erodes
   the cache hit rate and the savings it produces.

Raising `ef_search` to 100 widens the list enough that the querying org's match
reliably survives the post-filter at today's tenant counts, at a modest latency
cost. The recall contract is exercised by
`l2.rs::tests::multi_tenant_recall_each_org_finds_its_own_match`.

**The catch:** the needed `ef_search` grows with the *fraction of the index that
belongs to other tenants near the query*. As tenant count and per-tenant volume
grow, a fixed `ef_search = 100` buys less recall per query — recall-per-cost
worsens. That is the trigger to revisit.

## Tuning (`TT_L2_EF_SEARCH`)

- Set `TT_L2_EF_SEARCH=<n>` (integer ≥ 1) to override the default of 100 without
  a recompile. `PostgresL2Cache::new` reads it at construction
  (`ef_search_from_env`); unset / empty / unparseable / `< 1` falls back to 100.
- An explicit `PostgresL2Cache::with_ef_search(n)` still overrides the env value
  (used by tests / callers that pin a value).
- **Raise it** if you observe recall dips (cache hit-rate drop with no
  corpus/threshold change) as tenants grow. **Lower it** toward 40 only if
  lookup latency / Neon RAM is the binding constraint AND recall holds (e.g.
  after adding per-org partial indexes for the heavy tenants).
- Validate any change with `EXPLAIN (ANALYZE, BUFFERS)` on a representative
  org's lookup and by watching the L2 hit rate, not in the abstract.

## Revisit budget — when to move off the single shared index

Treat the single-index design as adequate until **any** of these holds, then
apply per-large-org partial indexes for the offending tenants (not a global
rebuild):

- **Tenant skew:** one org holds a disproportionate share of live
  `cache_entries` rows — rule of thumb **> ~25 %** of the live table, or
  **> ~100k** live vectors for a single org.
- **`ef_search` creep:** you have had to push `TT_L2_EF_SEARCH` materially above
  100 (e.g. ≥ ~200) just to hold recall, and lookup latency or Neon compute RAM
  is now the binding constraint.
- **Recall floor breached:** measured recall for heavy tenants drops below your
  target (e.g. < ~0.95 vs. a brute-force baseline) at an `ef_search` you can
  afford.

Measure live-row share before acting:

```sql
SELECT org_id, count(*) AS live_rows
  FROM cache_entries
 WHERE expires_at > now()
 GROUP BY org_id
 ORDER BY live_rows DESC
 LIMIT 20;
```

### Mitigation: per-large-org partial HNSW index (optional, manual)

For the **few** heavy tenants, add a partial HNSW index scoped to that org:

```sql
CREATE INDEX CONCURRENTLY cache_entries_embedding_hnsw_org_<slug>
  ON cache_entries
  USING hnsw (embedding vector_cosine_ops)
  WITH (m = 16, ef_construction = 64)
  WHERE org_id = '<big-org-uuid>';
```

A partial index contains only that org's vectors, so its graph walk has **no
cross-tenant dilution**: the org's lookups need no inflated `ef_search` and stop
competing with everyone else's candidates. The shared index stays as the
fallback for all other orgs; Postgres' planner picks the partial index for
matching `org_id = …` lookups.

- Full, copy-pasteable, clearly-commented template (with inspection, cost,
  `CONCURRENTLY` caveats, and rollback):
  **`docs/templates/l2-per-org-hnsw-index.sql`**. It is **not** under
  `crates/core/migrations/`, so the embedded migrator never auto-applies it —
  apply it by hand, off-peak, per heavy tenant only.
- Cost: each partial index adds RAM + write amplification for that org's
  rows, so do this for the heavy tenants only — **do not** add one per tenant.
- This is deliberately **low-risk**: additive partial indexes + a tunable
  `ef_search`. There is **no planned live drop/rebuild** of the shared
  `cache_entries_embedding_hnsw` index.

## Cross-references

- Index definition: `crates/core/migrations/0002_cache_entries.up.sql`.
- Lookup + `SET LOCAL hnsw.ef_search`: `crates/cache/src/l2.rs` (`PostgresL2Cache::lookup`).
- Default + env override: `DEFAULT_EF_SEARCH`, `EF_SEARCH_ENV` /
  `TT_L2_EF_SEARCH`, `ef_search_from_env` in `crates/cache/src/l2.rs`.
- Same shape in retrieval: `crates/retrieval/src/store/postgres.rs`
  (`retrieval_chunks_embedding_hnsw`, migration `0005_retrieval_chunks.up.sql`).
