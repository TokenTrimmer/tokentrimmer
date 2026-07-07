-- Document Lane D4: isolated vision-avoided saving accounting.
--
-- doc_vision_saved_est_usd is an ESTIMATE of an unmeasurable COUNTERFACTUAL: the
-- pre-routing distillation seam (D4c) swaps an image/document input part for
-- distilled TEXT, so the request that actually dispatched never contained the
-- image. The saving is the raw image tokens that WOULD have been billed minus
-- the distilled text tokens, priced at the served model's input rate, fee-
-- applied ($0 for Gemini per the D0 provider-direction guard). NEVER part of
-- cost_usd / baseline_cost_usd / the saved-usd headline (those reconcile against
-- the realized provider invoice — a request that never sent the image cannot be
-- invoice-reconciled on it). Surfaced on X-TokenTrimmer-Doc-Vision-Saved-Est-Usd.
-- Additive + back-compatible: existing rows (and every request in D4a, where the
-- seam does not yet run) carry 0. Dashboards may SUM(doc_vision_saved_est_usd) —
-- ALWAYS labeled "estimated".
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS doc_vision_saved_est_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
