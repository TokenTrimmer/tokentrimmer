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
    cost_limit_from_header, enforce_cost_limit, provider_override_from_header, resolve_credentials,
    resolve_credentials_for, timeout_ms_from_header, with_request_timeout, CostBreakdown,
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
            cache_read_input_tokens: None,
        },
    };
    let mut http = Json(response).into_response();
    attach_cost_headers(
        http.headers_mut(),
        trace_id,
        "sandbox",
        &req.model,
        &CostBreakdown::default(),
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
    // BYO-only (P0 #9): `None` means a VERIFIED org has no stored credential
    // for the source provider. Deferred like chat.rs — routing below may
    // rewrite to a provider the org HAS onboarded (the cross-provider
    // re-resolve fails closed on its own); the guard after the pin block
    // errors only when the serving provider still needs the missing source
    // credential. Until then the raw bearer is an inert placeholder.
    let resolved_source_creds =
        resolve_credentials(&state, org_id, provider.id(), &raw_bearer).await;
    let source_creds_missing = resolved_source_creds.is_none();
    let credentials =
        resolved_source_creds.unwrap_or_else(|| tt_shared::context::ProviderCredentials {
            api_key: tt_shared::context::SecretString::new(raw_bearer.clone()),
            base_url: None,
            extra_headers: Vec::new(),
        });
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
        run_id: None,
        node_id: None,
    };

    // 4. Baseline pricing on the ORIGINAL model, before routing rewrites it.
    // Capture the requested model too, for `gen_ai.request.model` on the span.
    let requested_model = req.model.clone();
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
    // Keep the route's original matcher estimate for its per-request ceiling.
    // It is evaluated only after an optional provider pin settles the actual
    // serving provider below.
    let route_max_cost_usd = route_match.as_ref().and_then(|m| m.max_cost_usd);
    let route_input_tokens = route_match
        .as_ref()
        .map(|m| m.input_tokens_estimate)
        .unwrap_or(0);
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

    // A pin can select a different provider and therefore a different price
    // for the routed model. Preserve the route ceiling's existing matched-route
    // scope, but apply it only after that final provider selection. Output
    // tokens are zero for embeddings; unknown pricing remains permissive.
    if matched {
        enforce_cost_limit(
            route_max_cost_usd,
            provider.pricing(&req.model).as_ref(),
            route_input_tokens,
            None,
        )?;
    }

    // BYO-only (P0 #9): same dispatch-time guard as chat.rs — the serving
    // provider is still the source provider and the verified org has no
    // stored credential for it → actionable 400 instead of forwarding the
    // org's TokenTrimmer key upstream. Cross-provider rewrites and pins
    // re-resolve + fail closed above; anonymous / no-store callers never set
    // `source_creds_missing`.
    if source_creds_missing && provider_pin.is_none() && provider.id() == source_provider_id {
        return Err(ApiError::MissingProviderCredential {
            provider: source_provider_id.clone(),
        });
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
    let __primary = provider.id();
    let __emb_outcome = with_request_timeout(ctx.deadline, async {
        let __started = std::time::Instant::now();
        let __emb = provider.embeddings(req, &ctx).await;
        crate::metrics::record_provider_latency(provider.id(), "embeddings", __started.elapsed());
        __emb.map_err(ApiError::from)
    })
    .await;
    if matches!(__emb_outcome, Err(ApiError::RequestTimeout { .. })) {
        crate::metrics::record_provider_timeout(__primary, "embeddings");
    }
    let resp = __emb_outcome?;

    // 7. Cost + headers + spend. Baseline against the original model when routed.
    let baseline_pricing = if matched {
        requested_pricing
    } else {
        routed_pricing.clone()
    };
    // Header saved_usd is TT-attributed only; the provider's automatic cache
    // discount (always 0 for embeddings today — no cached tokens) rides in
    // its own header.
    let breakdown = compute_cost(
        &resp.usage,
        routed_pricing.as_ref(),
        baseline_pricing.as_ref(),
        provider.fee_multiplier(),
    );
    state
        .spend_sink()
        .record(org_id, api_key_id, breakdown.cost_usd, Utc::now());
    // P0-1/P0-3: settle the served request. Embeddings always dispatch to the
    // provider (no cache-hit short-circuit), so this is a non-cached settle —
    // advances both the billed and served counters.
    state
        .spend_sink()
        .settle(org_id, api_key_id, false, Utc::now());

    // OTel GenAI semconv + TokenTrimmer cost span attributes, mirroring the
    // chat path so embeddings traffic shows up in the same spend/savings/tokens
    // dashboards. `operation = embeddings`; output tokens are 0 for embeddings.
    // Pulls from the same `breakdown` + `resp.usage` already computed above —
    // nothing recomputed. Captured before `resp` moves into the JSON body.
    let span_input_tokens = resp.usage.prompt_tokens;
    let span_output_tokens = resp.usage.completion_tokens;
    let route_matched_name = route_match.as_ref().map(|m| m.route_name.clone());
    tt_telemetry::gen_ai::record_request_attributes(
        &tracing::Span::current(),
        &tt_telemetry::gen_ai::RequestSpanAttributes {
            provider_id: provider.id(),
            request_model: &requested_model,
            response_model: &served_model,
            operation: "embeddings",
            cost: tt_telemetry::gen_ai::RequestSpanCost {
                input_tokens: span_input_tokens,
                output_tokens: span_output_tokens,
                cost_usd: breakdown.cost_usd,
                baseline_cost_usd: breakdown.baseline_cost_usd,
                saved_usd: breakdown.tt_saved_usd(),
                provider_cache_saved_usd: breakdown.provider_cache_saved_usd,
            },
            // Embeddings are never cache-served today; mark the outcome `none`
            // so the cache-hit-rate denominator can include them consistently.
            cache_outcome: Some("none"),
            route: route_matched_name.as_deref(),
            // Canary traffic-split + shadow mode + panels are chat-completions-only;
            // the embeddings path never sets them.
            traffic_split_pct: None,
            shadow_model: None,
            shadow_cost_usd: None,
            panel_strategy: None,
            panel_leg_count: None,
            panel_quorum_required: None,
            panel_quorum_met: None,
        },
    );

    // P2: synchronous served-counter bump, in-band, once per served embeddings
    // response. Embeddings always dispatch (no cache short-circuit) and write no
    // `request_logs` row today, so this is the sole sync served-truth for the
    // path — labelled `dispatch` for consistency with the chat/sse paths.
    crate::metrics::record_request_served("embeddings", "dispatch");

    let mut http = (StatusCode::OK, Json(resp)).into_response();
    attach_cost_headers(
        http.headers_mut(),
        trace_id,
        provider.id(),
        &served_model,
        &breakdown,
    );
    Ok(http)
}
