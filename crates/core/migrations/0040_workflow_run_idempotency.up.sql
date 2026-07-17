-- Gateway-owned durable workflow-run idempotency mapping.
--
-- A caller may lose the response after the gateway has accepted and started a
-- workflow.  Retrying that logical invocation must return/reconcile the same
-- run instead of starting another paid execution (and must retain the exact
-- definition version accepted at first submission).  The raw Idempotency-Key
-- never reaches this table: the application stores only a domain-separated
-- SHA-256 digest scoped to (org_id, workflow_id).
--
-- This is deliberately separate from the cloud control plane's
-- workflow_invocations queue ledger.  The queue records delivery intent; this
-- gateway-owned table records the actual execution mapping and can be used by
-- any caller, including a future independent dispatcher.

CREATE TABLE IF NOT EXISTS workflow_run_idempotency (
  org_id               UUID NOT NULL,
  workflow_id          UUID NOT NULL,
  invocation_key_hash  BYTEA NOT NULL CHECK (octet_length(invocation_key_hash) = 32),
  workflow_version     INT NOT NULL CHECK (workflow_version > 0),
  input_hash           BYTEA NOT NULL CHECK (octet_length(input_hash) = 32),
  request_options_hash BYTEA NOT NULL CHECK (octet_length(request_options_hash) = 32),
  run_id               UUID NOT NULL,
  created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),

  CONSTRAINT workflow_run_idempotency_pkey
    PRIMARY KEY (org_id, workflow_id, invocation_key_hash),

  -- Definitions are immutable versions. Keep the accepted version referentially
  -- intact so a retry can never drift to the latest saved definition. Cascades
  -- apply only when an existing workflow/run deletion intentionally removes the
  -- underlying tenant data, so this new no-FK ownership record cannot block the
  -- established account-purge order. Both FKs are deferred because the gateway
  -- inserts mapping + running row in one transaction, preserving the
  -- all-or-nothing create-or-reuse invariant.
  CONSTRAINT workflow_run_idempotency_definition_fk
    FOREIGN KEY (org_id, workflow_id, workflow_version)
    REFERENCES workflow_definitions (org_id, id, version)
    ON DELETE CASCADE
    DEFERRABLE INITIALLY DEFERRED,
  CONSTRAINT workflow_run_idempotency_run_fk
    FOREIGN KEY (run_id)
    REFERENCES workflow_runs (id)
    ON DELETE CASCADE
    DEFERRABLE INITIALLY DEFERRED
);

-- Supports reverse reconciliation/status tooling without putting a raw
-- invocation key in an operator-visible index.
CREATE INDEX IF NOT EXISTS workflow_run_idempotency_run_id_idx
  ON workflow_run_idempotency (run_id);

COMMENT ON TABLE workflow_run_idempotency IS
  'Gateway-owned opaque stable-invocation to workflow-run mapping; binds exact workflow version, canonical input, and execution options.';
