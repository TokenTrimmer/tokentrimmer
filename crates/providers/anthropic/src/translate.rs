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
//! - **Auto cache_control**: `cache_control: { type: "ephemeral" }` is applied
//!   to the system prefix when its estimated token count meets the model's
//!   per-model prompt-cache minimum (`prompt_cache_min_tokens` from the pricing
//!   catalog; 1024–4096 tokens). Below that minimum Anthropic silently refuses
//!   to cache, so injecting a breakpoint would be a no-op. On **multi-turn**
//!   conversations a second breakpoint is placed on the last message so the
//!   recurring history is a cache-READ on the next turn (Anthropic, unlike
//!   OpenAI, never auto-caches) — see `maybe_inject_message_cache_control`.
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
    /// Extended-thinking config, forwarded VERBATIM from the canonical
    /// request's `extra["thinking"]` when shape-valid and billing-safe (see
    /// [`forwardable_thinking`]). `None` (omitted) when the caller sent no
    /// config or an invalid one — the historical silent-drop semantics, now
    /// reported via `dropped_params("thinking")`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
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
    Text {
        text: String,
        /// Prompt-cache breakpoint; set only on the last block of the last
        /// message (see `maybe_inject_message_cache_control`).
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    /// Image from a URL.
    Image {
        source: AnthropicImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    /// A document (e.g. PDF) — Anthropic's native `document` block, which
    /// reuses the same `{type:url|base64,...}` source shape as an image
    /// (Document Lane D4a). Fires only when the pre-routing distillation seam
    /// (D4c) is off; normally documents are distilled to text before dispatch.
    Document {
        source: AnthropicImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    /// A tool call initiated by the assistant.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    /// A tool result sent back in a user turn.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
}

impl AnthropicContentBlock {
    /// Place a prompt-cache breakpoint on this block, regardless of variant.
    fn set_cache_control(&mut self, cc: AnthropicCacheControl) {
        let slot = match self {
            AnthropicContentBlock::Text { cache_control, .. }
            | AnthropicContentBlock::Image { cache_control, .. }
            | AnthropicContentBlock::Document { cache_control, .. }
            | AnthropicContentBlock::ToolUse { cache_control, .. }
            | AnthropicContentBlock::ToolResult { cache_control, .. } => cache_control,
        };
        *slot = Some(cc);
    }
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

/// The Anthropic output cap this request resolves to: `max_completion_tokens`
/// wins over `max_tokens` (matching the body translation below), defaulting
/// to 4096 when neither is set. Shared by [`translate_request`] and
/// [`forwardable_thinking`] so the billing-safety gate and the wire body can
/// never disagree.
fn resolved_max_tokens(req: &tt_shared::ChatCompletionRequest) -> u32 {
    req.max_completion_tokens.or(req.max_tokens).unwrap_or(4096)
}

/// The request's `extra["thinking"]` config, returned for VERBATIM forwarding
/// ONLY when shape-valid AND billing-safe:
///
/// - an object with `type` ∈ {"enabled", "disabled"};
/// - when enabled: `budget_tokens` is an integer `>= 1024` (Anthropic's
///   documented floor) AND `<` the resolved Anthropic `max_tokens` (which
///   INCLUDES thinking — Anthropic 400s a budget at/above it; forwarding an
///   invalid config would turn today's silent drop into a new failure mode).
///
/// Anything else returns `None`: translation keeps dropping it (the
/// pre-passthrough behavior), and `dropped_params` reports `thinking` so the
/// drop is no longer silent. The mirror rule: `dropped_params` and
/// `translate_request` both consult THIS function, like every other entry.
///
/// Interaction with `reasoning_budget_tokens` (gateway route action): the cap
/// runs upstream of this gate and checks only `type == "enabled"`, not
/// forwardability — so a config that would have been dropped here for
/// `budget_tokens >= max_tokens` can be lowered under the cap and become
/// FORWARDED (with the consequent temperature/top_p suppression). Deliberate:
/// the caller explicitly requested thinking, so honoring it at the capped
/// budget is more faithful than the old silent drop, and the cap's `>= 1024`
/// create-time validation means the floor here still holds.
pub(crate) fn forwardable_thinking(
    req: &tt_shared::ChatCompletionRequest,
) -> Option<serde_json::Value> {
    let v = req.extra.get("thinking")?;
    let obj = v.as_object()?;
    match obj.get("type").and_then(serde_json::Value::as_str)? {
        "disabled" => Some(v.clone()),
        "enabled" => {
            let budget = obj
                .get("budget_tokens")
                .and_then(serde_json::Value::as_u64)?;
            let max_tokens = u64::from(resolved_max_tokens(req));
            (budget >= 1024 && budget < max_tokens).then(|| v.clone())
        }
        _ => None,
    }
}

/// Whether [`forwardable_thinking`] forwards an ENABLED config for `req` —
/// the condition under which Anthropic rejects `temperature` / `top_p`
/// modifications, so translation must omit them and `dropped_params` must
/// report them.
pub(crate) fn forwards_enabled_thinking(req: &tt_shared::ChatCompletionRequest) -> bool {
    forwardable_thinking(req)
        .as_ref()
        .and_then(|v| v.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("enabled")
}

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
    // Extended-thinking passthrough: resolved BEFORE the move-heavy message
    // translation below (it reads `extra` + the max_tokens fields). An
    // ENABLED forwarded config makes Anthropic reject temperature/top_p
    // modifications, so both are omitted in that case (and reported via
    // `dropped_params` — the mirror rule).
    let thinking = forwardable_thinking(&req);
    let suppress_sampling_params = forwards_enabled_thinking(&req);
    // `max_tokens` is required by Anthropic and is its output cap. Honor the
    // caller's spend cap from either `max_tokens` or the newer
    // `max_completion_tokens` (the latter takes precedence when both are set)
    // so the ceiling is never silently dropped; default to 4096 when neither.
    // Resolved up here, before `req.messages` is moved by the loop below.
    let max_tokens = resolved_max_tokens(&req);

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
                        cache_control: None,
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
                let block = AnthropicContentBlock::ToolResult {
                    tool_use_id: tool_call_id,
                    content: text,
                    cache_control: None,
                };
                if let Some(last) = messages.last_mut() {
                    if last.role == "user" {
                        last.content.push(block);
                        continue;
                    }
                }
                messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![block],
                });
            }
        }
    }

    // Auto-inject a `cache_control` breakpoint on the system prefix, but only
    // when the cacheable prefix is long enough for THIS model. Anthropic only
    // caches a `cache_control` prefix once it meets a per-model minimum
    // (1024–4096 tokens); below that the breakpoint silently no-ops and nothing
    // is cached. The minimum lives in the pricing catalog as
    // `prompt_cache_min_tokens`; see `maybe_inject_cache_control`.
    maybe_inject_cache_control(&mut system_blocks, &req.model);

    // Also cache the recurring conversation prefix on multi-turn chats: place a
    // breakpoint on the last message so the settled history is a cache-READ on
    // the next turn instead of full-price re-sent input. Anthropic requires an
    // explicit breakpoint here (unlike OpenAI's automatic prefix caching), so
    // without this the entire growing history is billed at 1.0x every turn.
    maybe_inject_message_cache_control(&mut messages, &system_blocks, &req.model);

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
        // Anthropic rejects temperature/top_p modifications alongside
        // extended thinking — omitted when an enabled config is forwarded.
        temperature: if suppress_sampling_params {
            None
        } else {
            req.temperature
        },
        top_p: if suppress_sampling_params {
            None
        } else {
            req.top_p
        },
        stop_sequences: req.stop,
        tools,
        tool_choice,
        stream: req.stream,
        metadata,
        thinking,
        // Intentionally dropped (Anthropic rejects them):
        // n, seed, response_format, presence_penalty, frequency_penalty,
        // logit_bias, tt_extras
    })
}

