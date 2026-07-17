//! User-facing `/v1/routes` CRUD. Org is derived from the authenticated
//! `tt_live_` key (never caller-supplied). Requires a real key — anonymous /
//! dogfood / sandbox callers get 401.

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tt_auth::ApiKeyContext;
use tt_routing::catalog::is_catalog_route_name;
use tt_routing::{
    canonicalize_route_parts, canonicalize_route_value, validate_agentic_budget,
    validate_capability, validate_shadow_model, NewRoutePause, PausedBy, Route,
    RouteManagementView, RouteValidationIssue, RoutingStore,
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
/// Shared with other tenant-key-authed customer endpoints (e.g. `/v1/spend`).
pub(crate) fn require_org(ctx: Option<Extension<ApiKeyContext>>) -> Result<Uuid, ApiError> {
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

/// `catalog:` is owned by the dashboard control plane's set-level, one-time
/// owner/admin-confirmed materializer. A tenant API key can manage ordinary
/// routes, but cannot create or remove that namespace through the generic
/// gateway CRUD surface because it carries no corresponding catalog intent.
fn reject_reserved_catalog_name(name: &str) -> ApiResult<()> {
    if is_catalog_route_name(name) {
        return Err(ApiError::RouteValidation {
            issues: vec![RouteValidationIssue {
                field: "name".into(),
                code: "reserved_name".into(),
                message: "names beginning with `catalog:` are reserved for the dashboard catalog enable/repair flow with a fresh owner/admin confirmation".into(),
            }],
        });
    }
    Ok(())
}

/// `GET /v1/routes` — list every caller-org route, including disabled and
/// malformed legacy/manual rows. Runtime routing skips invalid rows, but a
/// management response must expose them with their raw definition and
/// `activation: invalid` so they can be repaired or deleted.
pub async fn list(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Json<Vec<RouteManagementView>>> {
    let org = require_org(ctx)?;
    let routes = store(&state)?
        .list_management_for_org(org)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(routes))
}

/// `POST /v1/routes` — canonical-validate + create.
///
/// The body is received as raw JSON on purpose: a normal `Json<NewRoute>`
/// extractor would deserialize before the handler could report unknown fields
/// or nested type mistakes at an addressable 422 path.
pub async fn create(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Json(raw): Json<serde_json::Value>,
) -> ApiResult<(axum::http::StatusCode, Json<RouteWriteResponse>)> {
    let org = require_org(ctx)?;
    let canonical =
        canonicalize_route_value(raw).map_err(|issues| ApiError::RouteValidation { issues })?;
    let spec = canonical.route;
    reject_reserved_catalog_name(&spec.name)?;
    let registry = state.registry.clone();
    validate_capability(&spec.when, &spec.then, |m| registry.model_info(m).cloned())
        .map_err(|error| route_validation_error("then.target_model", error.to_string()))?;
    // #454: a route whose `shadow_model` cannot resolve to a registered provider
    // is rejected at config time (not silently no-op'd at dispatch).
    validate_shadow_model(&spec.then, |m| registry.resolve(m).is_some())
        .map_err(|error| route_validation_error("then.shadow_model", error.to_string()))?;
    // Task 2: an opt-in `agentic_budget` whose `route_mechanical_to` (Sub-lever
    // 3 down-route target) cannot resolve, or whose `keep_recent_pairs` is 0, is
    // rejected at config time — same fail-at-config discipline as shadow_model
    // (resolve the target) + the C1 blast-radius bound (keep >= 1 recent
    // verbatim). No-op when the route declares no `agentic_budget`.
    validate_agentic_budget(&spec.then, |m| registry.resolve(m).is_some()).map_err(|error| {
        route_validation_error("then.agentic_budget.route_mechanical_to", error.to_string())
    })?;
    let route_store = store(&state)?;
    let created = route_store
        .create_route(org, spec)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // Read-after-write verifies the exact persisted representation that the
    // routing store will expose to the hot path. A successful write must never
    // be reported as active if the database/store changed or skipped a field.
    let stored = route_store
        .get_route(org, created.id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::Internal("route disappeared during activation check".into()))?;
    let activation = canonicalize_route_parts(
        Some(canonical.schema_version),
        stored.name.clone(),
        i32::try_from(stored.priority)
            .map_err(|_| ApiError::Internal("stored route priority exceeds i32".into()))?,
        stored.enabled,
        serde_json::to_value(&stored.when)
            .map_err(|error| ApiError::Internal(format!("route activation conditions: {error}")))?,
        serde_json::to_value(&stored.then)
            .map_err(|error| ApiError::Internal(format!("route activation target: {error}")))?,
    )
    .map_err(|issues| ApiError::Internal(format!("route activation invalid: {issues:?}")))?;
    if activation.canonical_hash != canonical.canonical_hash {
        return Err(ApiError::Internal(
            "route activation hash does not match the canonical write".into(),
        ));
    }
    let state = if stored.enabled { "active" } else { "disabled" };
    Ok((
        axum::http::StatusCode::CREATED,
        Json(RouteWriteResponse {
            route: stored,
            schema_version: canonical.schema_version,
            canonical_hash: canonical.canonical_hash,
            activation: RouteActivation { state },
        }),
    ))
}

fn route_validation_error(field: &str, message: String) -> ApiError {
    ApiError::RouteValidation {
        issues: vec![RouteValidationIssue {
            field: field.into(),
            code: "gateway_resolution_failed".into(),
            message,
        }],
    }
}

/// The successful write response carries the exact schema/hash the gateway
/// checked after storage. `route` is flattened to preserve the original v1
/// response fields for existing callers.
#[derive(Debug, Serialize)]
pub struct RouteWriteResponse {
    #[serde(flatten)]
    pub route: Route,
    pub schema_version: u32,
    pub canonical_hash: String,
    pub activation: RouteActivation,
}

#[derive(Debug, Serialize)]
pub struct RouteActivation {
    pub state: &'static str,
}

/// `GET /v1/routes/:id` — return the same management view as the list.
///
/// A persisted malformed route is still a real, org-owned object: returning a
/// 404 would hide the recovery path and incorrectly imply it never existed.
pub async fn get(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<RouteManagementView>> {
    let org = require_org(ctx)?;
    let route = store(&state)?
        .get_management_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no route with id {id}")))?;
    Ok(Json(route))
}

/// `POST /v1/routes/:id/pause?expected_revision=…` — manual sticky pause: the
/// route keeps matching for attribution but its rewrite (and every other cost
/// lever) is suppressed, so requests flow to the originally-requested model —
/// the EXPENSIVE, quality-safe direction. The observed definition generation
/// is required so a delayed request cannot pause a same-UUID replacement.
/// Pausing an already-paused current generation remains idempotent. Cleared
/// ONLY by [`resume`].
pub async fn pause(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
    Query(q): Query<RouteMutationQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let org = require_org(ctx)?;
    let expected_revision =
        parse_route_mutation_expected_revision(q.expected_revision.as_deref(), "pause")?;
    let store = store(&state)?;
    let paused = store
        .pause_route(
            org,
            id,
            expected_revision,
            NewRoutePause {
                paused_by: PausedBy::Manual,
                reason: "manual".into(),
                pass_rate: None,
                verdicts_in_window: None,
            },
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if paused {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "id": id,
            "paused": true,
            "expected_revision": expected_revision,
        })));
    }
    // A false pause may be the normal already-paused idempotent case, an
    // absent route, or a stale generation. Re-read only after the guarded
    // mutation so the response is actionable; it cannot turn this request
    // destructive.
    let current =
        failed_route_mutation_current(store, org, id, expected_revision, "pausing").await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "paused": current.paused,
        "expected_revision": expected_revision,
    })))
}

