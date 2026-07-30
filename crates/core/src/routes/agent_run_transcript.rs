//! Explicit export and erasure controls for short-lived agent transcripts.
//!
//! Durable run metadata remains in Postgres for billing/audit purposes. The
//! transcript itself is a one-hour L1/Redis record, available only for runs
//! that paused (or later resumed) and only until its last-write TTL expires.

use axum::{
    extract::{Path, State},
    http::{header::CACHE_CONTROL, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use tt_auth::ApiKeyContext;
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiResult},
    routes::agent_run::{fetch_run, run_key, Run},
    AppState, DOGFOOD_ORG_ID,
};

fn require_org(ctx: Option<Extension<ApiKeyContext>>) -> Result<Uuid, ApiError> {
    match ctx {
        Some(Extension(context)) if context.org_id != DOGFOOD_ORG_ID => Ok(context.org_id),
        _ => Err(ApiError::Unauthorized),
    }
}

fn private_no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// `GET /v1/agent/runs/:id/transcript` exports the caller-org's currently
/// retained transcript. It deliberately does not fall back to durable metadata:
/// an absent/expired transcript is a 404, even when the run summary remains.
pub async fn export_transcript(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let l1 = state.l1.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "agent transcript export requires the L1/Redis store (none configured)".into(),
        )
    })?;
    let run: Run = fetch_run(l1.cache.as_ref(), org, id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no retained transcript for run {id}")))?
        .to_run();
    Ok(private_no_store(Json(run).into_response()))
}

/// `DELETE /v1/agent/runs/:id/transcript` idempotently erases exactly one
/// caller-org transcript. The org is part of the cache key, so a caller cannot
/// address another tenant's record. The shared single-flight fence prevents a
/// concurrent resume from rewriting the record after deletion.
pub async fn delete_transcript(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let l1 = state.l1.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "agent transcript deletion requires the L1/Redis store (none configured)".into(),
        )
    })?;
    let key = run_key(org, id);
    let _guard = state
        .single_flight
        .try_become_leader(&key)
        .map_err(|_| ApiError::Conflict(format!("run {id} is already being resumed or deleted")))?;
    l1.cache
        .delete(&key)
        .await
        .map_err(|error| ApiError::Internal(format!("agent transcript delete: {error}")))?;
    Ok(private_no_store(StatusCode::NO_CONTENT.into_response()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tt_cache::{memory::InMemoryL1Cache, L1Cache};

    use super::*;
    use crate::{registry::ProviderRegistry, AppState};

    fn auth(org_id: Uuid) -> Option<Extension<ApiKeyContext>> {
        Some(Extension(ApiKeyContext {
            key_id: Uuid::new_v4(),
            org_id,
            tier: None,
            skip_shadow: false,
        }))
    }

    #[tokio::test]
    async fn deletion_is_exactly_org_scoped_and_idempotent() {
        let cache = Arc::new(InMemoryL1Cache::new());
        let org_a = Uuid::new_v4();
        let org_b = Uuid::new_v4();
        let run = Uuid::new_v4();
        let key_a = run_key(org_a, run);
        let key_b = run_key(org_b, run);
        cache.set(&key_a, b"a", 60).await.unwrap();
        cache.set(&key_b, b"b", 60).await.unwrap();

        let state = AppState::new(ProviderRegistry::new()).with_l1(cache.clone(), None);
        let response = delete_transcript(State(state.clone()), auth(org_a), Path(run))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
        assert!(cache.get(&key_a).await.unwrap().is_none());
        assert_eq!(cache.get(&key_b).await.unwrap(), Some(b"b".to_vec()));

        let again = delete_transcript(State(state), auth(org_a), Path(run))
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn deletion_conflicts_with_an_active_resume_fence() {
        let cache = Arc::new(InMemoryL1Cache::new());
        let org = Uuid::new_v4();
        let run = Uuid::new_v4();
        let state = AppState::new(ProviderRegistry::new()).with_l1(cache, None);
        let _resume_guard = state
            .single_flight
            .try_become_leader(&run_key(org, run))
            .unwrap();

        let error = delete_transcript(State(state), auth(org), Path(run))
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Conflict(_)));
    }
}
