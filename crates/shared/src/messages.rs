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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
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

    /// TokenTrimmer-internal extras (cache config, route hints, etc.) that are
    /// stripped before forwarding to the provider.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tt_extras: HashMap<String, serde_json::Value>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChunkDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
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
}
