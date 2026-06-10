//! Ingress translation for the Anthropic Messages API (`POST /v1/messages`).
//!
//! This is the *inbound* counterpart to [`crate::translate`]. Where
//! `translate.rs` converts a canonical [`tt_shared::ChatCompletionRequest`] into
//! Anthropic's wire shape (gateway → upstream), this module converts an inbound
//! Anthropic Messages **request** into the canonical request, and a canonical
//! **response** back into the Anthropic Messages shape (client → gateway → client).
//!
//! It exists so the hosted gateway can expose an Anthropic-native `/v1/messages`
//! ingress (Claude Code, the Anthropic SDKs) while running every request through
//! the same cost / routing / cache / credential pipeline as the OpenAI-compatible
//! `/v1/chat/completions` route — no pipeline fork.
//!
//! # Field coverage
//!
//! Inbound request → canonical:
//! - `system` (string or block array) → a leading [`Message::System`].
//! - `messages` with text / image / `tool_use` / `tool_result` blocks → canonical
//!   user / assistant / tool messages.
//! - `max_tokens`, `temperature`, `top_p`, `stop_sequences`, `stream`.
//! - `tools` (`{name,description,input_schema}`) → OpenAI `{type,function}` tools.
//! - `tool_choice` (`auto`/`any`/`none`/`tool`) → canonical [`ToolChoice`].
//! - `metadata.user_id` → `user`.
//!
//! Canonical response → Anthropic Messages:
//! - assistant text + `tool_calls` → `content` blocks (`text` / `tool_use`).
//! - `finish_reason` → `stop_reason`; `usage` → Anthropic `usage` (input/output +
//!   cache read/creation), reversing [`crate::translate::translate_usage`].

use serde::Deserialize;
use serde_json::{json, Value};
use tt_shared::{
    messages::{
        ContentPart, ImageUrl, Message, MessageContent, Tool, ToolCall, ToolCallFunction,
        ToolChoice, ToolChoiceFunction, ToolFunction,
    },
    ChatCompletionRequest, ChatCompletionResponse, ProviderError,
};

// ---------------------------------------------------------------------------
// Inbound Anthropic Messages request wire types (Deserialize)
// ---------------------------------------------------------------------------

/// Top-level inbound Anthropic Messages API request.
#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<InboundMessage>,
    #[serde(default)]
    pub system: Option<SystemPrompt>,
    /// Required by Anthropic; the gateway forwards it as-is.
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub tools: Vec<InboundTool>,
    #[serde(default)]
    pub tool_choice: Option<InboundToolChoice>,
    #[serde(default)]
    pub metadata: Option<InboundMetadata>,
}

/// The `system` field is either a bare string or an array of text blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

/// One block in a `system` block array.
#[derive(Debug, Deserialize)]
pub struct SystemBlock {
    #[serde(default)]
    pub text: String,
}

/// One inbound conversation message.
#[derive(Debug, Deserialize)]
pub struct InboundMessage {
    pub role: String,
    pub content: InboundContent,
}

/// Message content: a bare string or an array of typed content blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum InboundContent {
    Text(String),
    Blocks(Vec<InboundBlock>),
}

/// A content block within an inbound message.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundBlock {
    Text {
        text: String,
    },
    Image {
        source: InboundImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Value,
    },
}

/// Image source for an inbound `image` block: a URL or inline base64.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundImageSource {
    Url { url: String },
    Base64 { media_type: String, data: String },
}

/// An inbound tool definition (`{name, description, input_schema}`).
#[derive(Debug, Deserialize)]
pub struct InboundTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Value,
}

/// Inbound `tool_choice` directive.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

/// Inbound request metadata.
#[derive(Debug, Deserialize)]
pub struct InboundMetadata {
    #[serde(default)]
    pub user_id: Option<String>,
}

impl MessagesRequest {
    /// Parse a raw Anthropic Messages JSON body into a [`MessagesRequest`].
    pub fn from_json(body: &[u8]) -> Result<Self, ProviderError> {
        serde_json::from_slice(body).map_err(|e| {
            ProviderError::Deserialize(format!("invalid Anthropic Messages request: {e}"))
        })
    }

