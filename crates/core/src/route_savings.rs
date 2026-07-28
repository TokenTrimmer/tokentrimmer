//! Route-level savings attribution with the measurement tax NETTED (research
//! Phase 2.3 — the "Phase 2 nets this into routing attribution" follow-up the
//! #155 sampler promised).
//!
//! # Measurement Law (read before touching)
//!
//! Per-request header/ledger figures STAY GROSS — a single request doesn't
//! carry the amortized judge tax, so nothing here changes the chat cost path,
//! headers, or `request_logs` cost columns. The netting lands ONLY at this
//! aggregate surface, with the tax always broken out as its own line — never
//! silently subtracted:
//!
//! * `gross_saved_usd` — the historical compatibility figure: Σ per-row
//!   `GREATEST(baseline - cost - provider_cache_saved - cache_bust_penalty,
//!   0)`. It deliberately remains unchanged for API compatibility, but its
//!   row-level floor hides regressions and it does not include the later
//!   `summarizer_tax_usd` row field.
//! * `judge_tax_usd` — Σ `COALESCE(judge_cost_usd, 0) +
//!   COALESCE(baseline_cost_usd, 0)` over the route's `quality_verdicts`
//!   rows: the #155 paired judge's metered measurement spend (judge calls +
//!   baseline reference dispatches).
//! * `shadow_tax_usd` — Σ `COALESCE(shadow_cost_usd, 0)`: the #146 shadow
//!   arm's discarded verification spend.
//! * `cache_bust_usd` — Σ `cache_bust_penalty_usd`: the cache-bust NEGATIVE
//!   entry (agentic budget Sub-levers 1-3 + any pass), ITEMIZED so it is
//!   auditable. It is already included in the legacy gross floor and in the
//!   signed row estimate below, so it is never subtracted a second time.
//! * `summarizer_tax_usd` — Σ recorded auxiliary summarizer spend. It is not
//!   part of the legacy compatibility fields, but is included in the signed
//!   row estimate below and itemized so the auxiliary cost is never hidden.
//! * `positive_estimated_savings_usd`, `estimated_regressions_usd`, and
//!   `net_estimated_savings_usd` — the additive signed counterpart. For each
//!   request, its complete row delta is `baseline - cost -
//!   provider_cache_saved - cache_bust_penalty - summarizer_tax`. The first
//!   two fields sum its positive and negative magnitudes separately; the net
//!   is that signed row total less the judge and shadow taxes. No row-level
//!   floor is applied to the net, so a regression remains visible.
//! * `net_saved_usd = gross - judge_tax - shadow_tax`, and it MAY BE
//!   NEGATIVE — a regressing route whose verification spend exceeds its swap
//!   saving must show it. This legacy net remains unchanged; clients that need
//!   a complete row-level estimate should use `net_estimated_savings_usd`.
//! * `unmetered_tax_rows` — rows whose judge/shadow tax is UNMETERED (NULL
//!   costs). When > 0 those taxes are LOWER bounds, so both aggregate nets are
//!   UPPER bounds. No durable marker yet identifies an unmetered summarizer.
//!
//! Everything derives 100% from `request_logs` × `quality_verdicts` — no new
//! ledger. The join is a `route_id` GROUP BY (both tables persist it; #155
//! stamps `quality_verdicts.route_id`), NOT the per-request `trace_id` join
//! the 0014 comments originally anticipated — migration 0019 adds the
//! `(org_id, route_id, ts)` indexes this query needs. L2-hit judge tax
//! (`route_id IS NULL`) is by definition not route attribution and is
//! excluded here; its org-level reconciliation is the cloud half of Task #25.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// One route's savings over a window, with the measurement tax NETTED and
/// itemized. Derived 100% from request_logs + quality_verdicts — no new
/// ledger.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RouteSavingsNet {
    pub route_id: Uuid,
    /// request_logs rows attributed to the route in-window.
    pub requests: u64,
    /// Historical compatibility figure: Σ per-row
    /// GREATEST(baseline - cost - provider_cache_saved - cache_bust_penalty,
    /// 0). Its row-level floor hides regressions and it does not subtract the
    /// later `summarizer_tax_usd` column.
    pub gross_saved_usd: f64,
    /// Judge tax: Σ COALESCE(judge_cost_usd,0) + COALESCE(baseline_cost_usd,0)
    /// over the route's quality_verdicts rows (judge calls + baseline
    /// reference dispatches — #155's metered measurement spend).
    pub judge_tax_usd: f64,
    /// Verification tax of the #146 shadow arm: Σ COALESCE(shadow_cost_usd,0).
    pub shadow_tax_usd: f64,
    /// Σ `cache_bust_penalty_usd` over the route's rows. It is included inside
    /// both the legacy gross floor and the signed row estimate; this itemized
    /// field is for auditability and is never subtracted again.
    pub cache_bust_usd: f64,
    /// Σ recorded `summarizer_tax_usd` over the route's rows. It is included
    /// inside the signed row estimate, but not the legacy compatibility
    /// fields, and is itemized so auxiliary spend is auditable.
    pub summarizer_tax_usd: f64,
    /// gross - judge_tax - shadow_tax. MAY BE NEGATIVE (a regressing route
    /// whose verification spend exceeds its swap saving must show it) — never
    /// clamped at this aggregate level; only the per-request headline clamps.
    pub net_saved_usd: f64,
    /// Positive half of the complete signed request delta:
    /// Σ GREATEST(baseline - cost - provider_cache_saved - cache_bust_penalty
    /// - summarizer_tax, 0).
    pub positive_estimated_savings_usd: f64,
    /// Absolute magnitude of the negative half of the complete signed request
    /// delta. This keeps row-level regressions visible instead of losing them
    /// to the historical `gross_saved_usd` floor.
    pub estimated_regressions_usd: f64,
    /// Complete signed request delta (positive estimates minus regressions),
    /// less judge and shadow taxes. Cache-bust and summarizer taxes are already
    /// inside the row delta, so they are not subtracted again. May be negative.
    pub net_estimated_savings_usd: f64,
    /// Rows whose tax is UNMETERED (quality_verdicts: judge_cost_usd IS NULL,
    /// or baseline_dispatched AND baseline_cost_usd IS NULL; request_logs:
    /// shadow_model IS NOT NULL AND shadow_cost_usd IS NULL). When > 0 the
    /// taxes are LOWER bounds → both aggregate nets are UPPER bounds. There is
    /// not yet a durable marker for an unmetered summarizer cost.
    pub unmetered_tax_rows: u64,
    /// Σ `format_switch_saved_est_usd` over the route's rows — the
    /// format-switch lever's LABELED ESTIMATE (research Phase 3.3:
    /// JSON-equivalent reconstruction − emitted body). Reported as its own
    /// clearly-estimated line and NEVER netted into `gross_saved_usd` /
    /// `net_saved_usd` / `net_estimated_savings_usd` (those derive from row
    /// figures;
    /// diff savings need no extra field — they already ride
    /// `gross_saved_usd` via the baseline fold).
    pub format_switch_saved_est_usd: f64,
    pub verdicts: VerdictCounts,
}

