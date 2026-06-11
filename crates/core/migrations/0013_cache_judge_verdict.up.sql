-- L2 judge join (#458): record the auto-eval judge's verdict on cache entries.
--
-- When the sampled quality judge scores a response that was SERVED FROM the L2
-- semantic cache, we record the score + verdict here so the gateway can:
--   1. EVICT exactly the entry that served a clearly-degraded (High band)
--      response (a targeted single-row DELETE — never a bulk sweep), and
--   2. surface per-entry quality for dedup / analytics.
--
-- Both columns are ADDITIVE + NULLABLE by design (no default needed):
--   * quality_score  — latest judge score in [0,1] (1.0 preserved, 0.0 degraded);
--                      NULL until a judge scores a response served from this row
--                      (only ~2% of downgraded traffic is judged, so most rows
--                      stay NULL).
--   * judge_verdict  — latest verdict string ('acceptable' | 'degraded' |
--                      'unclear'); NULL until judged.
--
-- Existing rows + the pre-0013 INSERT path keep working unchanged: old inserts
-- that omit these columns simply leave them NULL.

ALTER TABLE cache_entries
    ADD COLUMN IF NOT EXISTS quality_score  REAL,
    ADD COLUMN IF NOT EXISTS judge_verdict  TEXT;

-- Partial index over judged-degraded rows only — keeps a future audit / cleanup
-- query (e.g. "show recently-evicted-quality entries") O(degraded-rows) rather
-- than O(N). Partial so it costs nothing for the overwhelmingly-NULL majority.
CREATE INDEX IF NOT EXISTS cache_entries_judge_degraded
    ON cache_entries (org_id, judge_verdict)
    WHERE judge_verdict IS NOT NULL;