    /// Translate this inbound Anthropic request into the canonical
    /// [`ChatCompletionRequest`] consumed by the gateway pipeline.
    pub fn into_chat_request(self) -> Result<ChatCompletionRequest, ProviderError> {
        let mut messages: Vec<Message> = Vec::new();

        // System prompt → a single leading System message (concatenated blocks).
        if let Some(system) = self.system {
            let text = match system {
                SystemPrompt::Text(t) => t,
                SystemPrompt::Blocks(blocks) => blocks
                    .into_iter()
                    .map(|b| b.text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            if !text.is_empty() {
                messages.push(Message::System {
                    content: MessageContent::Text(text),
                });
            }
        }

        for msg in self.messages {
            translate_inbound_message(msg, &mut messages)?;
        }

        let tools: Vec<Tool> = self
            .tools
            .into_iter()
            .map(|t| Tool {
                r#type: "function".to_string(),
                function: ToolFunction {
                    name: t.name,
                    description: t.description,
                    parameters: if t.input_schema.is_null() {
                        json!({})
                    } else {
                        t.input_schema
                    },
                },
            })
            .collect();

        let tool_choice = self.tool_choice.map(|tc| match tc {
            InboundToolChoice::Auto => ToolChoice::Auto("auto".to_string()),
            InboundToolChoice::Any => ToolChoice::Auto("required".to_string()),
            InboundToolChoice::None => ToolChoice::Auto("none".to_string()),
            InboundToolChoice::Tool { name } => ToolChoice::Specific {
                r#type: "function".to_string(),
                function: ToolChoiceFunction { name },
            },
        });

        Ok(ChatCompletionRequest {
            model: self.model,
            messages,
            temperature: self.temperature,
            top_p: self.top_p,
            max_tokens: Some(self.max_tokens),
            stream: self.stream,
            tools,
            tool_choice,
            response_format: None,
            stop: self.stop_sequences,
            presence_penalty: None,
            frequency_penalty: None,
            n: None,
            seed: None,
            user: self.metadata.and_then(|m| m.user_id),
            tt_extras: Default::default(),
            ..Default::default()
        })
    }
}

/// Translate one inbound Anthropic message into canonical messages, pushing the
/// result(s) onto `out`. A user turn carrying `tool_result` blocks expands into
/// one canonical [`Message::Tool`] per result (OpenAI represents tool results as
/// their own role), preserving any sibling text.
fn translate_inbound_message(
    msg: InboundMessage,
    out: &mut Vec<Message>,
) -> Result<(), ProviderError> {
    let blocks = match msg.content {
        InboundContent::Text(t) => vec![InboundBlock::Text { text: t }],
        InboundContent::Blocks(b) => b,
    };

    match msg.role.as_str() {
        "assistant" => {
            let mut parts: Vec<ContentPart> = Vec::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            for block in blocks {
                match block {
                    InboundBlock::Text { text } => parts.push(ContentPart::Text { text }),
                    InboundBlock::ToolUse { id, name, input } => tool_calls.push(ToolCall {
                        id,
                        r#type: "function".to_string(),
                        function: ToolCallFunction {
                            name,
                            arguments: input.to_string(),
                        },
                    }),
                    InboundBlock::Image { .. } => {
                        return Err(ProviderError::Unsupported(
                            "image blocks are not valid in an assistant message".to_string(),
                        ))
                    }
                    InboundBlock::ToolResult { .. } => {
                        return Err(ProviderError::Unsupported(
                            "tool_result blocks are not valid in an assistant message".to_string(),
                        ))
                    }
                }
            }
            out.push(Message::Assistant {
                content: content_from_parts(parts),
                tool_calls,
                name: None,
            });
        }
        // Anything else (including the standard "user") is a user turn. Anthropic
        // carries tool results inside a user turn; OpenAI breaks each out into a
        // dedicated tool-role message.
        _ => {
            let mut parts: Vec<ContentPart> = Vec::new();
            for block in blocks {
                match block {
                    InboundBlock::Text { text } => parts.push(ContentPart::Text { text }),
                    InboundBlock::Image { source } => {
                        parts.push(ContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url: match source {
                                    InboundImageSource::Url { url } => url,
                                    InboundImageSource::Base64 { media_type, data } => {
                                        format!("data:{media_type};base64,{data}")
                                    }
                                },
                                detail: None,
                            },
                        });
                    }
                    InboundBlock::ToolResult {
                        tool_use_id,
                        content,
                    } => {
                        // Flush any accumulated user text before the tool message
                        // so ordering is preserved.
                        if let Some(c) = content_from_parts(std::mem::take(&mut parts)) {
                            out.push(Message::User {
                                content: c,
                                name: None,
                            });
                        }
                        out.push(Message::Tool {
                            content: MessageContent::Text(tool_result_text(content)),
                            tool_call_id: tool_use_id,
                        });
                    }
                    InboundBlock::ToolUse { .. } => {
                        return Err(ProviderError::Unsupported(
                            "tool_use blocks are not valid in a user message".to_string(),
                        ))
                    }
                }
            }
            if let Some(c) = content_from_parts(parts) {
                out.push(Message::User {
                    content: c,
                    name: None,
                });
            }
        }
    }
    Ok(())
}

