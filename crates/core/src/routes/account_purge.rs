//! Signed internal executor for durable cloud account-purge cleanup tasks.

use axum::{
    extract::State,
    http::{header::CACHE_CONTROL, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ApiError, ApiResult, AppState};

pub const SIGNATURE_HEADER: &str = "x-tokentrimmer-cleanup-signature";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayPurgeRequest {
    pub version: u8,
    pub task_id: Uuid,
    pub org_id: Uuid,
    pub issued_at_unix: i64,
    pub expires_at_unix: i64,
}

#[derive(Serialize)]
struct GatewayPurgeResponse {
    version: u8,
    complete: bool,
    deleted: usize,
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Execute one bounded purge pass. This route deliberately sits outside the
/// tenant API-key middleware; the short-lived HMAC capability is its only
/// authority and is unavailable unless boot wires both Redis and the root key.
pub async fn purge_l1(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<GatewayPurgeRequest>,
) -> ApiResult<Response> {
    let authorizer = state.gateway_purge_authorizer.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable("gateway account-purge executor is not configured".into())
    })?;
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if request.version != 1
        || !authorizer.verify(
            signature,
            request.task_id,
            request.org_id,
            request.issued_at_unix,
            request.expires_at_unix,
            chrono::Utc::now().timestamp(),
        )
    {
        return Err(ApiError::Unauthorized);
    }
    let l1 = state.l1.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable("gateway account-purge Redis store is unavailable".into())
    })?;
    let progress =
        l1.cache.purge_org(request.org_id).await.map_err(|_| {
            ApiError::ServiceUnavailable("gateway account-purge pass failed".into())
        })?;
    let status = if progress.complete {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok(no_store(
        (
            status,
            Json(GatewayPurgeResponse {
                version: 1,
                complete: progress.complete,
                deleted: progress.deleted,
            }),
        )
            .into_response(),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;
    use crate::{build_router_with_retrieval, ProviderRegistry};
    use tt_cache::{memory::InMemoryL1Cache, L1Cache};

    #[tokio::test]
    async fn internal_route_needs_exact_capability_and_fences_the_org() {
        let org = Uuid::new_v4();
        let cache = Arc::new(InMemoryL1Cache::new());
        let key = format!("{org}:request");
        cache.set(&key, b"private", 60).await.unwrap();
        let authorizer = Arc::new(crate::GatewayPurgeAuthorizer::from_master_key(&[9; 32]));
        let state = AppState::new(ProviderRegistry::new())
            .with_l1(cache.clone(), None)
            .with_gateway_purge_authorizer(authorizer.clone());
        let app = build_router_with_retrieval(state, None);
        let task = Uuid::new_v4();
        let issued = chrono::Utc::now().timestamp();
        let expires = issued + 60;
        let body = serde_json::to_vec(&GatewayPurgeRequest {
            version: 1,
            task_id: task,
            org_id: org,
            issued_at_unix: issued,
            expires_at_unix: expires,
        })
        .unwrap();
        let signature = authorizer.signature_hex(task, org, issued, expires);

        let response = app
            .oneshot(
                Request::post("/internal/v1/account-purge/l1")
                    .header("content-type", "application/json")
                    .header(SIGNATURE_HEADER, signature)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
        assert_eq!(cache.get(&key).await.unwrap(), None);
        cache.set(&key, b"late", 60).await.unwrap();
        assert_eq!(cache.get(&key).await.unwrap(), None);
    }
}
