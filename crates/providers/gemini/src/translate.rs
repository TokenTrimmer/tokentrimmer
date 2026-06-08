//! Request/response translation for the Gemini adapter.
//!
//! Translates canonical OpenAI-format types (from `tt_shared`) to/from
//! Google Gemini's native wire format.
//!
//! # Key differences from OpenAI
//!
//! - **Model in URL**, not the request body — the model is removed from the
//!   request struct before it reaches this module.
//! - **System messages** are extracted to a top-level `systemInstruction` field.
//!   They must not appear in `contents`.
//! - **Role rename**: `assistant` → `model`, `tool` → `function`.
//! - **Tools format**: OpenAI `tools[i].function` → Gemini single `tools` entry
//!   wrapping all functions in `functionDeclarations[...]`.
//! - **Generation config**: `max_tokens` → `maxOutputTokens`, `top_p` → `topP`,
//!   `stop` → `stopSequences`, `response_format` → `responseMimeType` /
//!   `responseSchema`.
//! - **Response**: Gemini generates its own `id` (`chatcmpl-gem-<uuid>`); the
//!   `created` timestamp uses `Utc::now()` — **redact both in snapshots**.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::warn;
use tt_shared::{
    messages::{ContentPart, Message, MessageContent, ToolCall, ToolCallFunction, ToolChoice},
    usage::Usage,
    ChatCompletionResponse, Choice, ProviderError,
};
use uuid::Uuid;

use crate::pricing::BRACKET_THRESHOLD_TOKENS;

// ---------------------------------------------------------------------------
// Gemini request wire types
// ---------------------------------------------------------------------------

/// Top-level Gemini `generateContent` request body.
///
/// Note: model is **not** included here; it is part of the URL path.
#[derive(Debug, Serialize)]
pub struct GeminiRequest {
    /// Conversation turns. System messages are extracted to `systemInstruction`.
    pub contents: Vec<GeminiContent>,
    /// Optional system prompt.
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    /// Sampling and generation parameters.
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
    /// Available tools (wrapped in functionDeclarations).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<GeminiToolBlock>,
    /// Tool use forcing strategy.
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<GeminiToolConfig>,
}

/// A single conversation turn.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GeminiContent {
    /// `"user"`, `"model"`, or `"function"`.
    pub role: String,
    /// One or more content parts.
    pub parts: Vec<GeminiPart>,
}

/// A content part within a Gemini message.
#[derive(Debug, Clone)]
pub enum GeminiPart {
    /// Plain text.
    Text(String),
    /// Inline binary data (base64).
    InlineData(GeminiInlineData),
    /// Remote file reference.
    FileData(GeminiFileData),
    /// A function call made by the model.
    FunctionCall(GeminiFunctionCall),
    /// The result of a function call.
    FunctionResponse(GeminiFunctionResponse),
}

impl Serialize for GeminiPart {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            GeminiPart::Text(t) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("text", t)?;
                map.end()
            }
            GeminiPart::InlineData(d) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("inlineData", d)?;
                map.end()
            }
            GeminiPart::FileData(d) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("fileData", d)?;
                map.end()
            }
            GeminiPart::FunctionCall(fc) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("functionCall", fc)?;
                map.end()
            }
            GeminiPart::FunctionResponse(fr) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("functionResponse", fr)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for GeminiPart {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let map: serde_json::Value = serde_json::Value::deserialize(deserializer)?;
        if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
            return Ok(GeminiPart::Text(text.to_string()));
        }
        if let Some(fc) = map.get("functionCall") {
            let fc: GeminiFunctionCall =
                serde_json::from_value(fc.clone()).map_err(serde::de::Error::custom)?;
            return Ok(GeminiPart::FunctionCall(fc));
        }
        if let Some(fr) = map.get("functionResponse") {
            let fr: GeminiFunctionResponse =
                serde_json::from_value(fr.clone()).map_err(serde::de::Error::custom)?;
            return Ok(GeminiPart::FunctionResponse(fr));
        }
        if let Some(id) = map.get("inlineData") {
            let d: GeminiInlineData =
                serde_json::from_value(id.clone()).map_err(serde::de::Error::custom)?;
            return Ok(GeminiPart::InlineData(d));
        }
        if let Some(fd) = map.get("fileData") {
            let d: GeminiFileData =
                serde_json::from_value(fd.clone()).map_err(serde::de::Error::custom)?;
            return Ok(GeminiPart::FileData(d));
        }
        Err(serde::de::Error::custom("unknown GeminiPart variant"))
    }
}

