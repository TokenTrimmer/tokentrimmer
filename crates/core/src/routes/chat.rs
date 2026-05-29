//! `POST /v1/chat/completions` — OpenAI-compatible chat completion.
//!
//! Dispatch pipeline:
//!   1. Resolve provider from `request.model` via the registry.
//!   2. Build a synthetic [`RequestContext`] (real auth lands in Week 7).
//!   3. For `stream: false` (Week 12 +): try L2 semantic cache lookup; on hit
//!      return the cached response with `X-TokenTrimmer-Cache: hit-l2`.
//!   4. Otherwise dispatch to provider, then best-effort insert into L2 cache.
//!   5. For `stream: true`: dispatch streaming directly (fake-stream from
//!      cache is `w7-fake-stream-cache`, blocked).
//!
//! Deferred:
//!   - Real auth middleware populating `RequestContext` org_id (W7).
//!   - Routing rule engine (W4).
//!   - L1 exact-match lookup (W7 cache middleware).
//!   - Telemetry / audit row write (W7 telemetry pipeline).

use std::time::{Duration, Instant};

use axum::{
    extract::{Extension, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use tt_cache::{key::cache_key, CacheEntry, L1Entry};
use tt_telemetry::request_logs::{RequestLogRow, RequestLogWriter};
use uuid::Uuid;

use tt_auth::ApiKeyContext;
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::Choice,
    ChatCompletionRequest, ChatCompletionResponse, Message, MessageContent, ModelPricing,
    RequestContext, Usage,
};

use crate::{
    middleware::trace::TraceId,
    routes::sse::{self, StreamLogContext},
    state::L2Config,
    ApiError, ApiResult, AppState,
};

/// L2 cache TTL for newly-inserted entries. Spec §8.4 caps this per-tier
/// (24h / 7d / 30d); the gateway-level default is conservative until the
/// auth layer surfaces the caller's tier.
const L2_DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Handler for `POST /v1/chat/completions`.
///
/// Resolves the provider for `req.model`, builds a [`RequestContext`] from the
/// auth-middleware-supplied [`ApiKeyContext`] (when present) and the
/// credential store, then dispatches to either the streaming or non-streaming
/// provider path. Falls back to a synthetic context when auth is not wired
/// (tests, dev) — preserving the legacy "forward the Bearer token verbatim"
/// behaviour so existing integrations keep working.
pub async fn handler(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Json(mut req): Json<ChatCompletionRequest>,
) -> ApiResult<Response> {
    // Wall-clock start — fed into `request_logs.latency_ms`.
    let request_started = Instant::now();

    // 1. Resolve provider — 404 for unknown models. (May be re-resolved after
    //    routing rewrites req.model below.) `resolve` falls back to provider
    //    inference for valid-but-unlisted model ids so they dispatch instead
    //    of 404ing.
    let mut provider =
        state
            .registry
            .resolve(&req.model)
            .ok_or_else(|| ApiError::ModelNotFound {
                model: req.model.clone(),
            })?;

    // 2. Pull api_key from "Authorization: Bearer <key>" if present. This is
    //    the customer's TokenTrimmer key — the auth middleware already
    //    verified it (when configured); we re-read it here only to detect the
    //    sandbox `tt_test_*` short-circuit.
    let raw_bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .unwrap_or("")
        .to_string();

    // 2a. Sandbox short-circuit: `tt_test_*` keys return a deterministic
    //     synthetic response without contacting any real provider — for
    //     E2E test suites and customer integration testing without spend.
    if raw_bearer.starts_with("tt_test_") {
        return Ok(sandbox_response(&req, trace.0.as_str()));
    }

    // Prefer the TraceId extension set by trace middleware; fall back to header
    // (for callers that pre-supply it) or a fresh UUID.
    let trace_id = if !trace.0.is_empty() {
        Uuid::parse_str(&trace.0).unwrap_or_else(|_| Uuid::now_v7())
    } else {
        headers
            .get("x-tokentrimmer-trace-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_else(Uuid::now_v7)
    };

    // 2b. Identity + credentials.
    //
    // If the auth middleware attached an `ApiKeyContext`, use the real
    // org_id/key_id and look up the customer's stored upstream credentials
    // from the credential store. If anything is missing, fall back to the
    // legacy behavior of forwarding the raw Bearer to the upstream provider.
    // That fallback is what keeps `tt_test_*` E2E tests and unauthenticated
    // dev calls working through this handler.
    let (org_id, api_key_id) = match auth_ctx.as_deref() {
        Some(c) => (c.org_id, c.key_id),
        None => (Uuid::nil(), Uuid::nil()),
    };
    let credentials = resolve_credentials(&state, org_id, provider.id(), &raw_bearer).await;

    let ctx = RequestContext {
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

    // 2c. Routing engine. Rewrite `req.model` if the org has a matching
    //     enabled route — must happen BEFORE the cache lookup (so L1 keys
    //     and L2 lookups use the routed model) and BEFORE provider
    //     dispatch. The cache is per-org with a ~60s TTL, so this is cheap.
    //     Unknown-org / no-store / no-match all fall through unchanged.
    // Capture the originally-requested model's pricing BEFORE routing may
    // rewrite `req.model`. `baseline_cost_usd` is priced against this so a
    // downgrade route's saving shows up in saved_usd / request_logs instead
    // of collapsing to ~0 (which happens when baseline is priced against the
    // cheap routed-to model).
    let requested_pricing = provider.pricing(&req.model);
    let matched_route_id = apply_routing(&state, &ctx, &mut req).await;
    if matched_route_id.is_some() {
        // Provider may have changed if the rewrite crossed providers (v1
        // routes are same-provider, but the registry is the source of truth).
        provider = state
            .registry
            .resolve(&req.model)
            .ok_or_else(|| ApiError::ModelNotFound {
                model: req.model.clone(),
            })?;
    }

    // 3. Branch: streaming vs non-streaming.
    if req.stream {
        // 3α. L1 fake-stream — when a streaming request has a cached
        //     response, synthesize an SSE stream from the cached body
        //     instead of dispatching live. The chunk key matches the
        //     non-stream branch's `namespaced_l1_key` so streaming and
        //     non-streaming variants of the same prompt share cache
        //     entries.
        let l1_key = state
            .l1
            .as_ref()
            .map(|_| namespaced_l1_key(ctx.org_id, &req));
        if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key.as_ref()) {
            if let Ok(Some(bytes)) = l1.cache.get(key).await {
                if let Ok(entry) = L1Entry::from_bytes(&bytes) {
                    spawn_request_log(
                        state.request_log_writer.as_ref(),
                        request_log_for_l1_hit(
                            &entry,
                            &ctx,
                            trace_id,
                            request_started,
                            matched_route_id,
                        ),
                    );
                    let fake = sse::fake_stream_from_response(entry.response);
                    // L1 hit already logged above; no need for a second row.
                    return Ok(sse::stream_response(fake, &provider, trace_id, None));
                }
            }
        }

        // No cache hit (or no L1 wired) — dispatch live to the provider.
        // Estimate input tokens from the request messages (byte heuristic: len/4).
        let estimated_input_tokens = req
            .messages
            .iter()
            .map(|m| {
                let text_len = match m {
                    Message::User { content, .. } | Message::System { content } => match content {
                        MessageContent::Text(s) => s.len(),
                        MessageContent::Parts(parts) => parts
                            .iter()
                            .map(|p| match p {
                                tt_shared::ContentPart::Text { text } => text.len(),
                                _ => 0,
                            })
                            .sum(),
                    },
                    Message::Assistant { content, .. } => match content {
                        Some(MessageContent::Text(s)) => s.len(),
                        Some(MessageContent::Parts(parts)) => parts
                            .iter()
                            .map(|p| match p {
                                tt_shared::ContentPart::Text { text } => text.len(),
                                _ => 0,
                            })
                            .sum(),
                        None => 0,
                    },
                    Message::Tool { content, .. } => match content {
                        MessageContent::Text(s) => s.len(),
                        MessageContent::Parts(parts) => parts
                            .iter()
                            .map(|p| match p {
                                tt_shared::ContentPart::Text { text } => text.len(),
                                _ => 0,
                            })
                            .sum(),
                    },
                };
                text_len / 4
            })
            .sum::<usize>() as i32;

        let log_ctx = state.request_log_writer.as_ref().map(|w| StreamLogContext {
            writer: w.clone(),
            org_id: ctx.org_id,
            api_key_id: ctx.api_key_id,
            trace_id,
            provider_id: provider.id().to_string(),
            model: req.model.clone(),
            input_tokens: estimated_input_tokens,
            cached_tokens: 0,
            pricing: provider.pricing(&req.model),
            // Baseline against the originally-requested model when routed, so
            // the streamed request_logs row carries the real routing saving.
            baseline_pricing: if matched_route_id.is_some() {
                requested_pricing.clone()
            } else {
                provider.pricing(&req.model)
            },
            route_id: matched_route_id,
            tag: ctx.tag.clone(),
            request_started,
        });

        let stream = provider.chat_completion_stream(req, &ctx).await?;
        Ok(sse::stream_response(stream, &provider, trace_id, log_ctx))
    } else {
        // 3a. L1 exact-match cache. Cheapest lookup — try first. Best-effort:
        //     any Redis error falls through to L2/provider — we never fail a
        //     user request because the cache is unhealthy.
        let l1_key = state
            .l1
            .as_ref()
            .map(|_| namespaced_l1_key(ctx.org_id, &req));
        if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key.as_ref()) {
            match l1.cache.get(key).await {
                Ok(Some(bytes)) => match L1Entry::from_bytes(&bytes) {
                    Ok(entry) => {
                        // Log the L1 hit before returning. The hit baseline is
                        // either the envelope's own value or the synthetic
                        // fallback for pre-envelope cache rows.
                        spawn_request_log(
                            state.request_log_writer.as_ref(),
                            request_log_for_l1_hit(
                                &entry,
                                &ctx,
                                trace_id,
                                request_started,
                                matched_route_id,
                            ),
                        );
                        return Ok(build_hit_l1_response(entry, trace_id));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, key = %key, "l1 cache entry failed to deserialize");
                    }
                },
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "l1 lookup failed"),
            }
        }

        // 3b. Try L2 semantic cache lookup before dispatching to the provider.
        if let Some(l2) = state.l2.as_ref() {
            if let Some(query_text) = last_user_message_text(&req) {
                if let Ok(query_vec) = l2.embedder.embed(query_text).await {
                    if let Ok(Some((entry, similarity))) =
                        l2.cache.lookup(ctx.org_id, &query_vec, l2.threshold).await
                    {
                        // Cache hit — best-effort bump and return.
                        let _ = l2.cache.bump_hit_count(entry.id).await;
                        spawn_request_log(
                            state.request_log_writer.as_ref(),
                            request_log_for_l2_hit(
                                &entry,
                                &ctx,
                                trace_id,
                                request_started,
                                matched_route_id,
                            ),
                        );
                        return build_hit_l2_response(entry, similarity, trace_id);
                    }
                }
            }
        }

        // 3c. No cache hit — dispatch to provider.
        let response = provider.chat_completion(req.clone(), &ctx).await?;

        // 3d. Compute cost via provider pricing table BEFORE caching — the L1
        //     envelope carries baseline_cost_usd so hit responses can report
        //     accurate savings without re-running pricing later.
        let pricing = provider.pricing(&response.model);
        // Baseline is priced against the originally-requested model when a route
        // rewrote it; otherwise against the served model (same pricing → no
        // routing saving, only cache/discount savings).
        let baseline_pricing = if matched_route_id.is_some() {
            requested_pricing.clone()
        } else {
            pricing.clone()
        };
        let (cost_usd, baseline_cost_usd) = compute_cost(
            &response.usage,
            pricing.as_ref(),
            baseline_pricing.as_ref(),
            provider.fee_multiplier(),
        );
        let saved_usd = (baseline_cost_usd - cost_usd).max(0.0_f64);

        let provider_id = provider.id().to_string();
        let model_used = response.model.clone();

        // 3e. Best-effort L1 insert. Errors are logged but never block the request.
        if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key) {
            let entry = L1Entry::new(
                response.clone(),
                baseline_cost_usd,
                cost_usd,
                provider_id.clone(),
            );
            match entry.to_bytes() {
                Ok(bytes) => {
                    let l1_clone = l1.clone();
                    tokio::spawn(async move {
                        if let Err(e) = l1_clone.cache.set(&key, &bytes, l1_clone.ttl_secs).await {
                            tracing::warn!(error = %e, "l1 cache insert failed");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "l1 envelope serialization failed"),
            }
        }

        // 3f. Best-effort L2 insert. Errors are logged but never block the request.
        if let Some(l2) = state.l2.as_ref() {
            if let Some(query_text) = last_user_message_text(&req) {
                let l2_provider_id = provider_id.clone();
                let l2_model_used = response.model.clone();
                let response_clone = response.clone();
                let l2_clone = l2.clone();
                let org_id = ctx.org_id;
                let query_text_owned = query_text.to_string();
                tokio::spawn(async move {
                    insert_into_l2(
                        l2_clone,
                        org_id,
                        &query_text_owned,
                        response_clone,
                        l2_provider_id,
                        l2_model_used,
                    )
                    .await;
                });
            }
        }

        // 3g. Best-effort request_logs row. Cache-miss path: cached=false,
        //     cache_layer=None. L1/L2-hit paths log their own rows where
        //     they early-return.
        spawn_request_log(
            state.request_log_writer.as_ref(),
            RequestLogRow {
                id: Uuid::now_v7(),
                org_id: ctx.org_id,
                api_key_id: ctx.api_key_id,
                ts: Utc::now(),
                provider: provider_id.clone(),
                model: model_used.clone(),
                input_tokens: response.usage.prompt_tokens as i32,
                output_tokens: response.usage.completion_tokens as i32,
                cached_tokens: response.usage.cached_tokens as i32,
                cost_usd,
                baseline_cost_usd,
                cached: false,
                cache_layer: None,
                route_id: matched_route_id,
                latency_ms: request_started.elapsed().as_millis().min(i32::MAX as u128) as i32,
                upstream_latency_ms: None,
                status: 200,
                tag: ctx.tag.clone(),
                error_class: None,
                trace_id: Some(trace_id.to_string()),
                truncated: false,
            },
        );

        // 5. Serialize body and attach TokenTrimmer extension headers.
        let mut http_response = Json(response).into_response();
        attach_cost_headers(
            http_response.headers_mut(),
            trace_id,
            &provider_id,
            &model_used,
            cost_usd,
            baseline_cost_usd,
            saved_usd,
        );
        // Cache state: miss when ANY cache layer is configured but didn't hit;
        // none when both are disabled.
        let cache_state = if state.l1.is_some() || state.l2.is_some() {
            "miss"
        } else {
            "none"
        };
        if let Ok(v) = cache_state.parse() {
            http_response
                .headers_mut()
                .insert("x-tokentrimmer-cache", v);
        }
        Ok(http_response)
    }
}

/// Per-org namespaced L1 cache key. Prepending `org_id` keeps tenants
/// isolated within a shared Redis instance.
fn namespaced_l1_key(org_id: Uuid, req: &ChatCompletionRequest) -> String {
    format!("{}:{}", org_id, cache_key(req))
}

/// Build the response for an L1 cache hit.
///
/// Cost is always 0 on a hit. Baseline is taken from the envelope (set at
/// insert time when the original miss ran through pricing). Pre-envelope
/// entries surface as `version == 0`; for those we fall back to the
/// conservative synthetic baseline from cached [`Usage`] until they
/// TTL out.
fn build_hit_l1_response(entry: L1Entry, trace_id: Uuid) -> Response {
    let baseline_cost_usd = if entry.is_legacy_format() {
        synthetic_baseline_from_usage(&entry.response.usage)
    } else {
        entry.baseline_cost_usd
    };
    let model_used = entry.response.model.clone();

    let mut http_response = Json(entry.response).into_response();
    attach_cost_headers(
        http_response.headers_mut(),
        trace_id,
        "cache",
        &model_used,
        0.0,
        baseline_cost_usd,
        baseline_cost_usd,
    );
    if let Ok(v) = "hit-l1".parse() {
        http_response
            .headers_mut()
            .insert("x-tokentrimmer-cache", v);
    }
    http_response
}

/// Fallback baseline cost reconstruction from a cached response's [`Usage`].
/// Used only for legacy pre-envelope L1 entries; new inserts carry baseline
/// in the [`L1Entry`] directly.
fn synthetic_baseline_from_usage(usage: &Usage) -> f64 {
    let input = usage.prompt_tokens as f64 * 1.0 / 1_000_000.0;
    let output = usage.completion_tokens as f64 * 2.0 / 1_000_000.0;
    input + output
}

/// Extract the trailing user message's text content for embedding. Returns
/// `None` if the request has no user messages or the last user message is
/// multimodal-only (no text parts).
fn last_user_message_text(req: &ChatCompletionRequest) -> Option<&str> {
    for msg in req.messages.iter().rev() {
        if let Message::User { content, .. } = msg {
            return match content {
                MessageContent::Text(s) => Some(s.as_str()),
                MessageContent::Parts(parts) => parts.iter().find_map(|p| match p {
                    tt_shared::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                }),
            };
        }
    }
    None
}

/// Build the response for an L2 cache hit. Re-deserializes the cached body
/// and stamps the standard X-TokenTrimmer-* headers, with `Cache: hit-l2`.
fn build_hit_l2_response(
    entry: CacheEntry,
    _similarity: f32,
    trace_id: Uuid,
) -> ApiResult<Response> {
    let response: ChatCompletionResponse = serde_json::from_slice(&entry.response)
        .map_err(|e| ApiError::Internal(format!("l2 cache deserialize: {e}")))?;

    // Cost is zero on cache hit (no provider call). Baseline reflects what the
    // request would have cost without our cache.
    // Use the cached response's own pricing-derivable usage to fill baseline —
    // the model field on the entry is the provider model that originally served it.
    let baseline_cost_usd = synthetic_baseline_from_entry(&entry);
    let saved_usd = baseline_cost_usd; // 100% savings on a clean hit

    let provider_id = "cache".to_string();
    let model_used = entry.model.clone();
    let mut http_response = Json(response).into_response();
    attach_cost_headers(
        http_response.headers_mut(),
        trace_id,
        &provider_id,
        &model_used,
        0.0,
        baseline_cost_usd,
        saved_usd,
    );
    if let Ok(v) = "hit-l2".parse() {
        http_response
            .headers_mut()
            .insert("x-tokentrimmer-cache", v);
    }
    Ok(http_response)
}

/// Build a deterministic synthetic response for a `tt_test_*` sandbox key.
///
/// Skips provider dispatch entirely — useful for customer E2E tests that need
/// the full Gateway response shape (headers + body) without burning real LLM
/// tokens. The response body echoes a fixed sentence that includes the
/// requested model name so test assertions can verify routing.
fn sandbox_response(req: &ChatCompletionRequest, trace_id_str: &str) -> Response {
    let response = ChatCompletionResponse {
        id: format!("chatcmpl-sandbox-{}", Uuid::now_v7()),
        object: "chat.completion".into(),
        created: chrono::Utc::now().timestamp(),
        model: req.model.clone(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text(format!(
                    "[sandbox] TokenTrimmer test response for model={}",
                    req.model
                ))),
                tool_calls: vec![],
                name: None,
            },
            finish_reason: Some("stop".into()),
        }],
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 12,
            total_tokens: 22,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
        },
    };

    let trace_id = Uuid::parse_str(trace_id_str).unwrap_or_else(|_| Uuid::now_v7());
    let mut http_response = Json(response).into_response();
    attach_cost_headers(
        http_response.headers_mut(),
        trace_id,
        "sandbox",
        &req.model,
        0.0,
        0.0,
        0.0,
    );
    if let Ok(v) = "sandbox".parse() {
        http_response
            .headers_mut()
            .insert("x-tokentrimmer-cache", v);
    }
    http_response
}

