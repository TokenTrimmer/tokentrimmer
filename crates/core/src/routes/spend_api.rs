//! User-facing tenant spend surface, authed by the `tt_live_` key (never
//! caller-supplied) — requires a real key (anonymous / dogfood / sandbox → 401):
//!
//! * `GET  /v1/spend` — spend-today + MTD + budget-remaining. Backs the MCP
//!   `get_spend_today` / `check_budget_remaining` tools.
//! * `POST /v1/spend/limit` — set (or clear) the org's monthly spend cap, or a
//!   per-key cap (key-ownership-gated). Backs the MCP `set_cost_limit` tool. The
//!   gateway already READS these caps for enforcement ([`crate::tier_resolver`]);
//!   this is the matching tenant-self-serve write on the same auth seam.

use axum::{extract::State, Extension, Json};
use serde::{Deserialize, Serialize};
use tt_auth::ApiKeyContext;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::routes::routes_api::require_org;
use crate::spend::{day_start_utc, month_start_utc, SpendSummary};
use crate::AppState;

/// `GET /v1/spend` — the caller-org's spend summary. 503 until a
/// [`crate::spend::SpendSource`] is wired
/// ([`crate::AppState::with_spend_source`]); an org with no in-window traffic
/// answers an honest all-zero body, not 404.
pub async fn get_spend(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Json<SpendSummary>> {
    let org = require_org(ctx)?;
    let source = state.spend_source.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable("spend reporting is not configured on this gateway".into())
    })?;
    let now = chrono::Utc::now();
    let raw = source
        .org_spend(org, day_start_utc(now), month_start_utc(now))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(SpendSummary::assemble(org, raw)))
}

