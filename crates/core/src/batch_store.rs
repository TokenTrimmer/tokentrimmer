//! Durable store for the async Batch Lane (`batch_jobs`, migration 0024).
//!
//! Slice 2 persists one row per submitted batch and reads/updates it strictly
//! org-scoped: every method takes `org_id` and filters on it, so an org can
//! never see or mutate another org's batch — a cross-org `GET`/`cancel` finds no
//! row (returns `None`) and the handler 404s.
//!
//! Two impls:
//!   * [`InMemoryBatchStore`] — process-local, for the route tests + dev mode.
//!   * [`PostgresBatchStore`] — the `batch_jobs`-backed production store
//!     (DB-gated tests under `crates/core/tests/batch_jobs_pg.rs`).
//!
//! `realized_saved_usd` / `batch_baseline_usd` are intentionally NOT writable
//! here in slice 2 — the slice-3 poll worker fills them once a batch settles, so
//! no fabricated savings figure is persisted before completion.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tt_provider_openai::{Batch, BatchStatus};
use uuid::Uuid;

/// An error from the batch store. `Transient` is reserved for retryable storage
/// blips; today every failure surfaces as a generic storage error.
#[derive(Debug, thiserror::Error)]
pub enum BatchStoreError {
    #[error("batch store error: {0}")]
    Storage(String),
}

/// Map a slice-1 [`BatchStatus`] to its OpenAI wire string for persistence. The
/// stored status is the single source of truth a `GET` returns when the
/// on-demand provider refresh is skipped or fails.
pub fn batch_status_str(status: &BatchStatus) -> &'static str {
    match status {
        BatchStatus::Validating => "validating",
        BatchStatus::Failed => "failed",
        BatchStatus::InProgress => "in_progress",
        BatchStatus::Finalizing => "finalizing",
        BatchStatus::Completed => "completed",
        BatchStatus::Expired => "expired",
        BatchStatus::Cancelling => "cancelling",
        BatchStatus::Cancelled => "cancelled",
        // Forward-compatible fallback: the slice-1 client preserves unknown wire
        // statuses as `Unknown`; we persist a stable sentinel rather than guess.
        BatchStatus::Unknown => "unknown",
    }
}

/// A persisted `batch_jobs` row, org-scoped. `id` is the OpenAI-compatible batch
/// id the gateway returns (== the provider's id for the OpenAI-only slice 2).
#[derive(Debug, Clone, PartialEq)]
pub struct BatchJob {
    pub id: String,
    pub org_id: Uuid,
    pub api_key_id: Option<Uuid>,
    pub provider: String,
    pub provider_batch_id: Option<String>,
    pub input_file_id: Option<String>,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub status: String,
    pub endpoint: Option<String>,
    pub completion_window: Option<String>,
    pub request_total: i32,
    pub request_completed: i32,
    pub request_failed: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl BatchJob {
    /// Build a row from a freshly-created [`Batch`] for `org`/`provider`. Status
    /// and counts come straight off the provider response; the saved/baseline
    /// columns stay unset (slice-3 fills them).
    pub fn from_batch(
        org_id: Uuid,
        api_key_id: Option<Uuid>,
        provider: &str,
        batch: &Batch,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: batch.id.clone(),
            org_id,
            api_key_id,
            provider: provider.to_string(),
            provider_batch_id: Some(batch.id.clone()),
            input_file_id: batch.input_file_id.clone(),
            output_file_id: batch.output_file_id.clone(),
            error_file_id: batch.error_file_id.clone(),
            status: batch_status_str(&batch.status).to_string(),
            endpoint: batch.endpoint.clone(),
            completion_window: batch.completion_window.clone(),
            request_total: i32::try_from(batch.request_counts.total).unwrap_or(i32::MAX),
            request_completed: i32::try_from(batch.request_counts.completed).unwrap_or(i32::MAX),
            request_failed: i32::try_from(batch.request_counts.failed).unwrap_or(i32::MAX),
            created_at: now,
            updated_at: now,
        }
    }