/// Reconstruct a rough baseline cost for a cached entry. We don't have the
/// pricing table for `entry.model` here (no Provider reference), so use a
/// conservative default that produces non-zero savings without claiming more
/// than the cached call would have actually cost. This is a placeholder until
/// the L2 row carries its own original cost in a later schema migration.
fn synthetic_baseline_from_entry(entry: &CacheEntry) -> f64 {
    // Default to $1/M input, $2/M output — within an order of magnitude of
    // most chat models. Conservative; real per-model pricing is wired when
    // the L2 schema gains a `baseline_cost_usd` column.
    let input = entry.input_tokens as f64 * 1.0 / 1_000_000.0;
    let output = entry.output_tokens as f64 * 2.0 / 1_000_000.0;
    input + output
}

/// Background L2 insert. Swallows errors with a tracing log — never blocks
/// the user request.
async fn insert_into_l2(
    l2: L2Config,
    org_id: Uuid,
    query_text: &str,
    response: ChatCompletionResponse,
    _provider_id: String,
    model_used: String,
) {
    let embed = match l2.embedder.embed(query_text).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "l2 embed during insert failed");
            return;
        }
    };
    let response_bytes = match serde_json::to_vec(&response) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "l2 response serialization failed");
            return;
        }
    };
    let now = Utc::now();
    let entry = CacheEntry {
        id: Uuid::now_v7(),
        org_id,
        embedding: embed,
        response: response_bytes,
        model: model_used,
        input_tokens: response.usage.prompt_tokens,
        output_tokens: response.usage.completion_tokens,
        hit_count: 0,
        created_at: now,
        expires_at: now + chrono::Duration::from_std(L2_DEFAULT_TTL).unwrap_or_default(),
    };
    if let Err(e) = l2.cache.insert(entry).await {
        tracing::warn!(error = %e, "l2 cache insert failed");
    }
}

