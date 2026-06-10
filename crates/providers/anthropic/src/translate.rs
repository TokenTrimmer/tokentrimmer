//! Request/response translation for the Anthropic adapter.
//!
//! Translates canonical OpenAI-format types (from `tt_shared`) to/from
//! Anthropic's native wire format.
//!
//! # Key differences from OpenAI
//!
//! - **System messages** are extracted to a top-level `system` field; they must
//!   not appear in `messages`.
//! - **`max_tokens` is required** — defaults to 4096 if omitted.
//! - **Auto cache_control**: if the last system block is ≥ 1024 tokens (via
//!   `len/4` heuristic), `cache_control: { type: "ephemeral" }` is applied.
//! - **Tool format** differs: OpenAI `{type, function:{name,description,parameters}}`
//!   → Anthropic `{name, description, input_schema}`.
//! - **`tool_choice`**: OpenAI `auto` string → `{type:"auto"}`, specific →
//!   `{type:"tool", name}`.
//! - **Multimodal**: OpenAI `image_url` → Anthropic `{type:"image", source:{type:"url",...}}`.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tt_shared::{
    messages::{ContentPart, Message, MessageContent, ToolCall, ToolCallFunction, ToolChoice},
    usage::Usage,
    ChatCompletionResponse, Choice, ProviderError,
};

// ---------------------------------------------------------------------------
// Anthropic request wire types
// ---------------------------------------------------------------------------

/// Top-level Anthropic API request body.
#[derive(Debug, Serialize)]
pub struct AnthropicRequest {
    /// Model identifier (e.g. `"claude-sonnet-4-6"`).
    pub model: String,
    /// Conversation messages (system removed to `system` field).
    pub messages: Vec<AnthropicMessage>,
    /// System prompt blocks, absent when there are none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<AnthropicSystemBlock>>,
    /// Required by Anthropic; defaults to 4096 when the caller omits it.
    pub max_tokens: u32,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// Tools available to the model.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicTool>,
    /// Tool-use forcing strategy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    /// Whether to stream the response.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    /// Optional metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AnthropicMetadata>,
}

/// A single conversation turn sent to Anthropic.
#[derive(Debug, Serialize)]
pub struct AnthropicMessage {
    /// `"user"` or `"assistant"`.
    pub role: String,
    /// One or more content blocks.
    pub content: Vec<AnthropicContentBlock>,
}

/// Content block variants in an Anthropic message.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    /// Plain text.
    Text { text: String },
    /// Image from a URL.
    Image { source: AnthropicImageSource },
    /// A tool call initiated by the assistant.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool result sent back in a user turn.
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

/// Image source descriptor for Anthropic: a remote `url` or inline `base64`
/// bytes (serialized as `{"type":"url",...}` / `{"type":"base64",...}`).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicImageSource {
    /// Remote image URL.
    Url { url: String },
    /// Inline base64-encoded image bytes.
    Base64 { media_type: String, data: String },
}

/// A system block in the `system` array.
#[derive(Debug, Serialize)]
pub struct AnthropicSystemBlock {
    /// Always `"text"`.
    #[serde(rename = "type")]
    pub block_type: String,
    /// The system prompt text.
    pub text: String,
    /// If set, Anthropic will cache this block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

/// Cache-control directive for prompt caching.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AnthropicCacheControl {
    /// Always `"ephemeral"`.
    #[serde(rename = "type")]
    pub ctype: String,
}

/// A tool definition sent to Anthropic.
#[derive(Debug, Serialize)]
pub struct AnthropicTool {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's input parameters.
    pub input_schema: serde_json::Value,
}

/// Tool-choice forcing directive.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolChoice {
    /// Let the model decide.
    Auto,
    /// Force any available tool.
    Any,
    /// Disable all tool use.
    None,
    /// Force a specific tool by name.
    Tool { name: String },
}

