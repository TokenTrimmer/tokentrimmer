//! User-facing `/v1/routes` CRUD. Org is derived from the authenticated
//! `tt_live_` key (never caller-supplied). Requires a real key — anonymous /
//! dogfood / sandbox callers get 401.

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use chrono::{DateTime, Utc};
use tt_auth::ApiKeyContext;
use tt_routing::{
    validate_auto_pause, validate_capability, validate_output_shaping, validate_shadow_model,
    NewRoute, NewRoutePause, PausedBy, Route, RoutingStore,
};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::route_savings::VerdictCounts;
use crate::{AppState, DOGFOOD_ORG_ID};

/// Default savings window (hours) for `GET /v1/routes/:id/savings` — 30 days.
const DEFAULT_SAVINGS_WINDOW_HOURS: u32 = 720;
/// Maximum savings window (hours) — 90 days.
const MAX_SAVINGS_WINDOW_HOURS: u32 = 2160;

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
    let registry = state.registry.clone();
    validate_capability(&spec.when, &spec.then, |m| registry.model_info(m).cloned())
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
    // #454: a route whose `shadow_model` cannot resolve to a registered provider
    // is rejected at config time (not silently no-op'd at dispatch).
    validate_shadow_model(&spec.then, |m| registry.resolve(m).is_some())
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
    // Phase 2.3: malformed auto-pause config (floor outside (0,1], min == 0)
    // is rejected at config time — validated even when auto_pause is false.
    validate_auto_pause(&spec.then).map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
    // Phase 3.1/3.2: malformed output-shaping caps (effort outside low/medium,
    // thinking budget below Anthropic's 1024 floor) are rejected at config
    // time — validated even on disabled routes.
    // Phase 3.3/3.4: an unknown `format_switch` wire value or a route
    // declaring BOTH output-shaping levers is rejected at config time.
    validate_output_shaping(&spec.then).map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
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

/// `POST /v1/routes/:id/pause` — manual sticky pause: the route keeps
/// matching for attribution but its rewrite (and every other cost lever) is
/// suppressed, so requests flow to the originally-requested model — the
/// EXPENSIVE, quality-safe direction. Idempotent: pausing an already-paused
/// route is still 200. Cleared ONLY by [`resume`].
pub async fn pause(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let org = require_org(ctx)?;
    let store = store(&state)?;
    store
        .get_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no route with id {id}")))?;
    store
        .pause_route(
            org,
            id,
            NewRoutePause {
                paused_by: PausedBy::Manual,
                reason: "manual".into(),
                pass_rate: None,
                verdicts_in_window: None,
            },
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(
        serde_json::json!({ "ok": true, "id": id, "paused": true }),
    ))
}

/// `POST /v1/routes/:id/resume` — THE explicit re-enable; nothing else clears
/// a pause (auto or manual). `was_paused` reports whether a pause row was
/// actually removed (false = the route wasn't paused; still 200).
pub async fn resume(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let org = require_org(ctx)?;
    let store = store(&state)?;
    store
        .get_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no route with id {id}")))?;
    let was_paused = store
        .resume_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "ok": true, "id": id, "paused": false, "was_paused": was_paused
    })))
}

/// Query for [`savings`]: window length in hours (default 720 = 30 days,
/// clamped to `1..=2160`).
#[derive(serde::Deserialize)]
pub struct SavingsQuery {
    pub hours: Option<u32>,
}

/// `GET /v1/routes/:id/savings` response: the route's windowed savings with
/// the measurement tax NETTED and itemized — every tax line is its OWN field
/// (never silently subtracted), and `net_saved_usd` MAY BE NEGATIVE (a
/// regressing route whose verification spend exceeds its swap saving must
/// show it; only the per-request headline clamps at 0). When
/// `unmetered_tax_rows > 0` the taxes are lower bounds → the net is an upper
/// bound.
#[derive(Debug, serde::Serialize)]
pub struct RouteSavingsResponse {
    pub route_id: Uuid,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    /// Whether the route is currently sticky-paused.
    pub paused: bool,
    pub requests: u64,
    pub gross_saved_usd: f64,
    pub judge_tax_usd: f64,
    pub shadow_tax_usd: f64,
    pub net_saved_usd: f64,
    pub unmetered_tax_rows: u64,
    pub verdicts: VerdictCounts,
}

/// `GET /v1/routes/:id/savings?hours=N` — route-level netted savings
/// (research Phase 2.3). 503 until the gateway wires a
/// [`crate::route_savings::RouteSavingsSource`]
/// ([`crate::AppState::with_route_savings`]); a route with no in-window
/// traffic answers an honest all-zero body, not 404.
pub async fn savings(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
    Query(q): Query<SavingsQuery>,
) -> ApiResult<Json<RouteSavingsResponse>> {
    let org = require_org(ctx)?;
    let route = store(&state)?
        .get_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no route with id {id}")))?;
    let source = state.route_savings.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "route savings aggregation is not configured on this gateway".into(),
        )
    })?;
    let hours = q
        .hours
        .unwrap_or(DEFAULT_SAVINGS_WINDOW_HOURS)
        .clamp(1, MAX_SAVINGS_WINDOW_HOURS);
    let window_end = Utc::now();
    let window_start = window_end - chrono::Duration::hours(i64::from(hours));
    let rows = source
        .route_savings(org, window_start, window_end)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // A route with no in-window rows is honestly zero — not an error.
    let net = rows
        .into_iter()
        .find(|r| r.route_id == id)
        .unwrap_or_else(|| crate::route_savings::assemble(id, 0, 0.0, 0.0, 0.0, 0, 0, 0, 0, 0));
    Ok(Json(RouteSavingsResponse {
        route_id: id,
        window_start,
        window_end,
        paused: route.paused,
        requests: net.requests,
        gross_saved_usd: net.gross_saved_usd,
        judge_tax_usd: net.judge_tax_usd,
        shadow_tax_usd: net.shadow_tax_usd,
        net_saved_usd: net.net_saved_usd,
        unmetered_tax_rows: net.unmetered_tax_rows,
        verdicts: net.verdicts,
    }))
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