/// Verdict tallies for one route's in-window `quality_verdicts` rows.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, Default)]
pub struct VerdictCounts {
    /// All rows, incl. unclear/unjudged spend rows.
    pub judged: u64,
    pub acceptable: u64,
    pub degraded: u64,
    pub unclear: u64,
    /// acceptable / (acceptable + degraded); None when no classified verdicts
    /// (mirror of quality_sample::VerdictTally — unclear excluded).
    pub pass_rate: Option<f64>,
}

/// Exact signed row aggregates returned by [`ROUTE_SAVINGS_SQL`]. Kept
/// private because ordinary callers should consume the finished
/// [`RouteSavingsNet`], while the legacy public assemblers retain their
/// compatibility inputs.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SignedRowSavings {
    positive_estimated_savings_usd: f64,
    estimated_regressions_usd: f64,
    signed_row_delta_usd: f64,
    summarizer_tax_usd: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum SavingsError {
    #[error("backend error: {0}")]
    Backend(String),
}

/// Read-side source for per-route netted savings. Production wires
/// [`PostgresRouteSavingsSource`]; tests seed [`InMemoryRouteSavingsSource`].
#[async_trait]
pub trait RouteSavingsSource: Send + Sync {
    /// Per-route netted savings for org over `[window_start, window_end)`.
    /// Only route-attributed rows (route_id IS NOT NULL); the L2-hit judge
    /// tax (route_id NULL) is by definition not route attribution and is
    /// excluded here (org-level reconciliation of it is the cloud half).
    async fn route_savings(
        &self,
        org_id: Uuid,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Result<Vec<RouteSavingsNet>, SavingsError>;
}

/// Assemble one [`RouteSavingsNet`] from raw aggregate sums. Computes
/// `net = gross - judge_tax - shadow_tax` (NO clamp — a negative net must
/// show) and `pass_rate` (`None` when `acceptable + degraded == 0`).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn assemble(
    route_id: Uuid,
    requests: u64,
    gross: f64,
    judge_tax: f64,
    shadow_tax: f64,
    unmetered_rows: u64,
    judged: u64,
    acceptable: u64,
    degraded: u64,
    unclear: u64,
) -> RouteSavingsNet {
    assemble_with_estimates(
        route_id,
        requests,
        gross,
        judge_tax,
        shadow_tax,
        unmetered_rows,
        judged,
        acceptable,
        degraded,
        unclear,
        0.0,
    )
}