/// Inline binary data (base64-encoded).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GeminiInlineData {
    /// MIME type (e.g. `"image/jpeg"`).
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    /// Base64-encoded bytes.
    pub data: String,
}

/// Remote file reference.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GeminiFileData {
    /// MIME type.
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    /// Remote URI.
    #[serde(rename = "fileUri")]
    pub file_uri: String,
}

/// A function call emitted by the Gemini model.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GeminiFunctionCall {
    /// Function name.
    pub name: String,
    /// Function arguments as a JSON object (not a string).
    pub args: serde_json::Value,
}

/// The result of a function call, sent back to the model.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GeminiFunctionResponse {
    /// Must match the function name from the corresponding [`GeminiFunctionCall`].
    pub name: String,
    /// The function result.
    pub response: GeminiFunctionResponseContent,
}

/// Wrapper for function response content.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GeminiFunctionResponseContent {
    /// The textual content of the response.
    pub content: String,
}

/// System instruction (extracted from system messages).
#[derive(Debug, Serialize)]
pub struct GeminiSystemInstruction {
    /// Text parts of the system prompt.
    pub parts: Vec<GeminiTextPart>,
}

/// A text-only part used for system instructions.
#[derive(Debug, Serialize)]
pub struct GeminiTextPart {
    /// The system prompt text.
    pub text: String,
}

/// Sampling and generation configuration.
#[derive(Debug, Serialize)]
pub struct GeminiGenerationConfig {
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Top-P nucleus sampling.
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Maximum number of output tokens.
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Stop sequences.
    #[serde(rename = "stopSequences", skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// MIME type for structured output (`"application/json"` for JSON mode).
    #[serde(rename = "responseMimeType", skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    /// JSON schema for structured output.
    #[serde(rename = "responseSchema", skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<serde_json::Value>,
}