/// Optional request metadata.
#[derive(Debug, Serialize)]
pub struct AnthropicMetadata {
    /// External user ID for Anthropic's internal logging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Anthropic response wire types
// ---------------------------------------------------------------------------

/// Anthropic's non-streaming chat completion response.
#[derive(Debug, Deserialize)]
pub struct AnthropicResponse {
    /// Response ID (e.g. `"msg_01..."`).
    pub id: String,
    /// Returned model identifier (may include a date suffix).
    pub model: String,
    /// All content blocks in this response.
    pub content: Vec<AnthropicResponseBlock>,
    /// Why the model stopped generating.
    pub stop_reason: Option<String>,
    /// Token counts.
    pub usage: AnthropicUsage,
}

/// A content block in an Anthropic response.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicResponseBlock {
    /// Plain text output.
    Text { text: String },
    /// A tool invocation by the model.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

/// Usage data returned by Anthropic.
#[derive(Debug, Deserialize, Clone)]
pub struct AnthropicUsage {
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens generated.
    pub output_tokens: u64,
    /// Tokens written to the prompt cache on this call.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    /// Tokens read from the prompt cache.
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

// ---------------------------------------------------------------------------
// Request translation: canonical → Anthropic
// ---------------------------------------------------------------------------

/// Translate a [`tt_shared::ChatCompletionRequest`] into an
/// [`AnthropicRequest`] ready to serialize and POST.
///
/// Performs:
/// - System message extraction and optional cache_control injection.
/// - `max_tokens` defaulting.
/// - Tool / tool_choice format translation.
/// - Multimodal `image_url` → Anthropic `image` conversion.
/// - Stripping of OpenAI-only fields (`n`, `seed`, `response_format`, etc.)
/// - Forwarding `user` → `metadata.user_id`.
pub fn translate_request(
    req: tt_shared::ChatCompletionRequest,
) -> Result<AnthropicRequest, ProviderError> {
    let mut system_blocks: Vec<AnthropicSystemBlock> = Vec::new();
    let mut messages: Vec<AnthropicMessage> = Vec::new();

    for msg in req.messages {
        match msg {
            Message::System { content } => {
                let text = extract_text_from_content(content)?;
                system_blocks.push(AnthropicSystemBlock {
                    block_type: "text".to_string(),
                    text,
                    cache_control: None,
                });
            }
            Message::User { content, .. } => {
                let blocks = translate_content_blocks(content)?;
                messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: blocks,
                });
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let mut blocks: Vec<AnthropicContentBlock> = Vec::new();
                if let Some(c) = content {
                    blocks.extend(translate_content_blocks(c)?);
                }
                for tc in tool_calls {
                    let input: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                        .map_err(|e| {
                            ProviderError::Deserialize(format!(
                                "tool_call arguments not valid JSON: {e}"
                            ))
                        })?;
                    blocks.push(AnthropicContentBlock::ToolUse {
                        id: tc.id,
                        name: tc.function.name,
                        input,
                    });
                }
                messages.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content: blocks,
                });
            }
            Message::Tool {
                content,
                tool_call_id,
            } => {
                let text = extract_text_from_content(content)?;
                messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![AnthropicContentBlock::ToolResult {
                        tool_use_id: tool_call_id,
                        content: text,
                    }],
                });
            }
        }
    }

    // Apply auto cache_control to the last system block if it is long enough.
    // Heuristic: 1 token ≈ 4 characters.
    if let Some(last) = system_blocks.last_mut() {
        // char count, not byte len: multibyte (CJK) over-counts bytes and could
        // push a sub-1024-token block over the gate, which Anthropic 400s.
        let estimated_tokens = last.text.chars().count() / 4;
        if estimated_tokens >= 1024 {
            last.cache_control = Some(AnthropicCacheControl {
                ctype: "ephemeral".to_string(),
            });
        }
    }

    // `max_tokens` is required by Anthropic and is its output cap. Honor the
    // caller's spend cap from either `max_tokens` or the newer
    // `max_completion_tokens` (the latter takes precedence when both are set)
    // so the ceiling is never silently dropped; default to 4096 when neither.
    let max_tokens = req
        .max_completion_tokens
        .or(req.max_tokens)
        .unwrap_or_else(|| {
            tracing::debug!(
                "max_tokens/max_completion_tokens omitted — defaulting to 4096 for Anthropic"
            );
            4096
        });

    // Translate tools.
    let tools: Vec<AnthropicTool> = req
        .tools
        .into_iter()
        .map(|t| AnthropicTool {
            name: t.function.name,
            description: t.function.description,
            input_schema: t.function.parameters,
        })
        .collect();

    // Translate tool_choice.
    let tool_choice = req.tool_choice.map(translate_tool_choice);

    // `user` → `metadata.user_id`.
    let metadata = req.user.map(|u| AnthropicMetadata { user_id: Some(u) });

    Ok(AnthropicRequest {
        model: req.model,
        messages,
        system: if system_blocks.is_empty() {
            None
        } else {
            Some(system_blocks)
        },
        max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stop_sequences: req.stop,
        tools,
        tool_choice,
        stream: req.stream,
        metadata,
        // Intentionally dropped (Anthropic rejects them):
        // n, seed, response_format, presence_penalty, frequency_penalty,
        // logit_bias, tt_extras
    })
}