/// [`assemble`] plus the format-switch ESTIMATE line (its own field, never
/// netted — see [`RouteSavingsNet::format_switch_saved_est_usd`]). The
/// itemized cache-bust line defaults to 0 (use [`assemble_with_taxes`] to
/// surface it).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn assemble_with_estimates(
    route_id: Uuid,
    requests: u64,
    gross: f64,
    judge_tax: f64,
    shadow_tax: f64,
    unmetered_rows: u64,
    judged: u64,
    acceptable: u64,
    degraded: u64,
    unclear: u64,
    format_switch_saved_est: f64,
) -> RouteSavingsNet {
    assemble_with_taxes(
        route_id,
        requests,
        gross,
        judge_tax,
        shadow_tax,
        0.0,
        unmetered_rows,
        judged,
        acceptable,
        degraded,
        unclear,
        format_switch_saved_est,
    )
}

/// [`assemble_with_estimates`] plus the ITEMIZED cache-bust line
/// (`cache_bust` = Σ `cache_bust_penalty_usd`). The bust is ALREADY netted
/// inside `gross` at the row level (`GREATEST(... - cache_bust_penalty_usd,
/// 0)`), so it is surfaced here for transparency ONLY and is NOT subtracted
/// again from `net_saved_usd` (re-subtracting would double-count). See
/// [`RouteSavingsNet::cache_bust_usd`].
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn assemble_with_taxes(
    route_id: Uuid,
    requests: u64,
    gross: f64,
    judge_tax: f64,
    shadow_tax: f64,
    cache_bust: f64,
    unmetered_rows: u64,
    judged: u64,
    acceptable: u64,
    degraded: u64,
    unclear: u64,
    format_switch_saved_est: f64,
) -> RouteSavingsNet {
    let classified = acceptable + degraded;
    let pass_rate = if classified == 0 {
        None
    } else {
        Some(acceptable as f64 / classified as f64)
    };
    let net_saved_usd = gross - judge_tax - shadow_tax;
    RouteSavingsNet {
        route_id,
        requests,
        gross_saved_usd: gross,
        judge_tax_usd: judge_tax,
        shadow_tax_usd: shadow_tax,
        cache_bust_usd: cache_bust,
        summarizer_tax_usd: 0.0,
        // gross already nets the bust at the row level — do NOT re-subtract it
        // here (that would double-count the negative entry).
        net_saved_usd,
        // This compatibility assembler receives only the already-floored
        // legacy gross total, not raw row deltas. Treat that supplied figure as
        // the positive component so its additive signed fallback preserves the
        // legacy result. Production uses `assemble_with_signed_rows` below.
        positive_estimated_savings_usd: gross,
        estimated_regressions_usd: 0.0,
        net_estimated_savings_usd: net_saved_usd,
        unmetered_tax_rows: unmetered_rows,
        format_switch_saved_est_usd: format_switch_saved_est,
        verdicts: VerdictCounts {
            judged,
            acceptable,
            degraded,
            unclear,
            pass_rate,
        },
    }
}