/// Conservative fallback prompt-cache minimum (in tokens) used when the model
/// is not in the catalog or has no documented `prompt_cache_min_tokens`.
///
/// Anthropic's documented per-model minimums span 1024–4096 tokens. Picking the
/// largest known minimum means an injected breakpoint is guaranteed to be at or
/// above the real (unknown) minimum for any current Anthropic model, so we never
/// inject a breakpoint that would silently no-op. The trade-off is that a
/// borderline prefix (e.g. 2500 tokens) on an unrecognised model won't be
/// cached even though it might have qualified — the safe direction.
const FALLBACK_CACHE_MIN_TOKENS: u32 = 4096;

/// The Anthropic prompt-cache minimum prefix length, in tokens, for `model`.
///
/// Delegates to the shared
/// [`tt_shared::pricing::PricingCatalog::prompt_cache_min_tokens`] lookup
/// (exact `("anthropic", model)` match, then longest-prefix match for dated
/// wire ids like `claude-sonnet-4-6-20260101`) so this gate and the request-pass
/// stable-prefix split in `tt-core::passes` can never disagree. When the model
/// is unknown or has no documented minimum, returns
/// [`FALLBACK_CACHE_MIN_TOKENS`] and logs at debug so the skip is observable.
fn cache_min_tokens_for_model(model: &str) -> u32 {
    match tt_shared::pricing::catalog().prompt_cache_min_tokens("anthropic", model) {
        Some(min) => min,
        None => {
            tracing::debug!(
                model,
                fallback = FALLBACK_CACHE_MIN_TOKENS,
                "no prompt_cache_min_tokens in catalog for Anthropic model — \
                 using conservative fallback minimum"
            );
            FALLBACK_CACHE_MIN_TOKENS
        }
    }
}

