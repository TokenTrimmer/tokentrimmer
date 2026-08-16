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
        StopReason::BudgetBreach => "budget_breach",
        StopReason::Runaway => "runaway",
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
    /// Preflight cost-admission cap (USD), if set; a started turn can settle above it.
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
    /// Used by unit tests to verify terminal wire/database mappings.
    #[cfg(test)]
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

/// SELECT the most-recent runs for an org (list path, Task 6).
///
/// `NUMERIC(12,6)` columns are cast to `::float8` so sqlx can decode them as
/// `f64` without a `rust_decimal` dependency (the postgres driver supports
/// FLOAT8 ↔ f64 natively).
pub(crate) const LIST_SQL: &str = "\
SELECT id, org_id, status, model, turns, max_turns, \
       max_cost_usd::float8 AS max_cost_usd, cost_usd::float8 AS cost_usd, \
       stop_reason, tag \
FROM agent_runs \
WHERE org_id=$1 \
ORDER BY started_at DESC \
LIMIT $2";

/// SELECT a single run scoped by `(id, org_id)` (get path, Task 6).
///
/// `NUMERIC(12,6)` columns are cast to `::float8` for the same reason as
/// [`LIST_SQL`].
pub(crate) const GET_SQL: &str = "\
SELECT id, org_id, status, model, turns, max_turns, \
       max_cost_usd::float8 AS max_cost_usd, cost_usd::float8 AS cost_usd, \
       stop_reason, tag \
FROM agent_runs \
WHERE id=$1 AND org_id=$2";

/// Best-effort UPDATE that marks a paused run as `requires_action` (Task 6
/// paused-status fix). Called at both `LoopOutcome::Paused` sites so a run
/// whose Redis TTL expires reads as `requires_action` (not `running`) in the
/// durable store.
pub(crate) const MARK_REQUIRES_ACTION_SQL: &str = "\
UPDATE agent_runs \
SET status='requires_action' \
WHERE id=$1 AND org_id=$2";

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
// Read helpers (Task 6)
// ---------------------------------------------------------------------------

/// Map a Postgres row to [`AgentRunRecord`]. The NUMERIC columns are cast to
/// `::float8` in the query so `try_get::<f64, _>` succeeds without a
/// `rust_decimal` dependency.
fn record_from_row(row: &sqlx::postgres::PgRow) -> Result<AgentRunRecord, sqlx::Error> {
    use sqlx::Row;
    Ok(AgentRunRecord {
        id: row.try_get("id")?,
        org_id: row.try_get("org_id")?,
        status: row.try_get("status")?,
        model: row.try_get("model")?,
        turns: row.try_get("turns")?,
        max_turns: row.try_get("max_turns")?,
        max_cost_usd: row.try_get("max_cost_usd")?,
        cost_usd: row.try_get("cost_usd")?,
        stop_reason: row.try_get("stop_reason")?,
        tag: row.try_get("tag")?,
    })
}

/// Return up to `limit` runs for `org_id`, ordered most-recent first.
///
/// **Best-effort**: any DB error is logged at WARN and swallowed (returns an
/// empty `Vec`). Mirrors the tolerance of [`insert_agent_run`] and
/// [`finish_agent_run`] — a DB outage must never fail the caller's request.
pub(crate) async fn list_agent_runs(
    pool: &PgPool,
    org_id: Uuid,
    limit: i64,
) -> Vec<AgentRunRecord> {
    let result = sqlx::query(LIST_SQL)
        .bind(org_id) // $1 org_id UUID
        .bind(limit) //  $2 limit  BIGINT
        .fetch_all(pool)
        .await;
    match result {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| record_from_row(row).ok())
            .collect(),
        Err(e) => {
            tracing::warn!(
                %org_id,
                error = %e,
                "agent_runs LIST failed (best-effort, returning empty)"
            );
            Vec::new()
        }
    }
}

