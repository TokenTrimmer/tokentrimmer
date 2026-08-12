-- Durable, cross-replica monthly spend reservations.
--
-- `gateway_budget_scope_months` snapshots request-log spend when a capped scope
-- is first touched in a month, then becomes the source of truth for every
-- provider dispatch admitted through the reservation wrapper. Keeping active
-- reservations separate from settled spend makes concurrent admission atomic.
CREATE TABLE gateway_budget_scope_months (
  scope_kind          TEXT NOT NULL CHECK (scope_kind IN ('org', 'api_key')),
  scope_id            UUID NOT NULL,
  month_start         DATE NOT NULL,
  baseline_spend_usd  DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (baseline_spend_usd >= 0),
  reserved_usd        DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (reserved_usd >= 0),
  settled_spend_usd   DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (settled_spend_usd >= 0),
  updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (scope_kind, scope_id, month_start)
);

CREATE TABLE gateway_budget_reservations (
  id                  UUID PRIMARY KEY,
  org_id              UUID NOT NULL,
  api_key_id          UUID NOT NULL,
  trace_id            UUID NOT NULL,
  month_start         DATE NOT NULL,
  model               TEXT NOT NULL,
  estimated_usd       DOUBLE PRECISION NOT NULL CHECK (estimated_usd >= 0),
  settled_usd         DOUBLE PRECISION CHECK (settled_usd IS NULL OR settled_usd >= 0),
  reserves_org        BOOLEAN NOT NULL,
  reserves_api_key    BOOLEAN NOT NULL,
  status              TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'expired', 'settled')),
  created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
  lease_expires_at    TIMESTAMPTZ NOT NULL,
  settled_at          TIMESTAMPTZ,
  CHECK ((status = 'active' AND settled_usd IS NULL AND settled_at IS NULL)
      OR (status IN ('expired', 'settled') AND settled_usd IS NOT NULL AND settled_at IS NOT NULL))
);

-- Append-only financial evidence. Normal settlement adds the actual cost.
-- Lease expiry conservatively adds the estimate; a late provider result then
-- appends actual-estimate (negative values are explicit refunds).
CREATE TABLE gateway_budget_adjustments (
  id                  UUID PRIMARY KEY,
  reservation_id      UUID NOT NULL REFERENCES gateway_budget_reservations(id),
  org_id              UUID NOT NULL,
  api_key_id          UUID NOT NULL,
  month_start         DATE NOT NULL,
  kind                TEXT NOT NULL CHECK (kind IN ('settlement', 'lease_expiry', 'late_settlement_adjustment', 'manual_adjustment')),
  delta_usd           DOUBLE PRECISION NOT NULL,
  created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX gateway_budget_adjustments_reservation
  ON gateway_budget_adjustments (reservation_id, created_at);

CREATE INDEX gateway_budget_reservations_active_scope
  ON gateway_budget_reservations (org_id, month_start)
  WHERE status = 'active';

CREATE INDEX gateway_budget_reservations_active_key_scope
  ON gateway_budget_reservations (api_key_id, month_start)
  WHERE status = 'active';
