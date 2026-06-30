//! Tenant-facing spend summary for the org behind an authenticated
//! `tt_live_*` key — the read side of the "cost control plane" MCP tools
//! (`get_spend_today` / `check_budget_remaining`).
//!
//! Everything derives from `request_logs` (the gateway's own telemetry sink)
//! plus the org's monthly cap in `org_budget_caps` (written by the cloud
//! self-serve budget UI; the gateway only READS it here, exactly as
//! [`crate::tier_resolver`] does for enforcement). No new ledger, no new table.
//!
//! Truncated rows (byte-estimated cost on an oversized body) are EXCLUDED from
//! every sum — the same `FILTER (WHERE NOT truncated)` discipline the budget
//! alert and reconciliation paths use, so the figure never inflates on a
//! request the gateway couldn't price exactly.
//!
//! The *write* side (setting a cap) is deliberately NOT here: `org_budget_caps`
//! / `api_key_budget_caps` are owned by the cloud schema, so a tenant-authed
//! cap write belongs on the cloud surface (a follow-up); this module is
//! read-only.

use async_trait::async_trait;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use uuid::Uuid;

/// Raw per-org spend a [`SpendSource`] returns: the two windowed sums plus the
/// org's monthly cap (if one is set). The handler folds these into a
/// [`SpendSummary`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrgSpend {
    /// Σ `cost_usd` since UTC midnight today (truncated rows excluded).
    pub spent_today_usd: f64,
    /// Σ `cost_usd` since the 1st of the current UTC month (truncated excluded).
    pub spend_mtd_usd: f64,
    /// The org's monthly spend cap from `org_budget_caps`, or `None` if unset.
    pub monthly_cap_usd: Option<f64>,
}

/// Tenant-facing spend summary returned by `GET /v1/spend`. Mirrors the MCP
/// `SpendToday` + `BudgetRemaining` shapes so the cost-control tools can read it
/// directly.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct SpendSummary {
    pub org_id: Uuid,
    pub spent_today_usd: f64,
    pub spend_mtd_usd: f64,
    /// The org's monthly cap, or `null` when none is configured.
    pub monthly_cap_usd: Option<f64>,
    /// `monthly_cap_usd - spend_mtd_usd`, clamped at 0 (a fully-spent budget
    /// reads `0`, never negative). `null` when no cap is configured — "remaining"
    /// is undefined without a ceiling, and reporting a number would imply one.
    pub remaining_usd: Option<f64>,
    /// Whether a monthly cap is configured (i.e. `remaining_usd` is meaningful).
    /// The MCP tools surface an honest "no budget configured" when false rather
    /// than a fabricated headroom.
    pub configured: bool,
}

impl SpendSummary {
    /// Fold raw [`OrgSpend`] into the tenant-facing summary: `remaining` is
    /// `cap - mtd` clamped at 0, and present only when a cap exists.
    #[must_use]
    pub fn assemble(org_id: Uuid, raw: OrgSpend) -> Self {
        let remaining_usd = raw
            .monthly_cap_usd
            .map(|cap| (cap - raw.spend_mtd_usd).max(0.0));
        Self {
            org_id,
            spent_today_usd: raw.spent_today_usd,
            spend_mtd_usd: raw.spend_mtd_usd,
            monthly_cap_usd: raw.monthly_cap_usd,
            remaining_usd,
            configured: raw.monthly_cap_usd.is_some(),
        }
    }
}

/// UTC midnight that starts `now`'s day — the `spent_today_usd` window start.
#[must_use]
pub fn day_start_utc(now: DateTime<Utc>) -> DateTime<Utc> {
    Utc.from_utc_datetime(
        &now.date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 is always valid"),
    )
}

/// UTC midnight on the 1st of `now`'s month — the `spend_mtd_usd` window start.
#[must_use]
pub fn month_start_utc(now: DateTime<Utc>) -> DateTime<Utc> {
    let first = now
        .date_naive()
        .with_day(1)
        .expect("day 1 is always valid")
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is always valid");
    Utc.from_utc_datetime(&first)
}