/// `POST /v1/spend/limit` body. `monthly_cap_usd: null` CLEARS the cap. When
/// `key_id` is present the cap applies to that key (must belong to the caller's
/// org); otherwise it's the org-wide cap.
#[derive(Debug, Deserialize)]
pub struct SetSpendLimitRequest {
    /// Monthly USD spend cap; `null` clears it. Must be finite and ≥ 0.
    pub monthly_cap_usd: Option<f64>,
    /// When set, scope the cap to this key (org-owned); else org-wide.
    #[serde(default)]
    pub key_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct SetSpendLimitResponse {
    pub org_id: Uuid,
    pub key_id: Option<Uuid>,
    pub monthly_cap_usd: Option<f64>,
    /// Always `true` on a 2xx (the change was persisted) — mirrors the MCP
    /// `CostLimitSet.applied` contract.
    pub applied: bool,
}

/// `POST /v1/spend/limit` — set/clear the caller-org's monthly spend cap (or a
/// per-key cap). 503 until a [`crate::spend::SpendSource`] is wired; 400 on a
/// negative/non-finite cap; 404 when `key_id` isn't the caller-org's.
pub async fn set_spend_limit(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Json(req): Json<SetSpendLimitRequest>,
) -> ApiResult<Json<SetSpendLimitResponse>> {
    let org = require_org(ctx)?;
    // A cap must be a real, non-negative dollar amount (or null to clear).
    if let Some(c) = req.monthly_cap_usd {
        if !c.is_finite() || c < 0.0 {
            return Err(ApiError::InvalidRequest(
                "monthly_cap_usd must be a finite, non-negative number (or null to clear)".into(),
            ));
        }
    }
    let source = state.spend_source.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable("spend reporting is not configured on this gateway".into())
    })?;

    match req.key_id {
        Some(key_id) => {
            let owned = source
                .set_key_monthly_cap(org, key_id, req.monthly_cap_usd)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            if !owned {
                return Err(ApiError::NotFound(
                    "no such key for this org (a key cap can only be set on your own key)".into(),
                ));
            }
        }
        None => source
            .set_org_monthly_cap(org, req.monthly_cap_usd)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    }

    Ok(Json(SetSpendLimitResponse {
        org_id: org,
        key_id: req.key_id,
        monthly_cap_usd: req.monthly_cap_usd,
        applied: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ProviderRegistry;
    use crate::spend::{InMemorySpendSource, OrgSpend};
    use std::sync::Arc;
    use uuid::Uuid;

    fn state_seeded(org: Uuid, spend: OrgSpend) -> AppState {
        let src = InMemorySpendSource::new();
        src.set_for_org(org, spend);
        AppState::new(ProviderRegistry::new()).with_spend_source(Arc::new(src))
    }

    fn auth(org: Uuid) -> Option<Extension<ApiKeyContext>> {
        Some(Extension(ApiKeyContext {
            key_id: Uuid::nil(),
            org_id: org,
            tier: None,
            skip_shadow: false,
        }))
    }

    #[tokio::test]
    async fn get_spend_returns_summary_for_authed_org() {
        let org = Uuid::now_v7();
        let state = state_seeded(
            org,
            OrgSpend {
                spent_today_usd: 1.5,
                spend_mtd_usd: 6.0,
                monthly_cap_usd: Some(10.0),
            },
        );
        let Json(s) = get_spend(State(state), auth(org)).await.unwrap();
        assert_eq!(s.org_id, org);
        assert!((s.spent_today_usd - 1.5).abs() < 1e-12);
        assert!((s.spend_mtd_usd - 6.0).abs() < 1e-12);
        assert_eq!(s.monthly_cap_usd, Some(10.0));
        assert!((s.remaining_usd.unwrap() - 4.0).abs() < 1e-12);
        assert!(s.configured);
    }

    #[tokio::test]
    async fn get_spend_unknown_org_is_honest_zero_not_error() {
        // An authed org with no seeded traffic answers all-zero, not 404/500.
        let org = Uuid::now_v7();
        let state = AppState::new(ProviderRegistry::new())
            .with_spend_source(Arc::new(InMemorySpendSource::new()));
        let Json(s) = get_spend(State(state), auth(org)).await.unwrap();
        assert_eq!(s.spent_today_usd, 0.0);
        assert_eq!(s.remaining_usd, None);
        assert!(!s.configured);
    }

    #[tokio::test]
    async fn get_spend_rejects_unauthenticated() {
        let state = AppState::new(ProviderRegistry::new())
            .with_spend_source(Arc::new(InMemorySpendSource::new()));
        let err = get_spend(State(state), None).await.unwrap_err();
        assert!(matches!(err, ApiError::Unauthorized));
    }

    #[tokio::test]
    async fn get_spend_503_when_source_not_wired() {
        let org = Uuid::now_v7();
        let state = AppState::new(ProviderRegistry::new()); // no spend source
        let err = get_spend(State(state), auth(org)).await.unwrap_err();
        assert!(matches!(err, ApiError::ServiceUnavailable(_)));
    }

    // ----- POST /v1/spend/limit -----

    fn state_with(src: InMemorySpendSource) -> AppState {
        AppState::new(ProviderRegistry::new()).with_spend_source(Arc::new(src))
    }

    #[tokio::test]
    async fn set_spend_limit_org_cap_applies_and_is_readable() {
        let org = Uuid::now_v7();
        let src = InMemorySpendSource::new();
        let state = state_with(src.clone()); // Clone shares the Arc-backed inner.
        let Json(resp) = set_spend_limit(
            State(state.clone()),
            auth(org),
            Json(SetSpendLimitRequest {
                monthly_cap_usd: Some(12.0),
                key_id: None,
            }),
        )
        .await
        .unwrap();
        assert!(resp.applied);
        assert_eq!(resp.monthly_cap_usd, Some(12.0));
        assert_eq!(resp.key_id, None);
        // The cap is now what GET /v1/spend reports.
        let Json(s) = get_spend(State(state), auth(org)).await.unwrap();
        assert_eq!(s.monthly_cap_usd, Some(12.0));
    }

    #[tokio::test]
    async fn set_spend_limit_key_cap_applies() {
        let org = Uuid::now_v7();
        let key = Uuid::now_v7();
        let src = InMemorySpendSource::new();
        let state = state_with(src.clone());
        let Json(resp) = set_spend_limit(
            State(state),
            auth(org),
            Json(SetSpendLimitRequest {
                monthly_cap_usd: Some(3.0),
                key_id: Some(key),
            }),
        )
        .await
        .unwrap();
        assert!(resp.applied);
        assert_eq!(resp.key_id, Some(key));
        assert_eq!(src.key_cap(key), Some(Some(3.0)));
    }

    #[tokio::test]
    async fn set_spend_limit_unowned_key_is_404() {
        let org = Uuid::now_v7();
        let key = Uuid::now_v7();
        let src = InMemorySpendSource::new();
        src.mark_key_unowned(key);
        let state = state_with(src);
        let err = set_spend_limit(
            State(state),
            auth(org),
            Json(SetSpendLimitRequest {
                monthly_cap_usd: Some(1.0),
                key_id: Some(key),
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    #[tokio::test]
    async fn set_spend_limit_rejects_negative_and_nonfinite() {
        let org = Uuid::now_v7();
        for bad in [-1.0, f64::INFINITY, f64::NAN] {
            let err = set_spend_limit(
                State(state_with(InMemorySpendSource::new())),
                auth(org),
                Json(SetSpendLimitRequest {
                    monthly_cap_usd: Some(bad),
                    key_id: None,
                }),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ApiError::InvalidRequest(_)), "rejected {bad}");
        }
    }

    #[tokio::test]
    async fn set_spend_limit_rejects_unauthenticated() {
        let err = set_spend_limit(
            State(state_with(InMemorySpendSource::new())),
            None,
            Json(SetSpendLimitRequest {
                monthly_cap_usd: Some(1.0),
                key_id: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::Unauthorized));
    }

    #[tokio::test]
    async fn set_spend_limit_503_when_source_not_wired() {
        let org = Uuid::now_v7();
        let err = set_spend_limit(
            State(AppState::new(ProviderRegistry::new())),
            auth(org),
            Json(SetSpendLimitRequest {
                monthly_cap_usd: Some(1.0),
                key_id: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::ServiceUnavailable(_)));
    }
}
