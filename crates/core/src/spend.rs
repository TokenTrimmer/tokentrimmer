//! Tenant cost-control plane for the org behind an authenticated `tt_live_*`
//! key — backs the MCP `get_spend_today` / `check_budget_remaining` /
//! `set_cost_limit` tools.
//!
//! READ: spend derives from `request_logs` (the gateway's own telemetry sink) +
//! the org's monthly cap in `org_budget_caps`. Truncated rows (byte-estimated
//! cost on an oversized body) are EXCLUDED from every sum — the same
//! `FILTER (WHERE NOT truncated)` discipline the budget-alert + reconciliation
//! paths use, so the figure never inflates on a request the gateway couldn't
//! price exactly.
//!
//! WRITE: [`SpendSource::set_org_monthly_cap`] /
//! [`SpendSource::set_key_monthly_cap`] upsert `org_budget_caps` /
//! `api_key_budget_caps`. [`crate::budget_reservation`] reads those rows and
//! atomically reserves every capped provider attempt. The reporting view here
//! remains settled request telemetry and does not include active reservations.

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

/// Source for an org's spend + cap. Production wires [`PostgresSpendSource`];
/// tests seed [`InMemorySpendSource`].
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

    /// Set (or clear, with `None`) the org's monthly spend cap in
    /// `org_budget_caps`; [`crate::budget_reservation`] enforces that row.
    async fn set_org_monthly_cap(
        &self,
        org_id: Uuid,
        cap_usd: Option<f64>,
    ) -> Result<(), SpendError>;

    /// Set (or clear) a per-key monthly spend cap in `api_key_budget_caps`, but
    /// ONLY if `key_id` belongs to `org_id` (the ownership gate). Returns
    /// `Ok(false)` when the key is not the caller-org's (the handler maps that to
    /// 404), `Ok(true)` when applied.
    async fn set_key_monthly_cap(
        &self,
        org_id: Uuid,
        key_id: Uuid,
        cap_usd: Option<f64>,
    ) -> Result<bool, SpendError>;
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

/// The org's monthly cap. `$1=org`. Mirrors the durable reservation layer's
/// cap lookup. Absent row ⇒ no cap.
pub const ORG_CAP_SQL: &str =
    r#"SELECT monthly_cap_usd::float8 AS monthly_cap_usd FROM org_budget_caps WHERE org_id = $1"#;

/// Upsert the org's monthly USD cap. `$1=org`, `$2=cap` (`NULL` clears it). Only
/// `monthly_cap_usd` is touched — `monthly_request_cap` + the alert-band columns
/// keep their values on conflict (defaults on insert).
pub const SET_ORG_CAP_SQL: &str = r#"INSERT INTO org_budget_caps (org_id, monthly_cap_usd)
VALUES ($1, $2)
ON CONFLICT (org_id) DO UPDATE SET monthly_cap_usd = EXCLUDED.monthly_cap_usd"#;

/// Ownership gate: does `$1=key_id` belong to `$2=org_id`? A row ⇒ owned.
pub const KEY_OWNED_SQL: &str = r#"SELECT 1 FROM api_keys WHERE id = $1 AND org_id = $2"#;