/// `POST /v1/routes/:id/resume?expected_revision=…` — THE explicit re-enable;
/// nothing else clears a pause (auto or manual). The observed definition
/// generation is required so a delayed request cannot resume a same-UUID
/// replacement. `was_paused` reports whether a pause row was actually removed
/// (false = the current route wasn't paused; still 200).
pub async fn resume(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
    Query(q): Query<RouteMutationQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let org = require_org(ctx)?;
    let expected_revision =
        parse_route_mutation_expected_revision(q.expected_revision.as_deref(), "resume")?;
    let store = store(&state)?;
    let was_paused = store
        .resume_route(org, id, expected_revision)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if was_paused {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "id": id,
            "paused": false,
            "was_paused": true,
            "expected_revision": expected_revision,
        })));
    }
    let current =
        failed_route_mutation_current(store, org, id, expected_revision, "resuming").await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "paused": current.paused,
        "was_paused": false,
        "expected_revision": expected_revision,
    })))
}

/// Query for [`savings`]: window length in hours (default 720 = 30 days,
/// clamped to `1..=2160`).
#[derive(serde::Deserialize)]
pub struct SavingsQuery {
    pub hours: Option<u32>,
}

/// `GET /v1/routes/:id/savings` response: the route's windowed savings with
/// the measurement tax NETTED and itemized. Legacy `gross_saved_usd` and
/// `net_saved_usd` remain unchanged for compatibility; the additive
/// `net_estimated_savings_usd` preserves row-level regressions and includes
/// recorded cache-bust and summarizer taxes. Both aggregate nets may be
/// negative. When `unmetered_tax_rows > 0` the judge/shadow taxes are lower
/// bounds → the nets are upper bounds.
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
    /// Itemized row-level cache-bust penalty. Already included in both the
    /// legacy gross floor and the signed estimate; do not subtract it again.
    pub cache_bust_usd: f64,
    pub net_saved_usd: f64,
    /// Positive half of the complete signed row estimate.
    pub positive_estimated_savings_usd: f64,
    /// Absolute magnitude of complete row-level regressions.
    pub estimated_regressions_usd: f64,
    /// Signed complete row estimate less judge/shadow taxes. May be negative.
    pub net_estimated_savings_usd: f64,
    /// Itemized auxiliary summarizer cost already included in the signed row
    /// estimate; do not subtract it again.
    pub summarizer_tax_usd: f64,
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
        cache_bust_usd: net.cache_bust_usd,
        net_saved_usd: net.net_saved_usd,
        positive_estimated_savings_usd: net.positive_estimated_savings_usd,
        estimated_regressions_usd: net.estimated_regressions_usd,
        net_estimated_savings_usd: net.net_estimated_savings_usd,
        summarizer_tax_usd: net.summarizer_tax_usd,
        unmetered_tax_rows: net.unmetered_tax_rows,
        verdicts: net.verdicts,
    }))
}

