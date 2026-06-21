-- Batch-Lane reconciliation-parity columns (slice-3 follow-up): an IMMUTABLE
-- window key + the persisted batch-rate provider cost, so realized batch savings
-- can be (a) calendar-month attributed deterministically and (b) reconciled
-- against the real provider invoice with the same rigor as synchronous traffic
-- before they are ever folded into the signed monthly attestation.
--
-- `batch_completed_at` — the window key the cloud worker writes EXACTLY ONCE,
-- under the same `realized_saved_usd IS NULL` guard as the savings booking, so
-- it is immutable by construction. `updated_at` cannot serve this role: the
-- per-poll status-mirror UPDATE bumps it on every poll, so a closed month
-- windowed on it would recompute to a different number after the fact. The
-- provider's OpenAI Batch object carries no completion timestamp, so the value
-- is the booking time (`now()` at first book) captured once and frozen — a
-- deterministic, re-verifiable month attribution (booking time, not request
-- time, with a disclosed near-boundary edge). Legacy rows booked before this
-- migration keep `batch_completed_at = NULL` and are excluded from the
-- month-bounded fold forever (conservative; never back-filled from `updated_at`,
-- which is last-poll time, not completion time).
--
-- `batch_cost_usd` — the batch-rate cost the provider actually bills (computed
-- in `compute_realized_savings` but, until now, discarded after deriving
-- `realized_saved_usd`). Persisting it lets `invoice_recon` add the batch slice
-- to its metered SUM and spot-check it against the real provider bill, which is
-- the precondition for folding batch savings into the signed figure. It equals
-- `batch_baseline_usd - realized_saved_usd` by construction (batch cost is
-- always <= the standard-rate baseline), but is stored explicitly so the
-- reconciliation basis is robust to any future change in the savings formula.
--
-- Both columns are NULLABLE and written only by the slice-3 booking UPDATE.

ALTER TABLE batch_jobs ADD COLUMN batch_completed_at TIMESTAMPTZ;
ALTER TABLE batch_jobs ADD COLUMN batch_cost_usd      NUMERIC(12,6);

-- Serves the per-org calendar-month scan (fold + invoice-recon parity) without
-- touching legacy NULL-completed rows.
CREATE INDEX batch_jobs_org_completed_at_idx
    ON batch_jobs (org_id, batch_completed_at)
    WHERE batch_completed_at IS NOT NULL;