/// Inject a single `cache_control` breakpoint on the system prefix when, and
/// only when, the cacheable prefix meets `model`'s prompt-cache minimum.
///
/// The cacheable prefix is the concatenation of all system blocks (Anthropic
/// caches everything up to and including the breakpoint). Because the cumulative
/// token count is monotonic in block count, the *largest* qualifying prefix
/// boundary is always the last system block — so we place the breakpoint there
/// when the total system token count meets the minimum, and nowhere otherwise.
///
/// Estimation uses the shared [`tt_tokenize`] estimator (tiktoken `cl100k` as an
/// Anthropic proxy), which *undercounts* Anthropic tokens by ~15–20%; combined
/// with the catalog minimum this keeps us on the safe side of the gate — a
/// breakpoint we inject is comfortably above the real minimum, never below it.
///
/// Breadcrumb (cache-aware pass lane): `tt-core`'s `CacheClassifierPass` flags
/// volatile markers (timestamp/uuid/hex token) inside this very prefix as
/// `cache_dynamic_prefix:<kind>` diagnostics. We deliberately do NOT suppress
/// this injection on a volatile-looking system prompt: a single-request
/// heuristic cannot distinguish a per-call UUID from a STABLE one, and wrongly
/// skipping the breakpoint forfeits the ~0.1x read rate (worth far more than
/// the 1.25x write premium suppression would save). Wiring suppression would
/// need cross-request prefix-hash evidence — see the classifier's module docs.
fn maybe_inject_cache_control(system_blocks: &mut [AnthropicSystemBlock], model: &str) {
    if system_blocks.is_empty() {
        return;
    }

    let min_tokens = cache_min_tokens_for_model(model);

    // Estimated tokens of the full system prefix (all blocks concatenated).
    let prefix_tokens: u32 = system_blocks
        .iter()
        .map(|b| tt_tokenize::estimate_tokens("anthropic", &b.text))
        .sum();

    if prefix_tokens >= min_tokens {
        if let Some(last) = system_blocks.last_mut() {
            last.cache_control = Some(AnthropicCacheControl {
                ctype: "ephemeral".to_string(),
            });
        }
    } else {
        tracing::debug!(
            model,
            prefix_tokens,
            min_tokens,
            "system prefix below model prompt-cache minimum — skipping cache_control injection"
        );
    }
}

/// Estimated tokens of a single message's textual content. Images contribute
/// real tokens but are hard to size here, so they're ignored — undercounting
/// keeps us on the safe side of the cache-min gate, matching the system-prefix
/// estimator's deliberate undercount.
fn estimate_message_tokens(m: &AnthropicMessage) -> u32 {
    m.content
        .iter()
        .map(|b| match b {
            AnthropicContentBlock::Text { text, .. } => {
                tt_tokenize::estimate_tokens("anthropic", text)
            }
            AnthropicContentBlock::ToolResult { content, .. } => {
                tt_tokenize::estimate_tokens("anthropic", content)
            }
            AnthropicContentBlock::ToolUse { name, input, .. } => {
                tt_tokenize::estimate_tokens("anthropic", name)
                    + tt_tokenize::estimate_tokens("anthropic", &input.to_string())
            }
            AnthropicContentBlock::Image { .. } | AnthropicContentBlock::Document { .. } => 0,
        })
        .sum()
}