/// Finish a production aggregate that includes the complete signed row
/// estimate. `cache_bust` and `signed.summarizer_tax_usd` are already part of
/// `signed.signed_row_delta_usd`, so the final net subtracts only the external
/// judge and shadow measurement taxes.
#[allow(clippy::too_many_arguments)]
fn assemble_with_signed_rows(
    route_id: Uuid,
    requests: u64,
    gross: f64,
    judge_tax: f64,
    shadow_tax: f64,
    cache_bust: f64,
    signed: SignedRowSavings,
    unmetered_rows: u64,
    judged: u64,
    acceptable: u64,
    degraded: u64,
    unclear: u64,
    format_switch_saved_est: f64,
) -> RouteSavingsNet {
    let mut net = assemble_with_taxes(
        route_id,
        requests,
        gross,
        judge_tax,
        shadow_tax,
        cache_bust,
        unmetered_rows,
        judged,
        acceptable,
        degraded,
        unclear,
        format_switch_saved_est,
    );
    net.positive_estimated_savings_usd = signed.positive_estimated_savings_usd;
    net.estimated_regressions_usd = signed.estimated_regressions_usd;
    net.summarizer_tax_usd = signed.summarizer_tax_usd;
    net.net_estimated_savings_usd = signed.signed_row_delta_usd - judge_tax - shadow_tax;
    net
}

/// Canonical netting query: one statement, 3 binds ($1 = org_id,
/// $2 = window_start inclusive, $3 = window_end exclusive). NUMERIC sums are
/// cast to FLOAT8 in each CTE for sqlx f64 decode; the FULL OUTER JOIN keeps a
/// route visible when it has verdicts but no in-window traffic (or vice
/// versa). Served by the migration-0019 `(org_id, route_id, ts)` indexes.
pub const ROUTE_SAVINGS_SQL: &str = r#"WITH gross AS (
  SELECT route_id,
         COUNT(*)::bigint AS requests,
         COALESCE(SUM(GREATEST(baseline_cost_usd - cost_usd - provider_cache_saved_usd - cache_bust_penalty_usd, 0)), 0)::float8 AS gross_saved_usd,
         COALESCE(SUM(GREATEST(baseline_cost_usd - cost_usd - provider_cache_saved_usd - cache_bust_penalty_usd - COALESCE(summarizer_tax_usd, 0), 0)), 0)::float8 AS positive_estimated_savings_usd,
         COALESCE(SUM(GREATEST(cost_usd + provider_cache_saved_usd + cache_bust_penalty_usd + COALESCE(summarizer_tax_usd, 0) - baseline_cost_usd, 0)), 0)::float8 AS estimated_regressions_usd,
         COALESCE(SUM(baseline_cost_usd - cost_usd - provider_cache_saved_usd - cache_bust_penalty_usd - COALESCE(summarizer_tax_usd, 0)), 0)::float8 AS signed_row_delta_usd,
         COALESCE(SUM(COALESCE(shadow_cost_usd, 0)), 0)::float8 AS shadow_tax_usd,
         COALESCE(SUM(cache_bust_penalty_usd), 0)::float8 AS cache_bust_usd,
         COALESCE(SUM(COALESCE(summarizer_tax_usd, 0)), 0)::float8 AS summarizer_tax_usd,
         COALESCE(SUM(format_switch_saved_est_usd), 0)::float8 AS format_switch_saved_est_usd,
         COUNT(*) FILTER (WHERE shadow_model IS NOT NULL AND shadow_cost_usd IS NULL)::bigint AS unmetered_shadow_rows
  FROM request_logs
  WHERE org_id = $1 AND ts >= $2 AND ts < $3 AND route_id IS NOT NULL
  GROUP BY route_id),