/// A Gemini tool block wrapping one or more function declarations.
#[derive(Debug, Serialize)]
pub struct GeminiToolBlock {
    /// All function declarations in this tool block.
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

/// A single function declaration.
#[derive(Debug, Serialize)]
pub struct GeminiFunctionDeclaration {
    /// Function name.
    pub name: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the function's parameters.
    pub parameters: serde_json::Value,
}

/// Tool use forcing configuration.
#[derive(Debug, Serialize, Clone)]
pub struct GeminiToolConfig {
    /// Function calling configuration.
    #[serde(rename = "functionCallingConfig")]
    pub function_calling_config: GeminiFunctionCallingConfig,
}

/// Function calling config with mode and optional allowlist.
#[derive(Debug, Serialize, Clone)]
pub struct GeminiFunctionCallingConfig {
    /// `"AUTO"`, `"ANY"`, or `"NONE"`.
    pub mode: String,
    /// If `mode` is `"ANY"`, specifies which functions may be called.
    #[serde(rename = "allowedFunctionNames", skip_serializing_if = "Vec::is_empty")]
    pub allowed_function_names: Vec<String>,
}

// ---------------------------------------------------------------------------
// Gemini response wire types
// ---------------------------------------------------------------------------

/// Gemini `generateContent` response.
#[derive(Debug, Deserialize)]
pub struct GeminiResponse {
    /// One candidate per requested completion (usually 1).
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    /// Token usage metadata.
    #[serde(rename = "usageMetadata", default)]
    pub usage_metadata: Option<GeminiUsageMetadata>,
    /// The model version actually used.
    #[serde(rename = "modelVersion")]
    pub model_version: Option<String>,
}

/// A single completion candidate.
#[derive(Debug, Deserialize)]
pub struct GeminiCandidate {
    /// The generated content.
    pub content: Option<GeminiContent>,
    /// Why generation stopped.
    #[serde(rename = "finishReason")]
    pub finish_reason: Option<String>,
    /// Candidate index (0-based).
    #[serde(default)]
    pub index: u32,
}

/// Token usage from the Gemini API.
#[derive(Debug, Deserialize, Default)]
pub struct GeminiUsageMetadata {
    /// Input tokens consumed.
    #[serde(rename = "promptTokenCount", default)]
    pub prompt_token_count: u64,
    /// Output tokens generated.
    #[serde(rename = "candidatesTokenCount", default)]
    pub candidates_token_count: u64,
    /// Total tokens.
    #[serde(rename = "totalTokenCount", default)]
    pub total_token_count: u64,
    /// Tokens served from cache.
    #[serde(rename = "cachedContentTokenCount", default)]
    pub cached_content_token_count: u64,
}

// ---------------------------------------------------------------------------
// Request translation: canonical → Gemini
// ---------------------------------------------------------------------------

/// Validate a model id before it is interpolated into the Gemini REST URL path
/// `/v1beta/models/<model>:generateContent`. Only ASCII alphanumerics and the
/// characters `. _ -` are allowed, so a crafted model id cannot inject extra
/// path, query, or fragment segments. Returns `ProviderError::InvalidRequest`
/// for anything else.
pub fn validate_model_id(model: &str) -> Result<(), ProviderError> {
    let ok = !model.is_empty()
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(ProviderError::InvalidRequest(format!(
            "invalid Gemini model id {model:?}: only [A-Za-z0-9._-] is allowed"
        )))
    }
}