/// Compute `(actual_cost_usd, baseline_cost_usd)` from token usage and pricing.
///
/// `pricing` is the served model's rate; `actual_cost_usd` applies the
/// cached-token discount when `usage.cached_tokens > 0`.
///
/// `baseline_pricing` is the rate the request WOULD have paid without any
/// TokenTrimmer optimisation — i.e. the originally-requested model's rate at
/// full input price with no cache discount. When routing did not rewrite the
/// model, callers pass the same pricing for both so the baseline reflects the
/// served model's pre-discount cost. If `baseline_pricing` is `None`, it
/// falls back to `pricing` (conservative: reports no routing saving).
fn compute_cost(
    usage: &Usage,
    pricing: Option<&ModelPricing>,
    baseline_pricing: Option<&ModelPricing>,
    fee_multiplier: f64,
) -> (f64, f64) {
    let Some(pricing) = pricing else {
        return (0.0, 0.0);
    };

    let cached = usage.cached_tokens.min(usage.prompt_tokens);
    let non_cached_input = usage.prompt_tokens.saturating_sub(cached);

    // Use cached rate when available; fall back to regular input rate.
    let cached_rate = pricing
        .cached_input_per_million
        .unwrap_or(pricing.input_per_million);

    let cost_usd = (non_cached_input as f64) * pricing.input_per_million / 1_000_000.0
        + (cached as f64) * cached_rate / 1_000_000.0
        + (usage.completion_tokens as f64) * pricing.output_per_million / 1_000_000.0;

    // Baseline: full input × input rate + output × output rate (no cache
    // discount), priced against the originally-requested model.
    let baseline_pricing = baseline_pricing.unwrap_or(pricing);
    let baseline_cost_usd = (usage.prompt_tokens as f64) * baseline_pricing.input_per_million
        / 1_000_000.0
        + (usage.completion_tokens as f64) * baseline_pricing.output_per_million / 1_000_000.0;

    // Apply the provider surcharge (e.g. OpenRouter's 5% BYOK fee) to both cost
    // and baseline so saved_usd stays consistent (it scales by the same factor).
    (
        cost_usd * fee_multiplier,
        baseline_cost_usd * fee_multiplier,
    )
}

