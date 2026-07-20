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
use sha2::{Digest as _, Sha256};
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
// WorkflowRunIdempotencyBinding
// ---------------------------------------------------------------------------

/// The durable, gateway-owned binding for one logical workflow invocation.
///
/// The public API deliberately never persists the caller's raw
/// `Idempotency-Key`: [`workflow_run_invocation_key_hash`] domain-separates and
/// scopes it to its org + workflow before it reaches this record.  The two
/// request fingerprints make a duplicate safe to replay only when it still
/// denotes the same canonical input and execution options.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkflowRunIdempotencyBinding {
    pub run_id: Uuid,
    pub workflow_version: i32,
    pub input_hash: [u8; 32],
    pub request_options_hash: [u8; 32],
}

/// Input used when atomically claiming an idempotency key and creating its
/// initial `workflow_runs` row.  `run_id` and the persisted run fields come
/// from the accompanying [`WorkflowRunRecord`].
#[derive(Debug, Clone)]
pub(crate) struct NewWorkflowRunIdempotency {
    pub org_id: Uuid,
    pub workflow_id: Uuid,
    pub invocation_key_hash: [u8; 32],
    pub workflow_version: i32,
    pub input_hash: [u8; 32],
    pub request_options_hash: [u8; 32],
}

/// Result of atomically claiming a stable invocation key.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CreateOrReuseWorkflowRun {
    /// This caller inserted both the idempotency mapping and its initial run.
    Created,
    /// A previous caller already owns the logical invocation.
    Existing(WorkflowRunIdempotencyBinding),
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

/// Immutable version metadata exposed by the bounded workflow-history API.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkflowDefinitionVersionMeta {
    pub version: i32,
    pub content_hash: String,
    pub created_at: DateTime<Utc>,
}

/// One exact immutable definition plus its authoritative storage metadata.
#[derive(Debug, Clone)]
pub(crate) struct WorkflowDefinitionVersionRecord {
    pub definition: WorkflowDefinition,
    pub version: i32,
    pub content_hash: String,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// SQL constants — definitions
// ---------------------------------------------------------------------------

/// INSERT a new definition version, computing the next version atomically as
/// `COALESCE(MAX(version), 0) + 1` for the `(id, org_id)` pair. When `$5` is
/// present, the insert proceeds only if it equals that org/workflow's current
/// latest version (`0` means no retained version exists yet).
/// `ON CONFLICT (org_id, id, version) DO NOTHING` guards against concurrent races
/// and matches the `PRIMARY KEY (org_id, id, version)` from migration 0028.
/// Returns the inserted version via `RETURNING version`.
pub(crate) const INSERT_DEFINITION_SQL: &str = "\
WITH current_version AS (\
  SELECT MAX(version) AS latest_version \
  FROM workflow_definitions \
  WHERE id = $1 AND org_id = $2\
), next_v AS (\
  SELECT COALESCE(latest_version, 0) + 1 AS v \
  FROM current_version \
  WHERE $5::INT IS NULL OR COALESCE(latest_version, 0) = $5\
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

/// SELECT one immutable definition version, scoped by `(id, org_id)`.
/// Durable invocations must execute the version accepted at enqueue time, not
/// whichever version happens to be latest when a retry arrives.
pub(crate) const GET_DEFINITION_VERSION_SQL: &str = "\
SELECT definition, version, content_hash, created_at \
FROM workflow_definitions \
WHERE id = $1 AND org_id = $2 AND version = $3 \
LIMIT 1";

/// SELECT bounded immutable version metadata for one org-owned definition.
pub(crate) const LIST_DEFINITION_VERSIONS_SQL: &str = "\
SELECT version, content_hash, created_at \
FROM workflow_definitions \
WHERE id = $1 AND org_id = $2 \
ORDER BY version DESC \
LIMIT $3";

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

/// SELECT recent runs for one org-owned workflow, ordered most-recent first.
///
/// `NUMERIC(12,6)` columns are cast to `::float8` for the same reason as
/// [`GET_RUN_SQL`].
pub(crate) const LIST_WORKFLOW_RUNS_SQL: &str = "\
SELECT id, workflow_id, version, org_id, status, inputs, \
       cost_usd::float8 AS cost_usd, max_cost_usd::float8 AS max_cost_usd, \
       baseline_cost_usd::float8 AS baseline_cost_usd, \
       saved_usd::float8 AS saved_usd, \
       error, started_at, finished_at \
FROM workflow_runs \
WHERE org_id = $1 AND workflow_id = $2 \
ORDER BY started_at DESC \
LIMIT $3";

// ---------------------------------------------------------------------------
// SQL constants — durable run-idempotency mappings
// ---------------------------------------------------------------------------

/// Insert the gateway-owned stable-invocation mapping.  The mapping foreign
/// keys are deferrable because [`create_or_reuse_idempotent_run`] inserts this
/// row and its `workflow_runs` row in one transaction.
pub(crate) const INSERT_WORKFLOW_RUN_IDEMPOTENCY_SQL: &str = "\
INSERT INTO workflow_run_idempotency \
  (org_id, workflow_id, invocation_key_hash, workflow_version, input_hash, request_options_hash, run_id) \
VALUES ($1, $2, $3, $4, $5, $6, $7) \
ON CONFLICT (org_id, workflow_id, invocation_key_hash) DO NOTHING \
RETURNING run_id";

/// Strict initial-run insert used only with a durable idempotency mapping.
/// Unlike [`INSERT_RUN_SQL`], this intentionally has no fail-open conflict
/// clause: a successful mapping must commit with exactly one corresponding
/// initial run, or neither row is durable.
pub(crate) const INSERT_IDEMPOTENT_RUN_SQL: &str = "\
INSERT INTO workflow_runs \
  (id, workflow_id, version, org_id, status, inputs, cost_usd, max_cost_usd) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8)";

/// Read a mapping by its opaque, org/workflow-scoped stable-key digest.
pub(crate) const GET_WORKFLOW_RUN_IDEMPOTENCY_SQL: &str = "\
SELECT run_id, workflow_version, input_hash, request_options_hash \
FROM workflow_run_idempotency \
WHERE org_id = $1 AND workflow_id = $2 AND invocation_key_hash = $3";

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

fn idempotency_binding_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<WorkflowRunIdempotencyBinding, sqlx::Error> {
    use sqlx::Row;

    fn fixed_digest(bytes: Vec<u8>, column: &str) -> Result<[u8; 32], sqlx::Error> {
        bytes.try_into().map_err(|bytes: Vec<u8>| {
            sqlx::Error::Protocol(format!(
                "workflow_run_idempotency {column} has invalid length {}; expected 32",
                bytes.len()
            ))
        })
    }

    Ok(WorkflowRunIdempotencyBinding {
        run_id: row.try_get("run_id")?,
        workflow_version: row.try_get("workflow_version")?,
        input_hash: fixed_digest(row.try_get("input_hash")?, "input_hash")?,
        request_options_hash: fixed_digest(
            row.try_get("request_options_hash")?,
            "request_options_hash",
        )?,
    })
}

/// Hash a raw stable invocation key before persistence.  The org + workflow
/// scope lets unrelated workflows reuse a caller key without becoming a
/// correlation identifier, and the versioned domain prevents a future format
/// change from silently changing old mappings' meaning.
#[must_use]
pub(crate) fn workflow_run_invocation_key_hash(
    org_id: Uuid,
    workflow_id: Uuid,
    invocation_key: &str,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"tt.workflow-run-idempotency.v1\0");
    hasher.update(org_id.as_bytes());
    hasher.update(workflow_id.as_bytes());
    hasher.update(invocation_key.as_bytes());
    hasher.finalize().into()
}