/// Inject a `cache_control` breakpoint on the last block of the last message so
/// the recurring conversation prefix (system + tools + settled history) is a
/// cache-READ on subsequent turns instead of full-price re-sent input. Anthropic
/// caches everything up to and including the marked block via exact-prefix
/// matching; the next turn shares this prefix and reads it at ~0.1x. This is the
/// dominant cost on long Anthropic chats, where the re-sent history grows
/// ~quadratically and — without an explicit breakpoint here — is billed at full
/// price every turn (Anthropic, unlike OpenAI, does not auto-cache).
///
/// Gated on TWO conditions so it never makes a one-shot call *more* expensive:
/// 1. the conversation is already multi-turn (contains an assistant message) — a
///    first single-shot request would pay the 1.25x cache-WRITE premium with no
///    later read to amortize it;
/// 2. the total estimated prefix (system + messages) clears the model's
///    prompt-cache minimum, mirroring the system-block gate (below the minimum
///    Anthropic silently refuses to cache, so a breakpoint would be a no-op).
///
/// The system-prefix breakpoint from [`maybe_inject_cache_control`] is kept
/// alongside this one: it is a *durable* cache of the unchanging system/tools
/// prefix, while this message breakpoint is the *rolling* cache of the history.
/// Two breakpoints is well within Anthropic's limit of four.
fn maybe_inject_message_cache_control(
    messages: &mut [AnthropicMessage],
    system_blocks: &[AnthropicSystemBlock],
    model: &str,
) {
    // Only cache an ongoing conversation, never a one-shot request.
    if !messages.iter().any(|m| m.role == "assistant") {
        return;
    }

    let min_tokens = cache_min_tokens_for_model(model);
    let system_tokens: u32 = system_blocks
        .iter()
        .map(|b| tt_tokenize::estimate_tokens("anthropic", &b.text))
        .sum();
    let message_tokens: u32 = messages.iter().map(estimate_message_tokens).sum();
    let prefix_tokens = system_tokens.saturating_add(message_tokens);

    if prefix_tokens < min_tokens {
        tracing::debug!(
            model,
            prefix_tokens,
            min_tokens,
            "conversation prefix below model prompt-cache minimum — skipping message cache_control"
        );
        return;
    }

    if let Some(last_block) = messages.last_mut().and_then(|m| m.content.last_mut()) {
        last_block.set_cache_control(AnthropicCacheControl {
            ctype: "ephemeral".to_string(),
        });
    }
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
        MessageContent::Text(t) => Ok(vec![AnthropicContentBlock::Text {
            text: t,
            cache_control: None,
        }]),
        MessageContent::Parts(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        blocks.push(AnthropicContentBlock::Text {
                            text,
                            cache_control: None,
                        });
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
                        blocks.push(AnthropicContentBlock::Image {
                            source,
                            cache_control: None,
                        });
                    }
                    ContentPart::InputAudio { .. } => {
                        return Err(ProviderError::Unsupported(
                            "audio input is not supported by the Anthropic adapter".to_string(),
                        ));
                    }
                    // Document Lane (D4a): map to Anthropic's native `document`
                    // block, reusing the image source shape (base64 bytes or a
                    // remote/data URL). Only reached when the pre-routing
                    // distillation seam (D4c) is off.
                    ContentPart::Document { document } => {
                        let source = match document.source {
                            tt_shared::messages::DocumentSource::Base64 { media_type, data } => {
                                AnthropicImageSource::Base64 { media_type, data }
                            }
                            tt_shared::messages::DocumentSource::Url { url } => {
                                match tt_shared::messages::parse_data_url(&url) {
                                    Some((media_type, data)) => {
                                        AnthropicImageSource::Base64 { media_type, data }
                                    }
                                    None => AnthropicImageSource::Url { url },
                                }
                            }
                        };
                        blocks.push(AnthropicContentBlock::Document {
                            source,
                            cache_control: None,
                        });
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
        // Raw cache-read count: preserve the provider's Option-ness so
        // telemetry can distinguish "reported zero" from "didn't report".
        cache_read_input_tokens: u.cache_read_input_tokens,
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

    /// `"word "` tokenizes to ~1 token under cl100k, so `word_tokens(n)` is a
    /// system prompt of ~`n` estimated tokens — handy for landing just below /
    /// above a model's prompt-cache minimum.
    fn word_tokens(n: usize) -> String {
        "word ".repeat(n)
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
        // Sonnet 4.6's minimum is 2048 tokens; ~2500 estimated tokens clears it.
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![
            Message::System {
                content: MessageContent::Text(word_tokens(2500)),
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

    /// (a) A prompt below the model's minimum must NOT get a breakpoint, even
    /// though it would have qualified under the old fixed 1024-token gate.
    #[test]
    fn prefix_below_model_minimum_is_not_cached() {
        // ~1500 tokens: above the old 1024 gate, below Sonnet's 2048 minimum.
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![Message::System {
            content: MessageContent::Text(word_tokens(1500)),
        }];
        let body = translate_request(req).expect("translate ok");
        let system = body.system.expect("system present");
        assert!(
            system.last().unwrap().cache_control.is_none(),
            "1500-token prefix is below Sonnet's 2048 minimum — must not cache"
        );
    }

    /// (c) Two models with different minimums: the SAME ~2500-token prefix
    /// caches on Sonnet (min 2048) but NOT on Opus (min 4096).
    #[test]
    fn same_prefix_respects_per_model_minimum() {
        let make = |model: &str| {
            let mut req = base_request(model);
            req.messages = vec![Message::System {
                content: MessageContent::Text(word_tokens(2500)),
            }];
            translate_request(req)
                .expect("translate ok")
                .system
                .expect("system present")
        };

        // Sonnet 4.6: min 2048 → 2500 qualifies.
        assert!(
            make("claude-sonnet-4-6")
                .last()
                .unwrap()
                .cache_control
                .is_some(),
            "2500 tokens clears Sonnet's 2048 minimum"
        );
        // Opus 4.8: min 4096 → 2500 does not qualify.
        assert!(
            make("claude-opus-4-8")
                .last()
                .unwrap()
                .cache_control
                .is_none(),
            "2500 tokens is below Opus's 4096 minimum"
        );
    }

    /// A prefix above Opus's 4096 minimum is cached.
    #[test]
    fn opus_large_prefix_is_cached() {
        let mut req = base_request("claude-opus-4-8");
        req.messages = vec![Message::System {
            content: MessageContent::Text(word_tokens(4500)),
        }];
        let body = translate_request(req).expect("translate ok");
        assert!(
            body.system.unwrap().last().unwrap().cache_control.is_some(),
            "4500 tokens clears Opus's 4096 minimum"
        );
    }

    /// (b) The breakpoint lands on the LAST (largest) qualifying system prefix
    /// boundary, and only there — earlier blocks stay un-marked. Total prefix is
    /// the sum across blocks, so two ~1300-token blocks (2600 total) clear
    /// Sonnet's 2048 minimum even though neither block alone would.
    #[test]
    fn breakpoint_at_largest_prefix_boundary() {
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![
            Message::System {
                content: MessageContent::Text(word_tokens(1300)),
            },
            Message::System {
                content: MessageContent::Text(word_tokens(1300)),
            },
        ];
        let body = translate_request(req).expect("translate ok");
        let system = body.system.expect("system present");
        assert_eq!(system.len(), 2);
        assert!(
            system[0].cache_control.is_none(),
            "only the last (largest-prefix) boundary carries the breakpoint"
        );
        assert!(
            system[1].cache_control.is_some(),
            "cumulative prefix (2600 tokens) clears Sonnet's 2048 minimum"
        );
    }

    /// An unknown model falls back to the conservative 4096-token minimum: a
    /// ~2500-token prefix (which would cache on Sonnet) is NOT cached.
    #[test]
    fn unknown_model_uses_conservative_fallback() {
        let mut req = base_request("claude-future-model-not-in-catalog");
        req.messages = vec![Message::System {
            content: MessageContent::Text(word_tokens(2500)),
        }];
        let body = translate_request(req).expect("translate ok");
        assert!(
            body.system.unwrap().last().unwrap().cache_control.is_none(),
            "unknown model falls back to 4096 minimum — 2500 tokens skipped"
        );

        // But a prefix above the fallback minimum still caches.
        let mut req = base_request("claude-future-model-not-in-catalog");
        req.messages = vec![Message::System {
            content: MessageContent::Text(word_tokens(4500)),
        }];
        let body = translate_request(req).expect("translate ok");
        assert!(
            body.system.unwrap().last().unwrap().cache_control.is_some(),
            "4500 tokens clears the 4096 fallback minimum"
        );
    }

    /// A dated wire alias (e.g. `claude-sonnet-4-6-20260101`) resolves to the
    /// bare catalog id's minimum via longest-prefix match.
    #[test]
    fn dated_model_alias_resolves_to_catalog_minimum() {
        let mut req = base_request("claude-sonnet-4-6-20260101");
        req.messages = vec![Message::System {
            // ~2500 tokens: clears Sonnet's 2048 min but not the 4096 fallback,
            // so caching here proves the alias resolved to Sonnet, not fallback.
            content: MessageContent::Text(word_tokens(2500)),
        }];
        let body = translate_request(req).expect("translate ok");
        assert!(
            body.system.unwrap().last().unwrap().cache_control.is_some(),
            "dated alias must resolve to Sonnet's 2048 minimum (not the fallback)"
        );
    }

    #[test]
    fn cache_min_tokens_lookup_matches_catalog() {
        assert_eq!(cache_min_tokens_for_model("claude-sonnet-4-6"), 2048);
        assert_eq!(cache_min_tokens_for_model("claude-opus-4-8"), 4096);
        assert_eq!(cache_min_tokens_for_model("claude-haiku-4-5"), 4096);
        // Unknown → conservative fallback.
        assert_eq!(
            cache_min_tokens_for_model("no-such-model"),
            FALLBACK_CACHE_MIN_TOKENS
        );
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
        // Raw cache-read count threads through unfolded (telemetry NULL-vs-0).
        assert_eq!(usage.cache_read_input_tokens, Some(80));
    }

    /// When Anthropic reports no cache fields at all, the raw Options stay
    /// `None` (provider didn't report) while the folded `cached_tokens` is 0.
    #[test]
    fn usage_translation_without_cache_fields_keeps_raw_none() {
        let u = AnthropicUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let usage = translate_usage(u);
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, None);
        assert_eq!(usage.cache_creation_input_tokens, None);
    }

    /// Estimation goes through the shared tiktoken-based estimator, not a raw
    /// byte length, so multibyte (CJK) text is counted by tokens — a 1400-token
    /// CJK prefix stays below Sonnet's 2048 minimum and is not cached, where a
    /// naive byte-length heuristic would have over-counted and wrongly injected
    /// a breakpoint Anthropic then refuses to cache.
    #[test]
    fn cache_control_estimates_by_tokens_not_bytes() {
        use tt_shared::messages::{Message, MessageContent};
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![Message::System {
            content: MessageContent::Text("中".repeat(1400)),
        }];
        let body = translate_request(req).expect("translate ok");
        let sys = body.system.expect("system blocks present");
        assert!(
            sys.last().unwrap().cache_control.is_none(),
            "1400-token CJK prefix is below Sonnet's 2048 minimum — must not cache"
        );
    }

    // --- extended-thinking passthrough (research Phase 3.2 prerequisite) ---

    /// A shape-valid, billing-safe `extra["thinking"]` config is forwarded
    /// VERBATIM on the typed `thinking` field.
    #[test]
    fn thinking_forwarded_verbatim_when_valid() {
        let mut req = base_request("claude-sonnet-4-6");
        req.max_tokens = Some(16_000);
        req.temperature = None;
        req.extra.insert(
            "thinking".to_string(),
            serde_json::json!({"type":"enabled","budget_tokens":8192}),
        );
        let body = translate_request(req).expect("translate ok");
        assert_eq!(
            body.thinking,
            Some(serde_json::json!({"type":"enabled","budget_tokens":8192}))
        );
        let wire = serde_json::to_string(&body).unwrap();
        assert!(wire.contains("\"thinking\""), "{wire}");
        assert!(wire.contains("\"budget_tokens\":8192"), "{wire}");
    }

    /// A `disabled` config is forwarded too (explicitly turning thinking off
    /// is a valid client intent) — and does NOT suppress temperature.
    #[test]
    fn thinking_disabled_forwarded_and_temperature_kept() {
        let mut req = base_request("claude-sonnet-4-6");
        req.extra.insert(
            "thinking".to_string(),
            serde_json::json!({"type":"disabled"}),
        );
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.thinking, Some(serde_json::json!({"type":"disabled"})));
        assert_eq!(
            body.temperature,
            Some(0.7),
            "disabled thinking keeps temperature"
        );
    }

    /// `budget_tokens >= max_tokens` would 400 at Anthropic (max_tokens
    /// INCLUDES thinking) — the config is NOT forwarded (kept-drop semantics)
    /// and `dropped_params` reports it.
    #[test]
    fn thinking_budget_at_or_above_max_tokens_is_dropped() {
        let mut req = base_request("claude-sonnet-4-6");
        req.max_tokens = Some(4096);
        req.extra.insert(
            "thinking".to_string(),
            serde_json::json!({"type":"enabled","budget_tokens":4096}),
        );
        let provider = crate::AnthropicProvider::new(crate::ClientConfig::default());
        use tt_shared::Provider as _;
        assert!(provider
            .dropped_params(&req)
            .contains(&"thinking".to_string()));
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.thinking, None, "unsafe budget must not be forwarded");
    }

    /// Budgets below Anthropic's 1024 floor and malformed shapes keep today's
    /// drop semantics (now visible via `dropped_params`).
    #[test]
    fn thinking_invalid_shapes_are_dropped() {
        use tt_shared::Provider as _;
        let provider = crate::AnthropicProvider::new(crate::ClientConfig::default());
        for bad in [
            serde_json::json!({"type":"enabled","budget_tokens":512}),
            serde_json::json!({"type":"enabled"}),
            serde_json::json!({"type":"bogus","budget_tokens":8192}),
            serde_json::json!("enabled"),
            serde_json::json!(42),
        ] {
            let mut req = base_request("claude-sonnet-4-6");
            req.max_tokens = Some(16_000);
            req.extra.insert("thinking".to_string(), bad.clone());
            assert!(
                provider
                    .dropped_params(&req)
                    .contains(&"thinking".to_string()),
                "config {bad} must be reported dropped"
            );
            let body = translate_request(req).expect("translate ok");
            assert_eq!(body.thinking, None, "config {bad} must not be forwarded");
        }
    }

    /// Anthropic rejects temperature/top_p modifications with extended
    /// thinking: when an ENABLED config is forwarded, both are omitted from
    /// the wire body and `dropped_params` reports them.
    #[test]
    fn thinking_forwarded_drops_temperature_and_top_p() {
        use tt_shared::Provider as _;
        let provider = crate::AnthropicProvider::new(crate::ClientConfig::default());
        let mut req = base_request("claude-sonnet-4-6");
        req.max_tokens = Some(16_000);
        req.temperature = Some(0.7);
        req.top_p = Some(0.9);
        req.extra.insert(
            "thinking".to_string(),
            serde_json::json!({"type":"enabled","budget_tokens":8192}),
        );
        let dropped = provider.dropped_params(&req);
        assert!(dropped.contains(&"temperature".to_string()), "{dropped:?}");
        assert!(dropped.contains(&"top_p".to_string()), "{dropped:?}");
        assert!(
            !dropped.contains(&"thinking".to_string()),
            "a FORWARDED config is not dropped: {dropped:?}"
        );
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.temperature, None);
        assert_eq!(body.top_p, None);
        assert!(body.thinking.is_some());
    }

    /// No thinking config at all → nothing forwarded, nothing reported.
    #[test]
    fn no_thinking_extra_unchanged() {
        use tt_shared::Provider as _;
        let provider = crate::AnthropicProvider::new(crate::ClientConfig::default());
        let req = base_request("claude-sonnet-4-6");
        assert!(!provider
            .dropped_params(&req)
            .contains(&"thinking".to_string()));
        let body = translate_request(req).expect("translate ok");
        assert_eq!(body.thinking, None);
    }
}
