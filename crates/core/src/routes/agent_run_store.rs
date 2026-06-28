//! Durable Postgres store for `agent_runs` identity + terminal state (W0b Task 5).
//!
//! Kept out of the oversized `agent_run.rs` (ADR-011 <800-line gate). All SQL
//! uses runtime `sqlx::query` with positional `.bind(...)` — this crate is
//! configured for RUNTIME sqlx checking, so no `query!` macros or offline data.
//!
//! Both public functions are **best-effort**: a DB error is logged-and-swallowed
//! so a Postgres outage never fails a run request.

use sqlx::PgPool;
use uuid::Uuid;

use crate::routes::agent_run::RunStatus;
use crate::routes::agent_run_budget::StopReason;

// ---------------------------------------------------------------------------
// Enum → snake_case wire string helpers (mirrors the serde rename_all)
// ---------------------------------------------------------------------------

/// Map a `RunStatus` to its snake_case DB TEXT value.
///
/// Mirrors the `#[serde(rename_all = "snake_case")]` on `RunStatus` so the DB
/// column is byte-identical to the JSON wire string (`completed`, `incomplete`,
/// `failed`, `requires_action`). Kept as a function rather than `Display` to
/// avoid a blanket impl that could confuse serde.
pub(crate) fn status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Completed => "completed",
        RunStatus::Incomplete => "incomplete",
        RunStatus::Failed => "failed",
        RunStatus::RequiresAction => "requires_action",
    }
}

/// Map a `StopReason` to its snake_case DB TEXT value.
pub(crate) fn stop_reason_str(r: StopReason) -> &'static str {
    match r {
        StopReason::MaxTurns => "max_turns",
        StopReason::BudgetExhausted => "budget_exhausted",
    }
}

// ---------------------------------------------------------------------------
// AgentRunRecord
// ---------------------------------------------------------------------------

/// A row destined for (or read from) `agent_runs`. Enums are stored as their
/// snake_case wire strings; numeric types match the DB schema (`NUMERIC(12,6)`,
/// `INT`).
#[derive(Debug, Clone)]
pub(crate) struct AgentRunRecord {
    pub id: Uuid,
    pub org_id: Uuid,
    /// Snake-case status string: `"running"`, `"completed"`, `"incomplete"`,
    /// `"failed"`, `"requires_action"`.
    pub status: String,
    pub model: String,
    /// Completed turns (0 for a newly created run).
    pub turns: i32,
    /// Hard turn cap, if set.
    pub max_turns: Option<i32>,
    /// Hard cost ceiling (USD), if set.
    pub max_cost_usd: Option<f64>,
    /// Accumulated served cost (USD) across all completed turns.
    pub cost_usd: f64,
    /// Snake-case stop reason string, if set.
    pub stop_reason: Option<String>,
    /// Cost-attribution tag from `X-TokenTrimmer-Tag`, if set.
    pub tag: Option<String>,
}

impl AgentRunRecord {
    /// Build a record for a **newly created** run (`status = "running"`, `turns = 0`,
    /// `cost_usd = 0.0`). Call at run creation before the loop starts.
    pub(crate) fn new_running(
        id: Uuid,
        org_id: Uuid,
        model: String,
        max_turns: Option<u32>,
        max_cost_usd: Option<f64>,
        tag: Option<String>,
    ) -> Self {
        Self {
            id,
            org_id,
            status: "running".to_string(),
            model,
            turns: 0,
            max_turns: max_turns.map(|t| t as i32),
            max_cost_usd,
            cost_usd: 0.0,
            stop_reason: None,
            tag,
        }
    }

    /// Build a record for a **terminal** run. Converts `RunStatus` /
    /// `StopReason` to their snake_case wire strings.
    ///
    /// Used by the unit tests (pure, no DB) and as a helper for
    /// constructing args to [`finish_agent_run`].
    #[allow(dead_code)] // used in unit tests; available for future production callers (e.g. Task 6)
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_terminal(
        id: Uuid,
        org_id: Uuid,
        status: RunStatus,
        turns: u32,
        cost_usd: f64,
        stop_reason: Option<StopReason>,
        model: String,
        max_turns: Option<u32>,
        max_cost_usd: Option<f64>,
        tag: Option<String>,
    ) -> Self {
        Self {
            id,
            org_id,
            status: status_str(status).to_string(),
            model,
            turns: turns as i32,
            max_turns: max_turns.map(|t| t as i32),
            max_cost_usd,
            cost_usd,
            stop_reason: stop_reason.map(|r| stop_reason_str(r).to_string()),
            tag,
        }
    }
}

// ---------------------------------------------------------------------------
// SQL constants
// ---------------------------------------------------------------------------

/// INSERT a new run row. `ON CONFLICT DO NOTHING` makes the call idempotent
/// against duplicate run ids (should be impossible with UUIDv4, but guards
/// against accidental double-call during a retry).
pub(crate) const INSERT_SQL: &str = "\
INSERT INTO agent_runs \
  (id, org_id, status, model, turns, max_turns, max_cost_usd, cost_usd, stop_reason, tag) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
ON CONFLICT (id) DO NOTHING";

/// UPDATE a run to its terminal state. Scoped by `(id, org_id)` so a
/// wrong-org caller cannot overwrite another org's run.
pub(crate) const FINISH_SQL: &str = "\
UPDATE agent_runs \
SET status = $3, turns = $4, cost_usd = $5, stop_reason = $6, finished_at = now() \
WHERE id = $1 AND org_id = $2";

// ---------------------------------------------------------------------------
// Public store functions
// ---------------------------------------------------------------------------

