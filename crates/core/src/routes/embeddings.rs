//! `POST /v1/embeddings` — OpenAI-compatible embeddings with routing + cost.
//!
//! Mirrors the chat handler's non-streaming routed dispatch (minus cache,
//! streaming, and failover). Routing is evaluated against a synthetic chat
//! request built from the embedding input, then the rewritten model is copied
//! back onto the embeddings request.

use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use uuid::Uuid;

use tt_auth::ApiKeyContext;
// `EmbeddingInput` is only exported from `tt_shared::messages` (not the crate root).
use tt_shared::messages::{EmbeddingInput, Message, MessageContent};
use tt_shared::{ChatCompletionRequest, EmbeddingsRequest, RequestContext};

use crate::middleware::trace::TraceId;
use crate::routes::chat::{
    apply_routing, attach_cost_headers, compute_cost, estimate_cost_usd, resolve_credentials,
    resolve_credentials_for,
};
use crate::{ApiError, ApiResult, AppState};

/// Flatten the embedding input to text for routing evaluation only (token
/// estimate + prompt-contains). A batch joins on newlines.
fn input_as_text(input: &EmbeddingInput) -> String {
    match input {
        EmbeddingInput::Single(s) => s.clone(),
        EmbeddingInput::Batch(v) => v.join("\n"),
    }
}

pub async fn handler(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Json(mut req): Json<EmbeddingsRequest>,
) -> ApiResult<Response> {
    // 1. Resolve provider (re-resolved after routing may rewrite the model).
    let mut provider = state
        .registry
        .resolve(&req.model)
        .ok_or_else(|| ApiError::ModelNotFound {
            model: req.model.clone(),
        })?;

    // 2. Bearer + trace id.
    let raw_bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .unwrap_or("")
        .to_string();
    let trace_id = if !trace.0.is_empty() {
        Uuid::parse_str(&trace.0).unwrap_or_else(|_| Uuid::now_v7())
    } else {
        Uuid::now_v7()
    };

    // 3. Identity + credentials (embeddings aren't cached, so no caller_tier/L2).
    let (org_id, api_key_id) = match auth_ctx.as_deref() {
        Some(c) => (c.org_id, c.key_id),
        None => (Uuid::nil(), Uuid::nil()),
    };
    let source_provider_id = provider.id().to_string();
    let credentials = resolve_credentials(&state, org_id, provider.id(), &raw_bearer).await;
    let mut ctx = RequestContext {
        trace_id,
        org_id,
        api_key_id,
        credentials,
        tag: headers
            .get("x-tokentrimmer-tag")
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        deadline: None,
    };

    // 4. Baseline pricing on the ORIGINAL model, before routing rewrites it.
    let requested_pricing = provider.pricing(&req.model);

    // 5. Routing via a synthetic chat request (model + input text; no modality).
    let mut synth = ChatCompletionRequest {
        model: req.model.clone(),
        messages: vec![Message::User {
            content: MessageContent::Text(input_as_text(&req.input)),
            name: None,
        }],
        ..Default::default()
    };
    let route_match = apply_routing(&state, &ctx, &mut synth).await;
    req.model = synth.model; // adopt the routed model
    let matched = route_match.is_some();
    if matched {
        provider = state
            .registry
            .resolve(&req.model)
            .ok_or_else(|| ApiError::ModelNotFound {
                model: req.model.clone(),
            })?;
        // Cross-provider rewrite: re-resolve target credentials, fail closed.
        if provider.id() != source_provider_id {
            match resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, false).await {
                Some(c) => ctx.credentials = c,
                None => {
                    return Err(ApiError::MissingProviderCredential {
                        provider: provider.id().to_string(),
                    })
                }
            }
        }
        // Post-rewrite cost ceiling (V3d-2b). Output tokens are 0 for embeddings.
        if let Some(ceiling) = route_match.as_ref().and_then(|m| m.max_cost_usd) {
            if let Some(pr) = provider.pricing(&req.model) {
                let tokens = route_match
                    .as_ref()
                    .map(|m| m.input_tokens_estimate)
                    .unwrap_or(0);
                let routed_cost = estimate_cost_usd(&pr, tokens, None);
                if routed_cost > ceiling {
                    return Err(ApiError::CostLimitExceeded {
                        estimated_usd: routed_cost,
                        ceiling_usd: ceiling,
                    });
                }
            }
        }
    }

    // 6. Dispatch. Capture the served model + its pricing before `req` moves.
    let served_model = req.model.clone();
    let routed_pricing = provider.pricing(&served_model);
    let resp = provider.embeddings(req, &ctx).await?;

    // 7. Cost + headers + spend. Baseline against the original model when routed.
    let baseline_pricing = if matched {
        requested_pricing
    } else {
        routed_pricing.clone()
    };
    let (cost_usd, baseline_cost_usd) = compute_cost(
        &resp.usage,
        routed_pricing.as_ref(),
        baseline_pricing.as_ref(),
        provider.fee_multiplier(),
    );
    let saved_usd = (baseline_cost_usd - cost_usd).max(0.0_f64);
    state.spend_sink().record(org_id, cost_usd, Utc::now());

    let mut http = (StatusCode::OK, Json(resp)).into_response();
    attach_cost_headers(
        http.headers_mut(),
        trace_id,
        provider.id(),
        &served_model,
        cost_usd,
        baseline_cost_usd,
        saved_usd,
    );
    Ok(http)
}
