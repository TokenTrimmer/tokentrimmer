//! Per-request telemetry writer.
//!
//! Backs the `request_logs` table from `crates/core/migrations/0001_request_logs.up.sql`.
//! The gateway spawns a fire-and-forget call to [`RequestLogWriter::write`]
//! after every response so the dashboard's `/api/telemetry` endpoints
//! and the Plan replay engine have a uniform input.
//!
//! Why a trait rather than just sqlx everywhere: tests need to assert
//! "what did the gateway log?" without spinning up Postgres. The
//! [`InMemoryRequestLogWriter`] in this module is what those tests use;
//! production wires [`PostgresRequestLogWriter`] behind the `postgres`
//! feature.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Row to insert into `request_logs`. Field names mirror the SQL column
/// names so the writer impls don't need a translation layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLogRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    pub ts: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    /// Cached input tokens. `0` when nothing was served from cache.
    pub cached_tokens: i32,
    pub cost_usd: f64,
    pub baseline_cost_usd: f64,
    /// `true` when ANY cache layer served the response (L1 or L2).
    pub cached: bool,
    /// `Some("l1")` / `Some("l2")` / `None`. Matches the SQL CHECK constraint.
    pub cache_layer: Option<String>,
    pub route_id: Option<Uuid>,
    pub latency_ms: i32,
    pub upstream_latency_ms: Option<i32>,
    pub status: i32,
    pub tag: Option<String>,
    pub error_class: Option<String>,
    pub trace_id: Option<String>,
    /// `true` when the SSE stream was dropped before a `finish_reason` chunk
    /// arrived (client abort or upstream reset). `false` for non-streaming
    /// responses and for streams that completed cleanly.
    #[serde(default)]
    pub truncated: bool,
}

/// Errors returned by [`RequestLogWriter`].
#[derive(Debug, Error)]
pub enum RequestLogError {
    #[error("storage: {0}")]
    Storage(String),
}

/// Persistence contract for the `request_logs` table.
#[async_trait]
pub trait RequestLogWriter: Send + Sync {
    async fn write(&self, row: RequestLogRow) -> Result<(), RequestLogError>;
}

// ─── InMemory ──────────────────────────────────────────────────────────────

/// Process-local writer for tests. Stores rows in a `Vec` accessible
/// via [`InMemoryRequestLogWriter::rows`].
#[derive(Clone, Default)]
pub struct InMemoryRequestLogWriter {
    inner: Arc<Mutex<Vec<RequestLogRow>>>,
}

impl InMemoryRequestLogWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of all rows captured so far.
    pub fn rows(&self) -> Vec<RequestLogRow> {
        self.inner
            .lock()
            .expect("request log writer mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl RequestLogWriter for InMemoryRequestLogWriter {
    async fn write(&self, row: RequestLogRow) -> Result<(), RequestLogError> {
        self.inner
            .lock()
            .map_err(|e| RequestLogError::Storage(e.to_string()))?
            .push(row);
        Ok(())
    }
}

// ─── Postgres ──────────────────────────────────────────────────────────────

/// Postgres-backed writer, gated by the `postgres` feature.
#[cfg(feature = "postgres")]
pub mod postgres {
    use super::*;
    use sqlx::PgPool;

    /// Production writer. INSERTs into `request_logs`.
    #[derive(Clone)]
    pub struct PostgresRequestLogWriter {
        pool: PgPool,
    }

    impl PostgresRequestLogWriter {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }
    }

    impl std::fmt::Debug for PostgresRequestLogWriter {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PostgresRequestLogWriter")
                .field("pool", &"PgPool { .. }")
                .finish()
        }
    }

    #[async_trait]
    impl RequestLogWriter for PostgresRequestLogWriter {
        async fn write(&self, row: RequestLogRow) -> Result<(), RequestLogError> {
            sqlx::query(
                r#"INSERT INTO request_logs
                     (id, org_id, api_key_id, ts, provider, model,
                      input_tokens, output_tokens, cached_tokens,
                      cost_usd, baseline_cost_usd, cached, cache_layer,
                      route_id, latency_ms, upstream_latency_ms, status,
                      tag, error_class, trace_id)
                   VALUES
                     ($1, $2, $3, $4, $5, $6,
                      $7, $8, $9,
                      $10, $11, $12, $13,
                      $14, $15, $16, $17,
                      $18, $19, $20)"#,
            )
            .bind(row.id)
            .bind(row.org_id)
            .bind(row.api_key_id)
            .bind(row.ts)
            .bind(&row.provider)
            .bind(&row.model)
            .bind(row.input_tokens)
            .bind(row.output_tokens)
            .bind(row.cached_tokens)
            .bind(row.cost_usd)
            .bind(row.baseline_cost_usd)
            .bind(row.cached)
            .bind(row.cache_layer.as_deref())
            .bind(row.route_id)
            .bind(row.latency_ms)
            .bind(row.upstream_latency_ms)
            .bind(row.status)
            .bind(row.tag.as_deref())
            .bind(row.error_class.as_deref())
            .bind(row.trace_id.as_deref())
            .execute(&self.pool)
            .await
            .map_err(|e| RequestLogError::Storage(e.to_string()))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> RequestLogRow {
        RequestLogRow {
            id: Uuid::now_v7(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            ts: Utc::now(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            input_tokens: 100,
            output_tokens: 50,
            cached_tokens: 0,
            cost_usd: 0.0045,
            baseline_cost_usd: 0.0045,
            cached: false,
            cache_layer: None,
            route_id: None,
            latency_ms: 800,
            upstream_latency_ms: Some(750),
            status: 200,
            tag: None,
            error_class: None,
            trace_id: Some("trace-abc".into()),
            truncated: false,
        }
    }

    #[tokio::test]
    async fn in_memory_collects_rows() {
        let w = InMemoryRequestLogWriter::new();
        w.write(sample_row()).await.unwrap();
        w.write(sample_row()).await.unwrap();
        assert_eq!(w.rows().len(), 2);
    }

    #[tokio::test]
    async fn in_memory_round_trips_field_values() {
        let w = InMemoryRequestLogWriter::new();
        let row = sample_row();
        let id = row.id;
        let trace = row.trace_id.clone();
        w.write(row).await.unwrap();
        let captured = &w.rows()[0];
        assert_eq!(captured.id, id);
        assert_eq!(captured.trace_id, trace);
        assert_eq!(captured.provider, "openai");
        assert!((captured.cost_usd - 0.0045).abs() < 1e-9);
    }
}
