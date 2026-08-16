//! Request translation for the OpenAI adapter.
//!
//! Because the canonical wire format is OpenAI-compatible, translation is
//! minimal:
//!
//! 1. Strip `tt_extras` (unknown fields that OpenAI rejects).
//! 2. For reasoning models (`o3`, `o4-mini`): rename `max_tokens` →
//!    `max_completion_tokens` and drop `temperature` with a warning.
//! 3. Extract `usage.prompt_tokens_details.cached_tokens` from responses into
//!    [`tt_shared::Usage::cached_tokens`].

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tt_shared::{
    messages::{Message, ResponseFormat, Tool, ToolChoice},
    usage::Usage,
    ProviderError,
};

/// True for OpenAI reasoning models (`o3`, `o4-mini`, …), which take
/// `max_completion_tokens` instead of `max_tokens` and reject `temperature`.
/// This is part of the OpenAI wire request shape, so it lives in the compat
/// translation layer rather than in any provider's pricing table.
pub fn is_reasoning_model(model: &str) -> bool {
    model.starts_with("o3") || model.starts_with("o4") || model.starts_with("o1")
}

/// Params the compat layer silently drops for `req`. Reasoning models
/// (`o3`/`o4-mini`) reject `temperature` (see [`translate_request`]).
pub fn dropped_params(req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
    if is_reasoning_model(&req.model) && req.temperature.is_some() {
        vec!["temperature".to_string()]
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Outbound request
// ---------------------------------------------------------------------------

/// OpenAI-shaped request body that is safe to POST.
///
/// This mirrors [`tt_shared::ChatCompletionRequest`] but omits `tt_extras` and
/// adds the `max_completion_tokens` field needed by reasoning models.
#[derive(Debug, Serialize)]
pub struct OpenAiRequestBody {
    pub model: String,
    pub messages: Vec<Message>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Used for non-reasoning models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Used for reasoning models (o3, o4-mini).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// OpenAI `stream_options` object (e.g. `{ "include_usage": true }`),
    /// forwarded verbatim. The streaming path overrides this to request usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<serde_json::Value>,
    /// Genuinely-unknown / newer OpenAI fields, flattened back to the top level
    /// so they passthrough instead of being dropped.
    #[serde(flatten, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub extra: std::collections::HashMap<String, Value>,
}

/// Translate a canonical [`tt_shared::ChatCompletionRequest`] into an
/// [`OpenAiRequestBody`] ready to serialize and POST.
///
/// This strips `tt_extras` and applies reasoning-model parameter constraints.
pub fn translate_request(
    req: tt_shared::ChatCompletionRequest,
) -> Result<OpenAiRequestBody, ProviderError> {
    let reasoning = is_reasoning_model(&req.model);

    // An explicit caller-supplied `max_completion_tokens` is always authoritative
    // (it is the spend cap). For reasoning models we also rename the legacy
    // `max_tokens` → `max_completion_tokens`, but only when the explicit field is
    // absent. Non-reasoning models keep `max_tokens` and still forward an
    // explicit `max_completion_tokens` verbatim.
    let (max_tokens, max_completion_tokens, temperature) = if reasoning {
        if req.temperature.is_some() {
            tracing::warn!(
                model = %req.model,
                "reasoning models do not support temperature; dropping the field"
            );
        }
        let mct = req.max_completion_tokens.or(req.max_tokens);
        (None, mct, None)
    } else {
        (req.max_tokens, req.max_completion_tokens, req.temperature)
    };

    Ok(OpenAiRequestBody {
        model: req.model,
        messages: req.messages,
        temperature,
        top_p: req.top_p,
        max_tokens,
        max_completion_tokens,
        stream: req.stream,
        tools: req.tools,
        tool_choice: req.tool_choice,
        response_format: req.response_format,
        stop: req.stop,
        presence_penalty: req.presence_penalty,
        frequency_penalty: req.frequency_penalty,
        n: req.n,
        seed: req.seed,
        user: req.user,
        parallel_tool_calls: req.parallel_tool_calls,
        reasoning_effort: req.reasoning_effort,
        stream_options: req.stream_options,
        extra: req.extra,
        // tt_extras is intentionally not forwarded.
    })
}

// ---------------------------------------------------------------------------
// Inbound response — usage extraction
// ---------------------------------------------------------------------------

/// OpenAI usage block as returned in a chat-completion response (or streamed
/// usage chunk — the streaming path reuses this shape).
///
/// We deserialize it separately from the top-level response so that we can
/// pull out `prompt_tokens_details.cached_tokens` before constructing the
/// canonical [`Usage`]. Already-canonical fields (top-level `cached_tokens` /
/// `cache_read_input_tokens` / `cache_creation_input_tokens`, e.g. from a
/// TokenTrimmer hop or our own fake-stream) pass through so chained
/// deployments keep their cache telemetry and cached-rate pricing.
#[derive(Debug, Deserialize)]
pub struct OpenAiUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, deserialize_with = "lenient_prompt_tokens_details")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    // Passthrough for already-canonical usage shapes (TT hops, fake-streams).
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
}