#[derive(Debug, thiserror::Error)]
pub enum SpendError {
    #[error("backend error: {0}")]
    Backend(String),
}

/// Read-side source for an org's spend + cap. Production wires
/// [`PostgresSpendSource`]; tests seed [`InMemorySpendSource`].
#[async_trait]
pub trait SpendSource: Send + Sync {
    /// The org's spend since `day_start` (today) and `month_start` (MTD), plus
    /// its configured monthly cap. Both windows run to "now" implicitly (rows
    /// only exist in the past), so the source needs only the two lower bounds.
    async fn org_spend(
        &self,
        org_id: Uuid,
        day_start: DateTime<Utc>,
        month_start: DateTime<Utc>,
    ) -> Result<OrgSpend, SpendError>;
}

/// Two-window spend roll-up: `$1=org`, `$2=day_start`, `$3=month_start`. The
/// outer `WHERE ts >= $3` bounds to the wider (MTD) window; the today figure is
/// a `FILTER` subset of it, so a single scan answers both. Truncated rows are
/// excluded from both sums. NUMERIC → FLOAT8 for sqlx f64 decode.
pub const SPEND_SQL: &str = r#"SELECT
  COALESCE(SUM(cost_usd) FILTER (WHERE NOT truncated AND ts >= $2), 0)::float8 AS spent_today_usd,
  COALESCE(SUM(cost_usd) FILTER (WHERE NOT truncated), 0)::float8 AS spend_mtd_usd
FROM request_logs
WHERE org_id = $1 AND ts >= $3"#;

/// The org's monthly cap (read-only; written by the cloud budget UI). `$1=org`.
/// Mirrors the read in [`crate::tier_resolver`]. Absent row ⇒ no cap.
pub const ORG_CAP_SQL: &str =
    r#"SELECT monthly_cap_usd::float8 AS monthly_cap_usd FROM org_budget_caps WHERE org_id = $1"#;

/// Production source: runs [`SPEND_SQL`] + [`ORG_CAP_SQL`] against the gateway DB.
pub struct PostgresSpendSource {
    pool: sqlx::PgPool,
}

impl PostgresSpendSource {
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

impl std::fmt::Debug for PostgresSpendSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresSpendSource")
            .field("pool", &"PgPool { .. }")
            .finish()
    }
}

#[derive(sqlx::FromRow)]
struct SpendRow {
    spent_today_usd: f64,
    spend_mtd_usd: f64,
}

#[async_trait]
impl SpendSource for PostgresSpendSource {
    async fn org_spend(
        &self,
        org_id: Uuid,
        day_start: DateTime<Utc>,
        month_start: DateTime<Utc>,
    ) -> Result<OrgSpend, SpendError> {
        let row = sqlx::query_as::<_, SpendRow>(SPEND_SQL)
            .bind(org_id)
            .bind(day_start)
            .bind(month_start)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| SpendError::Backend(e.to_string()))?;
        // Missing cap row ⇒ no cap (None), NOT an error.
        let monthly_cap_usd: Option<f64> = sqlx::query_scalar(ORG_CAP_SQL)
            .bind(org_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| SpendError::Backend(e.to_string()))?
            .flatten();
        Ok(OrgSpend {
            spent_today_usd: row.spent_today_usd,
            spend_mtd_usd: row.spend_mtd_usd,
            monthly_cap_usd,
        })
    }
}

/// Test / dev source: per-org seeded values, windows ignored.
#[derive(Debug, Default, Clone)]
pub struct InMemorySpendSource {
    inner: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<Uuid, OrgSpend>>>,
}

impl InMemorySpendSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the [`OrgSpend`] returned for `org_id` (replaces any previous seed).
    pub fn set_for_org(&self, org_id: Uuid, spend: OrgSpend) {
        let mut g = self.inner.lock().expect("spend source poisoned");
        g.insert(org_id, spend);
    }
}

