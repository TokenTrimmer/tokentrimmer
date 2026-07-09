//! Org-scoped Postgres store for workflow definitions, runs, and node runs (W1a Task 4).
//!
//! Mirrors `agent_run_store.rs` conventions verbatim:
//! - `pub(crate) const *_SQL: &str` for every SQL statement
//! - `sqlx::query(SQL).bind(...).execute/fetch_*` — runtime sqlx, no `query!` macros
//! - `::float8` casts on every `NUMERIC(12,6)` read (`cost_usd`, `max_cost_usd`)
//! - `ON CONFLICT (id) DO NOTHING` on all INSERT paths
//! - Best-effort writes for node-run journaling (warn-and-continue, no `?`-propagate)
//! - `WHERE id=$1 AND org_id=$2` / `WHERE org_id=$1` org-scope on every query

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::types::WorkflowDefinition;

// ---------------------------------------------------------------------------
// WorkflowRunRecord
// ---------------------------------------------------------------------------

/// A row destined for (or read from) `workflow_runs`. Enums are stored as
/// their snake_case wire strings; numeric types match the DB schema
/// (`NUMERIC(12,6)`). Timestamps are read directly from the DB.
///
/// `error`, `started_at`, and `finished_at` are populated by `run_record_from_row`
/// (used in `get_run`/`list_runs`) but not directly read by the Task-8 handlers.
/// Wired in W1c dashboard. Narrow allow kept here rather than a file-level blanket.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct WorkflowRunRecord {
    pub id: Uuid,
    pub workflow_id: Uuid,
    pub version: i32,
    pub org_id: Uuid,
    /// Snake-case status string: `"running"`, `"completed"`, `"failed"`.
    pub status: String,
    pub inputs: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub max_cost_usd: Option<f64>,
    /// Sum of per-node baseline costs (what the run would have cost without
    /// routing). Added in W2a Task 5 (migration 0029).
    pub baseline_cost_usd: f64,
    /// `(baseline_cost_usd - cost_usd).max(0.0)`: USD saved by routing.
    /// Added in W2a Task 5 (migration 0029).
    pub saved_usd: f64,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// WorkflowDefMeta
// ---------------------------------------------------------------------------

