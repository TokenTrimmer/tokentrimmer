//! OpenAI-compatible request/response shapes. Canonical wire format across all
//! providers — adapters translate to/from provider-native formats.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::Usage;

// ---------------------------------------------------------------------------
// tt_extras cache-control types (Fix B / §2.7)
// ---------------------------------------------------------------------------

/// Cache behaviour requested by the caller via `tt_extras.cache`.
///
/// Absent (no `cache` key in `tt_extras`) is treated as [`CacheMode::Normal`].
///
/// JSON shape:
/// ```json
/// { "cache": { "mode": "bypass", "ttl_secs": 3600 } }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    /// Normal read-write caching (default when key absent).
    #[default]
    Normal,
    /// Skip lookup AND insert — always hit the provider, never populate cache.
    Bypass,
    /// Skip lookup, but DO insert (force-refresh stale entry).
    Refresh,
    /// Do lookup, but never insert (read-only cache consumer).
    #[serde(rename = "read-only")]
    ReadOnly,
}

/// Typed cache-control extracted from `tt_extras`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheControlConfig {
    /// Requested cache behaviour.
    #[serde(default)]
    pub mode: CacheMode,
    /// Override TTL for cache inserts. `None` = use the gateway default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

/// Parse [`CacheControlConfig`] from a request's `tt_extras` map.
///
/// Returns `None` when `tt_extras` does not contain a `"cache"` key.
/// Returns the default config (normal mode, no TTL override) when the key is
/// present but the value fails to deserialize — so a malformed field degrades
/// gracefully rather than hard-failing.
pub fn parse_cache_control(
    extras: &HashMap<String, serde_json::Value>,
) -> Option<CacheControlConfig> {
    let val = extras.get("cache")?;
    match serde_json::from_value::<CacheControlConfig>(val.clone()) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            // Log at warn level so operators can see bad payloads; fall back
            // to normal (don't block the request).
            tracing::warn!(
                error = %e,
                "tt_extras.cache deserialization failed — treating as normal"
            );
            Some(CacheControlConfig::default())
        }
    }
}

// ---------------------------------------------------------------------------
// tt_extras.panel types (Phase 1 — deep-research panel)
// ---------------------------------------------------------------------------

/// Per-request panel overrides from `tt_extras.panel`.
///
/// JSON shape:
/// ```json
/// {
///   "panel": {
///     "members": ["gpt-4o", "claude-3-5-sonnet"],
///     "arbiter_model": "gpt-4o",
///     "quorum": 2,
///     "max_cost_usd": 0.05
///   }
/// }
/// ```
///
/// All fields are optional; absent fields fall back to gateway defaults in
/// `PanelConfig::resolve` (defined in `tt-core`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PanelExtras {
    /// Explicit list of member model ids to fan out to. Overrides the gateway
    /// default when non-empty.
    #[serde(default)]
    pub members: Vec<String>,

    /// Override the arbiter model for Synthesize / BestOfN strategies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arbiter_model: Option<String>,

    /// Minimum number of legs that must succeed for the panel to return a result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quorum: Option<usize>,

    /// Hard cost ceiling in USD across all legs + arbitration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}

/// Parse [`PanelExtras`] from a request's `tt_extras` map.
///
/// Returns `None` when `tt_extras` does not contain a `"panel"` key.
/// Returns `None` when the key is present but the value fails to deserialize —
/// the panel is an expensive opt-in path and must fail safe to "no panel" rather
/// than silently activating panel resolution with empty/default overrides.
pub fn parse_panel_extras(extras: &HashMap<String, serde_json::Value>) -> Option<PanelExtras> {
    let val = extras.get("panel")?;
    match serde_json::from_value::<PanelExtras>(val.clone()) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "tt_extras.panel deserialization failed — treating as no panel extras"
            );
            None
        }
    }
}

#[cfg(test)]
mod cache_control_tests {
    use super::*;

