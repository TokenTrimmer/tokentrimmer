//! POST /v1/preview — synchronous cost preview.
//!
//! Mirrors the auth-key middleware applied to /v1/chat/completions. Body is
//! a subset of the chat-completion request; response is `tt_preview::PreviewResponse`.

use axum::{extract::State, http::StatusCode, Json};
use serde_json::json;

use crate::state::AppState;
use tt_preview::PreviewRequest;

pub async fn post_preview(
    State(state): State<AppState>,
    Json(req): Json<PreviewRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mut resp = tt_preview::preview(&req).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    // Enrich the route suggestions' QualityRiskBand hook from the live judge's
    // aggregate band per (requested → served) swap, where one has been scored.
    // No store wired (default) → suggestions stay honestly `Unknown`.
    if let Some(store) = state.judge_band_store.as_ref() {
        store.enrich_suggestions(&req.model, &mut resp.route_suggestions);
    }
    Ok(Json(serde_json::to_value(resp).unwrap()))
}
