-- Durable store for the async Batch Lane (slice 2: gateway OpenAI-compatible
-- Batch endpoints). One row per submitted batch, org-scoped.
--
-- `id` is the OpenAI-compatible batch id the gateway returns to the caller. For
-- the OpenAI-only slice 2 it is the provider's own batch id (e.g. `batch_abc`);
-- it stays the externally-visible handle so a later abstraction can map it to a
-- provider-specific id without changing the public surface. `provider_batch_id`
-- holds the upstream provider's id explicitly (== `id` for OpenAI today) so the
-- slice-3 poll worker can fan out by provider without re-parsing `id`.
--
-- Org-scoping: EVERY read/update filters on `org_id`, so one org can never see
-- or mutate another org's batch — a cross-org `GET /v1/batches/{id}` finds no
-- row and 404s. `(org_id, status)` indexes the per-org status scan the slice-3
-- worker and dashboard need; `(provider_batch_id)` indexes the worker's
-- provider-id → row lookup when reconciling poll results.
--
-- `realized_saved_usd` / `batch_baseline_usd` are NULLABLE and left unset by
-- slice 2 — the slice-3 worker fills them once a batch completes and its
-- realized vs. standard-rate cost can be reconciled against the provider
-- invoice. Keeping them out of slice 2 means no fabricated savings figure is
-- ever persisted before the batch actually settles.

CREATE TABLE batch_jobs (
    id                 TEXT PRIMARY KEY,
    org_id             UUID NOT NULL,
    api_key_id         UUID,
    provider           TEXT NOT NULL,
    provider_batch_id  TEXT,
    input_file_id      TEXT,
    output_file_id     TEXT,
    error_file_id      TEXT,
    status             TEXT NOT NULL,
    endpoint           TEXT,
    completion_window  TEXT,
    request_total      INTEGER NOT NULL DEFAULT 0,
    request_completed  INTEGER NOT NULL DEFAULT 0,
    request_failed     INTEGER NOT NULL DEFAULT 0,
    -- Filled by the slice-3 poll worker once the batch settles; never written
    -- by slice 2 (no realized figure exists pre-completion).
    realized_saved_usd NUMERIC(12,6),
    batch_baseline_usd NUMERIC(12,6),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX batch_jobs_org_status_idx ON batch_jobs (org_id, status);
CREATE INDEX batch_jobs_provider_batch_idx ON batch_jobs (provider_batch_id);