/// Query shared by all single-route destructive/stateful mutations. The
/// catalog set endpoints deliberately do not use this per-row contract.
#[derive(Debug, Deserialize)]
pub struct RouteMutationQuery {
    /// The positive revision returned by the immediately preceding route GET
    /// or list. Omitting it would let a stale browser/CLI apply an old pause,
    /// resume, or delete operation to a newer control-plane generation.
    pub expected_revision: Option<String>,
}

fn parse_route_mutation_expected_revision(raw: Option<&str>, operation: &str) -> ApiResult<i64> {
    let raw = raw.ok_or_else(|| {
        ApiError::InvalidRequest(format!(
            "expected_revision query parameter is required to {operation} a route"
        ))
    })?;
    let revision = raw.parse::<i64>().map_err(|_| {
        ApiError::InvalidRequest("expected_revision must be a positive decimal integer".into())
    })?;
    if revision < 1 {
        return Err(ApiError::InvalidRequest(
            "expected_revision must be a positive decimal integer".into(),
        ));
    }
    Ok(revision)
}

/// A guarded per-route mutation did not change state. Distinguish the safe
/// idempotent case (the exact current generation is still present) from an
/// absent row or a newer generation. The re-read is response-only: the store
/// already rejected the mutation atomically, so a concurrent third writer
/// cannot make this request destructive.
async fn failed_route_mutation_current(
    route_store: &tt_routing::CachingRoutingStore,
    org: Uuid,
    id: Uuid,
    expected_revision: i64,
    operation: &str,
) -> ApiResult<RouteManagementView> {
    let current = route_store
        .get_management_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no route with id {id}")))?;
    match current.revision.filter(|revision| *revision >= 1) {
        Some(current_revision) if current_revision == expected_revision => Ok(current),
        Some(current_revision) => Err(ApiError::Conflict(format!(
            "route revision conflict while {operation}: expected {expected_revision}, current {current_revision}; reload before retrying"
        ))),
        None => Err(ApiError::ServiceUnavailable(
            "route revision evidence is unavailable; refusing a state mutation without it".into(),
        )),
    }
}

/// `DELETE /v1/routes/:id?expected_revision=…`.
///
/// This public gateway path shares the control-plane `routes` table. It must
/// carry the revision observed by the caller so it cannot delete a newer
/// definition written by the dashboard, a plan, or another CLI invocation.
pub async fn delete(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
    Query(q): Query<RouteMutationQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let org = require_org(ctx)?;
    let expected_revision =
        parse_route_mutation_expected_revision(q.expected_revision.as_deref(), "delete")?;
    let route_store = store(&state)?;
    // The control-plane catalog endpoint owns set-level delete/repair
    // coordination. Deleting one catalog row here would create an unbound
    // partial-catalog mutation through an ordinary tenant API key.
    let current = route_store
        .get_management_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no route with id {id}")))?;
    reject_reserved_catalog_name(&current.name)?;
    let removed = route_store
        .delete_route(org, id, expected_revision)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if removed {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "id": id,
            "expected_revision": expected_revision,
        })));
    }

    // The guarded DELETE changes nothing on a false return. Re-read only to
    // make the response actionable; a concurrent third mutation can change
    // this classification but cannot make this stale request destructive.
    match route_store
        .get_management_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    {
        None => Err(ApiError::NotFound(format!("no route with id {id}"))),
        Some(current) => match current.revision {
            Some(current_revision) => Err(ApiError::Conflict(format!(
                "route revision conflict: expected {expected_revision}, current {current_revision}; reload before deleting"
            ))),
            None => Err(ApiError::ServiceUnavailable(
                "route revision evidence is unavailable; refusing to delete without it".into(),
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_gateway_route_writes_reject_catalog_namespace() {
        let error = reject_reserved_catalog_name("catalog:openai->gpt-4o-mini")
            .expect_err("generic gateway route CRUD must not materialize catalog rows");
        assert!(matches!(
            error,
            ApiError::RouteValidation { ref issues }
                if issues.len() == 1
                    && issues[0].field == "name"
                    && issues[0].code == "reserved_name"
        ));
        assert!(reject_reserved_catalog_name("customer-cost-guard").is_ok());
    }
}
