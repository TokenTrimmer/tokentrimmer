//! Durable Postgres persistence for paired A/B judge verdicts (research
//! Phase 0.4) — the `quality_verdicts` table (migration 0014).
//!
//! [`PostgresJudgeSink`] is a [`JudgeSink`] the gateway boot wires alongside
//! the in-memory band store via
//! [`crate::AppState::with_quality_judge_persistent`] (a
//! [`crate::quality_sample::FanoutJudgeSink`]), so one recorded verdict both
//! feeds the live `/v1/preview` enrichment AND lands durably for Phase 2
//! attribution netting. `quality_verdicts.request_id` is the request's trace
//! id; the Phase-2 join against `request_logs.trace_id` (a NULLABLE TEXT
//! column) needs `request_id::text = trace_id` — casting `trace_id::uuid`
//! would throw on non-UUID values — and `request_logs.trace_id` is unindexed
//! today, so Phase 2 should add an index alongside its netting query.
//!
//! # Invariants
//!
//! * **Best-effort, fail-open.** The sink runs inside the detached judge task
//!   (never on the user response path); an INSERT error is warn-logged and
//!   swallowed — it never panics or propagates.
//! * **Measurement tax stays out of the savings ledger.** `judge_cost_usd` /
//!   `baseline_cost_usd` live ONLY here (+ the
//!   `tokentrimmer.quality.judge_cost_usd` span attribute) — never in
//!   `request_logs.cost_usd`, never counted as or against savings. `NULL`
//!   cost = unmetered (billed but unpriced), never persisted as `0`.

use async_trait::async_trait;
use sqlx::PgPool;

use crate::quality_sample::{ab_position_str, verdict_str, JudgeOutcome, JudgeSink};

/// Production sink: INSERTs one row per judged sample into `quality_verdicts`.
pub struct PostgresJudgeSink {
    pool: PgPool,
}

impl PostgresJudgeSink {
    /// Wrap a pool. The table is created by migration 0014 via the standard
    /// gateway boot migration path.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl std::fmt::Debug for PostgresJudgeSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresJudgeSink")
            .field("pool", &"PgPool { .. }")
            .finish()
    }
}

/// INSERT SQL for `quality_verdicts`. Exposed as a constant so the column /
/// placeholder / bind counts can be verified in tests without duplicating the
/// string (the same guard pattern as
/// `tt_telemetry::request_logs::postgres::INSERT_SQL`).
pub const INSERT_SQL: &str = r#"INSERT INTO quality_verdicts
                 (id, org_id, route_id, request_id, ts,
                  requested_model, served_model, verdict, reason, judge_model,
                  judge_cost_usd, baseline_cost_usd, baseline_dispatched,
                  optimized_position, orders_judged, orders_agreed,
                  cache_entry_id, hit_similarity)
               VALUES
                 ($1, $2, $3, $4, $5,
                  $6, $7, $8, $9, $10,
                  $11, $12, $13,
                  $14, $15, $16,
                  $17, $18)"#;

/// Number of `.bind(...)` calls in [`PostgresJudgeSink::record`]. Must stay in
/// sync with [`INSERT_SQL`] and the actual bind chain.
pub const INSERT_BIND_COUNT: usize = 18;

#[async_trait]
impl JudgeSink for PostgresJudgeSink {
    async fn record(&self, outcome: JudgeOutcome) {
        let result = sqlx::query(INSERT_SQL)
            .bind(uuid::Uuid::now_v7()) // $1 id
            .bind(outcome.org_id) // $2 org_id
            .bind(outcome.route_id) // $3 route_id
            .bind(outcome.score.request_id) // $4 request_id (joins request_logs.trace_id)
            .bind(chrono::Utc::now()) // $5 ts
            .bind(&outcome.requested_model) // $6 requested_model
            .bind(&outcome.served_model) // $7 served_model
            .bind(verdict_str(outcome.score.verdict)) // $8 verdict
            .bind(&outcome.score.reason) // $9 reason
            .bind(&outcome.judge_model) // $10 judge_model
            .bind(outcome.judge_cost_usd) // $11 judge_cost_usd (NULL = unmetered)
            .bind(outcome.baseline_cost_usd) // $12 baseline_cost_usd
            .bind(outcome.baseline_dispatched) // $13 baseline_dispatched
            .bind(ab_position_str(outcome.optimized_position)) // $14 optimized_position
            .bind(i16::from(outcome.orders_judged)) // $15 orders_judged
            .bind(outcome.orders_agreed) // $16 orders_agreed
            .bind(outcome.cache_entry_id) // $17 cache_entry_id (NULL on dispatch path)
            .bind(outcome.hit_similarity) // $18 hit_similarity (NULL on dispatch path)
            .execute(&self.pool)
            .await;
        if let Err(e) = result {
            // Best-effort: the verdict still reached the in-memory band store
            // via the fanout; losing the durable row is logged, never fatal.
            tracing::warn!(
                error = %e,
                request_id = %outcome.score.request_id,
                "quality_verdicts insert failed (best-effort, verdict not persisted)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{INSERT_BIND_COUNT, INSERT_SQL};

    /// Guard: the INSERT_SQL column list, placeholder list, and the
    /// INSERT_BIND_COUNT constant must all agree (mirror of the
    /// `request_logs` guard — catches a column/placeholder drift without a
    /// live Postgres connection).
    #[test]
    fn insert_sql_column_placeholder_bind_counts_match() {
        // --- count columns in the INSERT column list -------------------------
        let col_start = INSERT_SQL.find('(').expect("INSERT_SQL must contain '('");
        let col_end = INSERT_SQL[col_start + 1..]
            .find(')')
            .expect("INSERT_SQL must contain closing ')'")
            + col_start
            + 1;
        let col_list = &INSERT_SQL[col_start + 1..col_end];
        let column_count = col_list.split(',').count();

        // --- count $N placeholders in the VALUES clause ----------------------
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