#[async_trait]
impl SpendSource for InMemorySpendSource {
    async fn org_spend(
        &self,
        org_id: Uuid,
        _day_start: DateTime<Utc>,
        _month_start: DateTime<Utc>,
    ) -> Result<OrgSpend, SpendError> {
        let g = self.inner.lock().expect("spend source poisoned");
        // Unknown org ⇒ honest all-zero, no cap (not an error).
        Ok(g.get(&org_id).copied().unwrap_or(OrgSpend {
            spent_today_usd: 0.0,
            spend_mtd_usd: 0.0,
            monthly_cap_usd: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_with_cap_computes_clamped_remaining_and_configured() {
        let org = Uuid::now_v7();
        let s = SpendSummary::assemble(
            org,
            OrgSpend {
                spent_today_usd: 1.25,
                spend_mtd_usd: 7.0,
                monthly_cap_usd: Some(10.0),
            },
        );
        assert!(s.configured);
        assert_eq!(s.monthly_cap_usd, Some(10.0));
        assert!((s.remaining_usd.unwrap() - 3.0).abs() < 1e-12);
        assert!((s.spent_today_usd - 1.25).abs() < 1e-12);
    }

    #[test]
    fn assemble_over_cap_clamps_remaining_at_zero() {
        let org = Uuid::now_v7();
        let s = SpendSummary::assemble(
            org,
            OrgSpend {
                spent_today_usd: 0.0,
                spend_mtd_usd: 12.5,
                monthly_cap_usd: Some(10.0),
            },
        );
        // Over budget: remaining is 0, never negative (the headline clamps).
        assert_eq!(s.remaining_usd, Some(0.0));
        assert!(s.configured);
    }

    #[test]
    fn assemble_without_cap_has_no_remaining_and_unconfigured() {
        let org = Uuid::now_v7();
        let s = SpendSummary::assemble(
            org,
            OrgSpend {
                spent_today_usd: 2.0,
                spend_mtd_usd: 40.0,
                monthly_cap_usd: None,
            },
        );
        assert!(!s.configured);
        assert_eq!(s.remaining_usd, None, "no cap ⇒ remaining is undefined");
        assert_eq!(s.monthly_cap_usd, None);
    }

    #[test]
    fn window_starts_are_utc_midnight_and_month_first() {
        let now = Utc.with_ymd_and_hms(2026, 6, 30, 14, 25, 13).unwrap();
        let day = day_start_utc(now);
        assert_eq!(day, Utc.with_ymd_and_hms(2026, 6, 30, 0, 0, 0).unwrap());
        let month = month_start_utc(now);
        assert_eq!(month, Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap());
        assert!(
            month <= day,
            "month start must precede (or equal) day start"
        );
    }

    #[test]
    fn spend_sql_has_three_distinct_binds_and_excludes_truncated() {
        for present in ["$1", "$2", "$3"] {
            assert!(SPEND_SQL.contains(present), "missing bind {present}");
        }
        assert!(!SPEND_SQL.contains("$4"), "unexpected extra bind");
        assert!(
            SPEND_SQL.contains("WHERE NOT truncated"),
            "truncated rows must be excluded from the sums"
        );
    }

    #[tokio::test]
    async fn in_memory_source_round_trips_and_defaults_unknown_org_to_zero() {
        let src = InMemorySpendSource::new();
        let org = Uuid::now_v7();
        let seeded = OrgSpend {
            spent_today_usd: 3.0,
            spend_mtd_usd: 9.0,
            monthly_cap_usd: Some(20.0),
        };
        src.set_for_org(org, seeded);
        let now = Utc::now();
        let got = src
            .org_spend(org, day_start_utc(now), month_start_utc(now))
            .await
            .unwrap();
        assert_eq!(got, seeded);
        // Unknown org ⇒ all-zero, no cap, not an error.
        let unknown = src
            .org_spend(Uuid::now_v7(), day_start_utc(now), month_start_utc(now))
            .await
            .unwrap();
        assert_eq!(unknown.spent_today_usd, 0.0);
        assert_eq!(unknown.monthly_cap_usd, None);
    }
}
