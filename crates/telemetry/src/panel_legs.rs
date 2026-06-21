//! Per-leg panel telemetry writer.
//!
//! Backs the `panel_legs` table from `crates/core/migrations/0026_panel_legs.up.sql`.
//! After a panel request, the gateway writes one row per member leg and one row
//! for the arbiter leg, all keyed by the parent `request_logs.id`.
//!
//! Mirrors the structure of [`crate::request_logs`]: trait + in-memory test impl;
//! the Postgres impl is Task 3 (feature-gated, same crate).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use uuid::Uuid;

use crate::request_logs::RequestLogError;

/// Row to insert into `panel_legs`. Field names mirror the SQL column names.
///
/// `request_log_id` ties this row to its parent `request_logs` row (no
/// enforced FK — matches the 0001 convention; see spec §3.1).
#[derive(Debug, Clone)]
pub struct PanelLegRow {
    /// FK (unenforced) → `request_logs.id`.
    pub request_log_id: Uuid,
    /// 0-based index for member legs; arbiter uses a high sentinel (e.g. `i32::MAX`).
    pub leg_index: i32,
    /// `"leg"` or `"arbiter"`.
    pub role: String,
    /// Per-leg provider (e.g. `"anthropic"`, `"openai"`).
    pub provider: String,
    /// Model identifier as sent to the provider.
    pub model: String,
    /// `NULL` when leg was skipped or usage was not reported.
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    /// Per-leg cost in USD. `NULL` = unmetered/unpriced; never coerced to 0.
    pub cost_usd: Option<f64>,
    /// End-to-end dispatch latency for this leg in milliseconds.
    /// `NULL` for skipped legs (no dispatch).
    pub latency_ms: Option<i64>,
    /// `"ok"` | `"error"` | `"timeout"` | `"skipped_no_cred"`.
    pub status: String,
    /// Machine-readable error class when `status == "error"`.
    pub error_class: Option<String>,
}

/// Persistence contract for the `panel_legs` table.
///
/// Accepts a batch so a single fire-and-forget task can write all N+1 legs
/// (member legs + arbiter) in one call.
#[async_trait]
pub trait PanelLegWriter: Send + Sync {
    async fn write_legs(&self, rows: Vec<PanelLegRow>) -> Result<(), RequestLogError>;
}

// ─── InMemory ──────────────────────────────────────────────────────────────

/// Process-local writer for tests. Stores rows in a `Vec` accessible via
/// [`InMemoryPanelLegWriter::rows`].
#[derive(Clone, Default)]
pub struct InMemoryPanelLegWriter {
    inner: Arc<Mutex<Vec<PanelLegRow>>>,
}