#[cfg(test)]
mod fee_tests {
    use super::*;

    fn flat_pricing() -> ModelPricing {
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            effective_at: Utc::now(),
        }
    }

    #[test]
    fn fee_multiplier_scales_cost_and_baseline() {
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
        };
        let p = flat_pricing();
        let (cost, base) = compute_cost(&usage, Some(&p), Some(&p), 1.0);
        let (cost_fee, base_fee) = compute_cost(&usage, Some(&p), Some(&p), 1.05);
        // 1M input @ $1/M = $1.00 with no fee.
        assert!((cost - 1.0).abs() < 1e-9, "cost = {cost}");
        // OpenRouter's 5% BYOK fee scales cost and baseline by 1.05.
        assert!((cost_fee - 1.05).abs() < 1e-9, "cost_fee = {cost_fee}");
        assert!(
            (base_fee - base * 1.05).abs() < 1e-12,
            "base_fee = {base_fee}"
        );
    }
}

/// Insert all six required `X-TokenTrimmer-*` response headers.
fn attach_cost_headers(
    headers: &mut axum::http::HeaderMap,
    trace_id: Uuid,
    provider_id: &str,
    model_used: &str,
    cost_usd: f64,
    baseline_cost_usd: f64,
    saved_usd: f64,
) {
    let pairs: &[(&str, String)] = &[
        ("x-tokentrimmer-trace-id", trace_id.to_string()),
        ("x-tokentrimmer-provider", provider_id.to_string()),
        ("x-tokentrimmer-model-used", model_used.to_string()),
        ("x-tokentrimmer-cost-usd", format!("{cost_usd:.6}")),
        (
            "x-tokentrimmer-baseline-cost-usd",
            format!("{baseline_cost_usd:.6}"),
        ),
        ("x-tokentrimmer-saved-usd", format!("{saved_usd:.6}")),
    ];

    for (name, value) in pairs {
        if let Ok(v) = value.parse() {
            headers.insert(*name, v);
        }
    }
}

