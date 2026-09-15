-- Reverse migration 0053: restore the DOUBLE PRECISION money columns.
--
-- This is a lossy down migration by design (NUMERIC(12,6) → DOUBLE PRECISION
-- reintroduces the representation error the up migration removed). It exists
-- so a failed rollout can be rolled back; a re-apply re-rounds to 6 decimals.

-- ── gateway_budget_scope_months ──────────────────────────────────────────────
ALTER TABLE gateway_budget_scope_months
    DROP CONSTRAINT IF EXISTS gateway_budget_scope_months_baseline_nonneg,
    DROP CONSTRAINT IF EXISTS gateway_budget_scope_months_reserved_nonneg,
    DROP CONSTRAINT IF EXISTS gateway_budget_scope_months_settled_nonneg;

ALTER TABLE gateway_budget_scope_months
    ADD COLUMN baseline_spend_usd_double DOUBLE PRECISION,
    ADD COLUMN reserved_usd_double       DOUBLE PRECISION,
    ADD COLUMN settled_spend_usd_double  DOUBLE PRECISION;

UPDATE gateway_budget_scope_months
SET baseline_spend_usd_double = baseline_spend_usd::double precision,
    reserved_usd_double       = reserved_usd::double precision,
    settled_spend_usd_double  = settled_spend_usd::double precision;

ALTER TABLE gateway_budget_scope_months
    ALTER COLUMN baseline_spend_usd_double SET NOT NULL,
    ALTER COLUMN reserved_usd_double       SET NOT NULL,
    ALTER COLUMN settled_spend_usd_double  SET NOT NULL,
    DROP COLUMN baseline_spend_usd,
    DROP COLUMN reserved_usd,
    DROP COLUMN settled_spend_usd;

ALTER TABLE gateway_budget_scope_months
    RENAME COLUMN baseline_spend_usd_double TO baseline_spend_usd;
ALTER TABLE gateway_budget_scope_months
    RENAME COLUMN reserved_usd_double TO reserved_usd;
ALTER TABLE gateway_budget_scope_months
    RENAME COLUMN settled_spend_usd_double TO settled_spend_usd;

ALTER TABLE gateway_budget_scope_months
    ADD CONSTRAINT gateway_budget_scope_months_baseline_spend_usd_check
        CHECK (baseline_spend_usd >= 0),
    ADD CONSTRAINT gateway_budget_scope_months_reserved_usd_check
        CHECK (reserved_usd >= 0),
    ADD CONSTRAINT gateway_budget_scope_months_settled_spend_usd_check
        CHECK (settled_spend_usd >= 0);

-- ── gateway_budget_reservations ──────────────────────────────────────────────
ALTER TABLE gateway_budget_reservations
    DROP CONSTRAINT IF EXISTS gateway_budget_reservations_estimated_nonneg,
    DROP CONSTRAINT IF EXISTS gateway_budget_reservations_settled_nonneg;

ALTER TABLE gateway_budget_reservations
    ADD COLUMN estimated_usd_double DOUBLE PRECISION,
    ADD COLUMN settled_usd_double   DOUBLE PRECISION;

UPDATE gateway_budget_reservations
SET estimated_usd_double = estimated_usd::double precision,
    settled_usd_double   = CASE
        WHEN settled_usd IS NULL THEN NULL
        ELSE settled_usd::double precision
    END;

ALTER TABLE gateway_budget_reservations
    ALTER COLUMN estimated_usd_double SET NOT NULL,
    DROP COLUMN estimated_usd,
    DROP COLUMN settled_usd;

ALTER TABLE gateway_budget_reservations
    RENAME COLUMN estimated_usd_double TO estimated_usd;
ALTER TABLE gateway_budget_reservations
    RENAME COLUMN settled_usd_double TO settled_usd;

ALTER TABLE gateway_budget_reservations
    ADD CONSTRAINT gateway_budget_reservations_estimated_usd_check
        CHECK (estimated_usd >= 0),
    ADD CONSTRAINT gateway_budget_reservations_settled_usd_check
        CHECK (settled_usd IS NULL OR settled_usd >= 0);

-- ── gateway_budget_adjustments ───────────────────────────────────────────────
ALTER TABLE gateway_budget_adjustments
    ADD COLUMN delta_usd_double DOUBLE PRECISION;

UPDATE gateway_budget_adjustments
SET delta_usd_double = delta_usd::double precision;

ALTER TABLE gateway_budget_adjustments
    ALTER COLUMN delta_usd_double SET NOT NULL,
    DROP COLUMN delta_usd;

ALTER TABLE gateway_budget_adjustments
    RENAME COLUMN delta_usd_double TO delta_usd;