/// Extract a plain-text string from a [`MessageContent`].
///
/// Returns `Err` if the content is a `Parts` block — system messages must be
/// plain text per the current implementation.
fn extract_text_from_content(content: MessageContent) -> Result<String, ProviderError> {
    match content {
        MessageContent::Text(t) => Ok(t),
        MessageContent::Parts(parts) => {
            // Concatenate all text parts; ignore non-text parts in system messages.
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

/// Convert [`MessageContent`] into Anthropic content blocks.
fn translate_content_blocks(
    content: MessageContent,
) -> Result<Vec<AnthropicContentBlock>, ProviderError> {
    match content {
        MessageContent::Text(t) => Ok(vec![AnthropicContentBlock::Text { text: t }]),
        MessageContent::Parts(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        blocks.push(AnthropicContentBlock::Text { text });
                    }
                    ContentPart::ImageUrl { image_url } => {
                        // A base64 `data:` URI must be sent as an inline base64
                        // source, not as a remote URL (Anthropic rejects that).
                        let source = match tt_shared::messages::parse_data_url(&image_url.url) {
                            Some((media_type, data)) => {
                                AnthropicImageSource::Base64 { media_type, data }
                            }
                            None => AnthropicImageSource::Url { url: image_url.url },
                        };
                        blocks.push(AnthropicContentBlock::Image { source });
                    }
                    ContentPart::InputAudio { .. } => {
                        return Err(ProviderError::Unsupported(
                            "audio input is not supported by the Anthropic adapter".to_string(),
                        ));
                    }
                }
            }
            Ok(blocks)
        }
    }
}

/// Convert a canonical [`ToolChoice`] to an [`AnthropicToolChoice`].
fn translate_tool_choice(choice: ToolChoice) -> AnthropicToolChoice {
    match choice {
        ToolChoice::Auto(s) if s == "none" => AnthropicToolChoice::None,
        ToolChoice::Auto(s) if s == "required" => AnthropicToolChoice::Any,
        ToolChoice::Auto(_) => AnthropicToolChoice::Auto, // "auto" + any unknown string
        ToolChoice::Specific { function, .. } => AnthropicToolChoice::Tool {
            name: function.name,
        },
    }
}

// ---------------------------------------------------------------------------
// Response translation: Anthropic → canonical
// ---------------------------------------------------------------------------

/// Deserialize and translate an Anthropic JSON response body into a canonical
/// [`ChatCompletionResponse`].
pub fn deserialize_response(body: &str) -> Result<ChatCompletionResponse, ProviderError> {
    let resp: AnthropicResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Deserialize(e.to_string()))?;
    Ok(translate_response(resp))
}

/// Translate an [`AnthropicResponse`] into a canonical [`ChatCompletionResponse`].
pub fn translate_response(resp: AnthropicResponse) -> ChatCompletionResponse {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in resp.content {
        match block {
            AnthropicResponseBlock::Text { text } => text_parts.push(text),
            AnthropicResponseBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    r#type: "function".to_string(),
                    function: ToolCallFunction {
                        name,
                        arguments: input.to_string(),
                    },
                });
            }
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(MessageContent::Text(text_parts.join("")))
    };

    let finish_reason = resp.stop_reason.as_deref().map(map_stop_reason);

    let message = Message::Assistant {
        content,
        tool_calls,
        name: None,
    };

    let usage = translate_usage(resp.usage);

    ChatCompletionResponse {
        id: resp.id,
        object: "chat.completion".to_string(),
        created: Utc::now().timestamp(),
        model: resp.model,
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: finish_reason.map(str::to_string),
        }],
        usage,
    }
}

