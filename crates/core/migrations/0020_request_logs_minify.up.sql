-- Minified-JSON output steering (research Phase 3.1 — RouteAction::minify_json).
--
-- minify_saved_est_usd is an ESTIMATE of an unmeasurable counterfactual: the
-- emitted JSON re-rendered pretty (serde_json 2-space) and re-tokenized with
-- the served model's tokenizer, minus the tokens actually emitted, priced at
-- the output rate the request was billed at, fee-applied. NEVER part of
-- cost_usd / baseline_cost_usd / the saved-usd headline (those reconcile
-- against the realized provider invoice). 0 when the instruction was not
-- injected, when the response was not valid JSON, for streaming responses
-- (v1 meters but does not estimate), TT cache hits, and rows predating this
-- migration. Dashboards may SUM(minify_saved_est_usd) — ALWAYS labeled
-- "estimated".
ALTER TABLE request_logs
  ADD COLUMN minify_saved_est_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
