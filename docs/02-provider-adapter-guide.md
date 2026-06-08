# TokenTrimmer Gateway — Provider Adapter Guide

**Status:** v1 spec
**Audience:** Core team and external contributors adding new provider integrations

---

## Purpose

This document defines the contract every provider adapter must implement, the translation patterns for converting OpenAI-format requests into provider-native formats and back, the streaming protocol expectations, and the error-handling discipline.

Read this before writing a new provider adapter. Reference this when reviewing PRs that add or modify adapters.

---

## 1. Design principles

1. **OpenAI format is the source of truth.** Customers send OpenAI-format requests. Gateway translates outward to providers, then back to OpenAI on the response. Customers never see provider-native formats.
2. **Adapters are isolated.** Each provider lives in its own crate (`crates/providers/<name>`). Adapters depend on `shared` types only, not on each other.
3. **Adapters are stateless.** No instance state beyond the HTTP client and pricing table. State (auth, telemetry, routing decisions) is passed in via `RequestContext`.
4. **Adapters do not log.** Logging happens at the Gateway core layer. Adapters return rich errors; core decides what to record.
5. **Adapters are tested in isolation.** Each adapter has its own fixtures, mock server, and contract test suite. CI runs them independently.
6. **Streaming is a first-class concern.** An adapter is not complete until it handles streaming correctly.

---

## 2. The `Provider` trait

The contract every adapter implements. Defined in `crates/shared/src/provider.rs`.

```rust
use async_trait::async_trait;
use futures_core::stream::BoxStream;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Unique provider identifier (lowercase, no spaces).
    /// Examples: "openai", "anthropic", "gemini", "ollama"
    fn id(&self) -> &'static str;

    /// Chat completion (non-streaming).
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError>;

    /// Chat completion (streaming).
    /// Returns a stream of OpenAI-format chunks.
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>;

    /// Embeddings (uniform OpenAI format).
    /// Return an error if the provider does not support embeddings.
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError>;

    /// Per-model pricing lookup. Drawn from the manually-curated
    /// `data/pricing.toml` snapshot embedded at build time — rates are updated
    /// by hand, not automatically. Returns `None` only when the model is absent.
    fn pricing(&self, model: &str) -> Option<ModelPricing>;

    /// List of model identifiers this provider serves.
    /// Used for routing validation and pricing table generation.
    fn models(&self) -> Vec<ModelInfo>;

    // --- default-method hooks (override only when the provider needs it) ---

    /// Cost multiplier for a provider surcharge on top of the model cost (e.g.
    /// OpenRouter's 5% BYOK fee). Default `1.0` (no surcharge).
    fn fee_multiplier(&self) -> f64 {
        1.0
    }

    /// Names of request params this adapter **silently drops** for `req`
    /// because the upstream rejects them. The gateway surfaces each as
    /// `X-TokenTrimmer-Warnings: param_dropped:<name>`. Default: nothing dropped.
    fn dropped_params(&self, _req: &ChatCompletionRequest) -> Vec<String> {
        Vec::new()
    }

    /// Whether this provider honors `response_format: json_schema`. Default
    /// `true` (forward verbatim). Override `false` for a `json_object`-only
    /// provider — the gateway then downgrades with a `response_format_downgrade`
    /// warning.
    fn supports_response_schema(&self) -> bool {
        true
    }

    /// Accepted `temperature` range `(min, max)`. The gateway clamps an
    /// out-of-range value to this and emits `temperature_clamped`. Default
    /// `(0.0, 2.0)` (the widest common range). Override only with a narrower
    /// range you are confident is correct.
    fn temperature_range(&self) -> (f32, f32) {
        (0.0, 2.0)
    }

    /// Provider liveness check. The default is a no-op `Ok(())` — it should NOT
    /// call the provider's pricey endpoints. Override for a real ping.
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}
```

The four default-method hooks above (`fee_multiplier`, `dropped_params`,
`supports_response_schema`, `temperature_range`) drive behaviors described
elsewhere in this guide — the `param_dropped` warnings, `response_format`
downgrade, temperature clamping, and the BYOK fee surcharge. A new adapter that
forgets to override them inherits the defaults silently, so review them when
adding a provider (see §6).

### Supporting types (shared)