/// Lightweight metadata row returned by `list_definitions`. Callers needing
/// the full definition should call `get_definition`.
#[derive(Debug, Clone)]
pub(crate) struct WorkflowDefMeta {
    pub id: Uuid,
    pub name: String,
    pub version: i32,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// SQL constants — definitions
// ---------------------------------------------------------------------------

/// INSERT a new definition version, computing the next version atomically as
/// `COALESCE(MAX(version), 0) + 1` for the `(id, org_id)` pair.
/// `ON CONFLICT (org_id, id, version) DO NOTHING` guards against concurrent races
/// and matches the `PRIMARY KEY (org_id, id, version)` from migration 0028.
/// Returns the inserted version via `RETURNING version`.
pub(crate) const INSERT_DEFINITION_SQL: &str = "\
WITH next_v AS (\
  SELECT COALESCE(MAX(version), 0) + 1 AS v \
  FROM workflow_definitions \
  WHERE id = $1 AND org_id = $2\
) \
INSERT INTO workflow_definitions (id, org_id, version, definition, content_hash) \
SELECT $1, $2, v, $3, $4 FROM next_v \
ON CONFLICT (org_id, id, version) DO NOTHING \
RETURNING version";

/// SELECT the latest version of a definition, scoped by `(id, org_id)`.
pub(crate) const GET_DEFINITION_SQL: &str = "\
SELECT definition, version \
FROM workflow_definitions \
WHERE id = $1 AND org_id = $2 \
ORDER BY version DESC \
LIMIT 1";

/// SELECT the latest-version row for each definition owned by an org.
/// `DISTINCT ON (id)` with `ORDER BY id, version DESC` picks the row with
/// the highest version per definition id.
pub(crate) const LIST_DEFINITIONS_SQL: &str = "\
SELECT DISTINCT ON (id) id, definition->>'name' AS name, version, created_at \
FROM workflow_definitions \
WHERE org_id = $1 \
ORDER BY id, version DESC \
LIMIT $2";

// ---------------------------------------------------------------------------
// SQL constants — runs
// ---------------------------------------------------------------------------

/// INSERT a workflow run (status `'running'`). `ON CONFLICT (id) DO NOTHING`
/// makes the call idempotent against duplicate run ids.
pub(crate) const INSERT_RUN_SQL: &str = "\
INSERT INTO workflow_runs \
  (id, workflow_id, version, org_id, status, inputs, cost_usd, max_cost_usd) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
ON CONFLICT (id) DO NOTHING";

/// UPDATE a run to its terminal state (`finished_at = now()`).
/// Scoped by `(id, org_id)` so a wrong-org caller cannot overwrite another
/// org's run.
pub(crate) const FINISH_RUN_SQL: &str = "\
UPDATE workflow_runs \
SET status = $3, cost_usd = $4, baseline_cost_usd = $5, saved_usd = $6, error = $7, finished_at = now() \
WHERE id = $1 AND org_id = $2";

/// Write the flow-level quality-gate verdict to the run row (migration 0036).
/// Best-effort + scoped by `(id, org_id)` — the gate's detached judge calls this
/// on completion. Mirrors `finish_run`'s fail-open posture (a write error logs +
/// returns; the run is unaffected — the verdict is an attestation attribute, not
/// a run-state change).
pub(crate) const UPSERT_QUALITY_VERDICT_SQL: &str = "\
UPDATE workflow_runs \
SET quality_verdict = $3 \
WHERE id = $1 AND org_id = $2";

/// SELECT a single run scoped by `(id, org_id)`.
///
/// `NUMERIC(12,6)` columns are cast to `::float8` so sqlx can decode them
/// as `f64` without a `rust_decimal` dependency (mirrors `agent_run_store`).
pub(crate) const GET_RUN_SQL: &str = "\
SELECT id, workflow_id, version, org_id, status, inputs, \
       cost_usd::float8 AS cost_usd, max_cost_usd::float8 AS max_cost_usd, \
       baseline_cost_usd::float8 AS baseline_cost_usd, \
       saved_usd::float8 AS saved_usd, \
       error, started_at, finished_at \
FROM workflow_runs \
WHERE id = $1 AND org_id = $2";

/// SELECT recent runs for an org, ordered most-recent first.
///
/// `NUMERIC(12,6)` columns are cast to `::float8` for the same reason as
/// [`GET_RUN_SQL`].
pub(crate) const LIST_RUNS_SQL: &str = "\
SELECT id, workflow_id, version, org_id, status, inputs, \
       cost_usd::float8 AS cost_usd, max_cost_usd::float8 AS max_cost_usd, \
       baseline_cost_usd::float8 AS baseline_cost_usd, \
       saved_usd::float8 AS saved_usd, \
       error, started_at, finished_at \
FROM workflow_runs \
WHERE org_id = $1 \
ORDER BY started_at DESC \
LIMIT $2";

// ---------------------------------------------------------------------------
// SQL constants — node runs
// ---------------------------------------------------------------------------

/// INSERT a workflow node run. `ON CONFLICT (id) DO NOTHING`.
/// Called best-effort from the engine — errors are warn-and-swallowed.
pub(crate) const INSERT_NODE_RUN_SQL: &str = "\
INSERT INTO workflow_node_runs \
  (id, run_id, node_id, attempt, status, output, cost_usd, model_used, error) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
ON CONFLICT (id) DO NOTHING";

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

fn run_record_from_row(row: &sqlx::postgres::PgRow) -> Result<WorkflowRunRecord, sqlx::Error> {
    use sqlx::Row;
    Ok(WorkflowRunRecord {
        id: row.try_get("id")?,
        workflow_id: row.try_get("workflow_id")?,
        version: row.try_get("version")?,
        org_id: row.try_get("org_id")?,
        status: row.try_get("status")?,
        inputs: row.try_get("inputs")?,
        cost_usd: row.try_get("cost_usd")?,
        max_cost_usd: row.try_get("max_cost_usd")?,
        baseline_cost_usd: row.try_get("baseline_cost_usd")?,
        saved_usd: row.try_get("saved_usd")?,
        error: row.try_get("error")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

// ---------------------------------------------------------------------------
// Definition store functions
// ---------------------------------------------------------------------------

/// Insert a new version of a workflow definition. Computes the next version
/// atomically (COALESCE(MAX(version), 0) + 1 for the `(id, org_id)` pair).
///
/// Returns:
/// - `Ok(Some(version))` — row inserted; `version` is the atomically computed version.
/// - `Ok(None)` — ON CONFLICT fired (another insert of the same `(org_id, id, version)` won the race).
/// - `Err(e)` — Postgres error.
pub(crate) async fn insert_definition(
    pool: &PgPool,
    org_id: Uuid,
    def: &WorkflowDefinition,
    content_hash: &str,
) -> Result<Option<i32>, sqlx::Error> {
    let definition_json = serde_json::to_value(def).map_err(|e| {
        sqlx::Error::Protocol(format!(
            "workflow_definitions INSERT: failed to serialize definition: {e}"
        ))
    })?;
    let row = sqlx::query(INSERT_DEFINITION_SQL)
        .bind(def.id) // $1 id           UUID
        .bind(org_id) // $2 org_id        UUID
        .bind(definition_json) // $3 definition    JSONB
        .bind(content_hash) // $4 content_hash  TEXT
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => {
            use sqlx::Row;
            let v = r.try_get::<i32, _>("version")?;
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

/// Fetch the latest version of a definition scoped by `(id, org_id)`.
/// Returns `None` on miss or DB error (best-effort).
pub(crate) async fn get_definition(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
) -> Option<(WorkflowDefinition, i32)> {
    let result = sqlx::query(GET_DEFINITION_SQL)
        .bind(id) // $1 id     UUID
        .bind(org_id) // $2 org_id UUID
        .fetch_optional(pool)
        .await;
    match result {
        Ok(Some(row)) => {
            use sqlx::Row;
            let version: i32 = row.try_get("version").ok()?;
            let definition_val: serde_json::Value = row.try_get("definition").ok()?;
            let def: WorkflowDefinition = serde_json::from_value(definition_val).ok()?;
            Some((def, version))
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                workflow_id = %id,
                error = %e,
                "workflow_definitions GET failed (best-effort, returning None)"
            );
            None
        }
    }
}

/// Return up to `limit` definitions for `org_id` (latest version per id),
/// ordered by id. Returns an empty `Vec` on DB error (best-effort).
pub(crate) async fn list_definitions(
    pool: &PgPool,
    org_id: Uuid,
    limit: i64,
) -> Vec<WorkflowDefMeta> {
    let result = sqlx::query(LIST_DEFINITIONS_SQL)
        .bind(org_id) // $1 org_id UUID
        .bind(limit) // $2 limit  BIGINT
        .fetch_all(pool)
        .await;
    match result {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| {
                use sqlx::Row;
                Some(WorkflowDefMeta {
                    id: row.try_get("id").ok()?,
                    name: row.try_get::<String, _>("name").ok()?,
                    version: row.try_get("version").ok()?,
                    created_at: row.try_get("created_at").ok()?,
                })
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                %org_id,
                error = %e,
                "workflow_definitions LIST failed (best-effort, returning empty)"
            );
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Run store functions
// ---------------------------------------------------------------------------

/// Insert a workflow run with `status = 'running'`. Awaited (the engine needs
/// the run row present before the first node executes). Errors are warn-and-
/// swallowed so a DB outage never fails the run request.
pub(crate) async fn insert_run(pool: &PgPool, rec: &WorkflowRunRecord) {
    let result = sqlx::query(INSERT_RUN_SQL)
        .bind(rec.id) // $1 id           UUID
        .bind(rec.workflow_id) // $2 workflow_id  UUID
        .bind(rec.version) // $3 version      INT
        .bind(rec.org_id) // $4 org_id       UUID
        .bind(&rec.status) // $5 status       TEXT
        .bind(&rec.inputs) // $6 inputs       JSONB
        .bind(rec.cost_usd) // $7 cost_usd     NUMERIC
        .bind(rec.max_cost_usd) // $8 max_cost_usd NUMERIC
        .execute(pool)
        .await;
    if let Err(e) = result {
        tracing::warn!(
            run_id = %rec.id,
            error = %e,
            "workflow_runs INSERT failed (best-effort, run not affected)"
        );
    }
}

/// Update a workflow run to its terminal state (`finished_at = now()`).
/// Scoped by `(id, org_id)`. Awaited; errors are warn-and-swallowed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_run(
    pool: &PgPool,
    id: Uuid,
    org_id: Uuid,
    status: &str,
    cost_usd: f64,
    baseline_cost_usd: f64,
    saved_usd: f64,
    error: Option<&str>,
) {
    let result = sqlx::query(FINISH_RUN_SQL)
        .bind(id) // $1 id                UUID
        .bind(org_id) // $2 org_id            UUID
        .bind(status) // $3 status            TEXT
        .bind(cost_usd) // $4 cost_usd          NUMERIC
        .bind(baseline_cost_usd) // $5 baseline_cost_usd  NUMERIC
        .bind(saved_usd) // $6 saved_usd          NUMERIC
        .bind(error) // $7 error             TEXT
        .execute(pool)
        .await;
    if let Err(e) = result {
        tracing::warn!(
            run_id = %id,
            error = %e,
            "workflow_runs UPDATE (finish) failed (best-effort, run not affected)"
        );
    }
}

/// Write the flow-level quality-gate verdict to the run row (migration 0036).
///
/// Best-effort + scoped by `(id, org_id)` (mirrors [`finish_run`]'s fail-open
/// posture: a write error logs + returns; the run is unaffected — the verdict
/// is an attestation attribute, not a run-state change). Called by the gate's
/// detached judge on completion (BACKLOG item #5). `verdict` is the stable code
/// from [`crate::workflow::quality_gate::QualityVerdict::code`]
/// (`equivalent` / `degraded` / `inconclusive`).
pub(crate) async fn upsert_quality_verdict(
    pool: &PgPool,
    id: Uuid,
    org_id: Uuid,
    verdict: &str,
) {
    let result = sqlx::query(UPSERT_QUALITY_VERDICT_SQL)
        .bind(id) // $1 id         UUID
        .bind(org_id) // $2 org_id     UUID
        .bind(verdict) // $3 quality_verdict TEXT
        .execute(pool)
        .await;
    if let Err(e) = result {
        tracing::warn!(
            run_id = %id,
            error = %e,
            "workflow_runs quality_verdict UPDATE failed (best-effort)"
        );
    }
}

/// Fetch a single run scoped by `(id, org_id)`. Returns `None` on miss or
/// DB error (best-effort).
///
/// Wired in W1c dashboard (GET /v1/workflows/:id/runs/:run_id).
#[allow(dead_code)]
pub(crate) async fn get_run(pool: &PgPool, id: Uuid, org_id: Uuid) -> Option<WorkflowRunRecord> {
    let result = sqlx::query(GET_RUN_SQL)
        .bind(id) // $1 id     UUID
        .bind(org_id) // $2 org_id UUID
        .fetch_optional(pool)
        .await;
    match result {
        Ok(Some(row)) => run_record_from_row(&row).ok(),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                run_id = %id,
                error = %e,
                "workflow_runs GET failed (best-effort, returning None)"
            );
            None
        }
    }
}

/// Return up to `limit` runs for `org_id`, ordered most-recent first.
/// Returns an empty `Vec` on DB error (best-effort).
///
/// Wired in W1c dashboard (GET /v1/workflows/runs or per-workflow run history).
#[allow(dead_code)]
pub(crate) async fn list_runs(pool: &PgPool, org_id: Uuid, limit: i64) -> Vec<WorkflowRunRecord> {
    let result = sqlx::query(LIST_RUNS_SQL)
        .bind(org_id) // $1 org_id UUID
        .bind(limit) // $2 limit  BIGINT
        .fetch_all(pool)
        .await;
    match result {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| run_record_from_row(row).ok())
            .collect(),
        Err(e) => {
            tracing::warn!(
                %org_id,
                error = %e,
                "workflow_runs LIST failed (best-effort, returning empty)"
            );
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Node run store function (best-effort)
// ---------------------------------------------------------------------------

/// Insert a workflow node run. **Best-effort**: DB errors are logged at WARN
/// and swallowed so a Postgres outage never fails the engine loop.
/// A fresh UUIDv4 is minted for each call (attempt counter defaults to 1).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_node_run(
    pool: &PgPool,
    run_id: Uuid,
    node_id: &str,
    status: &str,
    output: Option<serde_json::Value>,
    cost_usd: f64,
    model_used: Option<&str>,
    error: Option<&str>,
) {
    let id = Uuid::new_v4();
    let result = sqlx::query(INSERT_NODE_RUN_SQL)
        .bind(id) // $1 id         UUID
        .bind(run_id) // $2 run_id     UUID
        .bind(node_id) // $3 node_id    TEXT
        .bind(1_i32) // $4 attempt    INT (default 1)
        .bind(status) // $5 status     TEXT
        .bind(output) // $6 output     JSONB
        .bind(cost_usd) // $7 cost_usd   NUMERIC
        .bind(model_used) // $8 model_used TEXT
        .bind(error) // $9 error      TEXT
        .execute(pool)
        .await;
    if let Err(e) = result {
        tracing::warn!(
            %run_id,
            %node_id,
            error = %e,
            "workflow_node_runs INSERT failed (best-effort, run not affected)"
        );
    }
}

// ---------------------------------------------------------------------------
// Unit tests — pure (no DB required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::{
        WorkflowRunRecord, FINISH_RUN_SQL, GET_DEFINITION_SQL, GET_RUN_SQL, INSERT_DEFINITION_SQL,
        INSERT_NODE_RUN_SQL, INSERT_RUN_SQL, LIST_DEFINITIONS_SQL, LIST_RUNS_SQL,
    };

    // ------------------------------------------------------------------
    // WorkflowRunRecord construction
    // ------------------------------------------------------------------

    #[test]
    fn workflow_run_record_new_running_defaults() {
        let rec = WorkflowRunRecord {
            id: Uuid::nil(),
            workflow_id: Uuid::nil(),
            version: 1,
            org_id: Uuid::nil(),
            status: "running".to_string(),
            inputs: None,
            cost_usd: 0.0,
            max_cost_usd: None,
            baseline_cost_usd: 0.0,
            saved_usd: 0.0,
            error: None,
            started_at: Utc::now(),
            finished_at: None,
        };
        assert_eq!(rec.status, "running");
        assert_eq!(rec.cost_usd, 0.0);
        assert_eq!(rec.baseline_cost_usd, 0.0);
        assert_eq!(rec.saved_usd, 0.0);
        assert!(rec.finished_at.is_none());
        assert!(rec.error.is_none());
        assert!(rec.max_cost_usd.is_none());
    }

    #[test]
    fn workflow_run_record_terminal_fields() {
        let rec = WorkflowRunRecord {
            id: Uuid::nil(),
            workflow_id: Uuid::nil(),
            version: 2,
            org_id: Uuid::nil(),
            status: "completed".to_string(),
            inputs: Some(serde_json::json!({"prompt": "hello"})),
            cost_usd: 0.05,
            max_cost_usd: Some(1.0),
            baseline_cost_usd: 0.10,
            saved_usd: 0.05,
            error: None,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
        };
        assert_eq!(rec.status, "completed");
        assert!((rec.cost_usd - 0.05).abs() < 1e-9);
        assert!((rec.baseline_cost_usd - 0.10).abs() < 1e-9);
        assert!((rec.saved_usd - 0.05).abs() < 1e-9);
        assert_eq!(rec.version, 2);
        assert!(rec.finished_at.is_some());
    }

    // ------------------------------------------------------------------
    // SQL shape tests — definitions (TDD gate: written before constants)
    // ------------------------------------------------------------------

    #[test]
    fn insert_definition_sql_computes_next_version_atomically() {
        // Must use a CTE with COALESCE(MAX(version), 0) + 1 for atomic versioning.
        assert!(
            INSERT_DEFINITION_SQL.contains("COALESCE"),
            "INSERT_DEFINITION_SQL must use COALESCE for version computation"
        );
        assert!(
            INSERT_DEFINITION_SQL.contains("MAX(version)"),
            "INSERT_DEFINITION_SQL must select MAX(version)"
        );
        assert!(
            INSERT_DEFINITION_SQL.contains("RETURNING version"),
            "INSERT_DEFINITION_SQL must RETURN the inserted version"
        );
        assert!(
            INSERT_DEFINITION_SQL.contains("ON CONFLICT (org_id, id, version) DO NOTHING"),
            "INSERT_DEFINITION_SQL must guard against concurrent inserts with the org_id PK"
        );
        assert!(
            INSERT_DEFINITION_SQL.contains("INSERT INTO workflow_definitions"),
            "INSERT_DEFINITION_SQL must target workflow_definitions table"
        );
    }

    #[test]
    fn get_definition_sql_scopes_by_org_and_id() {
        assert!(
            GET_DEFINITION_SQL.contains("WHERE id = $1 AND org_id = $2"),
            "GET_DEFINITION_SQL must scope by (id, org_id)"
        );
        assert!(
            GET_DEFINITION_SQL.contains("ORDER BY version DESC"),
            "GET_DEFINITION_SQL must order by version DESC to get latest"
        );
        assert!(
            GET_DEFINITION_SQL.contains("FROM workflow_definitions"),
            "GET_DEFINITION_SQL must query workflow_definitions table"
        );
    }

    #[test]
    fn list_definitions_sql_contains_expected_clauses() {
        assert!(
            LIST_DEFINITIONS_SQL.contains("WHERE org_id = $1"),
            "LIST_DEFINITIONS_SQL must filter by org_id"
        );
        assert!(
            LIST_DEFINITIONS_SQL.contains("LIMIT $2"),
            "LIST_DEFINITIONS_SQL must apply a LIMIT parameter"
        );
        assert!(
            LIST_DEFINITIONS_SQL.contains("FROM workflow_definitions"),
            "LIST_DEFINITIONS_SQL must query workflow_definitions table"
        );
    }

    #[test]
    fn list_definitions_sql_extracts_name_from_jsonb() {
        assert!(
            LIST_DEFINITIONS_SQL.contains("definition->>'name'"),
            "LIST_DEFINITIONS_SQL must extract name from JSONB definition column"
        );
    }

    // ------------------------------------------------------------------
    // SQL shape tests — runs
    // ------------------------------------------------------------------

    #[test]
    fn insert_run_sql_has_conflict_guard() {
        assert!(
            INSERT_RUN_SQL.contains("INSERT INTO workflow_runs"),
            "INSERT_RUN_SQL must target workflow_runs table"
        );
        assert!(
            INSERT_RUN_SQL.contains("ON CONFLICT (id) DO NOTHING"),
            "INSERT_RUN_SQL must guard against duplicate run ids"
        );
    }

    #[test]
    fn finish_run_sql_scopes_by_org_and_id() {
        assert!(
            FINISH_RUN_SQL.contains("UPDATE workflow_runs"),
            "FINISH_RUN_SQL must target workflow_runs table"
        );
        assert!(
            FINISH_RUN_SQL.contains("WHERE id = $1 AND org_id = $2"),
            "FINISH_RUN_SQL must scope by (id, org_id)"
        );
    }

    #[test]
    fn finish_run_sql_sets_baseline_and_saved() {
        assert!(
            FINISH_RUN_SQL.contains("baseline_cost_usd = $5"),
            "FINISH_RUN_SQL must set baseline_cost_usd at bind position $5"
        );
        assert!(
            FINISH_RUN_SQL.contains("saved_usd = $6"),
            "FINISH_RUN_SQL must set saved_usd at bind position $6"
        );
        assert!(
            FINISH_RUN_SQL.contains("error = $7"),
            "FINISH_RUN_SQL must set error at bind position $7 (after baseline/saved)"
        );
    }

    #[test]
    fn get_run_sql_scopes_by_org_and_id() {
        assert!(
            GET_RUN_SQL.contains("WHERE id = $1 AND org_id = $2"),
            "GET_RUN_SQL must scope by (id, org_id)"
        );
        assert!(
            GET_RUN_SQL.contains("FROM workflow_runs"),
            "GET_RUN_SQL must query the workflow_runs table"
        );
    }

    #[test]
    fn list_runs_sql_contains_expected_clauses() {
        assert!(
            LIST_RUNS_SQL.contains("WHERE org_id = $1"),
            "LIST_RUNS_SQL must filter by org_id"
        );
        assert!(
            LIST_RUNS_SQL.contains("ORDER BY started_at DESC"),
            "LIST_RUNS_SQL must order by started_at DESC"
        );
        assert!(
            LIST_RUNS_SQL.contains("LIMIT $2"),
            "LIST_RUNS_SQL must apply a LIMIT parameter"
        );
        assert!(
            LIST_RUNS_SQL.contains("FROM workflow_runs"),
            "LIST_RUNS_SQL must query the workflow_runs table"
        );
    }

    #[test]
    fn get_run_and_list_runs_sql_cast_numeric_to_float8() {
        // NUMERIC(12,6) columns must be cast to ::float8 so sqlx can decode
        // them as f64 without a rust_decimal dependency (mirrors agent_run_store).
        assert!(
            GET_RUN_SQL.contains("cost_usd::float8"),
            "GET_RUN_SQL must cast cost_usd to float8"
        );
        assert!(
            GET_RUN_SQL.contains("max_cost_usd::float8"),
            "GET_RUN_SQL must cast max_cost_usd to float8"
        );
        assert!(
            GET_RUN_SQL.contains("baseline_cost_usd::float8"),
            "GET_RUN_SQL must cast baseline_cost_usd to float8"
        );
        assert!(
            GET_RUN_SQL.contains("saved_usd::float8"),
            "GET_RUN_SQL must cast saved_usd to float8"
        );
        assert!(
            LIST_RUNS_SQL.contains("cost_usd::float8"),
            "LIST_RUNS_SQL must cast cost_usd to float8"
        );
        assert!(
            LIST_RUNS_SQL.contains("max_cost_usd::float8"),
            "LIST_RUNS_SQL must cast max_cost_usd to float8"
        );
        assert!(
            LIST_RUNS_SQL.contains("baseline_cost_usd::float8"),
            "LIST_RUNS_SQL must cast baseline_cost_usd to float8"
        );
        assert!(
            LIST_RUNS_SQL.contains("saved_usd::float8"),
            "LIST_RUNS_SQL must cast saved_usd to float8"
        );
    }

    // ------------------------------------------------------------------
    // SQL shape tests — node runs
    // ------------------------------------------------------------------

    #[test]
    fn insert_node_run_sql_has_conflict_guard() {
        assert!(
            INSERT_NODE_RUN_SQL.contains("INSERT INTO workflow_node_runs"),
            "INSERT_NODE_RUN_SQL must target workflow_node_runs table"
        );
        assert!(
            INSERT_NODE_RUN_SQL.contains("ON CONFLICT (id) DO NOTHING"),
            "INSERT_NODE_RUN_SQL must guard against duplicate node run ids"
        );
    }
}