/// Translate a [`tt_shared::ChatCompletionRequest`] into a [`GeminiRequest`]
/// ready to serialize and POST.
///
/// The model is **not** included in the body; it belongs in the URL path.
///
/// Performs:
/// - System message extraction to `systemInstruction`.
/// - Role rename: `assistant` → `model`, `tool` → `function`.
/// - Tool call assistant messages → `functionCall` parts.
/// - Tool result messages → `functionResponse` parts.
/// - Tool / tool_choice format translation.
/// - Generation config mapping.
/// - Stripping of OpenAI-only fields (`n`, `seed`, `presence_penalty`, etc.)
pub fn translate_request(
    req: tt_shared::ChatCompletionRequest,
) -> Result<GeminiRequest, ProviderError> {
    let mut system_parts: Vec<GeminiTextPart> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::new();

    // We need to look ahead for tool call IDs when processing tool results.
    // Build a map of tool_call_id → function_name from assistant messages.
    let tool_call_id_to_name = build_tool_call_id_map(&req.messages);

    for msg in req.messages {
        match msg {
            Message::System { content } => {
                let text = extract_text_from_content(content)?;
                system_parts.push(GeminiTextPart { text });
            }
            Message::User { content, .. } => {
                let parts = translate_user_content(content)?;
                contents.push(GeminiContent {
                    role: "user".to_string(),
                    parts,
                });
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let mut parts: Vec<GeminiPart> = Vec::new();
                if let Some(c) = content {
                    parts.extend(translate_user_content(c)?);
                }
                for tc in tool_calls {
                    let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                        .map_err(|e| {
                            ProviderError::Deserialize(format!(
                                "tool_call arguments not valid JSON: {e}"
                            ))
                        })?;
                    parts.push(GeminiPart::FunctionCall(GeminiFunctionCall {
                        name: tc.function.name,
                        args,
                    }));
                }
                contents.push(GeminiContent {
                    role: "model".to_string(),
                    parts,
                });
            }
            Message::Tool {
                content,
                tool_call_id,
            } => {
                let text = extract_text_from_content(content)?;
                // Look up function name from the previous assistant message's tool_calls.
                let fn_name = tool_call_id_to_name
                    .get(&tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        warn!(
                            tool_call_id = %tool_call_id,
                            "could not find function name for tool_call_id; using id as fallback"
                        );
                        tool_call_id.clone()
                    });
                contents.push(GeminiContent {
                    role: "function".to_string(),
                    parts: vec![GeminiPart::FunctionResponse(GeminiFunctionResponse {
                        name: fn_name,
                        response: GeminiFunctionResponseContent { content: text },
                    })],
                });
            }
        }
    }

    let system_instruction = if system_parts.is_empty() {
        None
    } else {
        Some(GeminiSystemInstruction {
            parts: system_parts,
        })
    };

    // Translate generation config.
    let (response_mime_type, response_schema) = translate_response_format(req.response_format);
    let generation_config = build_generation_config(
        req.temperature,
        req.top_p,
        req.max_tokens,
        req.stop,
        response_mime_type,
        response_schema,
    );

    // Log if request exceeds 200K token bracket threshold (best-effort estimate).
    // The actual count comes from the response; this is a rough pre-flight check.
    if let Some(cfg) = &generation_config {
        if let Some(max_out) = cfg.max_output_tokens {
            // We don't have token counts pre-flight, but we log it when usage comes back.
            // This is a no-op placeholder; actual bracket logging happens in pricing usage.
            let _ = max_out;
        }
    }

    // Translate tools.
    let tools = if req.tools.is_empty() {
        vec![]
    } else {
        let decls: Vec<GeminiFunctionDeclaration> = req
            .tools
            .into_iter()
            .map(|t| GeminiFunctionDeclaration {
                name: t.function.name,
                description: t.function.description,
                parameters: t.function.parameters,
            })
            .collect();
        vec![GeminiToolBlock {
            function_declarations: decls,
        }]
    };

    let tool_config = req.tool_choice.map(translate_tool_choice);

    Ok(GeminiRequest {
        contents,
        system_instruction,
        generation_config,
        tools,
        tool_config,
        // Intentionally dropped: model (in URL), n, seed, presence_penalty,
        // frequency_penalty, user, stream (separate endpoint), tt_extras
    })
}

/// Build a map from `tool_call_id` → `function_name` by scanning all
/// assistant messages for their `tool_calls`.
fn build_tool_call_id_map(messages: &[Message]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for msg in messages {
        if let Message::Assistant { tool_calls, .. } = msg {
            for tc in tool_calls {
                map.insert(tc.id.clone(), tc.function.name.clone());
            }
        }
    }
    map
}

/// Extract plain text from a [`MessageContent`].
fn extract_text_from_content(content: MessageContent) -> Result<String, ProviderError> {
    match content {
        MessageContent::Text(t) => Ok(t),
        MessageContent::Parts(parts) => {
            let text = parts
                .into_iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            Ok(text)
        }
    }
}

/// Convert [`MessageContent`] into Gemini parts.
fn translate_user_content(content: MessageContent) -> Result<Vec<GeminiPart>, ProviderError> {
    match content {
        MessageContent::Text(t) => Ok(vec![GeminiPart::Text(t)]),
        MessageContent::Parts(parts) => {
            let mut gemini_parts = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        gemini_parts.push(GeminiPart::Text(text));
                    }
                    ContentPart::ImageUrl { image_url } => {
                        // A base64 `data:` URI is sent as inlineData; a remote
                        // URL is sent as fileData.
                        match tt_shared::messages::parse_data_url(&image_url.url) {
                            Some((mime_type, data)) => {
                                gemini_parts.push(GeminiPart::InlineData(GeminiInlineData {
                                    mime_type,
                                    data,
                                }));
                            }
                            None => {
                                let mime_type = guess_mime_from_url(&image_url.url);
                                gemini_parts.push(GeminiPart::FileData(GeminiFileData {
                                    mime_type,
                                    file_uri: image_url.url,
                                }));
                            }
                        }
                    }
                    ContentPart::InputAudio { .. } => {
                        return Err(ProviderError::Unsupported(
                            "audio input is not supported by the Gemini adapter".to_string(),
                        ));
                    }
                }
            }
            Ok(gemini_parts)
        }
    }
}