tax AS (
  SELECT route_id,
         COUNT(*)::bigint AS judged,
         COUNT(*) FILTER (WHERE verdict = 'acceptable')::bigint AS acceptable,
         COUNT(*) FILTER (WHERE verdict = 'degraded')::bigint  AS degraded,
         COUNT(*) FILTER (WHERE verdict = 'unclear')::bigint   AS unclear,
         COALESCE(SUM(COALESCE(judge_cost_usd, 0) + COALESCE(baseline_cost_usd, 0)), 0)::float8 AS judge_tax_usd,
         COUNT(*) FILTER (WHERE judge_cost_usd IS NULL OR (baseline_dispatched AND baseline_cost_usd IS NULL))::bigint AS unmetered_verdict_rows
  FROM quality_verdicts
  WHERE org_id = $1 AND ts >= $2 AND ts < $3 AND route_id IS NOT NULL
  GROUP BY route_id)
SELECT COALESCE(g.route_id, t.route_id) AS route_id,
       COALESCE(g.requests, 0) AS requests,
       COALESCE(g.gross_saved_usd, 0) AS gross_saved_usd,
       COALESCE(g.positive_estimated_savings_usd, 0) AS positive_estimated_savings_usd,
       COALESCE(g.estimated_regressions_usd, 0) AS estimated_regressions_usd,
       COALESCE(g.signed_row_delta_usd, 0) AS signed_row_delta_usd,
       COALESCE(g.shadow_tax_usd, 0) AS shadow_tax_usd,
       COALESCE(g.cache_bust_usd, 0) AS cache_bust_usd,
       COALESCE(g.summarizer_tax_usd, 0) AS summarizer_tax_usd,
       COALESCE(g.format_switch_saved_est_usd, 0) AS format_switch_saved_est_usd,
       COALESCE(g.unmetered_shadow_rows, 0) + COALESCE(t.unmetered_verdict_rows, 0) AS unmetered_tax_rows,
       COALESCE(t.judged, 0) AS judged,
       COALESCE(t.acceptable, 0) AS acceptable,
       COALESCE(t.degraded, 0) AS degraded,
       COALESCE(t.unclear, 0) AS unclear,
       COALESCE(t.judge_tax_usd, 0) AS judge_tax_usd
FROM gross g FULL OUTER JOIN tax t ON g.route_id = t.route_id
ORDER BY route_id"#;

/// Production source: runs [`ROUTE_SAVINGS_SQL`] against the gateway DB.
pub struct PostgresRouteSavingsSource {
    pool: sqlx::PgPool,
}

impl PostgresRouteSavingsSource {
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

impl std::fmt::Debug for PostgresRouteSavingsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresRouteSavingsSource")
            .field("pool", &"PgPool { .. }")
            .finish()
    }
}

/// Decoded shape of one [`ROUTE_SAVINGS_SQL`] result row.
#[derive(sqlx::FromRow)]
struct SavingsRow {
    route_id: Uuid,
    requests: i64,
    gross_saved_usd: f64,
    positive_estimated_savings_usd: f64,
    estimated_regressions_usd: f64,
    signed_row_delta_usd: f64,
    shadow_tax_usd: f64,
    cache_bust_usd: f64,
    summarizer_tax_usd: f64,
    format_switch_saved_est_usd: f64,
    unmetered_tax_rows: i64,
    judged: i64,
    acceptable: i64,
    degraded: i64,
    unclear: i64,
    judge_tax_usd: f64,
}