/// Fingerprint canonical workflow input JSON.  `serde_json::Value` uses a
/// deterministic object-key order in this workspace, so semantically equal
/// objects that differ only in wire key order map to the same invocation while
/// arrays and JSON number forms retain their normal exact semantics.
pub(crate) fn workflow_run_input_hash(
    inputs: &serde_json::Value,
) -> Result<[u8; 32], serde_json::Error> {
    let canonical = serde_json::to_vec(inputs)?;
    Ok(Sha256::digest(canonical).into())
}

/// Fingerprint request options which affect execution but are not workflow
/// inputs.  `stream` is intentionally excluded: it changes response delivery,
/// not the logical run, so a reconnect may safely ask for status JSON instead
/// of starting a second SSE execution.
pub(crate) fn workflow_run_request_options_hash(
    max_cost_usd: Option<f64>,
) -> Result<[u8; 32], serde_json::Error> {
    let canonical = serde_json::to_vec(&serde_json::json!({
        "max_cost_usd": max_cost_usd,
    }))?;
    Ok(Sha256::digest(canonical).into())
}

// ---------------------------------------------------------------------------
// Definition store functions
// ---------------------------------------------------------------------------

/// Insert a new version of a workflow definition. Computes the next version
/// atomically (COALESCE(MAX(version), 0) + 1 for the `(id, org_id)` pair).
///
/// Returns:
/// - `Ok(Some(version))` — row inserted; `version` is the atomically computed version.
/// - `Ok(None)` — the expected-latest precondition failed, or another insert of
///   the same `(org_id, id, version)` won the race.
/// - `Err(e)` — Postgres error.
pub(crate) async fn insert_definition(
    pool: &PgPool,
    org_id: Uuid,
    def: &WorkflowDefinition,
    content_hash: &str,
    expected_latest_version: Option<i32>,
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
        .bind(expected_latest_version) // $5 optional optimistic base version
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

/// Fetch one immutable workflow definition version scoped by `(id, org_id)`.
///
/// This is intentionally distinct from [`get_definition`]: a durable run
/// retry must never silently upgrade to the latest definition after an editor
/// saved a newer version.  Like the existing definition read, a missing row or
/// decode/database failure returns `None`; callers use their normal 404/503
/// boundary policy.
pub(crate) async fn get_definition_version(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    version: i32,
) -> Option<(WorkflowDefinition, i32)> {
    let result = sqlx::query(GET_DEFINITION_VERSION_SQL)
        .bind(id) // $1 id     UUID
        .bind(org_id) // $2 org_id UUID
        .bind(version) // $3 version INT
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
                workflow_version = version,
                error = %e,
                "workflow_definitions version GET failed (best-effort, returning None)"
            );
            None
        }
    }
}