```rust
// crates/shared/src/types.rs

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: Option<bool>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    pub response_format: Option<ResponseFormat>,
    pub stop: Option<StopSequences>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub n: Option<u32>,
    pub seed: Option<u64>,
    pub user: Option<String>,
    /// TokenTrimmer extensions (forwarded via extras, not sent to providers)
    #[serde(skip)]
    pub tt_extras: TtExtras,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System { content: MessageContent },
    User { content: MessageContent, name: Option<String> },
    Assistant {
        content: Option<MessageContent>,
        tool_calls: Option<Vec<ToolCall>>,
        name: Option<String>,
    },
    Tool { content: MessageContent, tool_call_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
    InputAudio { input_audio: AudioInput },
}

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub trace_id: Uuid,
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    pub credentials: ProviderCredentials,
    pub tag: Option<String>,
    pub deadline: Instant,
}

#[derive(Clone, Debug)]
pub struct ProviderCredentials {
    pub api_key: SecretString,
    pub base_url: Option<String>,    // for self-hosted overrides
    pub extra_headers: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct ModelPricing {
    pub input_per_million: Decimal,   // USD per 1M input tokens
    pub output_per_million: Decimal,
    pub cached_input_per_million: Option<Decimal>,  // if provider supports prompt caching
    pub effective_at: DateTime<Utc>,
}
```

> **Security — customer-supplied `base_url` / `extra_headers`:** these are untrusted in pass-through/BYOK mode and are validated by `crates/shared/src/url_guard.rs` before any request is dispatched. `validate_provider_url` rejects loopback/private/link-local/ULA/CGNAT/cloud-metadata hosts (https-only unless local providers are allowed, plus a best-effort DNS check), and `filter_extra_headers` strips headers that could override gateway auth/routing or inject hop-by-hop headers (`authorization`, `x-api-key`, `host`, `content-type`, `anthropic-version`, and the hop-by-hop set). The DNS check is defense-in-depth only (DNS-rebind/TOCTOU is out of scope — operators should add a network policy). See API reference §2.4. New adapters get this for free via the shared OpenAI-compatible base; do not bypass it.

### Error type

```rust
// crates/shared/src/error.rs

#[derive(thiserror::Error, Debug)]
pub enum ProviderError {
    #[error("authentication failed")]
    Unauthorized,

    #[error("rate limited (retry after {retry_after_ms}ms)")]
    RateLimited { retry_after_ms: u64 },

    #[error("model not found: {model}")]
    ModelNotFound { model: String },

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("provider returned error {status}: {message}")]
    ProviderUpstream { status: u16, message: String },

    #[error("timeout after {ms}ms")]
    Timeout { ms: u64 },

    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("deserialization error: {0}")]
    Deserialize(String),

    #[error("provider does not support this operation: {0}")]
    Unsupported(String),

    #[error("internal: {0}")]
    Internal(String),
}

impl ProviderError {
    /// Whether the request should be retried (with backoff) on this error.
    pub fn is_retriable(&self) -> bool {
        // Use `match` (not `matches!`): the `status >= 500` guard binds only in
        // the arm where `status` is in scope — a guard in `matches!` cannot span
        // multiple `|` alternatives.
        match self {
            ProviderError::RateLimited { .. } => true,
            ProviderError::Timeout { .. } => true,
            ProviderError::Network(_) => true,
            ProviderError::ProviderUpstream { status, .. } => *status >= 500,
            _ => false,
        }
    }
}
```

---

## 3. Translation patterns

### 3.1 Inbound (OpenAI → provider native)

Each provider has its own request format. The adapter is responsible for:
- Mapping `messages` to the provider's message format
- Extracting and relocating system messages if the provider treats them specially
- Translating `tools` / function definitions
- Mapping parameter names and ranges (temperature scales differ, `max_tokens` semantics differ, etc.)
- Handling multimodal content (images, audio)
- Preserving extras that have no provider equivalent (and noting them in telemetry)

### 3.2 Outbound (provider native → OpenAI)

The adapter is responsible for:
- Returning a `ChatCompletionResponse` with `id`, `model`, `created`, `usage`, `choices` populated
- Populating `usage.prompt_tokens`, `usage.completion_tokens`, `usage.cached_tokens` (if available)
- Mapping provider-specific fields (e.g., Anthropic `stop_reason`) to OpenAI equivalents
- Preserving tool calls in OpenAI format
- Handling cases where the provider's response is partial or empty

### 3.3 Streaming translation

Provider streaming formats vary:
- **OpenAI:** SSE with `data: {json}` events, ends with `data: [DONE]`
- **Anthropic:** SSE with typed events (`message_start`, `content_block_delta`, `message_delta`, `message_stop`, etc.)
- **Gemini:** Server-streamed JSON array, no SSE wrapper