    /// Render this row as an OpenAI-compatible `Batch` JSON object — the exact
    /// shape `POST`/`GET`/`cancel` return. Numeric/optional fields mirror the
    /// provider wire format; absent ids are omitted (never serialized as null
    /// strings).
    pub fn to_openai_json(&self) -> serde_json::Value {
        let mut obj = serde_json::json!({
            "id": self.id,
            "object": "batch",
            "status": self.status,
            "request_counts": {
                "total": self.request_total,
                "completed": self.request_completed,
                "failed": self.request_failed,
            },
            "created_at": self.created_at.timestamp(),
        });
        let map = obj.as_object_mut().expect("json object");
        if let Some(v) = &self.endpoint {
            map.insert("endpoint".into(), serde_json::json!(v));
        }
        if let Some(v) = &self.input_file_id {
            map.insert("input_file_id".into(), serde_json::json!(v));
        }
        if let Some(v) = &self.output_file_id {
            map.insert("output_file_id".into(), serde_json::json!(v));
        }
        if let Some(v) = &self.error_file_id {
            map.insert("error_file_id".into(), serde_json::json!(v));
        }
        if let Some(v) = &self.completion_window {
            map.insert("completion_window".into(), serde_json::json!(v));
        }
        obj
    }

    /// Apply a fresh provider [`Batch`] onto this row (on-demand `GET` refresh /
    /// post-`cancel` update): adopts status, output/error file ids, and counts,
    /// and bumps `updated_at`. Org-scoping is the caller's responsibility (the
    /// row was already fetched for this org).
    pub fn apply_provider_update(&mut self, batch: &Batch) {
        self.status = batch_status_str(&batch.status).to_string();
        if batch.output_file_id.is_some() {
            self.output_file_id = batch.output_file_id.clone();
        }
        if batch.error_file_id.is_some() {
            self.error_file_id = batch.error_file_id.clone();
        }
        if batch.input_file_id.is_some() {
            self.input_file_id = batch.input_file_id.clone();
        }
        if batch.endpoint.is_some() {
            self.endpoint = batch.endpoint.clone();
        }
        self.request_total = i32::try_from(batch.request_counts.total).unwrap_or(i32::MAX);
        self.request_completed = i32::try_from(batch.request_counts.completed).unwrap_or(i32::MAX);
        self.request_failed = i32::try_from(batch.request_counts.failed).unwrap_or(i32::MAX);
        self.updated_at = Utc::now();
    }
}

/// Persistence contract for `batch_jobs`. Every method is org-scoped.
#[async_trait]
pub trait BatchStore: Send + Sync {
    /// Insert a freshly-created batch row.
    async fn insert(&self, job: BatchJob) -> Result<(), BatchStoreError>;

    /// Fetch the org's row for `id`, or `None` (unknown id OR another org's id —
    /// both 404 at the handler).
    async fn get(&self, org_id: Uuid, id: &str) -> Result<Option<BatchJob>, BatchStoreError>;

    /// Update an existing org-scoped row's status + file ids + counts +
    /// `updated_at`. No-op (returns `Ok`) when the id isn't the org's.
    async fn update(&self, job: BatchJob) -> Result<(), BatchStoreError>;
}

// ─── InMemory ──────────────────────────────────────────────────────────────

/// Process-local store keyed by `(org_id, id)` so org-scoping is enforced by the
/// key itself: a `get`/`update` for the wrong org simply misses.
#[derive(Clone, Default)]
pub struct InMemoryBatchStore {
    inner: Arc<Mutex<HashMap<(Uuid, String), BatchJob>>>,
}

