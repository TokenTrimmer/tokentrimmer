//! User-facing `GET /v1/spend` — the org's spend-today + month-to-date + budget
//! remaining, derived from the authenticated `tt_live_` key (never
//! caller-supplied). Requires a real key — anonymous / dogfood / sandbox callers
//! get 401. Backs the MCP `get_spend_today` / `check_budget_remaining` tools.
//!
//! Read-only: setting a cap (`set_cost_limit`) writes the cloud-owned
//! `org_budget_caps` table and is a separate cloud surface (a follow-up).

use axum::{extract::State, Extension, Json};
use tt_auth::ApiKeyContext;

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
}