/// OpenAI `prompt_tokens_details` sub-object.
///
/// `cached_tokens` is `Option` so the NULL-vs-0 telemetry distinction holds at
/// key granularity: a details object *without* a `cached_tokens` key (e.g. one
/// carrying only `audio_tokens`) — or with an explicit `null` — means "provider
/// did not report cache reads" (`None` → SQL NULL), not "reported zero".
#[derive(Debug, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// Deserialize `prompt_tokens_details` leniently: a malformed value (string,
/// array, number — anything non-object) or a non-integer `cached_tokens`
/// degrades to "unreported" (`None`) instead of failing the whole usage
/// parse. A usage-block oddity from a nonconforming OpenAI-compat shim must
/// never error a response — and on streams a usage parse failure would
/// inject an error frame into an otherwise-healthy stream and lose the
/// terminal usage. Degrading is the conservative direction for the ledger:
/// the cached prompt is priced at the full input rate, never a fabricated
/// saving.
fn lenient_prompt_tokens_details<'de, D>(
    deserializer: D,
) -> Result<Option<PromptTokensDetails>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(deserializer)?;
    Ok(match v {
        Some(Value::Object(map)) => Some(PromptTokensDetails {
            cached_tokens: map.get("cached_tokens").and_then(lenient_token_count),
        }),
        _ => None,
    })
}

/// Accept a JSON number as a token count when it is a non-negative integer —
/// including integral floats like `80.0` from shims whose JSON encoders
/// float-ify integers. Anything else (negative, fractional, non-number) is
/// "unreported".
fn lenient_token_count(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| {
        v.as_f64()
            .filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= u64::MAX as f64)
            .map(|f| f as u64)
    })
}

impl From<OpenAiUsage> for Usage {
    fn from(u: OpenAiUsage) -> Self {
        // Keep the raw Option-ness: `None` = OpenAI reported no
        // prompt_tokens_details at all (or no cached_tokens key inside it),
        // distinct from a reported zero. The wire detail wins over the
        // canonical passthrough when both are present (mirrors the streaming
        // path; all real providers report exactly one shape).
        let detail = u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens);
        Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
            cached_tokens: detail.unwrap_or(u.cached_tokens),
            // OpenAI-wire providers never report cache writes; only an
            // already-canonical upstream (e.g. a TokenTrimmer hop fronting
            // Anthropic) populates this via passthrough.
            cache_creation_input_tokens: u.cache_creation_input_tokens,
            cache_read_input_tokens: detail.or(u.cache_read_input_tokens),
        }
    }
}