/// Fetch a single run scoped by `(id, org_id)`. Returns `None` when the row
/// is absent or when the DB errors (best-effort; the caller sees a miss either
/// way and will proceed to 404).
pub(crate) async fn get_agent_run(pool: &PgPool, id: Uuid, org_id: Uuid) -> Option<AgentRunRecord> {
    let result = sqlx::query(GET_SQL)
        .bind(id) //     $1 id     UUID
        .bind(org_id) // $2 org_id UUID
        .fetch_optional(pool)
        .await;
    match result {
        Ok(Some(row)) => record_from_row(&row).ok(),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                run_id = %id,
                error = %e,
                "agent_runs GET failed (best-effort, returning None)"
            );
            None
        }
    }
}

/// Best-effort UPDATE that sets a paused run's status to `requires_action`.
///
/// Called at both `LoopOutcome::Paused` sites so a stale paused run (whose
/// Redis TTL expired) reads as `requires_action` rather than `running` in the
/// durable Postgres store. Scoped by `(id, org_id)` — matches [`FINISH_SQL`]'s
/// scope so a wrong-org caller cannot corrupt another org's row.
pub(crate) async fn mark_run_requires_action(pool: &PgPool, id: Uuid, org_id: Uuid) {
    let result = sqlx::query(MARK_REQUIRES_ACTION_SQL)
        .bind(id) //     $1 id     UUID
        .bind(org_id) // $2 org_id UUID
        .execute(pool)
        .await;
    if let Err(e) = result {
        tracing::warn!(
            run_id = %id,
            error = %e,
            "agent_runs UPDATE (requires_action) failed (best-effort, run not affected)"
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

    use super::{AgentRunRecord, GET_SQL, LIST_SQL, MARK_REQUIRES_ACTION_SQL};

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
            (StopReason::BudgetBreach, "budget_breach"),
            (StopReason::Runaway, "runaway"),
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

    // ----- Task 6: read-path SQL query-shape tests (written first, TDD) -----

    #[test]
    fn list_sql_contains_expected_clauses() {
        // The query must scope by org, order most-recent first, and accept a LIMIT.
        assert!(
            LIST_SQL.contains("WHERE org_id=$1"),
            "LIST_SQL must filter by org_id"
        );
        assert!(
            LIST_SQL.contains("ORDER BY started_at DESC"),
            "LIST_SQL must order by started_at DESC"
        );
        assert!(
            LIST_SQL.contains("LIMIT $2"),
            "LIST_SQL must apply a LIMIT parameter"
        );
        assert!(
            LIST_SQL.contains("FROM agent_runs"),
            "LIST_SQL must query the agent_runs table"
        );
    }

    #[test]
    fn get_sql_scopes_by_org_and_id() {
        // Org-scoping is essential: a wrong-org caller must never retrieve
        // another org's run (WHERE id=$1 AND org_id=$2 is the gate).
        assert!(
            GET_SQL.contains("WHERE id=$1 AND org_id=$2"),
            "GET_SQL must scope by (id, org_id)"
        );
        assert!(
            GET_SQL.contains("FROM agent_runs"),
            "GET_SQL must query the agent_runs table"
        );
    }

    #[test]
    fn mark_requires_action_sql_sets_correct_status_and_scopes_org() {
        assert!(
            MARK_REQUIRES_ACTION_SQL.contains("status='requires_action'"),
            "MARK_REQUIRES_ACTION_SQL must set status to requires_action"
        );
        assert!(
            MARK_REQUIRES_ACTION_SQL.contains("WHERE id=$1 AND org_id=$2"),
            "MARK_REQUIRES_ACTION_SQL must scope by (id, org_id)"
        );
    }

    #[test]
    fn list_and_get_sql_cast_numeric_to_float8() {
        // NUMERIC(12,6) columns must be cast to ::float8 so sqlx can decode
        // them as f64 without a rust_decimal dependency.
        assert!(
            LIST_SQL.contains("::float8"),
            "LIST_SQL must cast NUMERIC columns to float8"
        );
        assert!(
            GET_SQL.contains("::float8"),
            "GET_SQL must cast NUMERIC columns to float8"
        );
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