/// Collapse content parts into a [`MessageContent`]: `None` when empty, a bare
/// `Text` when the only part is text, otherwise a `Parts` array.
fn content_from_parts(mut parts: Vec<ContentPart>) -> Option<MessageContent> {
    match parts.len() {
        0 => None,
        1 => match parts.pop().unwrap() {
            ContentPart::Text { text } => Some(MessageContent::Text(text)),
            other => Some(MessageContent::Parts(vec![other])),
        },
        _ => Some(MessageContent::Parts(parts)),
    }
}

/// Extract plain text from an Anthropic `tool_result` content field, which may be
/// a bare string or an array of `{type:"text",text}` blocks.
fn tool_result_text(content: Value) -> String {
    match content {
        Value::String(s) => s,
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Canonical response → Anthropic Messages response
// ---------------------------------------------------------------------------

/// Convert a canonical [`ChatCompletionResponse`] into an Anthropic Messages
/// response body (`{type:"message", role:"assistant", content:[...], ...}`).
///
/// Reverses [`crate::translate::translate_response`]: assistant text and
/// `tool_calls` become `text` / `tool_use` content blocks, `finish_reason`
/// becomes `stop_reason`, and usage is split back into Anthropic's fresh-input /
/// cache-read / cache-creation buckets (reversing `translate_usage`).
pub fn chat_response_to_messages(resp: &ChatCompletionResponse) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut stop_reason = "end_turn".to_string();

    if let Some(choice) = resp.choices.first() {
        if let Some(reason) = choice.finish_reason.as_deref() {
            stop_reason = map_finish_reason(reason).to_string();
        }
        if let Message::Assistant {
            content: msg_content,
            tool_calls,
            ..
        } = &choice.message
        {
            if let Some(text) = msg_content.as_ref().and_then(message_content_text) {
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
            }
            for tc in tool_calls {
                let input: Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_else(|_| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": tc.id,
                    "name": tc.function.name,
                    "input": input,
                }));
                stop_reason = "tool_use".to_string();
            }
        }
    }

    json!({
        "id": resp.id,
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": usage_to_anthropic(&resp.usage),
    })
}

/// Reverse of [`crate::translate::translate_usage`]: split the canonical
/// (cache-inclusive) `prompt_tokens` back into Anthropic's exclusive
/// `input_tokens` plus the cache read / creation buckets.
fn usage_to_anthropic(usage: &tt_shared::usage::Usage) -> Value {
    let cache_read = usage.cached_tokens;
    let cache_creation = usage.cache_creation_input_tokens.unwrap_or(0);
    // `prompt_tokens` is fresh + cache_read + cache_creation; recover fresh input.
    let input_tokens = usage
        .prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_creation);
    let mut out = json!({
        "input_tokens": input_tokens,
        "output_tokens": usage.completion_tokens,
    });
    if cache_read > 0 {
        out["cache_read_input_tokens"] = json!(cache_read);
    }
    if let Some(c) = usage.cache_creation_input_tokens {
        out["cache_creation_input_tokens"] = json!(c);
    }
    out
}

/// Map an OpenAI `finish_reason` back to an Anthropic `stop_reason`.
fn map_finish_reason(finish_reason: &str) -> &'static str {
    match finish_reason {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        // "stop" and anything else → end_turn.
        _ => "end_turn",
    }
}