/// Decide which credentials to send to the upstream provider.
///
/// Precedence:
///
/// 1. The credential store (if configured) — production path: per-org
///    upstream key, possibly with `base_url` / `extra_headers`.
/// 2. The raw Bearer token as a fallback — preserves legacy behavior where
///    customers pointed their OpenAI SDK at our gateway with their own
///    upstream key in the `Authorization` header.
///
/// On a store error we log and fall back to the raw Bearer rather than
/// failing the request: cache and credential lookup are best-effort layers.
async fn resolve_credentials(
    state: &AppState,
    org_id: Uuid,
    provider_id: &str,
    raw_bearer: &str,
) -> ProviderCredentials {
    if let Some(store) = state.credential_store.as_ref() {
        match store.get(org_id, provider_id).await {
            Ok(Some(c)) => return c,
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "credential store lookup failed"),
        }
    }
    ProviderCredentials {
        api_key: SecretString::new(raw_bearer.to_string()),
        base_url: None,
        extra_headers: Vec::new(),
    }
}

/// Fire-and-forget `request_logs` insert. The handler MUST NOT block on
/// this — telemetry rows are best-effort, and a slow DB write would
/// dominate p50.
fn spawn_request_log(writer: Option<&std::sync::Arc<dyn RequestLogWriter>>, row: RequestLogRow) {
    let Some(writer) = writer else { return };
    let writer = writer.clone();
    tokio::spawn(async move {
        if let Err(e) = writer.write(row).await {
            tracing::warn!(error = %e, "request_logs write failed");
        }
    });
}