/// Guess a MIME type from a URL's extension. Falls back to `"image/jpeg"`.
fn guess_mime_from_url(url: &str) -> String {
    let lower = url.to_lowercase();
    if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else {
        "image/jpeg".to_string()
    }
}

/// Build the `generationConfig` object from individual parameters.
fn build_generation_config(
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_tokens: Option<u32>,
    stop: Vec<String>,
    response_mime_type: Option<String>,
    response_schema: Option<serde_json::Value>,
) -> Option<GeminiGenerationConfig> {
    let has_anything = temperature.is_some()
        || top_p.is_some()
        || max_tokens.is_some()
        || !stop.is_empty()
        || response_mime_type.is_some()
        || response_schema.is_some();

    if !has_anything {
        return None;
    }

    Some(GeminiGenerationConfig {
        temperature,
        top_p,
        max_output_tokens: max_tokens,
        stop_sequences: stop,
        response_mime_type,
        response_schema,
    })
}

/// Translate `response_format` into Gemini's `responseMimeType` and
/// optional `responseSchema`.
fn translate_response_format(
    rf: Option<tt_shared::messages::ResponseFormat>,
) -> (Option<String>, Option<serde_json::Value>) {
    match rf {
        None => (None, None),
        Some(fmt) => {
            if fmt.r#type == "json_schema" || fmt.r#type == "json_object" {
                (Some("application/json".to_string()), fmt.json_schema)
            } else {
                (None, None)
            }
        }
    }
}

/// Translate a canonical [`ToolChoice`] to a [`GeminiToolConfig`].
fn translate_tool_choice(choice: ToolChoice) -> GeminiToolConfig {
    match choice {
        ToolChoice::Auto(s) if s == "none" => GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "NONE".to_string(),
                allowed_function_names: vec![],
            },
        },
        ToolChoice::Auto(s) if s == "required" => GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "ANY".to_string(), // ANY + empty allowlist = must call some provided fn
                allowed_function_names: vec![],
            },
        },
        ToolChoice::Auto(_) => GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "AUTO".to_string(),
                allowed_function_names: vec![],
            },
        },
        ToolChoice::Specific { function, .. } => GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "ANY".to_string(),
                allowed_function_names: vec![function.name],
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Response translation: Gemini → canonical
// ---------------------------------------------------------------------------

/// Deserialize and translate a Gemini JSON response body into a canonical
/// [`ChatCompletionResponse`].
pub fn deserialize_response(
    body: &str,
    requested_model: &str,
) -> Result<ChatCompletionResponse, ProviderError> {
    let resp: GeminiResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Deserialize(e.to_string()))?;
    Ok(translate_response(resp, requested_model))
}

/// Translate a [`GeminiResponse`] into a canonical [`ChatCompletionResponse`].
pub fn translate_response(resp: GeminiResponse, requested_model: &str) -> ChatCompletionResponse {
    let id = format!("chatcmpl-gem-{}", Uuid::new_v4());
    let created = Utc::now().timestamp();
    let model = resp
        .model_version
        .unwrap_or_else(|| requested_model.to_string());

    let usage = resp.usage_metadata.map(translate_usage).unwrap_or_default();

    // Log bracket warning if prompt tokens exceed threshold.
    if usage.prompt_tokens > BRACKET_THRESHOLD_TOKENS {
        tracing::debug!(
            prompt_tokens = usage.prompt_tokens,
            threshold = BRACKET_THRESHOLD_TOKENS,
            "prompt token count exceeds 200K bracket threshold; higher pricing tier applies"
        );
    }

    let choice = if let Some(candidate) = resp.candidates.into_iter().next() {
        let (message, finish_reason) = translate_candidate(candidate);
        Choice {
            index: 0,
            message,
            finish_reason,
        }
    } else {
        // Empty candidates — return an empty assistant message.
        Choice {
            index: 0,
            message: Message::Assistant {
                content: None,
                tool_calls: vec![],
                name: None,
            },
            finish_reason: Some("stop".to_string()),
        }
    };

    ChatCompletionResponse {
        id,
        object: "chat.completion".to_string(),
        created,
        model,
        choices: vec![choice],
        usage,
    }
}