/// Insert a new `agent_runs` row with `status = "running"`.
///
/// **Best-effort**: any DB error is logged (at WARN level) and swallowed so
/// a Postgres outage never fails the caller's request. This mirrors the
/// tolerance used by `spawn_request_log` for `request_logs` rows.
pub(crate) async fn insert_agent_run(pool: &PgPool, rec: &AgentRunRecord) {
    let result = sqlx::query(INSERT_SQL)
        .bind(rec.id) // $1  id           UUID
        .bind(rec.org_id) // $2  org_id       UUID
        .bind(&rec.status) // $3  status       TEXT
        .bind(&rec.model) // $4  model        TEXT
        .bind(rec.turns) // $5  turns        INT
        .bind(rec.max_turns) // $6  max_turns    INT
        .bind(rec.max_cost_usd) // $7  max_cost_usd NUMERIC
        .bind(rec.cost_usd) // $8  cost_usd     NUMERIC
        .bind(rec.stop_reason.as_deref()) // $9  stop_reason  TEXT
        .bind(rec.tag.as_deref()) // $10 tag          TEXT
        .execute(pool)
        .await;
    if let Err(e) = result {
        tracing::warn!(
            run_id = %rec.id,
            error = %e,
            "agent_runs INSERT failed (best-effort, run not affected)"
        );
    }
}

/// Update an `agent_runs` row to its terminal state (`finished_at = now()`).
///
/// **Best-effort**: any DB error is logged and swallowed (mirrors
/// [`insert_agent_run`] tolerance — a runs-table failure must not fail a
/// successfully completed run response).
pub(crate) async fn finish_agent_run(
    pool: &PgPool,
    id: Uuid,
    org_id: Uuid,
    status: RunStatus,
    turns: u32,
    cost_usd: f64,
    stop_reason: Option<StopReason>,
) {
    let result = sqlx::query(FINISH_SQL)
        .bind(id) // $1 id          UUID
        .bind(org_id) // $2 org_id      UUID
        .bind(status_str(status)) // $3 status      TEXT
        .bind(turns as i32) // $4 turns       INT
        .bind(cost_usd) // $5 cost_usd    NUMERIC
        .bind(stop_reason.map(stop_reason_str)) // $6 stop_reason TEXT
        .execute(pool)
        .await;
    if let Err(e) = result {
        tracing::warn!(
            run_id = %id,
            error = %e,
            "agent_runs UPDATE (finish) failed (best-effort, run not affected)"
        );
    }
}

// ---------------------------------------------------------------------------
// Unit tests — pure (no DB required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::routes::agent_run::RunStatus;
    use crate::routes::agent_run_budget::StopReason;

    use super::AgentRunRecord;

    // Brief-prescribed test (the canonical TDD anchor).
    #[test]
    fn agent_run_record_serializes_status_and_stop_reason() {
        let r = AgentRunRecord::from_terminal(
            Uuid::nil(),
            Uuid::nil(),
            RunStatus::Incomplete,
            2,
            0.50,
            Some(StopReason::BudgetExhausted),
            "gpt-4o".into(),
            None,
            None,
            None,
        );
        assert_eq!(r.status, "incomplete");
        assert_eq!(r.stop_reason.as_deref(), Some("budget_exhausted"));
    }

    #[test]
    fn agent_run_record_new_running_fields() {
        let r = AgentRunRecord::new_running(
            Uuid::nil(),
            Uuid::nil(),
            "claude-3-haiku-20240307".into(),
            Some(8),
            Some(1.0),
            Some("my-tag".into()),
        );
        assert_eq!(r.status, "running");
        assert_eq!(r.turns, 0);
        assert_eq!(r.cost_usd, 0.0);
        assert!(r.stop_reason.is_none());
        assert_eq!(r.max_turns, Some(8));
        assert_eq!(r.tag.as_deref(), Some("my-tag"));
    }

    #[test]
    fn all_run_statuses_produce_snake_case_strings() {
        let cases = [
            (RunStatus::Completed, "completed"),
            (RunStatus::Incomplete, "incomplete"),
            (RunStatus::Failed, "failed"),
            (RunStatus::RequiresAction, "requires_action"),
        ];
        for (status, expected) in cases {
            let r = AgentRunRecord::from_terminal(
                Uuid::nil(),
                Uuid::nil(),
                status,
                0,
                0.0,
                None,
                "m".into(),
                None,
                None,
                None,
            );
            assert_eq!(r.status, expected, "RunStatus::{status:?}");
        }
    }

    #[test]
    fn all_stop_reasons_produce_snake_case_strings() {
        let cases = [
            (StopReason::MaxTurns, "max_turns"),
            (StopReason::BudgetExhausted, "budget_exhausted"),
        ];
        for (reason, expected) in cases {
            let r = AgentRunRecord::from_terminal(
                Uuid::nil(),
                Uuid::nil(),
                RunStatus::Incomplete,
                0,
                0.0,
                Some(reason),
                "m".into(),
                None,
                None,
                None,
            );
            assert_eq!(
                r.stop_reason.as_deref(),
                Some(expected),
                "StopReason::{reason:?}"
            );
        }
    }

    #[test]
    fn completed_run_has_no_stop_reason() {
        let r = AgentRunRecord::from_terminal(
            Uuid::nil(),
            Uuid::nil(),
            RunStatus::Completed,
            5,
            0.12,
            None,
            "gpt-4o".into(),
            Some(8),
            Some(1.0),
            None,
        );
        assert_eq!(r.status, "completed");
        assert!(r.stop_reason.is_none());
        assert_eq!(r.turns, 5);
        assert!((r.cost_usd - 0.12).abs() < 1e-9);
        assert_eq!(r.max_turns, Some(8));
    }
}
