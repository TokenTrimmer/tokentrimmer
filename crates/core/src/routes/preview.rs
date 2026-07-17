//! POST /v1/preview — synchronous cost preview.
//!
//! Mirrors the auth-key middleware applied to /v1/chat/completions. Body is
//! a subset of the chat-completion request; response is `tt_preview::PreviewResponse`.
//!
//! ## Panel dry-run (Task 7)
//!
//! When `X-TokenTrimmer-Panel` is present, append a `panel` object to the
//! response with per-member + arbiter cost estimates computed from the panel
//! config.  **No provider is ever dispatched** — this endpoint remains
//! side-effect-free.
//!
//! The panel estimate uses `AppState.registry` for model→provider resolution
//! and per-provider pricing, while the base preview (`tt_preview::preview`)
//! uses its own internal pricing catalog.  The two are deliberately decoupled
//! so the preview can work without the AppState registry (the existing behavior
//! for non-panel requests is byte-identical to before).
//!
//! `panel.within_budget` is therefore an indicative comparison within this
//! dry-run only. It is not Fusion admission, a credential/provider-health or
//! latency check, a reservation, or a runtime spending ceiling; the additive
//! `estimate_evidence` object makes that boundary machine-readable.
//!
//! ### tt_extras.panel
//! `PreviewRequest` now accepts a `tt_extras` map. When `tt_extras.panel` is
//! present, its member list / arbiter override is used (same as the live chat
//! path); otherwise panel members / arbiter come from `PanelDefaults::from_env`.

use axum::{extract::State, http::HeaderMap, http::StatusCode, Json};
use serde_json::json;
use tt_shared::messages::parse_panel_extras;

use crate::{
    routes::chat::{cost_limit_from_header, estimate_cost_usd},
    routes::panel::{panel_from_header, PanelConfig, PanelDefaults},
    state::AppState,
};
use tt_preview::PreviewRequest;

pub async fn post_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
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

    // ── Panel dry-run (Task 7) ───────────────────────────────────────────────
    // When the request carries a panel trigger header, compute a per-leg cost
    // estimate WITHOUT dispatching anything. Reuse the same token counts
    // already computed by tt_preview::preview above for consistency.
    let mut base = serde_json::to_value(resp).unwrap();

    if let Some(strategy) = panel_from_header(&headers) {
        // Resolve per-request panel extras from tt_extras.panel (if present).
        let panel_extras = parse_panel_extras(&req.tt_extras);

        // Resolve the full panel config (header strategy + extras + env defaults).
        let defaults = PanelDefaults::from_env();
        let cfg = match PanelConfig::resolve(strategy, panel_extras.as_ref(), &defaults) {
            Ok(c) => c,
            Err(e) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": e.to_string() })),
                ))
            }
        };

        // Reuse the input-token estimate from the base preview response; it was
        // computed with the same tt_preview token estimator for consistency.
        let input_tokens = base["current"]["input_tokens_estimated"]
            .as_u64()
            .unwrap_or(0) as u32;
        let max_tokens = req.max_tokens;

        // Cost-limit ceiling from header (for within_budget).
        let ceiling = cost_limit_from_header(&headers);

        // Per-member cost estimates (None if model is unknown/unpriced in registry).
        let member_estimates: Vec<Option<f64>> = cfg
            .members
            .iter()
            .map(|m| {
                let provider = state.registry.resolve(&m.model)?;
                let pricing = provider.pricing(&m.model)?;
                Some(
                    estimate_cost_usd(&pricing, input_tokens, max_tokens)
                        * provider.fee_multiplier(),
                )
            })
            .collect();

        // Arbiter cost estimate.
        let arbiter_estimate: Option<f64> = {
            state
                .registry
                .resolve(&cfg.arbiter_model.model)
                .and_then(|p| {
                    p.pricing(&cfg.arbiter_model.model).map(|pricing| {
                        estimate_cost_usd(&pricing, input_tokens, max_tokens) * p.fee_multiplier()
                    })
                })
        };

        // Total: fail-closed — None if ANY leg (member or arbiter) is unpriceable.
        let any_null = member_estimates.iter().any(|e| e.is_none()) || arbiter_estimate.is_none();
        let total_estimated_cost_usd: Option<f64> = if any_null {
            None
        } else {
            let sum: f64 = member_estimates.iter().filter_map(|e| *e).sum::<f64>()
                + arbiter_estimate.unwrap_or(0.0);
            Some(sum)
        };

        // within_budget: None when no ceiling supplied; false when total is None
        // (fail-closed) or when total exceeds ceiling.
        let within_budget: Option<bool> = ceiling.map(|c| {
            total_estimated_cost_usd.map(|t| t <= c).unwrap_or(false) // unpriceable ⇒ false (fail-closed)
        });

        // Build the panel estimate JSON object.
        let members_json: Vec<serde_json::Value> = cfg
            .members
            .iter()
            .zip(member_estimates.iter())
            .map(|(m, est)| {
                let provider_id = state
                    .registry
                    .resolve(&m.model)
                    .map(|p| p.id().to_string())
                    .unwrap_or_default();
                json!({
                    "model": m.model,
                    "provider": provider_id,
                    "estimated_cost_usd": est,
                })
            })
            .collect();

        let arbiter_provider_id = state
            .registry
            .resolve(&cfg.arbiter_model.model)
            .map(|p| p.id().to_string())
            .unwrap_or_default();

        let panel_obj = json!({
            "strategy": strategy.as_str(),
            "members": members_json,
            "arbiter": {
                "model": cfg.arbiter_model.model,
                "provider": arbiter_provider_id,
                "estimated_cost_usd": arbiter_estimate,
            },
            "total_estimated_cost_usd": total_estimated_cost_usd,
            "within_budget": within_budget,
            "estimate_evidence": {
                "scope": "preview_only",
                "reason": "This catalog-only dry-run does not execute Fusion admission. within_budget is not a credential, provider-health, or latency check; it is not a cost reservation, runtime spend ceiling, or execution guarantee."
            },
        });

        // Merge the panel object into the base response at the top level.
        if let Some(obj) = base.as_object_mut() {
            obj.insert("panel".to_string(), panel_obj);
        }
    }

    Ok(Json(base))
}