/// Build the `request_logs` row for an L1 cache hit. The provider id
/// stored in the envelope (e.g. `"openai"`) is preserved so the
/// dashboard's per-provider breakdowns include cache hits; the cache
/// label only surfaces via the `cache_layer` column.
fn request_log_for_l1_hit(
    entry: &L1Entry,
    ctx: &RequestContext,
    trace_id: Uuid,
    request_started: Instant,
    route_id: Option<Uuid>,
) -> RequestLogRow {
    let baseline = if entry.is_legacy_format() {
        // Pre-envelope row — fall back to the conservative synthetic
        // baseline so saved_usd is still non-zero in the aggregate cards.
        synthetic_baseline_from_usage(&entry.response.usage)
    } else {
        entry.baseline_cost_usd
    };
    let provider_id = if entry.provider_id.is_empty() {
        "cache".to_string()
    } else {
        entry.provider_id.clone()
    };
    RequestLogRow {
        id: Uuid::now_v7(),
        org_id: ctx.org_id,
        api_key_id: ctx.api_key_id,
        ts: Utc::now(),
        provider: provider_id,
        model: entry.response.model.clone(),
        input_tokens: entry.response.usage.prompt_tokens as i32,
        output_tokens: entry.response.usage.completion_tokens as i32,
        cached_tokens: entry.response.usage.cached_tokens as i32,
        cost_usd: 0.0,
        baseline_cost_usd: baseline,
        cached: true,
        cache_layer: Some("l1".into()),
        route_id,
        latency_ms: clamp_latency_ms(request_started),
        upstream_latency_ms: None,
        status: 200,
        tag: ctx.tag.clone(),
        error_class: None,
        trace_id: Some(trace_id.to_string()),
        truncated: false,
    }
}

