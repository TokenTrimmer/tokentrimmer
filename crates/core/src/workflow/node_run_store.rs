//! Persistence for completed workflow-node journal entries.
//!
//! Node rows do not carry an org id, so every read joins through the owning
//! `workflow_runs` row and scopes that join to the authenticated org. The
//! timestamps are persistence timestamps: the synchronous engine journal is
//! flushed after execution, so callers must not present them as provider timing.

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
    pub cost_usd: f64,
    pub model_used: Option<String>,
    pub error: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

/// Insert one terminal journal entry. A fresh UUID keeps retries idempotent only
/// within the caller's single flush; the engine currently writes one row per
/// completed node execution.
pub(crate) const INSERT_NODE_RUN_SQL: &str = "\
INSERT INTO workflow_node_runs \
  (id, run_id, node_id, attempt, status, output, cost_usd, model_used, error) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
ON CONFLICT (id) DO NOTHING";

/// List a bounded journal for one run.
///
/// The join is the tenant boundary: `workflow_node_runs` predates an `org_id`
/// column. Stable tie-breaking by row id makes repeated reads deterministic,
/// while `started_at` preserves the order in which the post-run journal flush
/// inserted the rows.
pub(crate) const LIST_NODE_RUNS_FOR_RUN_SQL: &str = "\
SELECT nr.id, nr.node_id, nr.attempt, nr.status, nr.output, \
       nr.cost_usd::float8 AS cost_usd, nr.model_used, nr.error, \
       nr.started_at AS recorded_at \
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
        cost_usd: row.try_get("cost_usd")?,
        model_used: row.try_get("model_used")?,
        error: row.try_get("error")?,
        recorded_at: row.try_get("recorded_at")?,
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
    output: Option<serde_json::Value>,
    cost_usd: f64,
    model_used: Option<&str>,
    error: Option<&str>,
) {
    let id = Uuid::new_v4();
    let result = sqlx::query(INSERT_NODE_RUN_SQL)
        .bind(id)
        .bind(run_id)
        .bind(node_id)
        .bind(1_i32)
        .bind(status)
        .bind(output)
        .bind(cost_usd)
        .bind(model_used)
        .bind(error)
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
        assert!(INSERT_NODE_RUN_SQL.contains("ON CONFLICT (id) DO NOTHING"));
    }

    #[test]
    fn node_journal_read_is_tenant_scoped_bounded_and_stable() {
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL
            .contains("INNER JOIN workflow_runs AS wr ON wr.id = nr.run_id"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("WHERE nr.run_id = $1 AND wr.org_id = $2"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("ORDER BY nr.started_at ASC, nr.id ASC"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("LIMIT $3"));
        assert!(LIST_NODE_RUNS_FOR_RUN_SQL.contains("nr.cost_usd::float8"));
    }
}