/// Extract the `usage` field from a raw OpenAI response JSON value and convert
/// it to the canonical [`Usage`] type.
pub fn extract_usage(raw: &Value) -> Result<Usage, ProviderError> {
    let usage_val = raw
        .get("usage")
        .ok_or_else(|| ProviderError::Deserialize("missing 'usage' field".to_string()))?;
    let openai_usage: OpenAiUsage = serde_json::from_value(usage_val.clone())
        .map_err(|e| ProviderError::Deserialize(e.to_string()))?;
    Ok(openai_usage.into())
}

// ---------------------------------------------------------------------------
// Full response deserialization with usage fixup
// ---------------------------------------------------------------------------

/// Deserialize a raw OpenAI JSON response into a [`tt_shared::ChatCompletionResponse`],
/// with `usage.cached_tokens` populated from `prompt_tokens_details.cached_tokens`.
pub fn deserialize_response(
    body: &str,
) -> Result<tt_shared::ChatCompletionResponse, ProviderError> {
    // Parse the raw JSON so we can extract the usage separately.
    let raw: Value =
        serde_json::from_str(body).map_err(|e| ProviderError::Deserialize(e.to_string()))?;

    let canonical_usage = extract_usage(&raw)?;

    // Deserialize the rest of the response into the canonical type.
    let mut resp: tt_shared::ChatCompletionResponse =
        serde_json::from_value(raw).map_err(|e| ProviderError::Deserialize(e.to_string()))?;

    // Overwrite usage with the enriched version (cached_tokens populated).
    resp.usage = canonical_usage;

    Ok(resp)
}

// ---------------------------------------------------------------------------
// Embeddings request / response
// ---------------------------------------------------------------------------

/// Translate a canonical [`EmbeddingsRequest`] into the body sent to OpenAI.
///
/// The canonical shape already matches OpenAI's wire format exactly, so this
/// is a passthrough serialization with no field renaming.
pub fn translate_embeddings_request(
    req: tt_shared::EmbeddingsRequest,
) -> Result<tt_shared::EmbeddingsRequest, ProviderError> {
    // The canonical EmbeddingsRequest is already OpenAI-shaped; no translation needed.
    Ok(req)
}