#[async_trait]
impl RouteSavingsSource for PostgresRouteSavingsSource {
    async fn route_savings(
        &self,
        org_id: Uuid,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    ) -> Result<Vec<RouteSavingsNet>, SavingsError> {
        let rows = sqlx::query_as::<_, SavingsRow>(ROUTE_SAVINGS_SQL)
            .bind(org_id)
            .bind(window_start)
            .bind(window_end)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| SavingsError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| {
                assemble_with_signed_rows(
                    r.route_id,
                    r.requests.max(0) as u64,
                    r.gross_saved_usd,
                    r.judge_tax_usd,
                    r.shadow_tax_usd,
                    r.cache_bust_usd,
                    SignedRowSavings {
                        positive_estimated_savings_usd: r.positive_estimated_savings_usd,
                        estimated_regressions_usd: r.estimated_regressions_usd,
                        signed_row_delta_usd: r.signed_row_delta_usd,
                        summarizer_tax_usd: r.summarizer_tax_usd,
                    },
                    r.unmetered_tax_rows.max(0) as u64,
                    r.judged.max(0) as u64,
                    r.acceptable.max(0) as u64,
                    r.degraded.max(0) as u64,
                    r.unclear.max(0) as u64,
                    r.format_switch_saved_est_usd,
                )
            })
            .collect())
    }
}

/// Test / dev source: per-org seeded rows, window ignored.
#[derive(Debug, Default, Clone)]
pub struct InMemoryRouteSavingsSource {
    inner: Arc<Mutex<HashMap<Uuid, Vec<RouteSavingsNet>>>>,
}

impl InMemoryRouteSavingsSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the rows returned for `org_id` (replaces any previous seed).
    pub fn set_for_org(&self, org_id: Uuid, rows: Vec<RouteSavingsNet>) {
        let mut g = self.inner.lock().expect("savings source poisoned");
        g.insert(org_id, rows);
    }
}

