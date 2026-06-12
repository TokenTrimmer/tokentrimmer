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
    /// Provider-side automatic prompt-cache discount (USD) — savings the
    /// provider grants with or without TokenTrimmer. Kept separate so the
    /// TT-attributed saving (`baseline_cost_usd - cost_usd -
    /// provider_cache_saved_usd`) survives invoice reconciliation. `0.0` for
    /// TT cache hits (no provider call) and rows from before migration 0011.
    #[serde(default)]
    pub provider_cache_saved_usd: f64,
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
    /// Canary shadow-mode candidate model (`RouteAction::shadow_model`) that the
    /// gateway ALSO dispatched for this request, discarding its response. `None`
    /// for the overwhelming majority of rows (shadow is opt-in per route) and for
    /// rows written before migration 0012. Kept SEPARATE from `model` (the served
    /// model) so the shadow arm is auditable without inflating served-traffic
    /// stats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_model: Option<String>,
    /// Cost (USD) the discarded shadow dispatch incurred. Recorded in its OWN
    /// column — never folded into `cost_usd` — so reconciliation can attribute
    /// the extra (doubled) spend to the canary experiment rather than to served
    /// traffic. `None` when no shadow fired (and for rows before migration 0012).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_cost_usd: Option<f64>,
    /// Which arm the `RouteAction::traffic_pct` canary split assigned this
    /// request to: `Some("canary")` (routed to the new `target_model`),
    /// `Some("control")` (passed through on the original model), or `None` when
    /// the route declared no `traffic_pct` (unconditional rewrite) / no route
    /// matched (and for rows before migration 0012). Lets dashboards compare the
    /// two arms' cost/quality without re-deriving the sticky hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_split_arm: Option<String>,
    /// Raw provider-reported cache-read input tokens. `None` (-> SQL NULL) when
    /// the provider did not report the field OR no provider call was made (TT
    /// L1/L2 hits, truncated streams with no terminal usage). `Some(0)` means
    /// the provider explicitly reported zero. Rows from before migration 0015
    /// are NULL. The NOT NULL `cached_tokens` above keeps its folded
    /// (absent => 0) semantics for back-compat.
    ///
    /// One deliberate exception (streamed fold-rescue): a terminal usage chunk
    /// with folded `cached_tokens > 0` but the raw field absent (pre-fix
    /// adapter / older TT hop) is recorded as `Some(fold)` — a nonzero fold
    /// proves real provider cache reads and must not regress to NULL. A folded
    /// 0 without the raw field stays `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i32>,
    /// Raw provider-reported cache-write (creation) input tokens. Same NULL
    /// semantics; Anthropic-only in practice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i32>,
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

    /// INSERT SQL for `request_logs`. Exposed as a constant so the column /
    /// placeholder / bind counts can be verified in tests without duplicating
    /// the string.
    pub const INSERT_SQL: &str = r#"INSERT INTO request_logs
                     (id, org_id, api_key_id, ts, provider, model,
                      input_tokens, output_tokens, cached_tokens,
                      cost_usd, baseline_cost_usd, provider_cache_saved_usd,
                      cached, cache_layer,
                      route_id, latency_ms, upstream_latency_ms, status,
                      tag, error_class, trace_id,
                      truncated,
                      shadow_model, shadow_cost_usd, traffic_split_arm,
                      cache_read_input_tokens, cache_creation_input_tokens)
                   VALUES
                     ($1, $2, $3, $4, $5, $6,
                      $7, $8, $9,
                      $10, $11, $12,
                      $13, $14,
                      $15, $16, $17, $18,
                      $19, $20, $21,
                      $22,
                      $23, $24, $25,
                      $26, $27)"#;

    /// Number of `.bind(...)` calls in [`PostgresRequestLogWriter::write`].
    /// Must stay in sync with [`INSERT_SQL`] and the actual bind chain.
    pub const INSERT_BIND_COUNT: usize = 27;

    #[async_trait]
    impl RequestLogWriter for PostgresRequestLogWriter {
        async fn write(&self, row: RequestLogRow) -> Result<(), RequestLogError> {
            sqlx::query(INSERT_SQL)
                .bind(row.id) // $1
                .bind(row.org_id) // $2
                .bind(row.api_key_id) // $3
                .bind(row.ts) // $4
                .bind(&row.provider) // $5
                .bind(&row.model) // $6
                .bind(row.input_tokens) // $7
                .bind(row.output_tokens) // $8
                .bind(row.cached_tokens) // $9
                .bind(row.cost_usd) // $10
                .bind(row.baseline_cost_usd) // $11
                .bind(row.provider_cache_saved_usd) // $12
                .bind(row.cached) // $13
                .bind(row.cache_layer.as_deref()) // $14
                .bind(row.route_id) // $15
                .bind(row.latency_ms) // $16
                .bind(row.upstream_latency_ms) // $17
                .bind(row.status) // $18
                .bind(row.tag.as_deref()) // $19
                .bind(row.error_class.as_deref()) // $20
                .bind(row.trace_id.as_deref()) // $21
                .bind(row.truncated) // $22
                .bind(row.shadow_model.as_deref()) // $23
                .bind(row.shadow_cost_usd) // $24
                .bind(row.traffic_split_arm.as_deref()) // $25
                .bind(row.cache_read_input_tokens) // $26
                .bind(row.cache_creation_input_tokens) // $27
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
            provider_cache_saved_usd: 0.0,
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
            shadow_model: None,
            shadow_cost_usd: None,
            traffic_split_arm: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
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

    /// Persist a row with `truncated = true` and verify it round-trips through
    /// the in-memory writer.
    #[tokio::test]
    async fn in_memory_round_trips_truncated_true() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.truncated = true;
        w.write(row).await.unwrap();
        assert!(
            w.rows()[0].truncated,
            "truncated=true must survive write→read"
        );
    }

    /// A row carrying the canary columns (shadow_model / shadow_cost_usd /
    /// traffic_split_arm) round-trips through the writer with the shadow cost in
    /// its OWN field — never folded into `cost_usd`.
    #[tokio::test]
    async fn in_memory_round_trips_canary_columns() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.01;
        row.shadow_model = Some("claude-haiku-4-5".into());
        row.shadow_cost_usd = Some(0.004);
        row.traffic_split_arm = Some("canary".into());
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert_eq!(got.shadow_model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(got.shadow_cost_usd, Some(0.004));
        assert_eq!(got.traffic_split_arm.as_deref(), Some("canary"));
        // The shadow cost is SEPARATE — the primary cost is untouched.
        assert!((got.cost_usd - 0.01).abs() < 1e-9);
    }

    /// A row carrying the provider prompt-cache token columns round-trips, and
    /// `Some(0)` ("provider explicitly reported zero") stays distinct from
    /// `None` ("provider did not report" / "no provider call").
    #[tokio::test]
    async fn in_memory_round_trips_provider_cache_token_columns() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cache_read_input_tokens = Some(80);
        row.cache_creation_input_tokens = Some(20);
        w.write(row).await.unwrap();

        let mut zero = sample_row();
        zero.cache_read_input_tokens = Some(0);
        zero.cache_creation_input_tokens = None;
        w.write(zero).await.unwrap();

        let rows = w.rows();
        assert_eq!(rows[0].cache_read_input_tokens, Some(80));
        assert_eq!(rows[0].cache_creation_input_tokens, Some(20));
        // NULL-vs-0: a reported zero survives as Some(0), not None.
        assert_eq!(rows[1].cache_read_input_tokens, Some(0));
        assert_eq!(rows[1].cache_creation_input_tokens, None);
    }

    /// Legacy JSON (rows serialized before migration 0015) deserializes with
    /// both provider cache token fields defaulting to `None`, and `None`
    /// fields are omitted on re-serialize.
    #[test]
    fn provider_cache_token_columns_serde_backward_compat() {
        let legacy = r#"{
            "id":"00000000-0000-0000-0000-000000000000",
            "org_id":"00000000-0000-0000-0000-000000000000",
            "api_key_id":"00000000-0000-0000-0000-000000000000",
            "ts":"2026-06-09T00:00:00Z",
            "provider":"openai","model":"gpt-4o",
            "input_tokens":1,"output_tokens":1,"cached_tokens":0,
            "cost_usd":0.0,"baseline_cost_usd":0.0,
            "cached":false,"cache_layer":null,"route_id":null,
            "latency_ms":1,"upstream_latency_ms":null,"status":200,
            "tag":null,"error_class":null,"trace_id":null
        }"#;
        let row: RequestLogRow = serde_json::from_str(legacy).unwrap();
        assert_eq!(row.cache_read_input_tokens, None);
        assert_eq!(row.cache_creation_input_tokens, None);
        let j = serde_json::to_string(&row).unwrap();
        assert!(!j.contains("cache_read_input_tokens"), "{j}");
        assert!(!j.contains("cache_creation_input_tokens"), "{j}");
    }

    /// Legacy JSON (rows serialized before the canary columns existed)
    /// deserializes with all three new fields defaulting to `None`, and a
    /// `None`-valued row omits them on re-serialize (back-compat with old
    /// deploys / persisted rows).
    #[test]
    fn canary_columns_serde_backward_compat() {
        let legacy = r#"{
            "id":"00000000-0000-0000-0000-000000000000",
            "org_id":"00000000-0000-0000-0000-000000000000",
            "api_key_id":"00000000-0000-0000-0000-000000000000",
            "ts":"2026-06-09T00:00:00Z",
            "provider":"openai","model":"gpt-4o",
            "input_tokens":1,"output_tokens":1,"cached_tokens":0,
            "cost_usd":0.0,"baseline_cost_usd":0.0,
            "cached":false,"cache_layer":null,"route_id":null,
            "latency_ms":1,"upstream_latency_ms":null,"status":200,
            "tag":null,"error_class":null,"trace_id":null
        }"#;
        let row: RequestLogRow = serde_json::from_str(legacy).unwrap();
        assert_eq!(row.shadow_model, None);
        assert_eq!(row.shadow_cost_usd, None);
        assert_eq!(row.traffic_split_arm, None);
        let j = serde_json::to_string(&row).unwrap();
        assert!(!j.contains("shadow_model"), "{j}");
        assert!(!j.contains("shadow_cost_usd"), "{j}");
        assert!(!j.contains("traffic_split_arm"), "{j}");
    }

    /// Guard: the INSERT_SQL column list, placeholder list, and the
    /// INSERT_BIND_COUNT constant must all agree.
    ///
    /// Parses INSERT_SQL to count:
    ///   • column names between the first `(` and its matching `)`
    ///   • `$N` placeholders in the VALUES `(...)` clause
    /// and asserts both equal INSERT_BIND_COUNT.
    ///
    /// If a future edit adds a column but forgets a placeholder (or vice versa)
    /// this test will catch it without needing a live Postgres connection.
    #[cfg(feature = "postgres")]
    #[test]
    fn insert_sql_column_placeholder_bind_counts_match() {
        use crate::request_logs::postgres::{INSERT_BIND_COUNT, INSERT_SQL};

        // --- count columns in the INSERT column list -------------------------
        // Find the text between the first '(' and its matching ')'.
        let col_start = INSERT_SQL.find('(').expect("INSERT_SQL must contain '('");
        let col_end = INSERT_SQL[col_start + 1..]
            .find(')')
            .expect("INSERT_SQL must contain closing ')'")
            + col_start
            + 1;
        let col_list = &INSERT_SQL[col_start + 1..col_end];
        let column_count = col_list.split(',').count();

        // --- count $N placeholders in the VALUES clause ----------------------
        // Every placeholder matches "$" followed by one or more digits.
        let placeholder_count = {
            let mut n = 0usize;
            let bytes = INSERT_SQL.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'$' {
                    let rest = &INSERT_SQL[i + 1..];
                    if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                        n += 1;
                    }
                }
                i += 1;
            }
            n
        };

        assert_eq!(
            column_count, INSERT_BIND_COUNT,
            "INSERT_SQL has {column_count} columns but INSERT_BIND_COUNT is {INSERT_BIND_COUNT}"
        );
        assert_eq!(
            placeholder_count, INSERT_BIND_COUNT,
            "INSERT_SQL has {placeholder_count} $N placeholders but INSERT_BIND_COUNT is {INSERT_BIND_COUNT}"
        );
    }
}