/// Deserialize a raw OpenAI embeddings JSON response body into a canonical
/// [`EmbeddingsResponse`], mapping any serde error to [`ProviderError::Deserialize`].
pub fn deserialize_embeddings_response(
    body: &str,
) -> Result<tt_shared::EmbeddingsResponse, ProviderError> {
    serde_json::from_str(body).map_err(|e| ProviderError::Deserialize(e.to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::{messages::MessageContent, ChatCompletionRequest};

    fn base_request(model: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![Message::User {
                content: MessageContent::Text("Hello".to_string()),
                name: None,
            }],
            temperature: Some(0.7),
            top_p: None,
            max_tokens: Some(512),
            stream: false,
            tools: vec![],
            tool_choice: None,
            response_format: None,
            stop: vec![],
            presence_penalty: None,
            frequency_penalty: None,
            n: None,
            seed: None,
            user: None,
            tt_extras: std::collections::HashMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn dropped_params_temperature_only_for_reasoning_models() {
        // Reasoning model + temperature set → dropped.
        let req = base_request("o3");
        assert_eq!(dropped_params(&req), vec!["temperature".to_string()]);

        // Non-reasoning model: temperature is forwarded, not dropped.
        let req2 = base_request("gpt-4o");
        assert!(dropped_params(&req2).is_empty());

        // Reasoning model but no temperature set → nothing dropped.
        let mut req3 = base_request("o4-mini");
        req3.temperature = None;
        assert!(dropped_params(&req3).is_empty());
    }

    #[test]
    fn non_reasoning_passes_through() {
        let req = base_request("gpt-4o");
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.temperature, Some(0.7));
        assert_eq!(body.max_tokens, Some(512));
        assert!(body.max_completion_tokens.is_none());
    }

    #[test]
    fn reasoning_model_renames_max_tokens() {
        let req = base_request("o3");
        let body = translate_request(req).expect("translate ok");
        assert!(body.max_tokens.is_none());
        assert_eq!(body.max_completion_tokens, Some(512));
        // temperature dropped
        assert!(body.temperature.is_none());
    }

    #[test]
    fn tt_extras_not_serialized() {
        let mut req = base_request("gpt-4o");
        req.tt_extras
            .insert("route_hint".to_string(), serde_json::json!("us-east-1"));
        let body = translate_request(req).expect("translate ok");
        let serialized = serde_json::to_string(&body).expect("serialize ok");
        assert!(!serialized.contains("tt_extras"));
        assert!(!serialized.contains("route_hint"));
    }

    #[test]
    fn typed_compat_fields_forwarded() {
        let mut req = base_request("gpt-4o");
        req.parallel_tool_calls = Some(false);
        req.reasoning_effort = Some("high".to_string());
        req.stream_options = Some(serde_json::json!({ "include_usage": true }));
        let body = translate_request(req).expect("translate ok");
        let v = serde_json::to_value(&body).expect("serialize ok");
        assert_eq!(v["parallel_tool_calls"], false);
        assert_eq!(v["reasoning_effort"], "high");
        assert_eq!(
            v["stream_options"],
            serde_json::json!({"include_usage": true})
        );
    }

    #[test]
    fn max_completion_tokens_honored_for_reasoning_model() {
        // A client setting max_completion_tokens directly (the spend cap) must
        // be forwarded, not dropped.
        let mut req = base_request("o3");
        req.max_tokens = None;
        req.max_completion_tokens = Some(2048);
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.max_completion_tokens, Some(2048));
        assert!(body.max_tokens.is_none());
    }

    #[test]
    fn explicit_max_completion_tokens_wins_over_renamed_max_tokens() {
        // If a reasoning-model request carries BOTH the legacy max_tokens (which
        // we rename) and an explicit max_completion_tokens, the explicit field
        // is authoritative.
        let mut req = base_request("o3");
        req.max_tokens = Some(512);
        req.max_completion_tokens = Some(2048);
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.max_completion_tokens, Some(2048));
        assert!(body.max_tokens.is_none());
    }

    #[test]
    fn unknown_fields_passthrough_to_upstream() {
        let mut req = base_request("gpt-4o");
        req.extra
            .insert("service_tier".to_string(), serde_json::json!("flex"));
        req.extra
            .insert("logprobs".to_string(), serde_json::json!(true));
        let body = translate_request(req).expect("translate ok");
        let v = serde_json::to_value(&body).expect("serialize ok");
        assert_eq!(v["service_tier"], "flex");
        assert_eq!(v["logprobs"], true);
        // tt_extras must never leak.
        assert!(v.get("tt_extras").is_none());
    }

    #[test]
    fn usage_cached_tokens_populated() {
        let raw = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "prompt_tokens_details": { "cached_tokens": 80 }
            }
        });
        let usage = extract_usage(&raw).expect("extract ok");
        assert_eq!(usage.cached_tokens, 80);
        assert_eq!(usage.prompt_tokens, 100);
        // Raw Option preserved alongside the fold (telemetry NULL-vs-0).
        assert_eq!(usage.cache_read_input_tokens, Some(80));
    }

    #[test]
    fn usage_cached_tokens_absent_defaults_zero() {
        let raw = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        });
        let usage = extract_usage(&raw).expect("extract ok");
        assert_eq!(usage.cached_tokens, 0);
        // No prompt_tokens_details at all → raw stays None, NOT Some(0).
        assert_eq!(usage.cache_read_input_tokens, None);
    }

    /// A details object that carries other keys but NO `cached_tokens` key
    /// means the provider did not report cache reads — raw must stay None
    /// (SQL NULL), not become a fabricated Some(0).
    #[test]
    fn usage_details_without_cached_tokens_key_is_none() {
        let raw = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "prompt_tokens_details": { "audio_tokens": 5 }
            }
        });
        let usage = extract_usage(&raw).expect("extract ok");
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, None);
    }

    /// An explicit `"cached_tokens": null` must parse (not error the request)
    /// and map to "unreported" (None).
    #[test]
    fn usage_details_null_cached_tokens_is_lenient_none() {
        let raw = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "prompt_tokens_details": { "cached_tokens": null }
            }
        });
        let usage = extract_usage(&raw).expect("extract ok");
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, None);
    }

    /// A provider that reports `prompt_tokens_details.cached_tokens: 0` is
    /// explicitly saying "zero cache reads" — raw must be Some(0), not None.
    #[test]
    fn usage_cached_tokens_reported_zero_is_some_zero() {
        let raw = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "prompt_tokens_details": { "cached_tokens": 0 }
            }
        });
        let usage = extract_usage(&raw).expect("extract ok");
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, Some(0));
    }

    /// An already-canonical usage block (a TokenTrimmer hop or fake-stream
    /// upstream) passes its cache fields through on the NON-streaming path,
    /// mirroring the streaming passthrough — chained deployments keep cache
    /// telemetry and cached-rate pricing instead of logging NULL and billing
    /// the cached prompt at the full input rate.
    #[test]
    fn usage_canonical_cache_fields_pass_through() {
        let raw = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "cached_tokens": 40,
                "cache_read_input_tokens": 40,
                "cache_creation_input_tokens": 20
            }
        });
        let usage = extract_usage(&raw).expect("extract ok");
        assert_eq!(usage.cached_tokens, 40);
        assert_eq!(usage.cache_read_input_tokens, Some(40));
        assert_eq!(usage.cache_creation_input_tokens, Some(20));
    }

    /// When both shapes are present the OpenAI wire detail wins (mirrors the
    /// streaming precedence; real providers emit exactly one shape).
    #[test]
    fn usage_wire_detail_wins_over_canonical_passthrough() {
        let raw = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "cached_tokens": 40,
                "cache_read_input_tokens": 40,
                "prompt_tokens_details": { "cached_tokens": 80 }
            }
        });
        let usage = extract_usage(&raw).expect("extract ok");
        assert_eq!(usage.cached_tokens, 80);
        assert_eq!(usage.cache_read_input_tokens, Some(80));
    }

    /// A malformed `prompt_tokens_details` (non-object) or a non-integer
    /// `cached_tokens` must never fail the usage parse — it degrades to
    /// "unreported" (raw None, fold 0): conservative for the ledger, and on
    /// streams it keeps an error frame out of an otherwise-healthy stream.
    #[test]
    fn usage_malformed_details_is_lenient_unreported() {
        for details in [
            serde_json::json!("bogus"),
            serde_json::json!(42),
            serde_json::json!([1, 2]),
            serde_json::json!({ "cached_tokens": "80" }),
            serde_json::json!({ "cached_tokens": 80.5 }),
            serde_json::json!({ "cached_tokens": -3 }),
        ] {
            let raw = serde_json::json!({
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 50,
                    "total_tokens": 150,
                    "prompt_tokens_details": details
                }
            });
            let usage = extract_usage(&raw)
                .unwrap_or_else(|e| panic!("details {details:?} must not error: {e}"));
            assert_eq!(usage.cached_tokens, 0, "details {details:?}");
            assert_eq!(usage.cache_read_input_tokens, None, "details {details:?}");
        }
    }

    /// An integral float `cached_tokens` (a shim whose JSON encoder
    /// float-ifies integers) is accepted as the integer it denotes.
    #[test]
    fn usage_integral_float_cached_tokens_accepted() {
        let raw = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "prompt_tokens_details": { "cached_tokens": 80.0 }
            }
        });
        let usage = extract_usage(&raw).expect("extract ok");
        assert_eq!(usage.cached_tokens, 80);
        assert_eq!(usage.cache_read_input_tokens, Some(80));
    }
}