/// Pull a plain-text string out of a [`MessageContent`] (concatenating text parts).
fn message_content_text(content: &MessageContent) -> Option<String> {
    match content {
        MessageContent::Text(t) => Some(t.clone()),
        MessageContent::Parts(parts) => {
            let text = parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            Some(text)
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming: canonical ChatCompletionChunk frames → Anthropic SSE event frames
// ---------------------------------------------------------------------------

/// Stateful translator from canonical streaming [`ChatCompletionChunk`]s into
/// Anthropic Messages SSE event frames.
///
/// Anthropic's stream is a typed event sequence (`message_start`,
/// `content_block_start`, `content_block_delta`, `content_block_stop`,
/// `message_delta`, `message_stop`) rather than OpenAI's plain `data:` chunks.
/// Feed each canonical chunk to [`push_chunk`](Self::push_chunk) and emit the
/// returned frames; call [`finish`](Self::finish) once the source stream ends to
/// flush the closing `content_block_stop` / `message_delta` / `message_stop`.
///
/// Both text and tool-call deltas are modeled. A single text content block is
/// emitted at index 0; each distinct streamed `tool_calls` entry becomes its own
/// `tool_use` content block (`content_block_start` → `input_json_delta`s →
/// `content_block_stop`) so Claude Code's agentic loop receives runnable tools.
/// Canonical tool-call `arguments` are cumulative (the adapters re-emit the
/// accumulated string), so each delta forwards only the newly-appended suffix.
#[derive(Default)]
pub struct AnthropicSseEncoder {
    started: bool,
    text_block_open: bool,
    /// Next free Anthropic content-block index (text takes 0 when present).
    next_block_index: usize,
    /// Per tool-call-id streaming state, in first-seen order.
    tool_blocks: Vec<ToolBlockState>,
    stop_reason: Option<String>,
    output_tokens: u64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
}

/// Streaming state for one open `tool_use` content block.
struct ToolBlockState {
    /// Canonical tool-call id (matched across chunks to route deltas).
    id: String,
    /// Anthropic content-block index this tool occupies.
    block_index: usize,
    /// Length of the `arguments` string already forwarded as `input_json_delta`,
    /// so the next cumulative snapshot only emits its new suffix.
    emitted_args_len: usize,
}

impl AnthropicSseEncoder {
    /// New encoder with no events emitted yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate one canonical chunk into zero or more Anthropic SSE frames.
    ///
    /// Each returned `String` is a complete `event: <name>\ndata: <json>\n\n`
    /// frame ready to write to the wire.
    pub fn push_chunk(&mut self, chunk: &tt_shared::ChatCompletionChunk) -> Vec<String> {
        let mut frames: Vec<String> = Vec::new();

        if !self.started {
            self.started = true;
            frames.push(sse_frame(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": chunk.id,
                        "type": "message",
                        "role": "assistant",
                        "model": chunk.model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    }
                }),
            ));
        }

        if let Some(choice) = chunk.choices.first() {
            if let Some(text) = &choice.delta.content {
                if !text.is_empty() {
                    if !self.text_block_open {
                        self.text_block_open = true;
                        // Text always claims block index 0 (it is opened before any
                        // tool block in this model).
                        self.next_block_index = self.next_block_index.max(1);
                        frames.push(sse_frame(
                            "content_block_start",
                            &json!({
                                "type": "content_block_start",
                                "index": 0,
                                "content_block": {"type": "text", "text": ""},
                            }),
                        ));
                    }
                    frames.push(sse_frame(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {"type": "text_delta", "text": text},
                        }),
                    ));
                }
            }
            for tc in &choice.delta.tool_calls {
                frames.extend(self.push_tool_call(tc));
            }
            if let Some(reason) = &choice.finish_reason {
                self.stop_reason = Some(map_finish_reason(reason).to_string());
            }
        }

        if let Some(usage) = &chunk.usage {
            self.output_tokens = usage.completion_tokens;
            self.cache_read_tokens = usage.cached_tokens;
            self.cache_creation_tokens = usage.cache_creation_input_tokens.unwrap_or(0);
            self.input_tokens = usage
                .prompt_tokens
                .saturating_sub(self.cache_read_tokens)
                .saturating_sub(self.cache_creation_tokens);
        }

        frames
    }

    /// Translate one canonical streamed [`tt_shared::messages::ToolCall`] into
    /// Anthropic `tool_use` frames.
    ///
    /// On first sight of a tool-call id, opens a `tool_use` content block
    /// (`content_block_start`) at the next free block index. Then forwards the
    /// newly-appended `arguments` suffix as an `input_json_delta`. Canonical
    /// `arguments` snapshots are cumulative, so the prefix already emitted is
    /// stripped to avoid duplicating fragments. The block is closed in
    /// [`finish`](Self::finish).
    fn push_tool_call(&mut self, tc: &tt_shared::messages::ToolCall) -> Vec<String> {
        let mut frames: Vec<String> = Vec::new();

        let pos = match self.tool_blocks.iter().position(|b| b.id == tc.id) {
            Some(pos) => pos,
            None => {
                let block_index = self.next_block_index;
                self.next_block_index += 1;
                self.tool_blocks.push(ToolBlockState {
                    id: tc.id.clone(),
                    block_index,
                    emitted_args_len: 0,
                });
                frames.push(sse_frame(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": block_index,
                        "content_block": {
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": {},
                        },
                    }),
                ));
                self.tool_blocks.len() - 1
            }
        };

        let block = &mut self.tool_blocks[pos];
        let args = &tc.function.arguments;
        // Forward only the suffix not yet emitted. Cumulative snapshots grow by
        // appending, so the already-emitted prefix is a prefix of `args`; if a
        // provider ever sends a non-cumulative fragment we fall back to emitting
        // the whole fragment.
        let new_part = if args.len() >= block.emitted_args_len
            && args.is_char_boundary(block.emitted_args_len)
        {
            &args[block.emitted_args_len..]
        } else {
            args.as_str()
        };
        if !new_part.is_empty() {
            frames.push(sse_frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": block.block_index,
                    "delta": {"type": "input_json_delta", "partial_json": new_part},
                }),
            ));
            block.emitted_args_len = args.len();
        }

        frames
    }

    /// Flush the terminal frames after the source stream ends.
    ///
    /// Emits `content_block_stop` for the text block (when opened) and for every
    /// open `tool_use` block, then `message_delta` (stop_reason + output tokens)
    /// and `message_stop`. A no-op `message_start` is emitted first if no chunk
    /// was ever seen, so the output is always a well-formed Anthropic event
    /// sequence.
    pub fn finish(&mut self) -> Vec<String> {
        let mut frames: Vec<String> = Vec::new();

        if !self.started {
            self.started = true;
            frames.push(sse_frame(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": "",
                        "type": "message",
                        "role": "assistant",
                        "model": "",
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    }
                }),
            ));
        }

        if self.text_block_open {
            self.text_block_open = false;
            frames.push(sse_frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": 0}),
            ));
        }

        // Close every open tool_use block, in the order they were opened.
        for block in std::mem::take(&mut self.tool_blocks) {
            frames.push(sse_frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": block.block_index}),
            ));
        }

        let stop_reason = self
            .stop_reason
            .clone()
            .unwrap_or_else(|| "end_turn".into());
        let mut usage = json!({"output_tokens": self.output_tokens});
        if self.input_tokens > 0 {
            usage["input_tokens"] = json!(self.input_tokens);
        }
        if self.cache_read_tokens > 0 {
            usage["cache_read_input_tokens"] = json!(self.cache_read_tokens);
        }
        if self.cache_creation_tokens > 0 {
            usage["cache_creation_input_tokens"] = json!(self.cache_creation_tokens);
        }
        frames.push(sse_frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
                "usage": usage,
            }),
        ));

        frames.push(sse_frame("message_stop", &json!({"type": "message_stop"})));

        frames
    }
}

