//! Durable, cross-replica spend reservations for capped provider dispatches.
//!
//! The legacy [`crate::budget::DynamicBudgetEnforcer`] records realized spend
//! in process memory. It remains useful for DB-less OSS deployments and request
//! volume/rate limits, but it cannot make a monthly USD cap atomic across Fly
//! machines. This module provides the hosted path: reserve an upper-bound cost
//! in Postgres before an upstream call, then atomically replace the reservation
//! with provider-reported realized cost.

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use tt_shared::context::BudgetDispatchState;
use uuid::Uuid;

/// A reservation held against every configured scope that applies to one call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetReservation {
    pub id: Uuid,
    pub estimated_usd: f64,
}
/// Provider operation admitted by one durable reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDispatchKind {
    Chat,
    ChatStream,
    Embeddings,
    Batch,
}

impl BudgetDispatchKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::ChatStream => "chat_stream",
            Self::Embeddings => "embeddings",
            Self::Batch => "batch",
        }
    }
}

/// Stable identity and provenance for one provider-call attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetDispatch {
    pub key: [u8; 32],
    pub provider: &'static str,
    pub kind: BudgetDispatchKind,
}
/// Complete admission input for one provider-call attempt.
///
/// Bundling the identity, provenance, bound, and observation time keeps every
/// store implementation on the same atomic contract.
#[derive(Debug, Clone, Copy)]
pub struct BudgetReservationRequest<'a> {
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    pub trace_id: Uuid,
    pub dispatch: BudgetDispatch,
    pub model: &'a str,
    pub estimated_usd: Option<f64>,
    pub now: DateTime<Utc>,
}

/// Evidence used to replace an upper-bound reservation with final spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementBasis {
    ProviderUsage,
    ConservativeEstimate,
}

impl SettlementBasis {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderUsage => "provider_usage",
            Self::ConservativeEstimate => "conservative_estimate",
        }
    }
}

/// Derive a one-way request seed from an authenticated identity and the
/// caller's logical-request key. The returned state retains only the digest;
/// the raw idempotency key is never persisted.
#[must_use]
pub fn dispatch_state_for_idempotency(
    org_id: Uuid,
    api_key_id: Uuid,
    idempotency_key: &str,
) -> BudgetDispatchState {
    let mut digest = Sha256::new();
    digest.update(b"tokentrimmer:budget-dispatch-seed:v1\0");
    digest.update(org_id.as_bytes());
    digest.update(api_key_id.as_bytes());
    digest.update(idempotency_key.as_bytes());
    BudgetDispatchState::from_seed(digest.finalize().into())
}

/// Admission result. Uncapped callers avoid both a reservation row and a
/// settlement write.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReservationAdmission {
    NotCapped,
    Reserved(BudgetReservation),
}

#[derive(Debug, Error)]
pub enum BudgetReservationError {
    #[error(
        "monthly spend cap cannot admit estimated cost ${estimated_usd:.6}; remaining ${remaining_usd:.6}"
    )]
    Exceeded {
        estimated_usd: f64,
        remaining_usd: f64,
    },
    #[error("a capped request has no enforceable cost bound for model {model}")]
    PriceUnknown { model: String },
    #[error("durable budget reservation unavailable: {0}")]
    Unavailable(String),
}

impl From<sqlx::Error> for BudgetReservationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Unavailable(error.to_string())
    }
}

#[async_trait]
pub trait BudgetReservationStore: Send + Sync {
    /// Atomically reserve `estimated_usd` against the org and API-key caps that
    /// currently exist. `None` means the model has no defensible cost bound;
    /// this is allowed only when neither scope has a USD cap.
    async fn reserve(
        &self,
        request: BudgetReservationRequest<'_>,
    ) -> Result<ReservationAdmission, BudgetReservationError>;

    /// Replace an active reservation with realized provider cost. Idempotent:
    /// retrying settlement after an uncertain caller-side timeout is safe.
    async fn settle(
        &self,
        reservation: BudgetReservation,
        actual_usd: f64,
        basis: SettlementBasis,
        now: DateTime<Utc>,
    ) -> Result<(), BudgetReservationError>;
}

#[derive(Clone)]
pub struct PostgresBudgetReservationStore {
    pool: PgPool,
}
struct BudgetAdjustment {
    reservation_id: Uuid,
    org_id: Uuid,
    api_key_id: Uuid,
    month_start: NaiveDate,
    kind: &'static str,
    delta_usd: f64,
    now: DateTime<Utc>,
}