#[async_trait]
impl RouteSavingsSource for InMemoryRouteSavingsSource {
    async fn route_savings(
        &self,
        org_id: Uuid,
        _window_start: DateTime<Utc>,
        _window_end: DateTime<Utc>,
    ) -> Result<Vec<RouteSavingsNet>, SavingsError> {
        let g = self.inner.lock().expect("savings source poisoned");
        Ok(g.get(&org_id).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// net = gross - judge_tax - shadow_tax, including a NEGATIVE net (never
    /// clamped at the aggregate level); pass_rate None when no classified
    /// verdicts; unmetered rows flagged.
    #[test]
    fn assemble_netting_math() {
        let id = Uuid::now_v7();

        // Healthy route: positive net, classified pass rate.
        let healthy = assemble(id, 100, 10.0, 1.5, 0.5, 0, 10, 9, 1, 2);
        assert!((healthy.net_saved_usd - 8.0).abs() < 1e-12);
        assert!((healthy.gross_saved_usd - 10.0).abs() < 1e-12);
        assert!((healthy.judge_tax_usd - 1.5).abs() < 1e-12);
        assert!((healthy.shadow_tax_usd - 0.5).abs() < 1e-12);
        assert_eq!(healthy.verdicts.judged, 10);
        assert_eq!(healthy.verdicts.unclear, 2);
        assert!((healthy.verdicts.pass_rate.expect("classified") - 0.9).abs() < 1e-12);

        // Regressing route: verification spend exceeds the swap saving — the
        // net MUST go negative, never clamp (only the per-request headline
        // clamps at 0).
        let regressing = assemble(id, 5, 0.10, 0.25, 0.05, 0, 3, 1, 2, 0);
        assert!(
            (regressing.net_saved_usd - (-0.20)).abs() < 1e-12,
            "negative net must survive: {}",
            regressing.net_saved_usd
        );
        assert!((regressing.verdicts.pass_rate.unwrap() - (1.0 / 3.0)).abs() < 1e-12);

        // No classified verdicts (only unclear/unjudged spend rows) → the
        // pass rate is honestly unknown, not 0% or 100%.
        let unjudged = assemble(id, 2, 1.0, 0.0, 0.0, 1, 3, 0, 0, 3);
        assert_eq!(unjudged.verdicts.pass_rate, None);
        assert_eq!(
            unjudged.unmetered_tax_rows, 1,
            "unmetered rows must be flagged (taxes are lower bounds)"
        );
    }

    /// The cache-bust penalty is ITEMIZED as its own line (never silently
    /// buried inside the netted `gross_saved_usd`). It IS already netted into
    /// `gross_saved_usd` (the row-level `GREATEST(... - cache_bust_penalty,
    /// 0)`), so itemizing it here is purely informational — a CFO can see the
    /// negative entry that reduced the headline. The assembler does NOT
    /// re-subtract it (double-counting would understate the saving). Mirrors the
    /// `judge_tax_usd` itemization-not-silent-subtraction discipline.
    #[test]
    fn cache_bust_itemized_not_silently_subtracted() {
        let id = Uuid::now_v7();
        // gross already reflects the bust net; the bust line is the SAME figure
        // surfaced for transparency, not an extra subtraction.
        let row = assemble_with_taxes(id, 10, 5.0, 1.0, 0.5, 0.25, 0, 2, 2, 0, 0, 0.0);
        assert!((row.cache_bust_usd - 0.25).abs() < 1e-12);
        // gross/net are NOT reduced again by the itemized bust (it was already
        // netted at the row level inside gross).
        assert!((row.gross_saved_usd - 5.0).abs() < 1e-12);
        assert!((row.net_saved_usd - (5.0 - 1.0 - 0.5)).abs() < 1e-12);
        // The thin wrappers default the bust line to 0.
        let plain = assemble(id, 10, 5.0, 1.0, 0.5, 0, 2, 2, 0, 0);
        assert_eq!(plain.cache_bust_usd, 0.0);
        let with_est = assemble_with_estimates(id, 10, 5.0, 1.0, 0.5, 0, 2, 2, 0, 0, 7.25);
        assert_eq!(with_est.cache_bust_usd, 0.0);
    }

    /// The signed counterpart keeps a costly row visible instead of applying
    /// the legacy positive-only floor before aggregation. Cache-bust and
    /// summarizer spend are part of each row delta, then shown separately so
    /// neither is hidden nor subtracted twice.
    #[test]
    fn signed_estimate_preserves_row_regressions_and_known_taxes() {
        let id = Uuid::now_v7();
        // Two complete row deltas:
        //   +0.50 = 1.00 - 0.20 - 0.10 - 0.05 - 0.15
        //   -0.50 = 0.20 - 0.60 - 0.00 - 0.00 - 0.10
        // Their signed total is zero, but the legacy row floor retains only
        // the first row's pre-summarizer 0.65 gross contribution.
        let row = assemble_with_signed_rows(
            id,
            2,
            0.65,
            0.10,
            0.20,
            0.05,
            SignedRowSavings {
                positive_estimated_savings_usd: 0.50,
                estimated_regressions_usd: 0.50,
                signed_row_delta_usd: 0.0,
                summarizer_tax_usd: 0.25,
            },
            0,
            0,
            0,
            0,
            0,
            0.0,
        );

        // Existing consumers retain the historical fields unchanged.
        assert!((row.gross_saved_usd - 0.65).abs() < 1e-12);
        assert!((row.net_saved_usd - 0.35).abs() < 1e-12);
        // The additive fields expose both halves of the row-level delta.
        assert!((row.positive_estimated_savings_usd - 0.50).abs() < 1e-12);
        assert!((row.estimated_regressions_usd - 0.50).abs() < 1e-12);
        assert!((row.cache_bust_usd - 0.05).abs() < 1e-12);
        assert!((row.summarizer_tax_usd - 0.25).abs() < 1e-12);
        assert!(
            (row.net_estimated_savings_usd - (-0.30)).abs() < 1e-12,
            "the signed net must include the row regression and external taxes"
        );
    }

    /// The canonical SQL itemizes the cache-bust as its OWN summed column
    /// (`cache_bust_usd`) in addition to netting it inside `gross_saved_usd`'s
    /// `GREATEST(...)` — so the negative entry is auditable, never silently
    /// subtracted.
    #[test]
    fn route_savings_sql_itemizes_cache_bust() {
        assert!(
            ROUTE_SAVINGS_SQL.contains("AS cache_bust_usd"),
            "cache-bust must be itemized as its own summed column"
        );
        // Still netted inside the gross GREATEST(...) (unchanged behavior).
        assert!(
            ROUTE_SAVINGS_SQL.contains("- cache_bust_penalty_usd, 0)"),
            "cache-bust must stay netted in gross_saved_usd"
        );
    }

    /// The production query must carry the complete row delta through to the
    /// signed aggregate: costly rows become an explicit regression instead of
    /// disappearing under the legacy gross floor, and both known row taxes
    /// remain in that delta.
    #[test]
    fn route_savings_sql_has_signed_row_estimate_with_known_taxes() {
        for column in [
            "AS positive_estimated_savings_usd",
            "AS estimated_regressions_usd",
            "AS signed_row_delta_usd",
            "AS summarizer_tax_usd",
        ] {
            assert!(
                ROUTE_SAVINGS_SQL.contains(column),
                "signed route-savings query missing {column}"
            );
        }
        assert!(
            ROUTE_SAVINGS_SQL.contains("cache_bust_penalty_usd - COALESCE(summarizer_tax_usd, 0)"),
            "complete signed delta must include the cache-bust and summarizer taxes"
        );
        assert!(
            ROUTE_SAVINGS_SQL
                .contains("cost_usd + provider_cache_saved_usd + cache_bust_penalty_usd"),
            "costly rows must be recorded as explicit regressions"
        );
    }

    /// The format-switch ESTIMATE rides its OWN field and is NEVER netted
    /// into gross/net (the invoice-reconciled figures) — a CFO must be able
    /// to unpick the estimated lever from the realized ones.
    #[test]
    fn format_switch_estimate_never_netted() {
        let id = Uuid::now_v7();
        let with_est = assemble_with_estimates(id, 10, 5.0, 1.0, 0.5, 0, 2, 2, 0, 0, 7.25);
        assert!((with_est.format_switch_saved_est_usd - 7.25).abs() < 1e-12);
        // gross/net unchanged by the estimate.
        assert!((with_est.gross_saved_usd - 5.0).abs() < 1e-12);
        assert!((with_est.net_saved_usd - 3.5).abs() < 1e-12);
        // The thin wrapper defaults the estimate to 0.
        let plain = assemble(id, 10, 5.0, 1.0, 0.5, 0, 2, 2, 0, 0);
        assert_eq!(plain.format_switch_saved_est_usd, 0.0);
    }

    /// The canonical SQL uses exactly the binds the source supplies
    /// ($1=org, $2=start, $3=end) and no others.
    #[test]
    fn route_savings_sql_has_three_distinct_binds() {
        for present in ["$1", "$2", "$3"] {
            assert!(
                ROUTE_SAVINGS_SQL.contains(present),
                "missing bind {present}"
            );
        }
        assert!(
            !ROUTE_SAVINGS_SQL.contains("$4"),
            "unexpected extra bind in ROUTE_SAVINGS_SQL"
        );
    }

    #[tokio::test]
    async fn in_memory_source_round_trips_seeded_rows() {
        let src = InMemoryRouteSavingsSource::new();
        let org = Uuid::now_v7();
        let row = assemble(Uuid::now_v7(), 1, 1.0, 0.2, 0.0, 0, 1, 1, 0, 0);
        src.set_for_org(org, vec![row.clone()]);
        let got = src
            .route_savings(org, Utc::now(), Utc::now())
            .await
            .unwrap();
        assert_eq!(got, vec![row]);
        // Unknown org → empty, not an error.
        assert!(src
            .route_savings(Uuid::now_v7(), Utc::now(), Utc::now())
            .await
            .unwrap()
            .is_empty());
    }
}
