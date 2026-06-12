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
    /// provider_cache_saved_usd - cache_bust_penalty_usd`) survives invoice
    /// reconciliation. `0.0` for TT cache hits (no provider call) and rows
    /// from before migration 0011.
    #[serde(default)]
    pub provider_cache_saved_usd: f64,
    /// Estimated USD penalty of a deliberate, non-deterministic stable-prefix
    /// mutation (a booked `CacheBustEstimate`) — the NEGATIVE savings entry.
    /// Persisted so the row-derived TT headline (see `provider_cache_saved_usd`
    /// formula above) matches the `x-tokentrimmer-saved-usd` header / span on
    /// every request. An estimate of induced future cost: NEVER folded into
    /// `cost_usd` / `baseline_cost_usd`. `0.0` when no bust booked (all
    /// current traffic — redaction is ingress-deterministic and busts
    /// nothing), for TT cache hits, and for rows before migration 0016.
    #[serde(default)]
    pub cache_bust_penalty_usd: f64,
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
    /// `true` when a matched route's advisory **batch-eligibility** marker
    /// (`RouteAction::batch`, research Phase 2.1) survived the gateway's
    /// hard-ineligibility gate (non-streaming, non-interactive, served model
    /// carries a catalog batch tier check at mark time). The gateway is
    /// synchronous today, so an eligible row was STILL dispatched and billed
    /// normally — this marks route intent for the future async Batch Lane.
    /// `false` for unmarked / streaming / interactive traffic, TT cache hits,
    /// and rows from before migration 0017.
    #[serde(default)]
    pub batch_eligible: bool,
    /// FORGONE Batch-API discount (USD) for a batch-eligible request: realized
    /// `cost_usd` minus the served model's catalog batch-rate cost on the full
    /// prompt+completion, floored at 0, fee-applied. A projection of what the
    /// async Batch Lane would have saved — NEVER part of `cost_usd` /
    /// `baseline_cost_usd` / the TT saved-usd headline (those reconcile against
    /// the provider invoice). `/costs` + the digest may
    /// `SUM(batch_forgone_usd) WHERE batch_eligible` to say "$X/mo would be
    /// saved by the Batch Lane". `0.0` when not batch-eligible, when the served
    /// model carries no catalog batch tier (e.g. post-failover — no real rate,
    /// no claim), and for rows from before migration 0017.
    #[serde(default)]
    pub batch_forgone_usd: f64,
    /// `true` when a matched route's rewrite was suppressed by a sticky pause
    /// (research Phase 2.3 auto-pause / `POST /v1/routes/:id/pause`): the
    /// request flowed to the ORIGINALLY-requested model with every cost lever
    /// off. The route still attributes (`route_id` is stamped) so paused
    /// traffic is auditable per route; cost == baseline on these rows (no
    /// fabricated saving). `false` for every unrouted/unpaused request and
    /// rows from before migration 0019 (mirror of `truncated`).
    #[serde(default)]
    pub route_paused: bool,
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
                      cache_bust_penalty_usd,
                      cached, cache_layer,
                      route_id, latency_ms, upstream_latency_ms, status,
                      tag, error_class, trace_id,
                      truncated,
                      shadow_model, shadow_cost_usd, traffic_split_arm,
                      cache_read_input_tokens, cache_creation_input_tokens,
                      batch_eligible, batch_forgone_usd,
                      route_paused)
                   VALUES
                     ($1, $2, $3, $4, $5, $6,
                      $7, $8, $9,
                      $10, $11, $12,
                      $13,
                      $14, $15,
                      $16, $17, $18, $19,
                      $20, $21, $22,
                      $23,
                      $24, $25, $26,
                      $27, $28,
                      $29, $30,
                      $31)"#;

    /// Number of `.bind(...)` calls in [`PostgresRequestLogWriter::write`].
    /// Must stay in sync with [`INSERT_SQL`] and the actual bind chain.
    pub const INSERT_BIND_COUNT: usize = 31;

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
                .bind(row.cache_bust_penalty_usd) // $13
                .bind(row.cached) // $14
                .bind(row.cache_layer.as_deref()) // $15
                .bind(row.route_id) // $16
                .bind(row.latency_ms) // $17
                .bind(row.upstream_latency_ms) // $18
                .bind(row.status) // $19
                .bind(row.tag.as_deref()) // $20
                .bind(row.error_class.as_deref()) // $21
                .bind(row.trace_id.as_deref()) // $22
                .bind(row.truncated) // $23
                .bind(row.shadow_model.as_deref()) // $24
                .bind(row.shadow_cost_usd) // $25
                .bind(row.traffic_split_arm.as_deref()) // $26
                .bind(row.cache_read_input_tokens) // $27
                .bind(row.cache_creation_input_tokens) // $28
                .bind(row.batch_eligible) // $29
                .bind(row.batch_forgone_usd) // $30
                .bind(row.route_paused) // $31
                .bind(row.route_paused) // $29
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
            cache_bust_penalty_usd: 0.0,
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
            batch_eligible: false,
            batch_forgone_usd: 0.0,
            route_paused: false,
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
        // Pre-0016 rows default the bust penalty to 0 (no bust booked).
        assert_eq!(row.cache_bust_penalty_usd, 0.0);
        let j = serde_json::to_string(&row).unwrap();
        assert!(!j.contains("shadow_model"), "{j}");
        assert!(!j.contains("shadow_cost_usd"), "{j}");
        assert!(!j.contains("traffic_split_arm"), "{j}");
    }

    /// Legacy JSON (rows serialized before migration 0019) deserializes with
    /// `route_paused` defaulting to false, and a `route_paused = true` row
    /// round-trips through the in-memory writer (mirror of `truncated`).
    #[tokio::test]
    async fn route_paused_serde_backward_compat() {
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
        assert!(
            !row.route_paused,
            "pre-0019 rows must deserialize route_paused = false"
        );

        let w = InMemoryRequestLogWriter::new();
        let mut paused_row = sample_row();
        paused_row.route_paused = true;
        w.write(paused_row).await.unwrap();
        assert!(
            w.rows()[0].route_paused,
            "route_paused=true must survive write→read"
        );
    }

    /// The cache-bust penalty column (migration 0016) round-trips through the
    /// writer in its OWN field — never folded into `cost_usd` (it is an
    /// estimate of induced future cost, not a realized invoice figure).
    #[tokio::test]
    async fn in_memory_round_trips_cache_bust_penalty() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.01;
        row.cache_bust_penalty_usd = 0.0025;
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert!((got.cache_bust_penalty_usd - 0.0025).abs() < 1e-12);
        assert!((got.cost_usd - 0.01).abs() < 1e-12, "cost untouched");
    }

    /// The batch-eligibility columns (migration 0017) round-trip through the
    /// writer in their OWN fields — the forgone discount is a projection for
    /// the future async Batch Lane and is never folded into `cost_usd`.
    #[tokio::test]
    async fn in_memory_round_trips_batch_columns() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.025;
        row.batch_eligible = true;
        row.batch_forgone_usd = 0.0125;
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert!(got.batch_eligible, "batch_eligible must survive write→read");
        assert!((got.batch_forgone_usd - 0.0125).abs() < 1e-12);
        assert!((got.cost_usd - 0.025).abs() < 1e-12, "cost untouched");
    }

    /// Legacy JSON (rows serialized before migration 0017) deserializes with
    /// `batch_eligible = false` / `batch_forgone_usd = 0.0` — matching the SQL
    /// `NOT NULL DEFAULT` semantics for pre-migration rows.
    #[test]
    fn batch_columns_serde_backward_compat() {
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
        assert!(!row.batch_eligible, "pre-0017 rows default to ineligible");
        assert_eq!(row.batch_forgone_usd, 0.0, "pre-0017 rows forgo nothing");
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