/// Format a single Anthropic SSE frame: `event: <name>\ndata: <json>\n\n`.
fn sse_frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Build an Anthropic `event: error` SSE frame from an OpenAI-shaped in-band error
/// payload (`{"error":{"message":..,"type":..}}`).
///
/// The OpenAI-compatible streaming path emits a mid-stream upstream failure as an
/// in-band `data: {"error":{...}}` chunk rather than a typed event. Anthropic's
/// wire instead carries failures as a typed `error` event
/// (`{"type":"error","error":{"type":..,"message":..}}`), which Anthropic-wire
/// clients (Claude Code) surface as an error. This translates the former into the
/// latter so a mid-stream failure is not silently dropped and presented as a clean
/// (but truncated) success.
pub fn anthropic_error_frame(err: &Value) -> String {
    let inner = err.get("error").unwrap_or(err);
    let message = inner
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("upstream error");
    // Map the OpenAI in-band `upstream_error` type to Anthropic's `api_error`;
    // pass through any other concrete type verbatim.
    let err_type = match inner.get("type").and_then(Value::as_str) {
        Some("upstream_error") | None => "api_error",
        Some(other) => other,
    };
    sse_frame(
        "error",
        &json!({
            "type": "error",
            "error": {"type": err_type, "message": message},
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::messages::{Choice, ChunkChoice, ChunkDelta, ToolCall, ToolCallFunction};
    use tt_shared::usage::Usage;
    use tt_shared::ChatCompletionChunk;

    #[test]
    fn simple_text_request_round_trips() {
        let body = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 256,
            "messages": [{"role": "user", "content": "Hello"}],
        });
        let req = MessagesRequest::from_json(body.to_string().as_bytes())
            .unwrap()
            .into_chat_request()
            .unwrap();
        assert_eq!(req.model, "claude-sonnet-4-6");
        assert_eq!(req.max_tokens, Some(256));
        assert_eq!(req.messages.len(), 1);
        assert!(matches!(
            &req.messages[0],
            Message::User { content: MessageContent::Text(t), .. } if t == "Hello"
        ));
    }

    #[test]
    fn system_string_becomes_leading_system_message() {
        let body = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 16,
            "system": "Be brief.",
            "messages": [{"role": "user", "content": "Hi"}],
        });
        let req = MessagesRequest::from_json(body.to_string().as_bytes())
            .unwrap()
            .into_chat_request()
            .unwrap();
        assert!(matches!(
            &req.messages[0],
            Message::System { content: MessageContent::Text(t) } if t == "Be brief."
        ));
        assert!(matches!(&req.messages[1], Message::User { .. }));
    }

    #[test]
    fn system_block_array_concatenates() {
        let body = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 16,
            "system": [
                {"type": "text", "text": "A"},
                {"type": "text", "text": "B"}
            ],
            "messages": [{"role": "user", "content": "Hi"}],
        });
        let req = MessagesRequest::from_json(body.to_string().as_bytes())
            .unwrap()
            .into_chat_request()
            .unwrap();
        assert!(matches!(
            &req.messages[0],
            Message::System { content: MessageContent::Text(t) } if t == "A\nB"
        ));
    }

    #[test]
    fn tools_and_tool_choice_translate() {
        let body = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 16,
            "tools": [{
                "name": "get_weather",
                "description": "Get weather",
                "input_schema": {"type": "object", "properties": {}}
            }],
            "tool_choice": {"type": "tool", "name": "get_weather"},
            "messages": [{"role": "user", "content": "weather?"}],
        });
        let req = MessagesRequest::from_json(body.to_string().as_bytes())
            .unwrap()
            .into_chat_request()
            .unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].function.name, "get_weather");
        assert!(matches!(
            req.tool_choice,
            Some(ToolChoice::Specific { function, .. }) if function.name == "get_weather"
        ));
    }

    #[test]
    fn tool_result_in_user_turn_becomes_tool_message() {
        let body = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 16,
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "SF"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny"}
                ]}
            ],
        });
        let req = MessagesRequest::from_json(body.to_string().as_bytes())
            .unwrap()
            .into_chat_request()
            .unwrap();
        // user, assistant(tool_call), tool
        assert_eq!(req.messages.len(), 3);
        match &req.messages[1] {
            Message::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "toolu_1");
                assert_eq!(tool_calls[0].function.name, "get_weather");
            }
            other => panic!("expected assistant tool_call, got {other:?}"),
        }
        match &req.messages[2] {
            Message::Tool {
                content: MessageContent::Text(t),
                tool_call_id,
            } => {
                assert_eq!(tool_call_id, "toolu_1");
                assert_eq!(t, "sunny");
            }
            other => panic!("expected tool message, got {other:?}"),
        }
    }

    #[test]
    fn image_block_translates_base64_data_url() {
        let body = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]}],
        });
        let req = MessagesRequest::from_json(body.to_string().as_bytes())
            .unwrap()
            .into_chat_request()
            .unwrap();
        match &req.messages[0] {
            Message::User {
                content: MessageContent::Parts(parts),
                ..
            } => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(
                    &parts[1],
                    ContentPart::ImageUrl { image_url } if image_url.url == "data:image/png;base64,AAAA"
                ));
            }
            other => panic!("expected parts user message, got {other:?}"),
        }
    }

    #[test]
    fn response_with_text_maps_to_message_shape() {
        let resp = ChatCompletionResponse {
            id: "msg_abc".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "claude-sonnet-4-6".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("Hi there".to_string())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        };
        let v = chat_response_to_messages(&resp);
        assert_eq!(v["id"], "msg_abc");
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "Hi there");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 100);
        assert_eq!(v["usage"]["output_tokens"], 20);
    }

    #[test]
    fn response_with_tool_calls_maps_to_tool_use_block() {
        let resp = ChatCompletionResponse {
            id: "msg_t".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "claude-sonnet-4-6".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "toolu_9".to_string(),
                        r#type: "function".to_string(),
                        function: ToolCallFunction {
                            name: "get_weather".to_string(),
                            arguments: "{\"city\":\"SF\"}".to_string(),
                        },
                    }],
                    name: None,
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: Usage {
                prompt_tokens: 50,
                completion_tokens: 5,
                total_tokens: 55,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        };
        let v = chat_response_to_messages(&resp);
        assert_eq!(v["content"][0]["type"], "tool_use");
        assert_eq!(v["content"][0]["id"], "toolu_9");
        assert_eq!(v["content"][0]["name"], "get_weather");
        assert_eq!(v["content"][0]["input"]["city"], "SF");
        assert_eq!(v["stop_reason"], "tool_use");
    }

    #[test]
    fn usage_reverses_cache_buckets() {
        // prompt_tokens (110) = fresh(10) + cache_read(80) + cache_creation(20).
        let resp = ChatCompletionResponse {
            id: "msg_c".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "claude-sonnet-4-6".to_string(),
            choices: vec![],
            usage: Usage {
                prompt_tokens: 110,
                completion_tokens: 42,
                total_tokens: 152,
                cached_tokens: 80,
                cache_creation_input_tokens: Some(20),
            },
        };
        let v = chat_response_to_messages(&resp);
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["cache_read_input_tokens"], 80);
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 20);
        assert_eq!(v["usage"]["output_tokens"], 42);
    }

    fn text_chunk(content: Option<&str>, finish: Option<&str>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: "msg_s".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "claude-sonnet-4-6".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: content.map(str::to_string),
                    tool_calls: vec![],
                    extra: Default::default(),
                },
                finish_reason: finish.map(str::to_string),
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn sse_encoder_emits_anthropic_event_sequence() {
        let mut enc = AnthropicSseEncoder::new();
        let mut out = String::new();
        for frame in enc.push_chunk(&text_chunk(None, None)) {
            out.push_str(&frame);
        }
        for frame in enc.push_chunk(&text_chunk(Some("Hi"), None)) {
            out.push_str(&frame);
        }
        for frame in enc.push_chunk(&text_chunk(None, Some("stop"))) {
            out.push_str(&frame);
        }
        for frame in enc.finish() {
            out.push_str(&frame);
        }

        // Ordered Anthropic event names must all appear.
        for ev in [
            "event: message_start",
            "event: content_block_start",
            "event: content_block_delta",
            "event: content_block_stop",
            "event: message_delta",
            "event: message_stop",
        ] {
            assert!(out.contains(ev), "missing {ev} in:\n{out}");
        }
        // Text delta carries the streamed text.
        assert!(out.contains("\"text_delta\""));
        assert!(out.contains("\"text\":\"Hi\""));
        // Stop reason maps stop → end_turn.
        assert!(out.contains("\"stop_reason\":\"end_turn\""));
        // Frames are blank-line terminated.
        assert!(out.ends_with("\n\n"));
    }

    /// A chunk carrying a single streamed tool-call delta with the given
    /// cumulative `arguments` snapshot.
    fn tool_chunk(id: &str, name: &str, arguments: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: "msg_s".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "claude-sonnet-4-6".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: id.to_string(),
                        r#type: "function".to_string(),
                        function: ToolCallFunction {
                            name: name.to_string(),
                            arguments: arguments.to_string(),
                        },
                    }],
                    extra: Default::default(),
                },
                finish_reason: None,
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn sse_encoder_emits_tool_use_block_from_streamed_tool_call() {
        let mut enc = AnthropicSseEncoder::new();
        let mut out = String::new();
        // A leading text token, then a tool call whose arguments arrive as
        // cumulative snapshots (as the anthropic adapter re-emits them).
        for frame in enc.push_chunk(&text_chunk(Some("Let me check"), None)) {
            out.push_str(&frame);
        }
        for frame in enc.push_chunk(&tool_chunk("toolu_1", "get_weather", "{\"city\"")) {
            out.push_str(&frame);
        }
        for frame in enc.push_chunk(&tool_chunk("toolu_1", "get_weather", "{\"city\":\"SF\"}")) {
            out.push_str(&frame);
        }
        for frame in enc.push_chunk(&text_chunk(None, Some("tool_calls"))) {
            out.push_str(&frame);
        }
        for frame in enc.finish() {
            out.push_str(&frame);
        }

        // A tool_use content block is opened at index 1 (text holds index 0).
        assert!(
            out.contains("\"type\":\"tool_use\""),
            "missing tool_use block in:\n{out}"
        );
        assert!(out.contains("\"id\":\"toolu_1\""));
        assert!(out.contains("\"name\":\"get_weather\""));
        // serde_json Value maps serialize keys alphabetically, so a content_block_start
        // frame at index 1 reads `…"index":1,"type":"content_block_start"…`.
        assert!(
            out.contains("\"index\":1,\"type\":\"content_block_start\""),
            "tool block start at index 1 in:\n{out}"
        );
        // Cumulative arguments forwarded as input_json_delta suffixes (no dupes).
        assert!(out.contains("\"input_json_delta\""));
        assert!(out.contains("\"partial_json\":\"{\\\"city\\\""));
        assert!(out.contains("\"partial_json\":\":\\\"SF\\\"}"));
        // Both the text block (0) and the tool block (1) are closed.
        assert!(out.contains("\"index\":0,\"type\":\"content_block_stop\""));
        assert!(out.contains("\"index\":1,\"type\":\"content_block_stop\""));
        // finish_reason tool_calls maps to stop_reason tool_use.
        assert!(out.contains("\"stop_reason\":\"tool_use\""));
    }

    #[test]
    fn sse_encoder_tool_call_without_text_uses_block_index_zero() {
        let mut enc = AnthropicSseEncoder::new();
        let mut out = String::new();
        for frame in enc.push_chunk(&tool_chunk("toolu_a", "do_thing", "{}")) {
            out.push_str(&frame);
        }
        for frame in enc.push_chunk(&text_chunk(None, Some("tool_calls"))) {
            out.push_str(&frame);
        }
        for frame in enc.finish() {
            out.push_str(&frame);
        }
        // No text → the tool block claims index 0.
        assert!(out.contains("\"type\":\"tool_use\""));
        assert!(out.contains("\"index\":0,\"type\":\"content_block_start\""));
        assert!(out.contains("\"index\":0,\"type\":\"content_block_stop\""));
        assert!(out.contains("\"stop_reason\":\"tool_use\""));
    }

    #[test]
    fn sse_encoder_terminal_usage_reverses_buckets() {
        let mut enc = AnthropicSseEncoder::new();
        let mut chunk = text_chunk(Some("x"), Some("stop"));
        chunk.usage = Some(Usage {
            prompt_tokens: 110,
            completion_tokens: 42,
            total_tokens: 152,
            cached_tokens: 80,
            cache_creation_input_tokens: Some(20),
        });
        let _ = enc.push_chunk(&chunk);
        let tail: String = enc.finish().concat();
        assert!(tail.contains("\"output_tokens\":42"));
        assert!(tail.contains("\"input_tokens\":10"));
        assert!(tail.contains("\"cache_read_input_tokens\":80"));
        assert!(tail.contains("\"cache_creation_input_tokens\":20"));
    }

    #[test]
    fn anthropic_error_frame_maps_openai_upstream_error() {
        let openai = json!({"error": {"message": "boom", "type": "upstream_error"}});
        let frame = anthropic_error_frame(&openai);
        assert!(frame.starts_with("event: error\n"), "frame: {frame}");
        assert!(frame.contains("\"type\":\"error\""));
        // upstream_error maps to Anthropic's api_error.
        assert!(frame.contains("\"type\":\"api_error\""));
        assert!(frame.contains("\"message\":\"boom\""));
        assert!(frame.ends_with("\n\n"));
    }

    #[test]
    fn anthropic_error_frame_passes_through_concrete_type() {
        let openai = json!({"error": {"message": "slow down", "type": "overloaded_error"}});
        let frame = anthropic_error_frame(&openai);
        assert!(frame.contains("\"type\":\"overloaded_error\""));
        assert!(frame.contains("\"message\":\"slow down\""));
    }

    #[test]
    fn anthropic_error_frame_defaults_missing_fields() {
        let frame = anthropic_error_frame(&json!({"error": {}}));
        assert!(frame.contains("\"type\":\"api_error\""));
        assert!(frame.contains("\"message\":\"upstream error\""));
    }

    #[test]
    fn sse_encoder_finish_without_chunks_is_well_formed() {
        let mut enc = AnthropicSseEncoder::new();
        let out: String = enc.finish().concat();
        assert!(out.contains("event: message_start"));
        assert!(out.contains("event: message_delta"));
        assert!(out.contains("event: message_stop"));
        // No text block was opened, so no content_block_stop.
        assert!(!out.contains("content_block_stop"));
    }
}
