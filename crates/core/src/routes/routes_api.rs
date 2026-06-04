//! User-facing `/v1/routes` CRUD. Org is derived from the authenticated
//! `tt_live_` key (never caller-supplied). Requires a real key — anonymous /
//! dogfood / sandbox callers get 401.

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use tt_auth::ApiKeyContext;
use tt_routing::{validate_capability, validate_same_provider, NewRoute, Route, RoutingStore};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::{AppState, DOGFOOD_ORG_ID};

/// Resolve the caller's real org, or 401. Dogfood/absent contexts are rejected.
fn require_org(ctx: Option<Extension<ApiKeyContext>>) -> Result<Uuid, ApiError> {
    match ctx {
        Some(Extension(c)) if c.org_id != DOGFOOD_ORG_ID => Ok(c.org_id),
        _ => Err(ApiError::Unauthorized),
    }
}

fn store(state: &AppState) -> ApiResult<&std::sync::Arc<tt_routing::CachingRoutingStore>> {
    state.routing_store.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable("route management is not configured on this gateway".into())
    })
}

/// `GET /v1/routes` — list all of the caller-org's routes (incl. disabled).
pub async fn list(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Json<Vec<Route>>> {
    let org = require_org(ctx)?;
    let routes = store(&state)?
        .list_all_for_org(org)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(routes))
}

/// `POST /v1/routes` — validate + create.
pub async fn create(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Json(spec): Json<NewRoute>,
) -> ApiResult<(axum::http::StatusCode, Json<Route>)> {
    let org = require_org(ctx)?;
    validate_same_provider(&spec.when, &spec.then)
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
    let registry = state.registry.clone();
    validate_capability(&spec.when, &spec.then, |m| registry.model_info(m).cloned())
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
    let created = store(&state)?
        .create_route(org, spec)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok((axum::http::StatusCode::CREATED, Json(created)))
}

/// `GET /v1/routes/:id`.
pub async fn get(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Route>> {
    let org = require_org(ctx)?;
    let route = store(&state)?
        .get_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no route with id {id}")))?;
    Ok(Json(route))
}

/// `DELETE /v1/routes/:id`.
pub async fn delete(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let org = require_org(ctx)?;
    let removed = store(&state)?
        .delete_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if removed {
        Ok(Json(serde_json::json!({ "ok": true, "id": id })))
    } else {
        Err(ApiError::NotFound(format!("no route with id {id}")))
    }
}
