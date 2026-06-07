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
// `EmbeddingInput`/`EmbeddingData` are only exported from `tt_shared::messages`.
use tt_shared::messages::{EmbeddingData, EmbeddingInput, Message, MessageContent};
use tt_shared::{
    ChatCompletionRequest, EmbeddingsRequest, EmbeddingsResponse, RequestContext, Usage,
};

use crate::middleware::trace::TraceId;
use crate::routes::chat::{
    apply_provider_override, apply_routing, attach_cost_headers, compute_cost,
    cost_limit_from_header, enforce_cost_limit, estimate_cost_usd, provider_override_from_header,
    resolve_credentials, resolve_credentials_for, timeout_ms_from_header, with_request_timeout,
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

/// Deterministic synthetic embeddings for `tt_test_*` sandbox keys — no provider
/// call, zero cost. One small fixed vector per input item. Honors the documented
/// sandbox contract (docs/04-gateway-api-reference.md: "Embeddings return
/// deterministic vectors"); mirrors the chat handler's `sandbox_response`.
fn sandbox_embeddings_response(req: &EmbeddingsRequest, trace_id: Uuid) -> Response {
    let n = match &req.input {
        EmbeddingInput::Single(_) => 1,
        EmbeddingInput::Batch(v) => v.len(),
    };
    let data: Vec<EmbeddingData> = (0..n)
        .map(|i| EmbeddingData {
            object: "embedding".into(),
            index: u32::try_from(i).unwrap_or(0),
            embedding: vec![0.0, 0.1, 0.2, 0.3],
        })
        .collect();
    let response = EmbeddingsResponse {
        object: "list".into(),
        data,
        model: req.model.clone(),
        usage: Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
        },
    };
    let mut http = Json(response).into_response();
    attach_cost_headers(
        http.headers_mut(),
        trace_id,
        "sandbox",
        &req.model,
        0.0,
        0.0,
        0.0,
    );
    if let Ok(v) = "sandbox".parse() {
        http.headers_mut().insert("x-tokentrimmer-cache", v);
    }
    http
}

pub async fn handler(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Json(mut req): Json<EmbeddingsRequest>,
) -> ApiResult<Response> {
    // 1. Resolve provider (re-resolved after routing may rewrite the model).
    let mut provider =
        state
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

    // 2a. Sandbox short-circuit: `tt_test_*` keys return deterministic synthetic
    //     embeddings without contacting any real provider (mirrors chat.rs).
    if raw_bearer.starts_with("tt_test_") {
        return Ok(sandbox_embeddings_response(&req, trace_id));
    }

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
        deadline: timeout_ms_from_header(&headers).map(std::time::Duration::from_millis),
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
    let route_match = apply_routing(&state, &ctx, &mut synth, None).await?;
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

    // Explicit provider pin (X-TokenTrimmer-Provider) — overrides the
    // routed/inferred provider; the routed model is kept. See chat.rs.
    let provider_pin = provider_override_from_header(&headers);
    let (pinned_provider, pin_creds) = apply_provider_override(
        &state,
        provider_pin.as_deref(),
        org_id,
        &raw_bearer,
        &source_provider_id,
        provider,
    )
    .await?;
    provider = pinned_provider;
    if let Some(c) = pin_creds {
        ctx.credentials = c;
    }

    // Per-request cost ceiling from the `X-TokenTrimmer-Cost-Limit-Usd` header,
    // priced on the final (post-routing) embedding model. Output tokens are 0.
    {
        let cl_input_tokens =
            tt_tokenize::estimate_tokens(provider.id(), &input_as_text(&req.input));
        enforce_cost_limit(
            cost_limit_from_header(&headers),
            provider.pricing(&req.model).as_ref(),
            cl_input_tokens,
            None,
        )?;
    }

    // 6. Dispatch. Capture the served model + its pricing before `req` moves.
    let served_model = req.model.clone();
    let routed_pricing = provider.pricing(&served_model);
    let resp = with_request_timeout(ctx.deadline, async {
        let __started = std::time::Instant::now();
        let __emb = provider.embeddings(req, &ctx).await;
        crate::metrics::record_provider_latency(provider.id(), "embeddings", __started.elapsed());
        __emb.map_err(ApiError::from)
    })
    .await?;

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