The adapter must:
1. Establish the provider stream
2. Parse incoming chunks
3. Emit OpenAI-format `ChatCompletionChunk` events
4. Aggregate token counts across the stream
5. Emit a final chunk with `finish_reason` set
6. Close cleanly on error or completion

A helper crate `crates/shared/src/sse.rs` provides SSE parsing utilities.

---

## 4. Worked example: Anthropic adapter

The Anthropic adapter is the most complex non-OpenAI provider because it has its own message format, separate system prompt handling, and a typed-event streaming protocol. Use this as the reference pattern.

### 4.1 Crate layout

```
crates/providers/anthropic/
├── src/
│   ├── lib.rs              # Provider trait impl
│   ├── client.rs           # HTTP client setup
│   ├── translate.rs        # request/response translation
│   ├── stream.rs           # streaming translation
│   ├── pricing.rs          # pricing table
│   └── errors.rs           # provider-specific error mapping
├── tests/
│   ├── translate_test.rs
│   ├── stream_test.rs
│   └── fixtures/
└── Cargo.toml
```

### 4.2 Request translation — non-streaming

```rust
// crates/providers/anthropic/src/translate.rs

use shared::types::*;
use serde::{Serialize, Deserialize};

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicSystemBlock>>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,          // "user" or "assistant"
    content: Vec<AnthropicContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text { text: String },
    Image { source: AnthropicImageSource },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

#[derive(Serialize)]
struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<AnthropicCacheControl>,
}

#[derive(Serialize)]
struct AnthropicCacheControl {
    #[serde(rename = "type")]
    ctype: String,    // "ephemeral"
}

pub fn translate_request(req: &ChatCompletionRequest) -> Result<AnthropicRequest, ProviderError> {
    // 1. Separate system messages from conversation messages
    let mut system_blocks = Vec::new();
    let mut conversation = Vec::new();

    for msg in &req.messages {
        match msg {
            Message::System { content } => {
                let text = content.as_text()?;
                system_blocks.push(AnthropicSystemBlock {
                    block_type: "text".to_string(),
                    text,
                    cache_control: None,
                });
            }
            Message::User { content, .. } => {
                conversation.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: translate_content_to_anthropic(content)?,
                });
            }
            Message::Assistant { content, tool_calls, .. } => {
                let mut blocks = Vec::new();
                if let Some(c) = content {
                    blocks.extend(translate_content_to_anthropic(c)?);
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        blocks.push(AnthropicContentBlock::ToolUse {
                            id: call.id.clone(),
                            name: call.function.name.clone(),
                            input: serde_json::from_str(&call.function.arguments)
                                .map_err(|e| ProviderError::Deserialize(e.to_string()))?,
                        });
                    }
                }
                conversation.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content: blocks,
                });
            }
            Message::Tool { content, tool_call_id } => {
                conversation.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![AnthropicContentBlock::ToolResult {
                        tool_use_id: tool_call_id.clone(),
                        content: content.as_text()?,
                        is_error: false,
                    }],
                });
            }
        }
    }

    // 2. Apply automatic cache_control to last system block if it's long enough
    if let Some(last) = system_blocks.last_mut() {
        if count_tokens(&last.text) >= 1024 {
            last.cache_control = Some(AnthropicCacheControl {
                ctype: "ephemeral".to_string(),
            });
        }
    }

    // 3. Translate tools
    let tools = req.tools.as_ref().map(|ts| {
        ts.iter().map(translate_tool).collect()
    });

    // 4. max_tokens is required by Anthropic
    let max_tokens = req.max_tokens.unwrap_or(4096);

    Ok(AnthropicRequest {
        model: req.model.clone(),
        messages: conversation,
        system: if system_blocks.is_empty() { None } else { Some(system_blocks) },
        max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stop_sequences: req.stop.as_ref().map(|s| s.to_vec()),
        tools,
        stream: req.stream,
    })
}
```

**Key translation notes:**

1. **System messages move out.** Anthropic separates `system` from `messages`. Multiple system messages in the OpenAI request get concatenated into the `system` block array.
2. **Auto-cache on long system prompts.** If the last system block is ≥ 1,024 tokens, automatically apply `cache_control: ephemeral`. This is one of the biggest cost wins available — bake it in by default with config to disable.
3. **`max_tokens` is required.** OpenAI lets you omit it; Anthropic doesn't. Default to 4096.
4. **Tool results are wrapped in user messages.** Anthropic doesn't have a dedicated `tool` role — tool results come back as user messages with `tool_result` content blocks.
5. **Tool calls become `tool_use` content blocks.** Different from OpenAI's `tool_calls` array on assistant messages.