impl PostgresBudgetReservationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Return a store only when the shared cloud cap tables and the gateway
    /// reservation migration are visible. A public-only OSS database has no
    /// `org_budget_caps`/`api_key_budget_caps`; it keeps the existing DB-less
    /// cap behavior rather than paying one failing SQL query per dispatch.
    pub async fn detect(pool: PgPool) -> Result<Option<Self>, sqlx::Error> {
        let available: bool = sqlx::query_scalar(
            "SELECT to_regclass('public.org_budget_caps') IS NOT NULL \
                 AND to_regclass('public.api_key_budget_caps') IS NOT NULL \
                 AND to_regclass('public.request_logs') IS NOT NULL \
                 AND to_regclass('public.gateway_budget_scope_months') IS NOT NULL \
                 AND to_regclass('public.gateway_budget_reservations') IS NOT NULL",
        )
        .fetch_one(&pool)
        .await?;
        Ok(available.then(|| Self::new(pool)))
    }

    async fn configured_caps(
        tx: &mut Transaction<'_, Postgres>,
        org_id: Uuid,
        api_key_id: Uuid,
    ) -> Result<(Option<f64>, Option<f64>), BudgetReservationError> {
        let org_cap = sqlx::query_scalar::<_, Option<f64>>(
            "SELECT monthly_cap_usd FROM org_budget_caps WHERE org_id = $1 FOR SHARE",
        )
        .bind(org_id)
        .fetch_optional(&mut **tx)
        .await?
        .flatten();

        let key_cap = if api_key_id.is_nil() {
            None
        } else {
            sqlx::query_scalar::<_, Option<f64>>(
                "SELECT monthly_cap_usd FROM api_key_budget_caps \
                 WHERE api_key_id = $1 AND org_id = $2 FOR SHARE",
            )
            .bind(api_key_id)
            .bind(org_id)
            .fetch_optional(&mut **tx)
            .await?
            .flatten()
        };

        for cap in [org_cap, key_cap].into_iter().flatten() {
            if !cap.is_finite() || cap < 0.0 {
                return Err(BudgetReservationError::Unavailable(
                    "database contains an invalid monthly USD cap".to_string(),
                ));
            }
        }
        Ok((org_cap, key_cap))
    }

    async fn ensure_scope(
        tx: &mut Transaction<'_, Postgres>,
        scope_kind: &'static str,
        scope_id: Uuid,
        month_start: NaiveDate,
        next_month: NaiveDate,
    ) -> Result<(), BudgetReservationError> {
        sqlx::query(
            "INSERT INTO gateway_budget_scope_months \
                 (scope_kind, scope_id, month_start, baseline_spend_usd) \
             SELECT $1, $2, $3, COALESCE(SUM(cost_usd)::double precision, 0) \
             FROM request_logs \
             WHERE (($1 = 'org' AND org_id = $2) \
                 OR ($1 = 'api_key' AND api_key_id = $2)) \
               AND ts >= $3 AND ts < $4 \
             ON CONFLICT (scope_kind, scope_id, month_start) DO NOTHING",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .bind(month_start)
        .bind(next_month)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn lock_scope_total(
        tx: &mut Transaction<'_, Postgres>,
        scope_kind: &'static str,
        scope_id: Uuid,
        month_start: NaiveDate,
    ) -> Result<f64, BudgetReservationError> {
        let (baseline, reserved, settled): (f64, f64, f64) = sqlx::query_as(
            "SELECT baseline_spend_usd, reserved_usd, settled_spend_usd \
             FROM gateway_budget_scope_months \
             WHERE scope_kind = $1 AND scope_id = $2 AND month_start = $3 \
             FOR UPDATE",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .bind(month_start)
        .fetch_one(&mut **tx)
        .await?;
        Ok(baseline + reserved + settled)
    }

    async fn update_scope_settlement(
        tx: &mut Transaction<'_, Postgres>,
        scope_kind: &'static str,
        scope_id: Uuid,
        month_start: NaiveDate,
        reserved_release_usd: f64,
        settled_delta_usd: f64,
        now: DateTime<Utc>,
    ) -> Result<(), BudgetReservationError> {
        sqlx::query(
            "UPDATE gateway_budget_scope_months \
             SET reserved_usd = GREATEST(0, reserved_usd - $4), \
                 settled_spend_usd = settled_spend_usd + $5, \
                 updated_at = $6 \
             WHERE scope_kind = $1 AND scope_id = $2 AND month_start = $3",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .bind(month_start)
        .bind(reserved_release_usd)
        .bind(settled_delta_usd)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn append_adjustment(
        tx: &mut Transaction<'_, Postgres>,
        adjustment: BudgetAdjustment,
    ) -> Result<(), BudgetReservationError> {
        let BudgetAdjustment {
            reservation_id,
            org_id,
            api_key_id,
            month_start,
            kind,
            delta_usd,
            now,
        } = adjustment;
        sqlx::query(
            "INSERT INTO gateway_budget_adjustments \
                 (id, reservation_id, org_id, api_key_id, month_start, kind, delta_usd, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::new_v4())
        .bind(reservation_id)
        .bind(org_id)
        .bind(api_key_id)
        .bind(month_start)
        .bind(kind)
        .bind(delta_usd)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn expire_stale_for_caller(
        tx: &mut Transaction<'_, Postgres>,
        org_id: Uuid,
        api_key_id: Uuid,
        month_start: NaiveDate,
        org_scope_locked: bool,
        key_scope_locked: bool,
        now: DateTime<Utc>,
    ) -> Result<(), BudgetReservationError> {
        // An org reservation can belong to a different API key than the
        // request that discovers its expired lease. Lock every affected key
        // scope before locking reservation rows, preserving reserve/settle's
        // aggregate-before-row order and preventing a revoked idle key from
        // consuming org headroom forever.
        let stale_key_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT api_key_id \
             FROM gateway_budget_reservations \
             WHERE month_start = $3 AND status = 'active' AND lease_expires_at <= $4 \
               AND reserves_api_key \
               AND (NOT reserves_org OR $5) \
               AND (($5 AND reserves_org AND org_id = $1) \
                    OR ($6 AND api_key_id = $2)) \
             ORDER BY api_key_id",
        )
        .bind(org_id)
        .bind(api_key_id)
        .bind(month_start)
        .bind(now)
        .bind(org_scope_locked)
        .bind(key_scope_locked)
        .fetch_all(&mut **tx)
        .await?;
        for stale_key_id in stale_key_ids {
            if !key_scope_locked || stale_key_id != api_key_id {
                let _ = Self::lock_scope_total(tx, "api_key", stale_key_id, month_start).await?;
            }
        }

        let stale: Vec<(Uuid, Uuid, Uuid, f64, bool, bool)> = sqlx::query_as(
            "SELECT id, org_id, api_key_id, estimated_usd, reserves_org, reserves_api_key \
             FROM gateway_budget_reservations \
             WHERE month_start = $3 AND status = 'active' AND lease_expires_at <= $4 \
               AND (NOT reserves_org OR $5) \
               AND (($5 AND reserves_org AND org_id = $1) \
                    OR ($6 AND reserves_api_key AND api_key_id = $2)) \
             ORDER BY id FOR UPDATE",
        )
        .bind(org_id)
        .bind(api_key_id)
        .bind(month_start)
        .bind(now)
        .bind(org_scope_locked)
        .bind(key_scope_locked)
        .fetch_all(&mut **tx)
        .await?;
        for (
            id,
            reservation_org_id,
            reservation_api_key_id,
            estimated_usd,
            reserves_org,
            reserves_api_key,
        ) in stale
        {
            if reserves_org {
                Self::update_scope_settlement(
                    tx,
                    "org",
                    reservation_org_id,
                    month_start,
                    estimated_usd,
                    estimated_usd,
                    now,
                )
                .await?;
            }
            if reserves_api_key {
                Self::update_scope_settlement(
                    tx,
                    "api_key",
                    reservation_api_key_id,
                    month_start,
                    estimated_usd,
                    estimated_usd,
                    now,
                )
                .await?;
            }
            Self::append_adjustment(
                tx,
                BudgetAdjustment {
                    reservation_id: id,
                    org_id: reservation_org_id,
                    api_key_id: reservation_api_key_id,
                    month_start,
                    kind: "lease_expiry",
                    delta_usd: estimated_usd,
                    now,
                },
            )
            .await?;
            sqlx::query(
                "UPDATE gateway_budget_reservations \
                 SET status = 'expired', settled_usd = estimated_usd, settled_at = $2, \
                     settlement_basis = 'lease_expiry', settlement_observed_at = $2 \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }
}

fn month_bounds(now: DateTime<Utc>) -> (NaiveDate, NaiveDate) {
    let month_start = now
        .date_naive()
        .with_day(1)
        .expect("every calendar month has a first day");
    let next_month = if month_start.month() == 12 {
        NaiveDate::from_ymd_opt(month_start.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(month_start.year(), month_start.month() + 1, 1)
    }
    .expect("valid next calendar month");
    (month_start, next_month)
}

#[async_trait]
impl BudgetReservationStore for PostgresBudgetReservationStore {
    async fn reserve(
        &self,
        request: BudgetReservationRequest<'_>,
    ) -> Result<ReservationAdmission, BudgetReservationError> {
        let BudgetReservationRequest {
            org_id,
            api_key_id,
            trace_id,
            dispatch,
            model,
            estimated_usd,
            now,
        } = request;
        let mut tx = self.pool.begin().await?;
        let (org_cap, key_cap) = Self::configured_caps(&mut tx, org_id, api_key_id).await?;
        if org_cap.is_none() && key_cap.is_none() {
            tx.commit().await?;
            return Ok(ReservationAdmission::NotCapped);
        }

        let Some(estimated_usd) = estimated_usd else {
            return Err(BudgetReservationError::PriceUnknown {
                model: model.to_string(),
            });
        };
        if !estimated_usd.is_finite() || estimated_usd < 0.0 {
            return Err(BudgetReservationError::PriceUnknown {
                model: model.to_string(),
            });
        }
        if estimated_usd == 0.0 {
            tx.commit().await?;
            return Ok(ReservationAdmission::NotCapped);
        }

        let (month_start, next_month) = month_bounds(now);
        let mut scopes = Vec::with_capacity(2);
        if let Some(cap) = org_cap {
            scopes.push(("org", org_id, cap));
        }
        if let Some(cap) = key_cap {
            scopes.push(("api_key", api_key_id, cap));
        }

        // Org is always locked before API key. Reservation settlement and lease
        // expiry use the same order, preventing cross-scope deadlocks.
        for &(kind, id, _) in &scopes {
            Self::ensure_scope(&mut tx, kind, id, month_start, next_month).await?;
        }
        for &(kind, id, _) in &scopes {
            let _ = Self::lock_scope_total(&mut tx, kind, id, month_start).await?;
        }
        Self::expire_stale_for_caller(
            &mut tx,
            org_id,
            api_key_id,
            month_start,
            org_cap.is_some(),
            key_cap.is_some(),
            now,
        )
        .await?;
        let prior_dispatch: Option<(Uuid, String)> = sqlx::query_as(
            "SELECT id, status FROM gateway_budget_reservations \
             WHERE org_id = $1 AND api_key_id = $2 AND dispatch_key = $3 \
             FOR UPDATE",
        )
        .bind(org_id)
        .bind(api_key_id)
        .bind(dispatch.key.as_slice())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((reservation_id, status)) = prior_dispatch {
            return Err(BudgetReservationError::Unavailable(format!(
                "provider dispatch {reservation_id} was already admitted with status {status}"
            )));
        }

        let mut remaining_usd = f64::INFINITY;
        for &(kind, id, cap) in &scopes {
            let current = Self::lock_scope_total(&mut tx, kind, id, month_start).await?;
            remaining_usd = remaining_usd.min((cap - current).max(0.0));
            if current + estimated_usd > cap {
                return Err(BudgetReservationError::Exceeded {
                    estimated_usd,
                    remaining_usd,
                });
            }
        }

        let reservation = BudgetReservation {
            id: Uuid::new_v4(),
            estimated_usd,
        };
        let reserves_org = org_cap.is_some();
        let reserves_api_key = key_cap.is_some();
        let lease_expires_at = now + chrono::Duration::minutes(15);
        sqlx::query(
            "INSERT INTO gateway_budget_reservations \
                 (id, org_id, api_key_id, trace_id, month_start, provider, model, dispatch_kind, \
                  dispatch_key, estimated_usd, reserves_org, reserves_api_key, lease_expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(reservation.id)
        .bind(org_id)
        .bind(api_key_id)
        .bind(trace_id)
        .bind(month_start)
        .bind(dispatch.provider)
        .bind(model)
        .bind(dispatch.kind.as_str())
        .bind(dispatch.key.as_slice())
        .bind(estimated_usd)
        .bind(reserves_org)
        .bind(reserves_api_key)
        .bind(lease_expires_at)
        .execute(&mut *tx)
        .await?;

        for (kind, id, _) in scopes {
            sqlx::query(
                "UPDATE gateway_budget_scope_months \
                 SET reserved_usd = reserved_usd + $4, updated_at = $5 \
                 WHERE scope_kind = $1 AND scope_id = $2 AND month_start = $3",
            )
            .bind(kind)
            .bind(id)
            .bind(month_start)
            .bind(estimated_usd)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(ReservationAdmission::Reserved(reservation))
    }

    async fn settle(
        &self,
        reservation: BudgetReservation,
        actual_usd: f64,
        basis: SettlementBasis,
        now: DateTime<Utc>,
    ) -> Result<(), BudgetReservationError> {
        let (actual_usd, basis) = if actual_usd.is_finite() && actual_usd >= 0.0 {
            (actual_usd, basis)
        } else {
            (
                reservation.estimated_usd,
                SettlementBasis::ConservativeEstimate,
            )
        };
        type ReservationRow = (Uuid, Uuid, NaiveDate, f64, bool, bool, String, Option<f64>);

        let mut tx = self.pool.begin().await?;
        let initial: Option<ReservationRow> = sqlx::query_as(
            "SELECT org_id, api_key_id, month_start, estimated_usd, \
                    reserves_org, reserves_api_key, status, settled_usd \
             FROM gateway_budget_reservations WHERE id = $1",
        )
        .bind(reservation.id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((org_id, api_key_id, month_start, _, org_scope, key_scope, _, _)) = initial else {
            return Err(BudgetReservationError::Unavailable(format!(
                "reservation {} does not exist",
                reservation.id
            )));
        };

        // Match reserve's lock order: scope aggregates first, reservation row
        // second. Two independent process instances therefore serialize rather
        // than deadlock or both spending the same headroom.
        if org_scope {
            let _ = Self::lock_scope_total(&mut tx, "org", org_id, month_start).await?;
        }
        if key_scope {
            let _ = Self::lock_scope_total(&mut tx, "api_key", api_key_id, month_start).await?;
        }

        let (
            org_id,
            api_key_id,
            month_start,
            estimated_usd,
            org_scope,
            key_scope,
            status,
            prior_settled,
        ): ReservationRow = sqlx::query_as(
            "SELECT org_id, api_key_id, month_start, estimated_usd, \
                    reserves_org, reserves_api_key, status, settled_usd \
             FROM gateway_budget_reservations WHERE id = $1 FOR UPDATE",
        )
        .bind(reservation.id)
        .fetch_one(&mut *tx)
        .await?;
        if status == "settled" {
            tx.commit().await?;
            return Ok(());
        }

        let (reserved_release, settled_delta, adjustment_kind) = if status == "active" {
            (estimated_usd, actual_usd, "settlement")
        } else {
            (
                0.0,
                actual_usd - prior_settled.unwrap_or(estimated_usd),
                "late_settlement_adjustment",
            )
        };
        if org_scope {
            Self::update_scope_settlement(
                &mut tx,
                "org",
                org_id,
                month_start,
                reserved_release,
                settled_delta,
                now,
            )
            .await?;
        }
        if key_scope {
            Self::update_scope_settlement(
                &mut tx,
                "api_key",
                api_key_id,
                month_start,
                reserved_release,
                settled_delta,
                now,
            )
            .await?;
        }
        Self::append_adjustment(
            &mut tx,
            BudgetAdjustment {
                reservation_id: reservation.id,
                org_id,
                api_key_id,
                month_start,
                kind: adjustment_kind,
                delta_usd: settled_delta,
                now,
            },
        )
        .await?;
        sqlx::query(
            "UPDATE gateway_budget_reservations \
             SET status = 'settled', settled_usd = $2, settled_at = $3, \
                 settlement_basis = $4, settlement_observed_at = $3 \
             WHERE id = $1",
        )
        .bind(reservation.id)
        .bind(actual_usd)
        .bind(now)
        .bind(basis.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

mod provider;
pub(crate) use provider::{derive_budget_dispatch, BudgetedProvider};

#[cfg(test)]
mod tests;