impl InMemoryBatchStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl BatchStore for InMemoryBatchStore {
    async fn insert(&self, job: BatchJob) -> Result<(), BatchStoreError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|e| BatchStoreError::Storage(e.to_string()))?;
        g.insert((job.org_id, job.id.clone()), job);
        Ok(())
    }

    async fn get(&self, org_id: Uuid, id: &str) -> Result<Option<BatchJob>, BatchStoreError> {
        let g = self
            .inner
            .lock()
            .map_err(|e| BatchStoreError::Storage(e.to_string()))?;
        Ok(g.get(&(org_id, id.to_string())).cloned())
    }

    async fn update(&self, job: BatchJob) -> Result<(), BatchStoreError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|e| BatchStoreError::Storage(e.to_string()))?;
        let key = (job.org_id, job.id.clone());
        // Org-scoped: only overwrite a row that already exists for this org.
        if g.contains_key(&key) {
            g.insert(key, job);
        }
        Ok(())
    }
}

// ─── Postgres ──────────────────────────────────────────────────────────────

/// `batch_jobs`-backed store. Every query carries `org_id` in its `WHERE`, so a
/// cross-org id never matches.
#[derive(Clone)]
pub struct PostgresBatchStore {
    pool: sqlx::PgPool,
}

impl PostgresBatchStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

/// Column tuple read back from `batch_jobs`, in SELECT order.
type BatchRow = (
    String,
    Uuid,
    Option<Uuid>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    i32,
    i32,
    i32,
    DateTime<Utc>,
    DateTime<Utc>,
);

const SELECT_COLS: &str = "id, org_id, api_key_id, provider, provider_batch_id, input_file_id, \
     output_file_id, error_file_id, status, endpoint, completion_window, request_total, \
     request_completed, request_failed, created_at, updated_at";

fn row_to_job(r: BatchRow) -> BatchJob {
    BatchJob {
        id: r.0,
        org_id: r.1,
        api_key_id: r.2,
        provider: r.3,
        provider_batch_id: r.4,
        input_file_id: r.5,
        output_file_id: r.6,
        error_file_id: r.7,
        status: r.8,
        endpoint: r.9,
        completion_window: r.10,
        request_total: r.11,
        request_completed: r.12,
        request_failed: r.13,
        created_at: r.14,
        updated_at: r.15,
    }
}

