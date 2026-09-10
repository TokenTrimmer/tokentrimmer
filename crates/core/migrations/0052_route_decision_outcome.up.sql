-- R02 schema foundation: persist the routing decision outcome (the bounded
-- result of route evaluation) on every request log row. The gateway already
-- computes this (RouteApplicationOutcome in selection.rs) but currently only
-- logs it at debug level. This column makes it queryable so a customer can
-- see WHY routing did or didn't apply (no_match, forced_route_not_found,
-- paused, capability_suppressed, or accepted_for_action_pipeline) without
-- reading server logs.
--
-- Value-free: stores only the bounded outcome enum name. NULL for
-- pre-migration rows and requests with no routing store configured (dev/local).
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS route_decision_outcome TEXT;