impl InMemoryPanelLegWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of all rows captured so far.
    pub fn rows(&self) -> Vec<PanelLegRow> {
        self.inner
            .lock()
            .expect("panel leg writer mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl PanelLegWriter for InMemoryPanelLegWriter {
    async fn write_legs(&self, rows: Vec<PanelLegRow>) -> Result<(), RequestLogError> {
        self.inner
            .lock()
            .map_err(|e| RequestLogError::Storage(e.to_string()))?
            .extend(rows);
        Ok(())
    }
}

// ─── Noop ──────────────────────────────────────────────────────────────────

/// No-op writer that silently drops all rows. Used as the default when no
/// Postgres connection is available (e.g. local dev without a DB).
pub struct NoopPanelLegWriter;

#[async_trait]
impl PanelLegWriter for NoopPanelLegWriter {
    async fn write_legs(&self, _rows: Vec<PanelLegRow>) -> Result<(), RequestLogError> {
        Ok(())
    }
}

// ─── Postgres ──────────────────────────────────────────────────────────────

/// Postgres-backed writer, gated by the `postgres` feature.
#[cfg(feature = "postgres")]
pub mod postgres {
    use super::*;
    use sqlx::PgPool;

    /// Production writer. INSERTs all rows into `panel_legs` in a single
    /// multi-row statement (one round-trip per batch).
    #[derive(Clone)]
    pub struct PostgresPanelLegWriter {
        pool: PgPool,
    }

    impl PostgresPanelLegWriter {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }
    }

    impl std::fmt::Debug for PostgresPanelLegWriter {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PostgresPanelLegWriter")
                .field("pool", &"PgPool { .. }")
                .finish()
        }
    }

    /// Number of columns per row in the `panel_legs` INSERT.
    /// Must stay in sync with the `push_values` closure and the table schema.
    pub const INSERT_COLUMN_COUNT: usize = 12;

    #[async_trait]
    impl PanelLegWriter for PostgresPanelLegWriter {
        async fn write_legs(&self, rows: Vec<PanelLegRow>) -> Result<(), RequestLogError> {
            if rows.is_empty() {
                return Ok(());
            }

            let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
                "INSERT INTO panel_legs \
                 (request_log_id, leg_index, role, provider, model, \
                  input_tokens, output_tokens, cached_tokens, \
                  cost_usd, latency_ms, status, error_class) ",
            );

            qb.push_values(rows, |mut b, r| {
                b.push_bind(r.request_log_id) // request_log_id
                    .push_bind(r.leg_index) // leg_index
                    .push_bind(r.role) // role
                    .push_bind(r.provider) // provider
                    .push_bind(r.model) // model
                    .push_bind(r.input_tokens) // input_tokens
                    .push_bind(r.output_tokens) // output_tokens
                    .push_bind(r.cached_tokens) // cached_tokens
                    .push_bind(r.cost_usd) // cost_usd
                    .push_bind(r.latency_ms) // latency_ms
                    .push_bind(r.status) // status
                    .push_bind(r.error_class); // error_class
            });

            qb.build()
                .execute(&self.pool)
                .await
                .map_err(classify_sqlx_error)?;
            Ok(())
        }
    }

    /// Map a `sqlx::Error` to a [`RequestLogError`], splitting TRANSIENT blips
    /// from TERMINAL failures (mirrors `request_logs::postgres::classify_sqlx_error`).
    fn classify_sqlx_error(e: sqlx::Error) -> RequestLogError {
        let transient = matches!(
            e,
            sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_)
        ) || matches!(&e, sqlx::Error::Tls(_));
        if transient {
            RequestLogError::Transient(e.to_string())
        } else {
            RequestLogError::Storage(e.to_string())
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_leg(request_log_id: Uuid, leg_index: i32, role: &str) -> PanelLegRow {
        PanelLegRow {
            request_log_id,
            leg_index,
            role: role.to_string(),
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5".to_string(),
            input_tokens: Some(100),
            output_tokens: Some(50),
            cached_tokens: None,
            cost_usd: Some(0.0012),
            latency_ms: Some(320),
            status: "ok".to_string(),
            error_class: None,
        }
    }

    /// Write two rows → `rows().len() == 2` and a field round-trips.
    #[tokio::test]
    async fn in_memory_collects_rows_and_round_trips() {
        let w = InMemoryPanelLegWriter::new();
        let parent_id = Uuid::now_v7();
        let row1 = sample_leg(parent_id, 0, "leg");
        let row2 = sample_leg(parent_id, 1, "leg");
        w.write_legs(vec![row1, row2]).await.unwrap();
        let rows = w.rows();
        assert_eq!(rows.len(), 2);
        // Field round-trip: request_log_id and model survive the write→read.
        assert_eq!(rows[0].request_log_id, parent_id);
        assert_eq!(rows[0].model, "claude-haiku-4-5");
        assert_eq!(rows[1].leg_index, 1);
    }

    /// Two separate `write_legs` calls accumulate into the same store.
    #[tokio::test]
    async fn in_memory_accumulates_across_calls() {
        let w = InMemoryPanelLegWriter::new();
        let id = Uuid::now_v7();
        w.write_legs(vec![sample_leg(id, 0, "leg")]).await.unwrap();
        w.write_legs(vec![sample_leg(id, i32::MAX, "arbiter")])
            .await
            .unwrap();
        let rows = w.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].role, "leg");
        assert_eq!(rows[1].role, "arbiter");
    }

    /// Optional (nullable) fields survive a write→read unchanged.
    #[tokio::test]
    async fn in_memory_round_trips_nullable_fields() {
        let w = InMemoryPanelLegWriter::new();
        let id = Uuid::now_v7();
        let mut row = sample_leg(id, 0, "leg");
        row.cost_usd = None;
        row.latency_ms = None;
        row.error_class = Some("timeout".to_string());
        w.write_legs(vec![row]).await.unwrap();
        let got = &w.rows()[0];
        assert_eq!(got.cost_usd, None);
        assert_eq!(got.latency_ms, None);
        assert_eq!(got.error_class.as_deref(), Some("timeout"));
    }

    /// Noop writer returns Ok and does not accumulate rows.
    #[tokio::test]
    async fn noop_writer_drops_rows() {
        let w = NoopPanelLegWriter;
        let id = Uuid::now_v7();
        let result = w.write_legs(vec![sample_leg(id, 0, "leg")]).await;
        assert!(result.is_ok());
    }

    /// `Default` for `InMemoryPanelLegWriter` produces an empty store.
    #[test]
    fn default_is_empty() {
        let w = InMemoryPanelLegWriter::default();
        assert_eq!(w.rows().len(), 0);
    }

    /// Guard: `INSERT_COLUMN_COUNT` must equal the number of `.push_bind(`
    /// calls in `PostgresPanelLegWriter::write_legs`. Scans this file's source
    /// between the `push_values` opening and its closing `});` marker.
    ///
    /// A mismatch causes a runtime panic from the QueryBuilder (wrong number of
    /// values per row). This test catches it without a live Postgres connection.
    #[cfg(feature = "postgres")]
    #[test]
    fn push_values_bind_count_matches_insert_column_count() {
        use crate::panel_legs::postgres::INSERT_COLUMN_COUNT;

        let source = include_str!("panel_legs.rs");
        let start = source
            .find("qb.push_values(rows,")
            .expect("write_legs() must call push_values");
        let rest = &source[start..];
        let end = rest
            .find("});")
            .expect("push_values closure must end with `});`");
        let closure = &rest[..end];
        let bind_calls = closure.matches(".push_bind(").count();
        assert_eq!(
            bind_calls, INSERT_COLUMN_COUNT,
            "push_values closure has {bind_calls} .push_bind( calls but \
             INSERT_COLUMN_COUNT is {INSERT_COLUMN_COUNT} — a mismatch causes \
             a runtime panic for every panel_legs INSERT"
        );
    }
}