#[async_trait]
impl BatchStore for PostgresBatchStore {
    async fn insert(&self, job: BatchJob) -> Result<(), BatchStoreError> {
        sqlx::query(
            "INSERT INTO batch_jobs \
             (id, org_id, api_key_id, provider, provider_batch_id, input_file_id, \
              output_file_id, error_file_id, status, endpoint, completion_window, \
              request_total, request_completed, request_failed, created_at, updated_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)",
        )
        .bind(&job.id)
        .bind(job.org_id)
        .bind(job.api_key_id)
        .bind(&job.provider)
        .bind(&job.provider_batch_id)
        .bind(&job.input_file_id)
        .bind(&job.output_file_id)
        .bind(&job.error_file_id)
        .bind(&job.status)
        .bind(&job.endpoint)
        .bind(&job.completion_window)
        .bind(job.request_total)
        .bind(job.request_completed)
        .bind(job.request_failed)
        .bind(job.created_at)
        .bind(job.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| BatchStoreError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn get(&self, org_id: Uuid, id: &str) -> Result<Option<BatchJob>, BatchStoreError> {
        let sql = format!("SELECT {SELECT_COLS} FROM batch_jobs WHERE org_id = $1 AND id = $2");
        let row: Option<BatchRow> = sqlx::query_as(&sql)
            .bind(org_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| BatchStoreError::Storage(e.to_string()))?;
        Ok(row.map(row_to_job))
    }

    async fn update(&self, job: BatchJob) -> Result<(), BatchStoreError> {
        // Org-scoped UPDATE: a row belonging to another org never matches the
        // WHERE, so this is a safe no-op for cross-org ids.
        sqlx::query(
            "UPDATE batch_jobs SET \
               provider_batch_id = $3, input_file_id = $4, output_file_id = $5, \
               error_file_id = $6, status = $7, endpoint = $8, completion_window = $9, \
               request_total = $10, request_completed = $11, request_failed = $12, \
               updated_at = $13 \
             WHERE org_id = $1 AND id = $2",
        )
        .bind(job.org_id)
        .bind(&job.id)
        .bind(&job.provider_batch_id)
        .bind(&job.input_file_id)
        .bind(&job.output_file_id)
        .bind(&job.error_file_id)
        .bind(&job.status)
        .bind(&job.endpoint)
        .bind(&job.completion_window)
        .bind(job.request_total)
        .bind(job.request_completed)
        .bind(job.request_failed)
        .bind(job.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| BatchStoreError::Storage(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_batch() -> Batch {
        serde_json::from_value(serde_json::json!({
            "id": "batch_abc123",
            "object": "batch",
            "endpoint": "/v1/chat/completions",
            "input_file_id": "file-in",
            "completion_window": "24h",
            "status": "validating",
            "request_counts": { "total": 10, "completed": 0, "failed": 0 }
        }))
        .unwrap()
    }

    #[test]
    fn from_batch_copies_provider_fields_and_leaves_savings_unset() {
        let org = Uuid::now_v7();
        let job = BatchJob::from_batch(org, None, "openai", &sample_batch());
        assert_eq!(job.id, "batch_abc123");
        assert_eq!(job.org_id, org);
        assert_eq!(job.provider, "openai");
        assert_eq!(job.provider_batch_id.as_deref(), Some("batch_abc123"));
        assert_eq!(job.status, "validating");
        assert_eq!(job.endpoint.as_deref(), Some("/v1/chat/completions"));
        assert_eq!(job.request_total, 10);
    }

    #[test]
    fn to_openai_json_omits_absent_ids() {
        let job = BatchJob::from_batch(Uuid::now_v7(), None, "openai", &sample_batch());
        let v = job.to_openai_json();
        assert_eq!(v["object"], "batch");
        assert_eq!(v["id"], "batch_abc123");
        assert_eq!(v["status"], "validating");
        // No output yet → the key is absent, not a null.
        assert!(v.get("output_file_id").is_none());
        assert_eq!(v["request_counts"]["total"], 10);
    }

    #[tokio::test]
    async fn in_memory_is_org_scoped() {
        let store = InMemoryBatchStore::new();
        let org_a = Uuid::now_v7();
        let org_b = Uuid::now_v7();
        let job = BatchJob::from_batch(org_a, None, "openai", &sample_batch());
        store.insert(job.clone()).await.unwrap();

        // org_a sees it.
        assert!(store.get(org_a, "batch_abc123").await.unwrap().is_some());
        // org_b does NOT (cross-org isolation).
        assert!(store.get(org_b, "batch_abc123").await.unwrap().is_none());

        // An update under org_b must not touch org_a's row.
        let mut hijack = job.clone();
        hijack.org_id = org_b;
        hijack.status = "cancelled".into();
        store.update(hijack).await.unwrap();
        let still = store.get(org_a, "batch_abc123").await.unwrap().unwrap();
        assert_eq!(
            still.status, "validating",
            "org_b update must not mutate org_a"
        );
    }

    #[tokio::test]
    async fn apply_provider_update_adopts_status_and_files() {
        let mut job = BatchJob::from_batch(Uuid::now_v7(), None, "openai", &sample_batch());
        let completed: Batch = serde_json::from_value(serde_json::json!({
            "id": "batch_abc123",
            "object": "batch",
            "status": "completed",
            "output_file_id": "file-out",
            "error_file_id": "file-err",
            "request_counts": { "total": 10, "completed": 9, "failed": 1 }
        }))
        .unwrap();
        job.apply_provider_update(&completed);
        assert_eq!(job.status, "completed");
        assert_eq!(job.output_file_id.as_deref(), Some("file-out"));
        assert_eq!(job.error_file_id.as_deref(), Some("file-err"));
        assert_eq!(job.request_completed, 9);
        assert_eq!(job.request_failed, 1);
    }
}
