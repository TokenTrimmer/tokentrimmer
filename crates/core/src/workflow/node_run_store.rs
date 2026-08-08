//! Persistence for completed workflow-node journal entries.
//!
//! Node rows do not carry an org id, so every read joins through the owning
//! `workflow_runs` row and scopes that join to the authenticated org. The
//! New rows carry the gateway workflow-node envelope captured by the engine.
//! Legacy rows have a null `finished_at`; their `started_at` is only the old
//! post-run persistence timestamp. Neither form is provider-attempt timing.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row as _};
use uuid::Uuid;

/// One persisted workflow-node journal entry.
#[derive(Debug, Clone)]
pub(crate) struct WorkflowNodeRunRecord {
    pub id: Uuid,
    pub node_id: String,
    pub attempt: i32,
    pub status: String,
    pub output: Option<serde_json::Value>,
    /// Bounded, value-free capture of the node's consumed template references
    /// (secrets redacted to `"***"`); `None` when the node records no input.
    pub input: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub model_used: Option<String>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Insert one terminal journal entry. A fresh UUID keeps retries idempotent only
/// within the caller's single flush; the engine currently writes one row per
/// completed node execution.
pub(crate) const INSERT_NODE_RUN_SQL: &str = "\
INSERT INTO workflow_node_runs \
  (id, run_id, node_id, attempt, status, input, output, cost_usd, model_used, error, started_at, finished_at) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
ON CONFLICT (id) DO NOTHING";

/// List a bounded journal for one run.
///
/// The join is the tenant boundary: `workflow_node_runs` predates an `org_id`
/// column. Stable tie-breaking by row id makes repeated reads deterministic,
/// while `started_at` preserves engine start order for new rows and post-run
/// persistence order for legacy rows.
pub(crate) const LIST_NODE_RUNS_FOR_RUN_SQL: &str = "\
SELECT nr.id, nr.node_id, nr.attempt, nr.status, nr.output, nr.input, \
       nr.cost_usd::float8 AS cost_usd, nr.model_used, nr.error, \
       nr.started_at, nr.finished_at \
FROM workflow_node_runs AS nr \
INNER JOIN workflow_runs AS wr ON wr.id = nr.run_id \
WHERE nr.run_id = $1 AND wr.org_id = $2 \
ORDER BY nr.started_at ASC, nr.id ASC \
LIMIT $3";

fn record_from_row(row: &sqlx::postgres::PgRow) -> Result<WorkflowNodeRunRecord, sqlx::Error> {
    Ok(WorkflowNodeRunRecord {
        id: row.try_get("id")?,
        node_id: row.try_get("node_id")?,
        attempt: row.try_get("attempt")?,
        status: row.try_get("status")?,
        output: row.try_get("output")?,
        input: row.try_get("input")?,
        cost_usd: row.try_get("cost_usd")?,
        model_used: row.try_get("model_used")?,
        error: row.try_get("error")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

/// Insert a workflow node run. Database errors are logged and swallowed so a
/// journal outage never changes the already-computed workflow result.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_node_run(
    pool: &PgPool,
    run_id: Uuid,
    node_id: &str,
    status: &str,
    input: Option<serde_json::Value>,
    output: Option<serde_json::Value>,
    cost_usd: f64,
    model_used: Option<&str>,
    error: Option<&str>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
) {
    let id = Uuid::new_v4();
    let result = sqlx::query(INSERT_NODE_RUN_SQL)
        .bind(id)
        .bind(run_id)
        .bind(node_id)
        .bind(1_i32)
        .bind(status)
        .bind(input)
        .bind(output)
        .bind(cost_usd)
        .bind(model_used)
        .bind(error)
        .bind(started_at)
        .bind(finished_at)
        .execute(pool)
        .await;
    if let Err(error) = result {
        tracing::warn!(
            %run_id,
            %node_id,
            %error,
            "workflow_node_runs INSERT failed (best-effort, run not affected)"
        );
    }
}

/// Read a bounded, tenant-scoped node journal.
pub(crate) async fn list_node_runs_for_run(
    pool: &PgPool,
    run_id: Uuid,
    org_id: Uuid,
    limit: i64,
) -> Result<Vec<WorkflowNodeRunRecord>, sqlx::Error> {
    sqlx::query(LIST_NODE_RUNS_FOR_RUN_SQL)
        .bind(run_id)
        .bind(org_id)
        .bind(limit)
        .fetch_all(pool)
        .await?
        .iter()
        .map(record_from_row)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{INSERT_NODE_RUN_SQL, LIST_NODE_RUNS_FOR_RUN_SQL};

    #[test]
    fn insert_node_run_sql_has_conflict_guard() {
        assert!(INSERT_NODE_RUN_SQL.contains("INSERT INTO workflow_node_runs"));
        assert!(INSERT_NODE_RUN_SQL.contains("started_at, finished_at"));
        assert!(INSERT_NODE_RUN_SQL.contains("ON CONFLICT (id) DO NOTHING"));
        assert!(INSERT_NODE_RUN_SQL.contains("input"));
        assert!(INSERT_NODE_RUN_SQL.contains("output"));
    }

    #[test]
    fn node_journal_read_is_tenant_scoped_bounded_and_stable() {
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL
            .contains("INNER JOIN workflow_runs AS wr ON wr.id = nr.run_id"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("WHERE nr.run_id = $1 AND wr.org_id = $2"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("ORDER BY nr.started_at ASC, nr.id ASC"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("LIMIT $3"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("nr.cost_usd::float8"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("nr.started_at, nr.finished_at"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("nr.input"));
    }
}
