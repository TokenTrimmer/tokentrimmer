-- Content-aware compression (P1a): isolated estimated-savings + flywheel label.
--
-- content_compress_saved_est_usd is an ISOLATED, ESTIMATED value: the input
-- tokens the content_compress structural backend (JSON whitespace-minify, CSV
-- padding trim, log repeated-line collapse — all opt-in via
-- RouteAction.content_compress, off by default) removed before dispatch, priced
-- at the served model's input rate, fee-applied. Like doc_vision_saved_est_usd
-- (migration 0032) it is NEVER part of cost_usd / baseline_cost_usd / the
-- saved-usd headline (those reconcile against the realized provider invoice); it
-- is a conservative estimate surfaced on X-TokenTrimmer-Content-Compress-Saved-
-- Est-Usd. Additive + back-compat: existing rows (and every request whose route
-- did not opt in) carry 0. Dashboards may SUM(content_compress_saved_est_usd) —
-- ALWAYS labeled "estimated".
--
-- content_compress_kind is the metrics-only flywheel label: the DOMINANT content
-- kind the backend compacted on this request ('json' / 'csv' / 'log'), or NULL
-- when the route did not opt in / nothing compacted. NO request content — the
-- ZDR-safe training signal (the opt-in raw before/after pair capture is a
-- separate, off-by-default path). NULL for existing rows.
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS content_compress_saved_est_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS content_compress_kind TEXT;
