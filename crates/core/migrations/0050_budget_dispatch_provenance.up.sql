-- Durable identity for each provider-call attempt. Existing reservations remain
-- readable with NULL provenance; every reservation created by the upgraded
-- gateway supplies all three dispatch fields.
ALTER TABLE gateway_budget_reservations
    ADD COLUMN provider TEXT,
    ADD COLUMN dispatch_kind TEXT,
    ADD COLUMN dispatch_key BYTEA,
    ADD COLUMN settlement_basis TEXT,
    ADD COLUMN settlement_observed_at TIMESTAMPTZ,
    ADD CONSTRAINT gateway_budget_reservations_dispatch_kind_check
        CHECK (dispatch_kind IS NULL OR dispatch_kind IN ('chat', 'chat_stream', 'embeddings', 'batch')),
    ADD CONSTRAINT gateway_budget_reservations_dispatch_key_check
        CHECK (dispatch_key IS NULL OR octet_length(dispatch_key) = 32),
    ADD CONSTRAINT gateway_budget_reservations_settlement_basis_check
        CHECK (
            settlement_basis IS NULL
            OR settlement_basis IN ('provider_usage', 'conservative_estimate', 'lease_expiry')
        );

-- A replayed logical provider attempt must never reserve or dispatch twice,
-- including after a process restart or on a different gateway replica.
CREATE UNIQUE INDEX gateway_budget_reservations_dispatch_key_unique
    ON gateway_budget_reservations (org_id, api_key_id, dispatch_key)
    WHERE dispatch_key IS NOT NULL;
