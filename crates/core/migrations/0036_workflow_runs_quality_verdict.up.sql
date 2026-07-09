-- Flow-level end-to-end quality gate — the verdict column on workflow_runs
-- (BACKLOG item #5, Slice 1 public wiring; the cloud-ledger twin is cloud
-- migration 0043). The gateway's `workflow::quality_gate` (Slice 1) writes this
-- at run completion on sampled runs only, after the run is finalized. NULL for
-- pre-gate / not-sampled / failed-before-answer runs — the cloud mint reads it
-- to decide v1 (NULL / not_sampled) vs v2 (a sampled verdict).
--
-- Idempotent (IF NOT EXISTS): the cloud migration 0043 may have already applied
-- this ALTER on the shared Neon (the two services run separate _sqlx_migrations
-- ledgers — public._sqlx_migrations vs tt_cloud_migrations._sqlx_migrations —
-- but both write the shared `public` schema tables). A self-hosted gateway
-- running only the public migrations needs this to write the verdict.
ALTER TABLE workflow_runs
  ADD COLUMN IF NOT EXISTS quality_verdict TEXT;
