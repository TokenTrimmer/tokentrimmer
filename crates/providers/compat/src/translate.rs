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
    matches!(model, "o3" | "o4-mini")
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
}

/// Translate a canonical [`tt_shared::ChatCompletionRequest`] into an
/// [`OpenAiRequestBody`] ready to serialize and POST.
///
/// This strips `tt_extras` and applies reasoning-model parameter constraints.
pub fn translate_request(
    req: tt_shared::ChatCompletionRequest,
) -> Result<OpenAiRequestBody, ProviderError> {
    let reasoning = is_reasoning_model(&req.model);

    let (max_tokens, max_completion_tokens, temperature) = if reasoning {
        if req.temperature.is_some() {
            tracing::warn!(
                model = %req.model,
                "reasoning models do not support temperature; dropping the field"
            );
        }
        // Rename max_tokens → max_completion_tokens for reasoning models.
        (None, req.max_tokens, None)
    } else {
        (req.max_tokens, None, req.temperature)
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
        // tt_extras is intentionally not forwarded.
    })
}

// ---------------------------------------------------------------------------
// Inbound response — usage extraction
// ---------------------------------------------------------------------------

/// OpenAI usage block as returned in a chat-completion response.
///
/// We deserialize it separately from the top-level response so that we can
/// pull out `prompt_tokens_details.cached_tokens` before constructing the
/// canonical [`Usage`].
#[derive(Debug, Deserialize)]
pub struct OpenAiUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// OpenAI `prompt_tokens_details` sub-object.
#[derive(Debug, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

impl From<OpenAiUsage> for Usage {
    fn from(u: OpenAiUsage) -> Self {
        let cached_tokens = u
            .prompt_tokens_details
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
            cached_tokens,
            cache_creation_input_tokens: None,
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
        }
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
    }
}