/// Upsert a per-key monthly USD cap. `$1=key_id`, `$2=org_id`, `$3=cap` (`NULL`
/// clears). `monthly_request_cap` is preserved on conflict.
pub const SET_KEY_CAP_SQL: &str = r#"INSERT INTO api_key_budget_caps (api_key_id, org_id, monthly_cap_usd)
VALUES ($1, $2, $3)
ON CONFLICT (api_key_id) DO UPDATE SET monthly_cap_usd = EXCLUDED.monthly_cap_usd, updated_at = now()"#;

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

    async fn set_org_monthly_cap(
        &self,
        org_id: Uuid,
        cap_usd: Option<f64>,
    ) -> Result<(), SpendError> {
        sqlx::query(SET_ORG_CAP_SQL)
            .bind(org_id)
            .bind(cap_usd)
            .execute(&self.pool)
            .await
            .map_err(|e| SpendError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn set_key_monthly_cap(
        &self,
        org_id: Uuid,
        key_id: Uuid,
        cap_usd: Option<f64>,
    ) -> Result<bool, SpendError> {
        // Ownership gate — never let a tenant cap a key that isn't theirs.
        let owned: Option<i32> = sqlx::query_scalar(KEY_OWNED_SQL)
            .bind(key_id)
            .bind(org_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| SpendError::Backend(e.to_string()))?;
        if owned.is_none() {
            return Ok(false);
        }
        sqlx::query(SET_KEY_CAP_SQL)
            .bind(key_id)
            .bind(org_id)
            .bind(cap_usd)
            .execute(&self.pool)
            .await
            .map_err(|e| SpendError::Backend(e.to_string()))?;
        Ok(true)
    }
}

/// Test / dev source: per-org seeded values, windows ignored.
#[derive(Debug, Default, Clone)]
pub struct InMemorySpendSource {
    inner: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<Uuid, OrgSpend>>>,
    /// Per-key caps written via `set_key_monthly_cap` (for assertions).
    key_caps: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<Uuid, Option<f64>>>>,
    /// Keys to treat as NOT owned (so `set_key_monthly_cap` returns `false` →
    /// the handler 404s). Empty ⇒ every key is owned.
    unowned: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<Uuid>>>,
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

    /// Mark `key_id` as not belonging to any org, so `set_key_monthly_cap`
    /// returns `Ok(false)` (exercises the handler's 404 path).
    pub fn mark_key_unowned(&self, key_id: Uuid) {
        self.unowned
            .lock()
            .expect("spend source poisoned")
            .insert(key_id);
    }

    /// The cap last written for `key_id` (for test assertions); `None` ⇒ never set.
    #[must_use]
    pub fn key_cap(&self, key_id: Uuid) -> Option<Option<f64>> {
        self.key_caps
            .lock()
            .expect("spend source poisoned")
            .get(&key_id)
            .copied()
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

    async fn set_org_monthly_cap(
        &self,
        org_id: Uuid,
        cap_usd: Option<f64>,
    ) -> Result<(), SpendError> {
        let mut g = self.inner.lock().expect("spend source poisoned");
        let e = g.entry(org_id).or_insert(OrgSpend {
            spent_today_usd: 0.0,
            spend_mtd_usd: 0.0,
            monthly_cap_usd: None,
        });
        e.monthly_cap_usd = cap_usd;
        Ok(())
    }

    async fn set_key_monthly_cap(
        &self,
        _org_id: Uuid,
        key_id: Uuid,
        cap_usd: Option<f64>,
    ) -> Result<bool, SpendError> {
        if self
            .unowned
            .lock()
            .expect("spend source poisoned")
            .contains(&key_id)
        {
            return Ok(false);
        }
        self.key_caps
            .lock()
            .expect("spend source poisoned")
            .insert(key_id, cap_usd);
        Ok(true)
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

    #[test]
    fn cap_write_sql_shapes() {
        // Org cap: upsert keyed on org_id, only the USD cap is written.
        assert!(SET_ORG_CAP_SQL.contains("INTO org_budget_caps"));
        assert!(SET_ORG_CAP_SQL.contains("ON CONFLICT (org_id) DO UPDATE"));
        assert!(SET_ORG_CAP_SQL.contains("monthly_cap_usd = EXCLUDED.monthly_cap_usd"));
        assert!(
            !SET_ORG_CAP_SQL.contains("monthly_request_cap"),
            "must not clobber the request cap"
        );
        // Ownership gate filters by BOTH key id and org.
        assert!(KEY_OWNED_SQL.contains("id = $1") && KEY_OWNED_SQL.contains("org_id = $2"));
        // Key cap: upsert keyed on api_key_id.
        assert!(SET_KEY_CAP_SQL.contains("INTO api_key_budget_caps"));
        assert!(SET_KEY_CAP_SQL.contains("ON CONFLICT (api_key_id) DO UPDATE"));
    }

    #[tokio::test]
    async fn in_memory_set_org_cap_reflects_in_org_spend() {
        let src = InMemorySpendSource::new();
        let org = Uuid::now_v7();
        let now = Utc::now();
        // Set a cap on a previously-unseen org → org_spend now reports it.
        src.set_org_monthly_cap(org, Some(25.0)).await.unwrap();
        let got = src
            .org_spend(org, day_start_utc(now), month_start_utc(now))
            .await
            .unwrap();
        assert_eq!(got.monthly_cap_usd, Some(25.0));
        // Clearing it (None) round-trips.
        src.set_org_monthly_cap(org, None).await.unwrap();
        let cleared = src
            .org_spend(org, day_start_utc(now), month_start_utc(now))
            .await
            .unwrap();
        assert_eq!(cleared.monthly_cap_usd, None);
    }

    #[tokio::test]
    async fn in_memory_set_key_cap_records_and_respects_ownership() {
        let src = InMemorySpendSource::new();
        let org = Uuid::now_v7();
        let key = Uuid::now_v7();
        // Owned key → applied (true) + recorded.
        assert!(src.set_key_monthly_cap(org, key, Some(5.0)).await.unwrap());
        assert_eq!(src.key_cap(key), Some(Some(5.0)));
        // A key marked unowned → false (handler will 404), nothing recorded.
        let foreign = Uuid::now_v7();
        src.mark_key_unowned(foreign);
        assert!(!src
            .set_key_monthly_cap(org, foreign, Some(9.0))
            .await
            .unwrap());
        assert_eq!(src.key_cap(foreign), None);
    }
}