/// Map Anthropic `stop_reason` to OpenAI `finish_reason`.
pub fn map_stop_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        "end_turn" => "stop",
        "max_tokens" => "length",
        "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        _ => "stop",
    }
}

/// Convert [`AnthropicUsage`] into canonical [`Usage`].
pub fn translate_usage(u: AnthropicUsage) -> Usage {
    // OpenAI subset convention (what `tt-core::compute_cost` assumes):
    // `prompt_tokens` is the TOTAL input INCLUDING cache reads, and
    // `cached_tokens` is the cache-read subset. Anthropic reports
    // `input_tokens` EXCLUSIVE of cache reads, so add them back — otherwise
    // compute_cost bills only the fresh tokens and drops the cache-read cost.
    let cached = u.cache_read_input_tokens.unwrap_or(0);
    let created = u.cache_creation_input_tokens.unwrap_or(0);
    // Full prompt = fresh input + cache reads + cache creation, so total_tokens
    // reflects the real prompt size and total == prompt + completion holds.
    let prompt_tokens = u.input_tokens + cached + created;
    Usage {
        prompt_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: prompt_tokens + u.output_tokens,
        cached_tokens: cached,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
    }
}

// ---------------------------------------------------------------------------
// Tests (unit level; see tests/ for snapshot tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tt_shared::{
        messages::{Message, MessageContent, Tool, ToolFunction},
        ChatCompletionRequest,
    };

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
            tt_extras: HashMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn basic_translate_passes_through() {
        let req = base_request("claude-sonnet-4-6");
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.model, "claude-sonnet-4-6");
        assert_eq!(body.max_tokens, 512);
        assert_eq!(body.temperature, Some(0.7));
        assert!(body.system.is_none());
    }

    #[test]
    fn max_tokens_defaults_to_4096() {
        let mut req = base_request("claude-sonnet-4-6");
        req.max_tokens = None;
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.max_tokens, 4096);
    }

    #[test]
    fn system_message_extracted_to_top_level() {
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![
            Message::System {
                content: MessageContent::Text("Be helpful.".to_string()),
            },
            Message::User {
                content: MessageContent::Text("Hello".to_string()),
                name: None,
            },
        ];
        let body = translate_request(req).expect("translate ok");
        let system = body.system.expect("system should be present");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0].text, "Be helpful.");
        // Conversation messages should not contain the system message.
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
    }

    #[test]
    fn short_system_no_cache_control() {
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![
            Message::System {
                content: MessageContent::Text("Short system.".to_string()),
            },
            Message::User {
                content: MessageContent::Text("Hi".to_string()),
                name: None,
            },
        ];
        let body = translate_request(req).expect("translate ok");
        let system = body.system.expect("system should be present");
        assert!(
            system[0].cache_control.is_none(),
            "short system should have no cache_control"
        );
    }

    #[test]
    fn long_system_gets_cache_control() {
        // ~1500 tokens × 4 chars/token = 6000 chars
        let long_text = "a".repeat(6000);
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![
            Message::System {
                content: MessageContent::Text(long_text),
            },
            Message::User {
                content: MessageContent::Text("Hi".to_string()),
                name: None,
            },
        ];
        let body = translate_request(req).expect("translate ok");
        let system = body.system.expect("system should be present");
        assert!(
            system[0].cache_control.is_some(),
            "long system should have cache_control"
        );
        assert_eq!(system[0].cache_control.as_ref().unwrap().ctype, "ephemeral");
    }

    #[test]
    fn tt_extras_not_in_output() {
        let mut req = base_request("claude-sonnet-4-6");
        req.tt_extras
            .insert("route_hint".to_string(), serde_json::json!("us-east-1"));
        let body = translate_request(req).expect("translate ok");
        let serialized = serde_json::to_string(&body).expect("serialize ok");
        assert!(!serialized.contains("tt_extras"));
        assert!(!serialized.contains("route_hint"));
    }

    #[test]
    fn tool_translated_to_anthropic_format() {
        let mut req = base_request("claude-sonnet-4-6");
        req.tools = vec![Tool {
            r#type: "function".to_string(),
            function: ToolFunction {
                name: "search".to_string(),
                description: Some("Search the web".to_string()),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            },
        }];
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.tools.len(), 1);
        assert_eq!(body.tools[0].name, "search");
        assert_eq!(body.tools[0].description.as_deref(), Some("Search the web"));
    }

    #[test]
    fn tool_choice_auto_string_translates() {
        use tt_shared::messages::ToolChoice;
        let mut req = base_request("claude-sonnet-4-6");
        req.tool_choice = Some(ToolChoice::Auto("auto".to_string()));
        let body = translate_request(req).expect("translate ok");
        assert!(matches!(body.tool_choice, Some(AnthropicToolChoice::Auto)));
    }

    #[test]
    fn tool_choice_specific_translates() {
        use tt_shared::messages::{ToolChoice, ToolChoiceFunction};
        let mut req = base_request("claude-sonnet-4-6");
        req.tool_choice = Some(ToolChoice::Specific {
            r#type: "function".to_string(),
            function: ToolChoiceFunction {
                name: "my_tool".to_string(),
            },
        });
        let body = translate_request(req).expect("translate ok");
        assert!(matches!(
            body.tool_choice,
            Some(AnthropicToolChoice::Tool { name }) if name == "my_tool"
        ));
    }

    #[test]
    fn tool_choice_required_translates_to_any() {
        use tt_shared::messages::ToolChoice;
        let mut req = base_request("claude-sonnet-4-6");
        req.tool_choice = Some(ToolChoice::Auto("required".to_string()));
        let body = translate_request(req).expect("translate ok");
        assert!(matches!(body.tool_choice, Some(AnthropicToolChoice::Any)));
        // Wire format: required → {"type":"any"}.
        let v = serde_json::to_value(body.tool_choice.unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({ "type": "any" }));
    }

    #[test]
    fn tool_choice_none_translates_to_none() {
        use tt_shared::messages::ToolChoice;
        let mut req = base_request("claude-sonnet-4-6");
        req.tool_choice = Some(ToolChoice::Auto("none".to_string()));
        let body = translate_request(req).expect("translate ok");
        assert!(matches!(body.tool_choice, Some(AnthropicToolChoice::None)));
        // Wire format: none → {"type":"none"} (Anthropic "disable all tool use").
        let v = serde_json::to_value(body.tool_choice.unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({ "type": "none" }));
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason("end_turn"), "stop");
        assert_eq!(map_stop_reason("max_tokens"), "length");
        assert_eq!(map_stop_reason("stop_sequence"), "stop");
        assert_eq!(map_stop_reason("tool_use"), "tool_calls");
    }

    #[test]
    fn usage_translation_with_cache_fields() {
        let u = AnthropicUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: Some(20),
            cache_read_input_tokens: Some(80),
        };
        let usage = translate_usage(u);
        // prompt_tokens is the FULL input: fresh + cache reads + cache creation
        // (100 + 80 + 20 = 200); cached_tokens is the cache-read subset.
        assert_eq!(usage.prompt_tokens, 200);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 250);
        assert_eq!(usage.cached_tokens, 80);
        assert_eq!(usage.cache_creation_input_tokens, Some(20));
    }

    #[test]
    fn cache_control_uses_char_count_not_bytes() {
        use tt_shared::messages::{Message, MessageContent};
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![Message::System {
            content: MessageContent::Text("中".repeat(1400)),
        }];
        let body = translate_request(req).expect("translate ok");
        let sys = body.system.expect("system blocks present");
        assert!(
            sys.last().unwrap().cache_control.is_none(),
            "must not cache a sub-1024-token block"
        );

        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![Message::System {
            content: MessageContent::Text("a".repeat(4096)),
        }];
        let body = translate_request(req).expect("translate ok");
        assert!(body.system.unwrap().last().unwrap().cache_control.is_some());
    }
}