    fn extras(json: &str) -> HashMap<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn no_cache_key_returns_none() {
        assert!(parse_cache_control(&extras("{}")).is_none());
    }

    #[test]
    fn bypass_mode_parsed() {
        let cfg = parse_cache_control(&extras(r#"{"cache":{"mode":"bypass"}}"#)).unwrap();
        assert_eq!(cfg.mode, CacheMode::Bypass);
        assert!(cfg.ttl_secs.is_none());
    }

    #[test]
    fn refresh_mode_with_ttl() {
        let cfg = parse_cache_control(&extras(r#"{"cache":{"mode":"refresh","ttl_secs":3600}}"#))
            .unwrap();
        assert_eq!(cfg.mode, CacheMode::Refresh);
        assert_eq!(cfg.ttl_secs, Some(3600));
    }

    #[test]
    fn read_only_mode() {
        let cfg = parse_cache_control(&extras(r#"{"cache":{"mode":"read-only"}}"#)).unwrap();
        assert_eq!(cfg.mode, CacheMode::ReadOnly);
    }

    #[test]
    fn absent_mode_defaults_to_normal() {
        let cfg = parse_cache_control(&extras(r#"{"cache":{}}"#)).unwrap();
        assert_eq!(cfg.mode, CacheMode::Normal);
    }

    #[test]
    fn malformed_value_falls_back_to_default() {
        let cfg = parse_cache_control(&extras(r#"{"cache":"not-an-object"}"#)).unwrap();
        assert_eq!(cfg.mode, CacheMode::Normal);
    }
}

#[cfg(test)]
mod panel_extras_tests {
    use super::*;

    fn extras(json: &str) -> HashMap<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn absent_panel_key_is_none() {
        assert!(parse_panel_extras(&extras("{}")).is_none());
    }

    #[test]
    fn valid_panel_parses() {
        let panel = parse_panel_extras(&extras(
            r#"{"panel":{"members":["gpt-4o","claude-3-5-sonnet"],"quorum":2}}"#,
        ))
        .unwrap();
        assert_eq!(panel.members, vec!["gpt-4o", "claude-3-5-sonnet"]);
        assert_eq!(panel.quorum, Some(2));
    }

    #[test]
    fn malformed_panel_integer_value_is_none() {
        // panel value is a plain integer — fails to deserialize as PanelExtras.
        // Must NOT activate panel resolution (fail-safe to None).
        assert!(parse_panel_extras(&extras(r#"{"panel":42}"#)).is_none());
    }

    #[test]
    fn malformed_panel_string_value_is_none() {
        // panel value is a bare string — fails to deserialize as PanelExtras.
        // Must NOT activate panel resolution (fail-safe to None).
        assert!(parse_panel_extras(&extras(r#"{"panel":"not-an-object"}"#)).is_none());
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Newer OpenAI spend cap for reasoning models (`o3`, `o4-mini`, …). When a
    /// client sets this it MUST be honored end-to-end — dropping it silently
    /// removes the caller's output ceiling and changes spend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    /// OpenAI `stream_options` (e.g. `{ "include_usage": true }`). Kept as an
    /// opaque value so the full object passes through to OpenAI-shaped upstreams
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Whether the model may emit tool calls in parallel (OpenAI
    /// `parallel_tool_calls`). Forwarded to OpenAI-shaped upstreams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Reasoning-effort hint for reasoning models (`"low"`/`"medium"`/`"high"`).
    /// Materially changes output and cost, so it must reach the upstream when
    /// supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,

    /// TokenTrimmer-internal extras (cache config, route hints, etc.) that are
    /// stripped before forwarding to the provider.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tt_extras: HashMap<String, serde_json::Value>,

    /// Genuinely-unknown / newer OpenAI fields not modeled above. Captured via
    /// `#[serde(flatten)]` so they passthrough to OpenAI-shaped upstreams
    /// instead of being silently dropped on deserialize. Never includes the
    /// named fields above (serde consumes those first).
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: MessageContent,
    },
    User {
        content: MessageContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<MessageContent>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Tool {
        content: MessageContent,
        tool_call_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
    InputAudio { input_audio: InputAudio },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputAudio {
    pub data: String,
    pub format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub r#type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Auto(String),
    Specific {
        #[serde(rename = "type")]
        r#type: String,
        function: ToolChoiceFunction,
    },
}

impl ToolChoice {
    /// Let the model decide whether to call a tool (`"auto"`).
    #[must_use]
    pub fn auto() -> Self {
        ToolChoice::Auto("auto".to_string())
    }

    /// Forbid tool calls — force a plain text answer (`"none"`).
    #[must_use]
    pub fn none() -> Self {
        ToolChoice::Auto("none".to_string())
    }

    /// Require the model to call some tool (`"required"`).
    #[must_use]
    pub fn required() -> Self {
        ToolChoice::Auto("required".to_string())
    }

    /// Require the model to call a specific named function.
    #[must_use]
    pub fn function(name: impl Into<String>) -> Self {
        ToolChoice::Specific {
            r#type: "function".to_string(),
            function: ToolChoiceFunction { name: name.into() },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Stringified JSON arguments — OpenAI convention.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    pub finish_reason: Option<String>,
}

/// One SSE event from a streaming chat completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Genuinely-unknown / newer provider chunk fields (e.g.
    /// `system_fingerprint`, `service_tier`). Captured via `#[serde(flatten)]`
    /// so upstream SSE chunks round-trip through the gateway unchanged rather
    /// than being silently dropped.
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
    /// Unknown per-choice fields (e.g. `logprobs`) preserved verbatim.
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChunkDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Unknown per-delta fields (e.g. `refusal`) preserved verbatim.
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsRequest {
    pub model: String,
    pub input: EmbeddingInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Batch(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsResponse {
    pub object: String,
    pub data: Vec<EmbeddingData>,
    pub model: String,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingData {
    pub object: String,
    pub index: u32,
    pub embedding: Vec<f32>,
}

/// Parse a base64 `data:` URL into `(media_type, base64_payload)`.
///
/// Returns `None` for non-`data:` URLs, non-base64 data URLs, or a malformed/
/// empty media type. Provider adapters use this to forward inline image bytes
/// as the provider's native base64 image part instead of mistakenly sending the
/// whole `data:` URI as a *remote* URL reference (which the upstream rejects).
#[must_use]
pub fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    // Only base64 payloads are supported (the canonical image transport).
    let media_with_params = meta.strip_suffix(";base64")?;
    // Drop any RFC-2397 media-type parameters (e.g. `;charset=utf-8`) — providers
    // expect a bare MIME type like `image/png` in the base64 image part.
    let media_type = media_with_params.split(';').next().unwrap_or("");
    if media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((media_type.to_string(), data.to_string()))
}

#[cfg(test)]
mod embeddings_default_tests {
    use super::*;

    #[test]
    fn chat_request_default_is_empty() {
        let r = ChatCompletionRequest::default();
        assert_eq!(r.model, "");
        assert!(r.messages.is_empty());
        assert!(!r.stream);
        assert!(r.tools.is_empty());
        assert!(r.max_tokens.is_none());
    }

    #[test]
    fn typed_compat_fields_roundtrip() {
        let json = serde_json::json!({
            "model": "o3",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_completion_tokens": 4096,
            "stream_options": { "include_usage": true },
            "parallel_tool_calls": false,
            "reasoning_effort": "high",
        });
        let req: ChatCompletionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.max_completion_tokens, Some(4096));
        assert_eq!(req.parallel_tool_calls, Some(false));
        assert_eq!(req.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            req.stream_options,
            Some(serde_json::json!({ "include_usage": true }))
        );
        // The flatten map must NOT capture the named fields.
        assert!(req.extra.is_empty());

        let out = serde_json::to_value(&req).unwrap();
        assert_eq!(out["max_completion_tokens"], 4096);
        assert_eq!(
            out["stream_options"],
            serde_json::json!({"include_usage": true})
        );
        assert_eq!(out["parallel_tool_calls"], false);
        assert_eq!(out["reasoning_effort"], "high");
    }

    #[test]
    fn unknown_fields_passthrough_via_flatten() {
        // A genuinely-unknown / newer OpenAI field must survive deserialize and
        // re-serialize verbatim rather than being silently dropped.
        let json = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }],
            "logprobs": true,
            "top_logprobs": 5,
            "service_tier": "auto",
        });
        let req: ChatCompletionRequest = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(req.extra.get("logprobs"), Some(&serde_json::json!(true)));
        assert_eq!(req.extra.get("top_logprobs"), Some(&serde_json::json!(5)));
        assert_eq!(
            req.extra.get("service_tier"),
            Some(&serde_json::json!("auto"))
        );

        let out = serde_json::to_value(&req).unwrap();
        assert_eq!(out["logprobs"], true);
        assert_eq!(out["top_logprobs"], 5);
        assert_eq!(out["service_tier"], "auto");
    }

    #[test]
    fn streaming_chunk_unknown_fields_passthrough() {
        // Unknown / newer provider fields on a streaming chunk (and on its nested
        // choice/delta) must survive deserialize and re-serialize verbatim rather
        // than being silently dropped on the round-trip passthrough.
        let json = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 1716598234,
            "model": "gpt-4o",
            "system_fingerprint": "fp_abc123",
            "choices": [{
                "index": 0,
                "delta": { "content": "hi", "refusal": null },
                "finish_reason": null,
                "logprobs": { "content": [] }
            }]
        });
        let chunk: ChatCompletionChunk = serde_json::from_value(json).unwrap();
        assert_eq!(
            chunk.extra.get("system_fingerprint"),
            Some(&serde_json::json!("fp_abc123"))
        );
        assert_eq!(
            chunk.choices[0].extra.get("logprobs"),
            Some(&serde_json::json!({ "content": [] }))
        );
        assert_eq!(
            chunk.choices[0].delta.extra.get("refusal"),
            Some(&serde_json::Value::Null)
        );

        let out = serde_json::to_value(&chunk).unwrap();
        assert_eq!(out["system_fingerprint"], "fp_abc123");
        assert_eq!(
            out["choices"][0]["logprobs"],
            serde_json::json!({ "content": [] })
        );
        assert_eq!(
            out["choices"][0]["delta"]["refusal"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn parse_data_url_extracts_media_type_and_payload() {
        assert_eq!(
            parse_data_url("data:image/png;base64,iVBORw0KGgo="),
            Some(("image/png".to_string(), "iVBORw0KGgo=".to_string()))
        );
        // Non-data URLs and non-base64 / malformed data URLs return None.
        assert_eq!(parse_data_url("https://example.com/cat.png"), None);
        assert_eq!(parse_data_url("data:image/png,notbase64"), None);
        assert_eq!(parse_data_url("data:;base64,abc"), None);
        assert_eq!(parse_data_url("data:image/png;base64,"), None);
        // Media-type parameters are stripped to a bare MIME type.
        assert_eq!(
            parse_data_url("data:image/png;charset=utf-8;base64,iVBORw0KGgo="),
            Some(("image/png".to_string(), "iVBORw0KGgo=".to_string()))
        );
    }

    #[test]
    fn tool_choice_constructors_serialize_to_the_wire_form() {
        // The string variants stay an untagged bare string …
        assert_eq!(
            serde_json::to_value(ToolChoice::auto()).unwrap(),
            serde_json::json!("auto")
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::none()).unwrap(),
            serde_json::json!("none")
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::required()).unwrap(),
            serde_json::json!("required")
        );
        // … and `function(name)` produces the object form.
        assert_eq!(
            serde_json::to_value(ToolChoice::function("get_weather")).unwrap(),
            serde_json::json!({ "type": "function", "function": { "name": "get_weather" } })
        );
    }
}