/// Strictly fetch one immutable version for the read-only version-history API.
///
/// Unlike the execution helper above, this preserves database and decode errors
/// so the HTTP layer can return a generic failure instead of misreporting a
/// temporary storage problem as a missing version.
pub(crate) async fn get_definition_version_record(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    version: i32,
) -> Result<Option<WorkflowDefinitionVersionRecord>, sqlx::Error> {
    let row = sqlx::query(GET_DEFINITION_VERSION_SQL)
        .bind(id)
        .bind(org_id)
        .bind(version)
        .fetch_optional(pool)
        .await?;
    row.map(|row| {
        use sqlx::Row;
        let definition_value: serde_json::Value = row.try_get("definition")?;
        let definition = serde_json::from_value(definition_value).map_err(|error| {
            sqlx::Error::Protocol(format!(
                "workflow definition {id} version {version} failed to decode: {error}"
            ))
        })?;
        Ok(WorkflowDefinitionVersionRecord {
            definition,
            version: row.try_get("version")?,
            content_hash: row.try_get("content_hash")?,
            created_at: row.try_get("created_at")?,
        })
    })
    .transpose()
}

/// Strictly list up to `limit` immutable versions for one org-owned workflow.
pub(crate) async fn list_definition_versions(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    limit: i64,
) -> Result<Vec<WorkflowDefinitionVersionMeta>, sqlx::Error> {
    let rows = sqlx::query(LIST_DEFINITION_VERSIONS_SQL)
        .bind(id)
        .bind(org_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|row| {
            use sqlx::Row;
            Ok(WorkflowDefinitionVersionMeta {
                version: row.try_get("version")?,
                content_hash: row.try_get("content_hash")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
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

/// Look up a stable invocation mapping.  This is intentionally strict rather
/// than best-effort: callers must never fail open and run a second workflow if
/// idempotency storage is temporarily unavailable.
pub(crate) async fn get_workflow_run_idempotency(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
    invocation_key_hash: &[u8; 32],
) -> Result<Option<WorkflowRunIdempotencyBinding>, sqlx::Error> {
    let row = sqlx::query(GET_WORKFLOW_RUN_IDEMPOTENCY_SQL)
        .bind(org_id) // $1 org_id UUID
        .bind(workflow_id) // $2 workflow_id UUID
        .bind(invocation_key_hash.as_slice()) // $3 opaque SHA-256 digest BYTEA
        .fetch_optional(pool)
        .await?;
    row.map(|row| idempotency_binding_from_row(&row))
        .transpose()
}

/// Atomically create the run mapping and initial `running` row, or return the
/// previous mapping.  A transaction is essential here: a timeout after the
/// gateway accepts a run must leave a durable mapping that a later retry can
/// reconcile, while any failed initial insert rolls the mapping back rather
/// than stranding an ambiguous invocation key.
pub(crate) async fn create_or_reuse_idempotent_run(
    pool: &PgPool,
    mapping: &NewWorkflowRunIdempotency,
    rec: &WorkflowRunRecord,
) -> Result<CreateOrReuseWorkflowRun, sqlx::Error> {
    if mapping.org_id != rec.org_id
        || mapping.workflow_id != rec.workflow_id
        || mapping.workflow_version != rec.version
    {
        return Err(sqlx::Error::Protocol(
            "workflow idempotency mapping does not match its initial run".into(),
        ));
    }

    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(INSERT_WORKFLOW_RUN_IDEMPOTENCY_SQL)
        .bind(mapping.org_id) // $1 org_id UUID
        .bind(mapping.workflow_id) // $2 workflow_id UUID
        .bind(mapping.invocation_key_hash.as_slice()) // $3 digest BYTEA
        .bind(mapping.workflow_version) // $4 immutable definition version INT
        .bind(mapping.input_hash.as_slice()) // $5 canonical input digest BYTEA
        .bind(mapping.request_options_hash.as_slice()) // $6 execution options digest BYTEA
        .bind(rec.id) // $7 run_id UUID
        .fetch_optional(&mut *tx)
        .await?;

    if inserted.is_some() {
        // The mapping FK is DEFERRABLE INITIALLY DEFERRED (migration 0040), so
        // its run reference may be inserted immediately before this row within
        // the same transaction.  Any run insert/commit failure rolls the mapping
        // back; this path never leaves a key claimed without a run.
        sqlx::query(INSERT_IDEMPOTENT_RUN_SQL)
            .bind(rec.id) // $1 id UUID
            .bind(rec.workflow_id) // $2 workflow_id UUID
            .bind(rec.version) // $3 version INT
            .bind(rec.org_id) // $4 org_id UUID
            .bind(&rec.status) // $5 status TEXT
            .bind(&rec.inputs) // $6 inputs JSONB
            .bind(rec.cost_usd) // $7 cost_usd NUMERIC
            .bind(rec.max_cost_usd) // $8 max_cost_usd NUMERIC
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(CreateOrReuseWorkflowRun::Created);
    }

    // `ON CONFLICT DO NOTHING` waits for a concurrent claimant.  A separate
    // statement under Read Committed is required after that wait so this
    // transaction sees the winning committed row rather than its pre-insert
    // statement snapshot.
    let row = sqlx::query(GET_WORKFLOW_RUN_IDEMPOTENCY_SQL)
        .bind(mapping.org_id)
        .bind(mapping.workflow_id)
        .bind(mapping.invocation_key_hash.as_slice())
        .fetch_one(&mut *tx)
        .await?;
    let existing = idempotency_binding_from_row(&row)?;
    tx.commit().await?;
    Ok(CreateOrReuseWorkflowRun::Existing(existing))
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
pub(crate) async fn upsert_quality_verdict(pool: &PgPool, id: Uuid, org_id: Uuid, verdict: &str) {
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
    match get_run_strict(pool, id, org_id).await {
        Ok(run) => run,
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

/// Strict counterpart to [`get_run`].  Idempotency reconciliation uses this
/// variant so a temporary read failure cannot be mistaken for a missing mapping
/// and cause a second paid execution.
pub(crate) async fn get_run_strict(
    pool: &PgPool,
    id: Uuid,
    org_id: Uuid,
) -> Result<Option<WorkflowRunRecord>, sqlx::Error> {
    let result = sqlx::query(GET_RUN_SQL)
        .bind(id) // $1 id     UUID
        .bind(org_id) // $2 org_id UUID
        .fetch_optional(pool)
        .await?;
    result.map(|row| run_record_from_row(&row)).transpose()
}

/// Return up to `limit` runs for one org-owned workflow, newest first.
/// Returns an empty `Vec` on DB error (best-effort).
///
/// Wired in W1c dashboard (`GET /v1/workflows/:id/runs`).
#[allow(dead_code)]
pub(crate) async fn list_workflow_runs(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
    limit: i64,
) -> Vec<WorkflowRunRecord> {
    let result = sqlx::query(LIST_WORKFLOW_RUNS_SQL)
        .bind(org_id) // $1 org_id UUID
        .bind(workflow_id) // $2 workflow_id UUID
        .bind(limit) // $3 limit BIGINT
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
// Unit tests — pure (no DB required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use crate::workflow::types::{BudgetPolicy, WorkflowDefinition};

    use super::{
        create_or_reuse_idempotent_run, get_definition_version_record, get_run_strict,
        get_workflow_run_idempotency, insert_definition, list_definition_versions,
        workflow_run_input_hash, workflow_run_invocation_key_hash,
        workflow_run_request_options_hash, CreateOrReuseWorkflowRun, NewWorkflowRunIdempotency,
        WorkflowRunRecord, FINISH_RUN_SQL, GET_DEFINITION_SQL, GET_DEFINITION_VERSION_SQL,
        GET_RUN_SQL, GET_WORKFLOW_RUN_IDEMPOTENCY_SQL, INSERT_DEFINITION_SQL,
        INSERT_IDEMPOTENT_RUN_SQL, INSERT_RUN_SQL, INSERT_WORKFLOW_RUN_IDEMPOTENCY_SQL,
        LIST_DEFINITIONS_SQL, LIST_DEFINITION_VERSIONS_SQL, LIST_WORKFLOW_RUNS_SQL,
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
            INSERT_DEFINITION_SQL.contains("$5::INT IS NULL"),
            "INSERT_DEFINITION_SQL must retain backward-compatible unconditional writes"
        );
        assert!(
            INSERT_DEFINITION_SQL.contains("COALESCE(latest_version, 0) = $5"),
            "INSERT_DEFINITION_SQL must atomically enforce the optional latest-version precondition"
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
    fn get_definition_version_sql_scopes_the_immutable_version() {
        assert!(
            GET_DEFINITION_VERSION_SQL.contains("WHERE id = $1 AND org_id = $2 AND version = $3"),
            "GET_DEFINITION_VERSION_SQL must scope by org, workflow, and exact version"
        );
        assert!(
            GET_DEFINITION_VERSION_SQL.contains("FROM workflow_definitions"),
            "GET_DEFINITION_VERSION_SQL must query workflow_definitions"
        );
        for column in ["content_hash", "created_at"] {
            assert!(
                GET_DEFINITION_VERSION_SQL.contains(column),
                "GET_DEFINITION_VERSION_SQL must return {column}"
            );
        }
    }

    #[test]
    fn list_definition_versions_sql_is_org_scoped_bounded_and_newest_first() {
        assert!(LIST_DEFINITION_VERSIONS_SQL.contains("WHERE id = $1 AND org_id = $2"));
        assert!(LIST_DEFINITION_VERSIONS_SQL.contains("ORDER BY version DESC"));
        assert!(LIST_DEFINITION_VERSIONS_SQL.contains("LIMIT $3"));
        assert!(LIST_DEFINITION_VERSIONS_SQL.contains("content_hash"));
        assert!(LIST_DEFINITION_VERSIONS_SQL.contains("created_at"));
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
    fn list_workflow_runs_sql_contains_expected_clauses() {
        assert!(
            LIST_WORKFLOW_RUNS_SQL.contains("WHERE org_id = $1 AND workflow_id = $2"),
            "LIST_WORKFLOW_RUNS_SQL must filter by org and workflow"
        );
        assert!(
            LIST_WORKFLOW_RUNS_SQL.contains("ORDER BY started_at DESC"),
            "LIST_WORKFLOW_RUNS_SQL must order by started_at DESC"
        );
        assert!(
            LIST_WORKFLOW_RUNS_SQL.contains("LIMIT $3"),
            "LIST_WORKFLOW_RUNS_SQL must apply a LIMIT parameter"
        );
        assert!(
            LIST_WORKFLOW_RUNS_SQL.contains("FROM workflow_runs"),
            "LIST_WORKFLOW_RUNS_SQL must query the workflow_runs table"
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
            LIST_WORKFLOW_RUNS_SQL.contains("cost_usd::float8"),
            "LIST_WORKFLOW_RUNS_SQL must cast cost_usd to float8"
        );
        assert!(
            LIST_WORKFLOW_RUNS_SQL.contains("max_cost_usd::float8"),
            "LIST_WORKFLOW_RUNS_SQL must cast max_cost_usd to float8"
        );
        assert!(
            LIST_WORKFLOW_RUNS_SQL.contains("baseline_cost_usd::float8"),
            "LIST_WORKFLOW_RUNS_SQL must cast baseline_cost_usd to float8"
        );
        assert!(
            LIST_WORKFLOW_RUNS_SQL.contains("saved_usd::float8"),
            "LIST_WORKFLOW_RUNS_SQL must cast saved_usd to float8"
        );
    }

    // ------------------------------------------------------------------
    // Durable run-idempotency contract
    // ------------------------------------------------------------------

    #[test]
    fn invocation_key_hash_is_scoped_deterministic_and_opaque() {
        let org = Uuid::from_u128(1);
        let workflow = Uuid::from_u128(2);
        let first = workflow_run_invocation_key_hash(org, workflow, "delivery-7");
        assert_eq!(
            first,
            workflow_run_invocation_key_hash(org, workflow, "delivery-7")
        );
        assert_ne!(
            first,
            workflow_run_invocation_key_hash(org, Uuid::from_u128(3), "delivery-7")
        );
        assert_ne!(
            first,
            workflow_run_invocation_key_hash(Uuid::from_u128(4), workflow, "delivery-7")
        );
        assert_ne!(
            first,
            workflow_run_invocation_key_hash(org, workflow, "delivery-8")
        );
        assert_ne!(first.as_slice(), b"delivery-7");
    }

    #[test]
    fn canonical_input_hash_ignores_object_wire_order_but_binds_value() {
        let first = workflow_run_input_hash(&serde_json::json!({
            "event": {"kind": "invoice", "id": 7},
            "items": ["a", "b"],
        }))
        .expect("canonical input serializes");
        let same_value_different_wire_order = workflow_run_input_hash(&serde_json::json!({
            "items": ["a", "b"],
            "event": {"id": 7, "kind": "invoice"},
        }))
        .expect("canonical input serializes");
        let changed_value = workflow_run_input_hash(&serde_json::json!({
            "event": {"kind": "invoice", "id": 8},
            "items": ["a", "b"],
        }))
        .expect("canonical input serializes");

        assert_eq!(first, same_value_different_wire_order);
        assert_ne!(first, changed_value);
    }

    #[test]
    fn request_options_hash_binds_budget_but_not_stream_transport() {
        let no_cap = workflow_run_request_options_hash(None).expect("options serialize");
        let capped = workflow_run_request_options_hash(Some(0.05)).expect("options serialize");
        assert_ne!(
            no_cap, capped,
            "a changed execution cap must not reuse a run"
        );
        assert_eq!(
            capped,
            workflow_run_request_options_hash(Some(0.05)).expect("options serialize"),
            "the same logical options must replay deterministically"
        );
    }

    #[test]
    fn idempotency_sql_uses_opaque_org_workflow_scoped_mapping() {
        assert!(
            INSERT_WORKFLOW_RUN_IDEMPOTENCY_SQL.contains("INSERT INTO workflow_run_idempotency"),
            "mapping insert must target the gateway-owned mapping table"
        );
        assert!(
            INSERT_WORKFLOW_RUN_IDEMPOTENCY_SQL
                .contains("ON CONFLICT (org_id, workflow_id, invocation_key_hash) DO NOTHING"),
            "one stable key must create-or-reuse per org + workflow"
        );
        assert!(
            INSERT_WORKFLOW_RUN_IDEMPOTENCY_SQL.contains("RETURNING run_id"),
            "the mapping insert must identify whether this caller owns run creation"
        );
        assert!(
            GET_WORKFLOW_RUN_IDEMPOTENCY_SQL
                .contains("WHERE org_id = $1 AND workflow_id = $2 AND invocation_key_hash = $3"),
            "mapping lookup must remain tenant/workflow scoped"
        );
        assert!(
            INSERT_IDEMPOTENT_RUN_SQL.contains("INSERT INTO workflow_runs"),
            "a claimed mapping must create the initial workflow run"
        );
        assert!(
            !INSERT_IDEMPOTENT_RUN_SQL.contains("ON CONFLICT"),
            "the mapping/run transaction must fail atomically rather than hide a run insert failure"
        );
    }

    #[test]
    fn idempotency_migration_keeps_version_input_and_run_foreign_keys() {
        let migration = include_str!("../../migrations/0040_workflow_run_idempotency.up.sql");
        for fragment in [
            "CREATE TABLE IF NOT EXISTS workflow_run_idempotency",
            "invocation_key_hash  BYTEA NOT NULL CHECK (octet_length(invocation_key_hash) = 32)",
            "input_hash           BYTEA NOT NULL CHECK (octet_length(input_hash) = 32)",
            "request_options_hash BYTEA NOT NULL CHECK (octet_length(request_options_hash) = 32)",
            "PRIMARY KEY (org_id, workflow_id, invocation_key_hash)",
            "FOREIGN KEY (org_id, workflow_id, workflow_version)",
            "REFERENCES workflow_definitions (org_id, id, version)",
            "FOREIGN KEY (run_id)",
            "REFERENCES workflow_runs (id)",
            "ON DELETE CASCADE",
            "DEFERRABLE INITIALLY DEFERRED",
        ] {
            assert!(
                migration.contains(fragment),
                "missing migration contract: {fragment}"
            );
        }
        assert!(
            !migration.contains("idempotency_key TEXT"),
            "the raw caller key must never be persisted"
        );
    }

    /// DB-gated regression for the timeout/retry boundary: two callers claiming
    /// the same stable key must create exactly one initial run, while the loser
    /// receives the winner's durable mapping. This is intentionally ignored in
    /// the normal unit suite because it needs a clean Postgres schema.
    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
    async fn idempotent_run_claim_creates_or_reuses_one_durable_run() {
        let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
        crate::migrate_only(&url)
            .await
            .expect("migrate public schema");
        let pool = crate::connect(&url, 4).await.expect("connect");

        let org = Uuid::new_v4();
        let workflow = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workflow_definitions (id, org_id, version, definition, content_hash) \
             VALUES ($1, $2, 1, '{}'::jsonb, 'idempotency-db-test')",
        )
        .bind(workflow)
        .bind(org)
        .execute(&pool)
        .await
        .expect("seed immutable workflow version");

        let key_hash = workflow_run_invocation_key_hash(org, workflow, "delivery-42");
        let input_hash =
            workflow_run_input_hash(&serde_json::json!({ "delivery": 42 })).expect("hash input");
        let request_options_hash =
            workflow_run_request_options_hash(Some(0.25)).expect("hash options");
        let first_run = WorkflowRunRecord {
            id: Uuid::new_v4(),
            workflow_id: workflow,
            version: 1,
            org_id: org,
            status: "running".to_owned(),
            inputs: Some(serde_json::json!({ "delivery": 42 })),
            cost_usd: 0.0,
            max_cost_usd: Some(0.25),
            baseline_cost_usd: 0.0,
            saved_usd: 0.0,
            error: None,
            started_at: Utc::now(),
            finished_at: None,
        };
        let second_run = WorkflowRunRecord {
            id: Uuid::new_v4(),
            ..first_run.clone()
        };
        let first_mapping = NewWorkflowRunIdempotency {
            org_id: org,
            workflow_id: workflow,
            invocation_key_hash: key_hash,
            workflow_version: 1,
            input_hash,
            request_options_hash,
        };
        let second_mapping = first_mapping.clone();

        let (first, second) = tokio::join!(
            create_or_reuse_idempotent_run(&pool, &first_mapping, &first_run),
            create_or_reuse_idempotent_run(&pool, &second_mapping, &second_run),
        );
        let first = first.expect("first claim");
        let second = second.expect("second claim");
        assert_eq!(
            matches!(first, CreateOrReuseWorkflowRun::Created) as usize
                + matches!(second, CreateOrReuseWorkflowRun::Created) as usize,
            1,
            "only one concurrent claimant may create a workflow run"
        );

        let mapping = get_workflow_run_idempotency(&pool, org, workflow, &key_hash)
            .await
            .expect("mapping read")
            .expect("one durable mapping");
        assert!(
            mapping.run_id == first_run.id || mapping.run_id == second_run.id,
            "mapping must point to one claimant's run"
        );
        let mapped_run = get_run_strict(&pool, mapping.run_id, org)
            .await
            .expect("mapped run read")
            .expect("mapping and run commit atomically");
        assert_eq!(mapped_run.workflow_id, workflow);
        assert_eq!(mapped_run.version, 1);

        sqlx::query("DELETE FROM workflow_runs WHERE id = ANY($1)")
            .bind(vec![first_run.id, second_run.id])
            .execute(&pool)
            .await
            .expect("cascade mapping cleanup");
        sqlx::query(
            "DELETE FROM workflow_definitions WHERE org_id = $1 AND id = $2 AND version = 1",
        )
        .bind(org)
        .bind(workflow)
        .execute(&pool)
        .await
        .expect("cleanup definition");
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
    async fn definition_version_history_is_exact_bounded_and_org_scoped() {
        let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
        crate::migrate_only(&url)
            .await
            .expect("migrate public schema");
        let pool = crate::connect(&url, 4).await.expect("connect");
        let workflow_id = Uuid::new_v4();
        let org_a = Uuid::new_v4();
        let org_b = Uuid::new_v4();
        let definition = |name: &str| WorkflowDefinition {
            id: workflow_id,
            version: 0,
            name: name.to_owned(),
            nodes: vec![],
            edges: vec![],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
            triggers: vec![],
        };

        for (org, version, name, hash) in [
            (org_a, 1, "org-a-v1", "hash-a1"),
            (org_a, 2, "org-a-v2", "hash-a2"),
            (org_b, 1, "org-b-v1", "hash-b1"),
        ] {
            sqlx::query(
                "INSERT INTO workflow_definitions \
                 (id, org_id, version, definition, content_hash) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(workflow_id)
            .bind(org)
            .bind(version)
            .bind(serde_json::to_value(definition(name)).expect("serialize definition"))
            .bind(hash)
            .execute(&pool)
            .await
            .expect("seed workflow version");
        }

        let org_a_versions = list_definition_versions(&pool, org_a, workflow_id, 101)
            .await
            .expect("list org A versions");
        assert_eq!(
            org_a_versions
                .iter()
                .map(|row| (row.version, row.content_hash.as_str()))
                .collect::<Vec<_>>(),
            vec![(2, "hash-a2"), (1, "hash-a1")]
        );

        let exact = get_definition_version_record(&pool, org_a, workflow_id, 1)
            .await
            .expect("read exact org A version")
            .expect("org A version exists");
        assert_eq!(exact.version, 1);
        assert_eq!(exact.content_hash, "hash-a1");
        assert_eq!(exact.definition.name, "org-a-v1");
        let exact_to = get_definition_version_record(&pool, org_a, workflow_id, 2)
            .await
            .expect("read exact org A comparison target")
            .expect("org A comparison target exists");
        let diff = crate::routes::workflow_versions::diff_version_records(&exact, &exact_to)
            .expect("compare exact org A versions");
        assert_eq!(diff.data.len(), 1);
        assert_eq!(diff.data[0].path, "/name");
        assert!(!diff.truncated);
        assert!(get_definition_version_record(&pool, org_a, workflow_id, 3)
            .await
            .expect("read absent version")
            .is_none());

        let other_org = get_definition_version_record(&pool, org_b, workflow_id, 1)
            .await
            .expect("read exact org B version")
            .expect("org B version exists");
        assert_eq!(other_org.definition.name, "org-b-v1");
        assert_eq!(other_org.content_hash, "hash-b1");

        // The same clean-schema regression also proves the optimistic write
        // boundary used by Dashboard edits: one matching writer advances the
        // immutable sequence, while a stale or concurrent peer appends no row.
        let guarded_workflow_id = Uuid::new_v4();
        let guarded_definition = |name: &str| WorkflowDefinition {
            id: guarded_workflow_id,
            version: 0,
            name: name.to_owned(),
            nodes: vec![],
            edges: vec![],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
            triggers: vec![],
        };
        let first = insert_definition(
            &pool,
            org_a,
            &guarded_definition("guarded-v1"),
            "guarded-hash-v1",
            Some(0),
        )
        .await
        .expect("create with no-existing-version precondition");
        assert_eq!(first, Some(1));

        let left_definition = guarded_definition("guarded-v2-left");
        let right_definition = guarded_definition("guarded-v2-right");
        let (left, right) = tokio::join!(
            insert_definition(
                &pool,
                org_a,
                &left_definition,
                "guarded-hash-v2-left",
                Some(1),
            ),
            insert_definition(
                &pool,
                org_a,
                &right_definition,
                "guarded-hash-v2-right",
                Some(1),
            ),
        );
        let left = left.expect("left conditional writer");
        let right = right.expect("right conditional writer");
        assert_eq!(
            usize::from(left == Some(2)) + usize::from(right == Some(2)),
            1,
            "exactly one concurrent writer may advance expected version 1"
        );
        assert_eq!(
            insert_definition(
                &pool,
                org_a,
                &guarded_definition("stale-write"),
                "guarded-hash-stale",
                Some(1),
            )
            .await
            .expect("stale conditional writer"),
            None,
            "a stale expected version must append no immutable row"
        );
        let guarded_versions = list_definition_versions(&pool, org_a, guarded_workflow_id, 10)
            .await
            .expect("list conditionally written versions");
        assert_eq!(
            guarded_versions
                .iter()
                .map(|row| row.version)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );

        // Release-state transitions use the same retained records but keep a
        // separate append-only environment ledger. Both the source and target
        // revisions are compare-and-swap boundaries.
        use crate::workflow::release_store::{self, WorkflowEnvironment, WorkflowReleaseAction};
        let development_v1 =
            release_store::publish_development(&pool, org_a, guarded_workflow_id, 1, 0)
                .await
                .expect("publish version 1 to development")
                .expect("initial development release");
        assert_eq!(development_v1.revision, 1);
        assert_eq!(development_v1.workflow_version, 1);
        assert_eq!(development_v1.action, WorkflowReleaseAction::Publish);
        assert!(
            release_store::publish_development(&pool, org_a, guarded_workflow_id, 2, 0,)
                .await
                .expect("stale initial publication")
                .is_none()
        );
        let development_v2 =
            release_store::publish_development(&pool, org_a, guarded_workflow_id, 2, 1)
                .await
                .expect("publish version 2 to development")
                .expect("second development release");
        assert_eq!(development_v2.revision, 2);
        assert_eq!(development_v2.workflow_version, 2);
        assert!(
            release_store::publish_development(&pool, org_a, guarded_workflow_id, 1, 2)
                .await
                .expect("backward development publication")
                .is_none(),
            "an older version must use same-environment release history rollback"
        );

        let staging = release_store::promote_environment(
            &pool,
            org_a,
            guarded_workflow_id,
            WorkflowEnvironment::Development,
            WorkflowEnvironment::Staging,
            0,
            2,
        )
        .await
        .expect("promote development to staging")
        .expect("initial staging release");
        assert_eq!(staging.revision, 1);
        assert_eq!(staging.workflow_version, 2);
        assert_eq!(staging.action, WorkflowReleaseAction::Promote);
        assert_eq!(
            staging.source_environment,
            Some(WorkflowEnvironment::Development)
        );
        assert_eq!(staging.source_revision, Some(2));

        let production = release_store::promote_environment(
            &pool,
            org_a,
            guarded_workflow_id,
            WorkflowEnvironment::Staging,
            WorkflowEnvironment::Production,
            0,
            1,
        )
        .await
        .expect("promote staging to production")
        .expect("initial production release");
        assert_eq!(production.workflow_version, 2);
        assert_eq!(production.source_revision, Some(1));

        let current = release_store::list_current_releases(&pool, org_a, guarded_workflow_id)
            .await
            .expect("list environment releases");
        assert_eq!(current.len(), 3);
        assert!(current.iter().all(|release| release.workflow_version == 2));
        assert!(
            release_store::list_current_releases(&pool, org_b, guarded_workflow_id,)
                .await
                .expect("list other-org releases")
                .is_empty()
        );

        let rollback = release_store::rollback_environment(
            &pool,
            org_a,
            guarded_workflow_id,
            WorkflowEnvironment::Development,
            1,
            2,
        )
        .await
        .expect("roll back development")
        .expect("development rollback release");
        assert_eq!(rollback.revision, 3);
        assert_eq!(rollback.workflow_version, 1);
        assert_eq!(rollback.action, WorkflowReleaseAction::Rollback);
        assert_eq!(
            rollback.source_environment,
            Some(WorkflowEnvironment::Development)
        );
        assert_eq!(rollback.source_revision, Some(1));
        let development_history = release_store::list_release_history(
            &pool,
            org_a,
            guarded_workflow_id,
            WorkflowEnvironment::Development,
            101,
        )
        .await
        .expect("list development release history");
        assert_eq!(
            development_history
                .iter()
                .map(|release| (
                    release.revision,
                    release.workflow_version,
                    release.action,
                    release.source_revision,
                ))
                .collect::<Vec<_>>(),
            vec![
                (3, 1, WorkflowReleaseAction::Rollback, Some(1)),
                (2, 2, WorkflowReleaseAction::Publish, None),
                (1, 1, WorkflowReleaseAction::Publish, None),
            ]
        );
        assert!(release_store::rollback_environment(
            &pool,
            org_a,
            guarded_workflow_id,
            WorkflowEnvironment::Development,
            2,
            2,
        )
        .await
        .expect("stale rollback")
        .is_none());

        for org in [org_a, org_b] {
            sqlx::query("DELETE FROM workflow_definitions WHERE org_id = $1 AND id = $2")
                .bind(org)
                .bind(workflow_id)
                .execute(&pool)
                .await
                .expect("cleanup workflow versions");
        }
        sqlx::query("DELETE FROM workflow_definitions WHERE org_id = $1 AND id = $2")
            .bind(org_a)
            .bind(guarded_workflow_id)
            .execute(&pool)
            .await
            .expect("cleanup conditionally written versions");
    }
}
