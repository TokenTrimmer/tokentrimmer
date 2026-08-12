DROP INDEX IF EXISTS gateway_budget_reservations_dispatch_key_unique;

ALTER TABLE gateway_budget_reservations
    DROP CONSTRAINT IF EXISTS gateway_budget_reservations_settlement_basis_check,
    DROP CONSTRAINT IF EXISTS gateway_budget_reservations_dispatch_key_check,
    DROP CONSTRAINT IF EXISTS gateway_budget_reservations_dispatch_kind_check,
    DROP COLUMN IF EXISTS settlement_observed_at,
    DROP COLUMN IF EXISTS settlement_basis,
    DROP COLUMN IF EXISTS dispatch_key,
    DROP COLUMN IF EXISTS dispatch_kind,
    DROP COLUMN IF EXISTS provider;