### 4.3 Response translation — non-streaming

```rust
#[derive(Deserialize)]
struct AnthropicResponse {
    id: String,
    #[serde(rename = "type")]
    response_type: String,
    role: String,
    content: Vec<AnthropicResponseBlock>,
    model: String,
    stop_reason: String,
    stop_sequence: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicResponseBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
}

pub fn translate_response(resp: AnthropicResponse) -> ChatCompletionResponse {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in resp.content {
        match block {
            AnthropicResponseBlock::Text { text } => text_parts.push(text),
            AnthropicResponseBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: input.to_string(),
                    },
                });
            }
        }
    }

    let content = if text_parts.is_empty() { None } else {
        Some(MessageContent::Text(text_parts.join("")))
    };

    let finish_reason = match resp.stop_reason.as_str() {
        "end_turn" => "stop",
        "max_tokens" => "length",
        "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        other => other,
    };

    ChatCompletionResponse {
        id: resp.id,
        object: "chat.completion".to_string(),
        created: Utc::now().timestamp() as u64,
        model: resp.model,
        choices: vec![Choice {
            index: 0,
            message: AssistantMessage {
                role: "assistant".to_string(),
                content,
                tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            },
            finish_reason: Some(finish_reason.to_string()),
        }],
        usage: Usage {
            prompt_tokens: resp.usage.input_tokens,
            completion_tokens: resp.usage.output_tokens,
            total_tokens: resp.usage.input_tokens + resp.usage.output_tokens,
            cached_tokens: resp.usage.cache_read_input_tokens,
        },
    }
}
```

### 4.4 Streaming translation

Anthropic streams typed SSE events. Translate them into OpenAI-format chunks:

```rust
// crates/providers/anthropic/src/stream.rs

use shared::sse::SseStream;
use async_stream::try_stream;

pub fn translate_stream(
    sse: SseStream,
    response_id: String,
    model: String,
) -> impl Stream<Item = Result<ChatCompletionChunk, ProviderError>> {
    try_stream! {
        let mut current_tool_call: Option<PartialToolCall> = None;

        for await event in sse {
            let event = event?;
            match event.event_type.as_str() {
                "message_start" => {
                    // initial chunk with role
                    yield empty_chunk_with_role(&response_id, &model);
                }
                "content_block_start" => {
                    let data: ContentBlockStart = serde_json::from_str(&event.data)?;
                    if let ContentBlock::ToolUse { id, name, .. } = data.content_block {
                        current_tool_call = Some(PartialToolCall {
                            id, name, arguments: String::new(),
                        });
                    }
                }
                "content_block_delta" => {
                    let data: ContentBlockDelta = serde_json::from_str(&event.data)?;
                    match data.delta {
                        Delta::TextDelta { text } => {
                            yield text_chunk(&response_id, &model, &text);
                        }
                        Delta::InputJsonDelta { partial_json } => {
                            if let Some(tc) = current_tool_call.as_mut() {
                                tc.arguments.push_str(&partial_json);
                                yield tool_call_chunk(&response_id, &model, tc);
                            }
                        }
                    }
                }
                "content_block_stop" => {
                    current_tool_call = None;
                }
                "message_delta" => {
                    let data: MessageDelta = serde_json::from_str(&event.data)?;
                    // contains stop_reason and final usage
                    yield final_chunk(&response_id, &model, data.delta.stop_reason, data.usage);
                }
                "message_stop" => {
                    // terminal event; nothing to emit
                    break;
                }
                "ping" => continue,
                _ => continue,
            }
        }
    }
}
```

**Key streaming notes:**

1. **Anthropic's tool-use deltas are JSON fragments.** They accumulate into a single tool call. OpenAI emits the same logical events but as separate `tool_calls` array updates.
2. **Final usage arrives in `message_delta`**, not `message_stop`. Capture it there.
3. **`ping` events are heartbeats.** Ignore them.
4. **Stream errors must propagate.** A mid-stream provider error must close the stream with an error, not silently truncate.

### 4.5 Error mapping

Provider-specific error mapping:

```rust
// crates/providers/anthropic/src/errors.rs

pub fn map_status(status: u16, body: &str) -> ProviderError {
    let parsed: Option<AnthropicErrorBody> = serde_json::from_str(body).ok();
    let message = parsed
        .as_ref()
        .map(|p| p.error.message.clone())
        .unwrap_or_else(|| body.to_string());

    match status {
        401 => ProviderError::Unauthorized,
        429 => {
            let retry_after_ms = parsed
                .and_then(|p| p.error.retry_after_seconds)
                .map(|s| s * 1000)
                .unwrap_or(1000);
            ProviderError::RateLimited { retry_after_ms }
        }
        400 => ProviderError::InvalidRequest(message),
        404 => ProviderError::ModelNotFound { model: extract_model(&message) },
        500..=599 => ProviderError::ProviderUpstream { status, message },
        _ => ProviderError::ProviderUpstream { status, message },
    }
}
```

### 4.6 Pricing table

```rust
// crates/providers/anthropic/src/pricing.rs

pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    match model {
        "claude-3-5-sonnet-20241022" | "claude-3-5-sonnet" => Some(ModelPricing {
            input_per_million: dec!(3.00),
            output_per_million: dec!(15.00),
            cached_input_per_million: Some(dec!(0.30)),  // 90% discount on cache reads
            effective_at: Utc::now(),
        }),
        "claude-3-5-haiku-20241022" | "claude-3-5-haiku" => Some(ModelPricing {
            input_per_million: dec!(0.80),
            output_per_million: dec!(4.00),
            cached_input_per_million: Some(dec!(0.08)),
            effective_at: Utc::now(),
        }),
        "claude-3-opus-20240229" | "claude-3-opus" => Some(ModelPricing {
            input_per_million: dec!(15.00),
            output_per_million: dec!(75.00),
            cached_input_per_million: Some(dec!(1.50)),
            effective_at: Utc::now(),
        }),
        _ => None,
    }
}
```

Pricing is a manually-curated snapshot in `data/pricing.toml`, embedded at build time and refreshed on release cadence (not auto-refreshed). Each entry carries an `effective_at` timestamp so historical telemetry replays against the rate that was in effect. Auto-refresh from a config URL is a possible future improvement, but today a price update means editing `data/pricing.toml` and cutting a release.

---

## 5. Provider-specific notes

### 5.1 OpenAI

- Largely 1:1 mapping. Adapter is the simplest.
- Watch for: `o1` / `o3` reasoning models have different parameter constraints (no `temperature`, different `max_completion_tokens` name).
- Streaming: standard SSE, terminator `data: [DONE]`.
- Prompt caching: automatic; surfaced in `usage.prompt_tokens_details.cached_tokens`.

### 5.2 Anthropic

- See worked example above.
- Watch for: `max_tokens` required; cache_control must be explicit; tool format differs.
- Streaming: typed SSE events.
- Prompt caching: explicit via `cache_control` blocks.

### 5.3 Google Gemini

- Major differences:
  - REST endpoint structure is different (`/v1beta/models/{model}:generateContent`)
  - System prompt is `systemInstruction`, separate field
  - Tools are `tools` but format differs (uses `functionDeclarations`)
  - Streaming uses `:streamGenerateContent` endpoint, returns JSON array
- Pricing varies significantly by context length (128K vs 1M+)
- Context caching available via `cachedContent` resource (separate API)

### 5.4 Mistral, Groq, Together AI, OpenRouter

- All OpenAI-compatible. Adapter is mostly pass-through with these differences:
  - Different `base_url`
  - Some models support different parameter ranges
  - Pricing tables vary
- These can share a common `OpenAICompatibleProvider` implementation with provider-specific pricing and model lists.

### 5.5 Local providers (Ollama, vLLM, LM Studio)

- All OpenAI-compatible by design.
- Adapter sets `cost_per_million = 0` for all models.
- Differences:
  - Often slower
  - Often less reliable (model loading, GPU contention)
  - Should default to higher timeouts
  - Default base URLs: Ollama `http://localhost:11434/v1`, vLLM `http://localhost:8000/v1`, LM Studio `http://localhost:1234/v1`

These share an `OpenAICompatibleProvider` impl with `is_local: true` flag.

---

## 6. Adding a new provider — checklist

When you're adding a new provider, work through this list:

- [ ] Create `crates/providers/<name>/` with `Cargo.toml` and `src/lib.rs`
- [ ] Add to workspace `Cargo.toml`
- [ ] Implement `Provider` trait
- [ ] Review the default-method hooks and override any the provider needs: `dropped_params` (params the upstream rejects → `param_dropped` warnings), `supports_response_schema` (set `false` for `json_object`-only providers), `temperature_range` (narrower than `(0.0, 2.0)` only if certain), `fee_multiplier` (BYOK/surcharge)
- [ ] Implement `translate_request` (OpenAI → provider format)
- [ ] Implement `translate_response` (provider → OpenAI format)
- [ ] Implement streaming translation
- [ ] Implement error mapping (provider error format → `ProviderError`)
- [ ] Populate pricing table for all supported models
- [ ] Add `models()` returning provider model list
- [ ] Add fixtures in `tests/fixtures/` for sample requests and responses
- [ ] Write `tests/translate_test.rs` covering at least: text-only, multimodal, tool use, multi-turn
- [ ] Write `tests/stream_test.rs` covering at least: text stream, tool-use stream, error mid-stream, early disconnect
- [ ] Add to provider registry in `crates/core/src/registry.rs`
- [ ] Document in `docs/providers/<name>.md` covering: setup, environment variables, supported models, known limitations
- [ ] Add CI matrix entry for the provider
- [ ] Add provider to README provider list

---

## 7. Testing patterns

### 7.1 Unit tests — translation

Each translation function gets fixture-driven tests:

```rust
#[test]
fn translate_request_handles_system_message() {
    let input = include_str!("fixtures/request_with_system.json");
    let req: ChatCompletionRequest = serde_json::from_str(input).unwrap();

    let translated = translate_request(&req).unwrap();

    insta::assert_json_snapshot!(translated);
}
```

Use `insta` for snapshot testing — golden output files reviewed on change.

### 7.2 Integration tests — mock server

Use `httpmock` to spin up a fake provider:

```rust
#[tokio::test]
async fn handles_rate_limit_with_retry() {
    let server = MockServer::start();

    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(429).header("retry-after", "1").body(r#"{"error": {"type": "rate_limit"}}"#);
    });

    let provider = AnthropicProvider::with_base_url(server.base_url());
    let result = provider.chat_completion(test_request(), &test_ctx()).await;

    assert!(matches!(result, Err(ProviderError::RateLimited { retry_after_ms: 1000 })));
    mock.assert();
}
```

### 7.3 Contract tests — real provider

Weekly CI job that hits real providers with minimal requests (small budget allocated per run):

```rust
#[tokio::test]
#[ignore = "contract test, requires real API key"]
async fn anthropic_real_chat_completion() {
    let provider = AnthropicProvider::from_env();
    let req = minimal_request();
    let resp = provider.chat_completion(req, &test_ctx()).await.unwrap();

    assert!(!resp.choices.is_empty());
    assert!(resp.usage.total_tokens > 0);
}
```

Run with `cargo test -- --ignored` in scheduled CI only.

---

## 8. Versioning and compatibility

- Providers update their APIs. Adapters must remain stable for customers.
- Each adapter has its own version, tracked in the crate's `Cargo.toml`.
- Breaking provider changes require: (1) new model added with new pricing, (2) old model deprecated with warning emitted via telemetry, (3) old model removed after one minor version.
- Customers can pin a provider adapter version via Gateway config.

---

## 9. Common pitfalls

1. **Token counting drift.** Each provider has its own tokenizer. Don't estimate token counts; use the provider's reported counts from the response.
2. **Streaming buffering.** Don't accumulate the full stream before forwarding. Each chunk should pass through with minimal delay.
3. **Hidden defaults.** Providers have defaults that differ from OpenAI. Don't assume; check the docs and surface differences in tests.
4. **Error normalization.** Tempting to map every provider error to `ProviderUpstream`. Don't — use specific variants so the routing engine can make smart decisions (retry, fallback, fail).
5. **Tool format edge cases.** Tool calling diverges most between providers. Test multi-tool, parallel tool calls, and tool errors.
6. **Multimodal handling.** Image and audio inputs are encoded differently per provider. Don't ship multimodal support until you've tested it end-to-end.
7. **System prompt placement.** Some providers separate system; some include it in messages; some only allow one. Get this right per provider.

---

## 10. Reference

- Anthropic API docs: https://docs.anthropic.com
- OpenAI API docs: https://platform.openai.com/docs
- Google Gemini API docs: https://ai.google.dev/api
- Mistral API docs: https://docs.mistral.ai
- Groq API docs: https://console.groq.com/docs

When in doubt, the OpenAI API spec is the canonical reference for the format Gateway expects.