/// Translate a single Gemini candidate into a canonical [`Message`] and finish reason.
fn translate_candidate(candidate: GeminiCandidate) -> (Message, Option<String>) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    if let Some(content) = candidate.content {
        for part in content.parts {
            match part {
                GeminiPart::Text(t) => text_parts.push(t),
                GeminiPart::FunctionCall(fc) => {
                    tool_calls.push(ToolCall {
                        id: format!("call_{}", Uuid::new_v4()),
                        r#type: "function".to_string(),
                        function: ToolCallFunction {
                            name: fc.name,
                            arguments: fc.args.to_string(),
                        },
                    });
                }
                _ => {} // Ignore other part types in responses
            }
        }
    }

    let message_content = if text_parts.is_empty() {
        None
    } else {
        Some(MessageContent::Text(text_parts.join("")))
    };

    // If there are tool calls, the finish reason is "tool_calls" regardless of what Gemini says.
    let finish_reason = if !tool_calls.is_empty() {
        Some("tool_calls".to_string())
    } else {
        candidate
            .finish_reason
            .as_deref()
            .map(map_finish_reason)
            .map(str::to_string)
    };

    let message = Message::Assistant {
        content: message_content,
        tool_calls,
        name: None,
    };

    (message, finish_reason)
}

/// Map Gemini `finishReason` to OpenAI `finish_reason`.
pub fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" => "content_filter",
        "RECITATION" => "content_filter",
        "OTHER" => "stop",
        _ => "stop",
    }
}

/// Convert [`GeminiUsageMetadata`] into canonical [`Usage`].
pub fn translate_usage(u: GeminiUsageMetadata) -> Usage {
    let prompt = u.prompt_token_count;
    let mut completion = u.candidates_token_count;
    let mut total = u.total_token_count;
    // Gemini can omit candidatesTokenCount/totalTokenCount on partial responses.
    // Reconcile so `total == prompt + completion` (mirrors the Anthropic adapter).
    if completion == 0 && total > prompt {
        completion = total - prompt;
    }
    if total == 0 {
        total = prompt + completion;
    }
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        cached_tokens: u.cached_content_token_count,
        cache_creation_input_tokens: None,
    }
}

// Unit tests live in tests/translate.rs (snapshot tests cover all translation paths).

#[cfg(test)]
mod tests {
    use super::GeminiUsageMetadata;

    #[test]
    fn translate_usage_reconciles_partial_metadata() {
        let u = super::translate_usage(GeminiUsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 0,
            total_token_count: 25,
            cached_content_token_count: 0,
        });
        assert_eq!(u.completion_tokens, 15);
        assert_eq!(u.total_tokens, 25);
        let u = super::translate_usage(GeminiUsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 5,
            total_token_count: 0,
            cached_content_token_count: 0,
        });
        assert_eq!(u.total_tokens, 15);
        let u = super::translate_usage(GeminiUsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 5,
            total_token_count: 15,
            cached_content_token_count: 2,
        });
        assert_eq!(
            (
                u.prompt_tokens,
                u.completion_tokens,
                u.total_tokens,
                u.cached_tokens
            ),
            (10, 5, 15, 2)
        );
    }
}