/// Build the `request_logs` row for an L2 cache hit.
fn request_log_for_l2_hit(
    entry: &CacheEntry,
    ctx: &RequestContext,
    trace_id: Uuid,
    request_started: Instant,
    route_id: Option<Uuid>,
) -> RequestLogRow {
    let baseline = synthetic_baseline_from_entry(entry);
    RequestLogRow {
        id: Uuid::now_v7(),
        org_id: ctx.org_id,
        api_key_id: ctx.api_key_id,
        ts: Utc::now(),
        provider: "cache".into(),
        model: entry.model.clone(),
        input_tokens: entry.input_tokens as i32,
        output_tokens: entry.output_tokens as i32,
        cached_tokens: 0,
        cost_usd: 0.0,
        baseline_cost_usd: baseline,
        cached: true,
        cache_layer: Some("l2".into()),
        route_id,
        latency_ms: clamp_latency_ms(request_started),
        upstream_latency_ms: None,
        status: 200,
        tag: ctx.tag.clone(),
        error_class: None,
        trace_id: Some(trace_id.to_string()),
        truncated: false,
    }
}

fn clamp_latency_ms(started: Instant) -> i32 {
    started.elapsed().as_millis().min(i32::MAX as u128) as i32
}

/// Look up the org's routing engine (cached ~60s) and evaluate it against
/// the incoming request. On a match, rewrites `req.model` in place and
/// returns the matched route id so callers can stamp it on the request_logs
/// row. Returns `None` (and does not modify `req`) when:
///
/// - no routing store is configured (dev / free tier),
/// - the request has no resolvable org (synthetic context),
/// - the backend errors (we log + fall through — never fail user traffic),
/// - or no enabled route matches.
async fn apply_routing(
    state: &AppState,
    ctx: &RequestContext,
    req: &mut ChatCompletionRequest,
) -> Option<Uuid> {
    let store = state.routing_store.as_ref()?;
    if ctx.org_id == Uuid::nil() {
        return None;
    }

    let engine = match store.engine_for(ctx.org_id).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, org_id = %ctx.org_id, "routing store lookup failed — passing request through unrouted");
            return None;
        }
    };

    // Cheap input-tokens estimate: len(last user text)/4, clamped to u32.
    // The engine docs explicitly leave tokenization to callers; this
    // heuristic matches what we use elsewhere on the hot path.
    let input_tokens = last_user_message_text(req)
        .map(|s| (s.len() / 4).min(u32::MAX as usize) as u32)
        .unwrap_or(0);

    let m = engine.evaluate(req, ctx, input_tokens)?;
    let original = std::mem::replace(&mut req.model, m.then.target_model.clone());
    tracing::debug!(
        org_id = %ctx.org_id,
        route_id = %m.id,
        from = %original,
        to = %req.model,
        "routing rewrite"
    );
    Some(m.id)
}
