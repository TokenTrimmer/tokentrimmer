-- C05 / D2: migrate the durable monthly-reservation money columns off
-- `DOUBLE PRECISION` onto `NUMERIC(12,6)`, the money-contract storage floor
-- (M1) that every new money column must use (the DDL rule from the migration
-- policy section).
--
-- Why: DOUBLE PRECISION is a binary float; summing a month of reservations
-- accumulates representation error that violates M2 on the durable side. The
-- in-request ledger already carries integer micro-USD; this brings the durable
-- accumulator to the same 6-decimal floor. Exposure today is sub-micro-USD
-- monthly drift, but the money contract makes the durable boundary
-- authoritative, so the type must match the floor.
--
-- Shape: ADD the NUMERIC column, BACKFILL from the double column, DROP the
-- double column, RENAME the NUMERIC column into place. This avoids an
-- in-place USING cast that would round through the float value; the ROUND()
-- pins each value to the 6-decimal floor explicitly.
--
-- Reads in `budget_reservation.rs` decode through an explicit `::float8` (the
-- same pattern `spend.rs`/`route_savings.rs` already use for NUMERIC money),
-- so the Rust call sites keep their `f64` arithmetic while the DURABLE value
-- is exact.

-- ── gateway_budget_scope_months ──────────────────────────────────────────────
ALTER TABLE gateway_budget_scope_months
    ADD COLUMN baseline_spend_usd_numeric NUMERIC(12,6),
    ADD COLUMN reserved_usd_numeric       NUMERIC(12,6),
    ADD COLUMN settled_spend_usd_numeric  NUMERIC(12,6);

UPDATE gateway_budget_scope_months
SET baseline_spend_usd_numeric = ROUND(baseline_spend_usd::numeric, 6),
    reserved_usd_numeric       = ROUND(reserved_usd::numeric, 6),
    settled_spend_usd_numeric  = ROUND(settled_spend_usd::numeric, 6);

ALTER TABLE gateway_budget_scope_months
    ALTER COLUMN baseline_spend_usd_numeric SET NOT NULL,
    ALTER COLUMN reserved_usd_numeric       SET NOT NULL,
    ALTER COLUMN settled_spend_usd_numeric  SET NOT NULL,
    DROP COLUMN baseline_spend_usd,
    DROP COLUMN reserved_usd,
    DROP COLUMN settled_spend_usd;

ALTER TABLE gateway_budget_scope_months
    RENAME COLUMN baseline_spend_usd_numeric TO baseline_spend_usd;
ALTER TABLE gateway_budget_scope_months
    RENAME COLUMN reserved_usd_numeric TO reserved_usd;
ALTER TABLE gateway_budget_scope_months
    RENAME COLUMN settled_spend_usd_numeric TO settled_spend_usd;

-- The original 0049 columns carried DEFAULT 0; the ADD/BACKFILL/RENAME shape
-- above does not inherit a default, so restore it explicitly. `ensure_scope`
-- inserts only `baseline_spend_usd` and relies on these two defaulting to 0.
ALTER TABLE gateway_budget_scope_months
    ALTER COLUMN reserved_usd      SET DEFAULT 0,
    ALTER COLUMN settled_spend_usd SET DEFAULT 0;

ALTER TABLE gateway_budget_scope_months
    ADD CONSTRAINT gateway_budget_scope_months_baseline_nonneg
        CHECK (baseline_spend_usd >= 0),
    ADD CONSTRAINT gateway_budget_scope_months_reserved_nonneg
        CHECK (reserved_usd >= 0),
    ADD CONSTRAINT gateway_budget_scope_months_settled_nonneg
        CHECK (settled_spend_usd >= 0);

-- ── gateway_budget_reservations ──────────────────────────────────────────────
ALTER TABLE gateway_budget_reservations
    ADD COLUMN estimated_usd_numeric NUMERIC(12,6),
    ADD COLUMN settled_usd_numeric   NUMERIC(12,6);

UPDATE gateway_budget_reservations
SET estimated_usd_numeric = ROUND(estimated_usd::numeric, 6),
    settled_usd_numeric   = CASE
        WHEN settled_usd IS NULL THEN NULL
        ELSE ROUND(settled_usd::numeric, 6)
    END;

ALTER TABLE gateway_budget_reservations
    ALTER COLUMN estimated_usd_numeric SET NOT NULL,
    DROP COLUMN estimated_usd,
    DROP COLUMN settled_usd;

ALTER TABLE gateway_budget_reservations
    RENAME COLUMN estimated_usd_numeric TO estimated_usd;
ALTER TABLE gateway_budget_reservations
    RENAME COLUMN settled_usd_numeric TO settled_usd;

ALTER TABLE gateway_budget_reservations
    ADD CONSTRAINT gateway_budget_reservations_estimated_nonneg
        CHECK (estimated_usd >= 0),
    ADD CONSTRAINT gateway_budget_reservations_settled_nonneg
        CHECK (settled_usd IS NULL OR settled_usd >= 0);

-- ── gateway_budget_adjustments ───────────────────────────────────────────────
-- Deltas are SIGNED (a late-settlement refund is negative), so there is no
-- non-negative CHECK here — matching the original column's lack of one.
ALTER TABLE gateway_budget_adjustments
    ADD COLUMN delta_usd_numeric NUMERIC(12,6);

UPDATE gateway_budget_adjustments
SET delta_usd_numeric = ROUND(delta_usd::numeric, 6);

ALTER TABLE gateway_budget_adjustments
    ALTER COLUMN delta_usd_numeric SET NOT NULL,
    DROP COLUMN delta_usd;

ALTER TABLE gateway_budget_adjustments
    RENAME COLUMN delta_usd_numeric TO delta_usd;

COMMENT ON COLUMN gateway_budget_scope_months.baseline_spend_usd IS
    'C05/M1: money at the NUMERIC(12,6) storage floor (was DOUBLE PRECISION before migration 0053).';
COMMENT ON COLUMN gateway_budget_scope_months.reserved_usd IS
    'C05/M1: money at the NUMERIC(12,6) storage floor.';
COMMENT ON COLUMN gateway_budget_scope_months.settled_spend_usd IS
    'C05/M1: money at the NUMERIC(12,6) storage floor.';
COMMENT ON COLUMN gateway_budget_reservations.estimated_usd IS
    'C05/M1: money at the NUMERIC(12,6) storage floor (was DOUBLE PRECISION before migration 0053).';
COMMENT ON COLUMN gateway_budget_reservations.settled_usd IS
    'C05/M1: money at the NUMERIC(12,6) storage floor.';
COMMENT ON COLUMN gateway_budget_adjustments.delta_usd IS
    'C05/M1: signed money delta at the NUMERIC(12,6) storage floor (was DOUBLE PRECISION before migration 0053).';
