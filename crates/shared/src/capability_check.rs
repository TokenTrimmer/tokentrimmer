//! Capability and context-window guard for the routing / failover path.
//!
//! [`RequiredCapabilities`] is derived from a [`ChatCompletionRequest`] and
//! checked against a candidate model's [`ModelInfo`] before a route rewrite or
//! failover dispatch is committed.  The check is intentionally permissive:
//!
//! - When `ModelInfo` is **unknown** for a candidate (not in the registry
//!   catalog) we allow it through — we only skip when we *positively know* a
//!   capability is missing.
//! - A capability that the request needs but the model info does **not** list
//!   causes the candidate to be skipped (the caller emits a tracing event and
//!   tries the next candidate or falls back to the original model).
//!
//! # Token counting
//!
//! [`estimate_input_tokens`] concatenates all message text and delegates to
//! [`tt_tokenize::estimate_tokens`], keyed on `provider_id` so tiktoken is
//! used for OpenAI/Anthropic and the char/4 heuristic is used elsewhere.
//! Image/audio bytes are not measured — the guard is a best-effort floor, not
//! an exact window-packing count.

use crate::{
    messages::{ContentPart, Message, MessageContent},
    pricing::{Capability, ModelInfo},
    ChatCompletionRequest,
};

/// The set of capabilities a [`ChatCompletionRequest`] requires.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequiredCapabilities {
    /// At least one message contains an image_url or input_audio content part.
    pub vision: bool,
    /// The request has non-empty `tools`, or any assistant message contains
    /// `tool_calls`.
    pub tools: bool,
    /// `response_format.type` is `"json_object"` or `"json_schema"`.
    pub json_mode: bool,
}

impl RequiredCapabilities {
    /// Derive the required capabilities from a chat completion request.
    pub fn from_request(req: &ChatCompletionRequest) -> Self {
        let mut caps = Self::default();

        // tools / function-calling
        if !req.tools.is_empty() {
            caps.tools = true;
        }

        // response_format → json mode
        if let Some(rf) = &req.response_format {
            if rf.r#type == "json_object" || rf.r#type == "json_schema" {
                caps.json_mode = true;
            }
        }

        // scan messages for vision content and tool_calls
        for msg in &req.messages {
            match msg {
                Message::User { content, .. } | Message::System { content } => {
                    if let MessageContent::Parts(parts) = content {
                        for part in parts {
                            match part {
                                ContentPart::ImageUrl { .. } | ContentPart::InputAudio { .. } => {
                                    caps.vision = true;
                                }
                                ContentPart::Text { .. } => {}
                            }
                        }
                    }
                }
                Message::Assistant { tool_calls, .. } => {
                    if !tool_calls.is_empty() {
                        caps.tools = true;
                    }
                }
                Message::Tool { .. } => {
                    // A Tool message in context means the conversation already
                    // used tool-calling; the next turn may need it too.
                    caps.tools = true;
                }
            }
        }

        caps
    }

    /// Returns `true` when all required capabilities are listed in
    /// `info.capabilities` **and** `max_input_tokens >= estimated_tokens`.
    ///
    /// Pass `estimated_tokens = 0` to skip the context-window check.
    #[must_use]
    pub fn satisfied_by(&self, info: &ModelInfo, estimated_tokens: u64) -> bool {
        if self.vision && !info.capabilities.contains(&Capability::Vision) {
            return false;
        }
        if self.tools && !info.capabilities.contains(&Capability::Tools) {
            return false;
        }
        if self.json_mode && !info.capabilities.contains(&Capability::JsonMode) {
            return false;
        }
        if estimated_tokens > 0 && info.max_input_tokens < estimated_tokens {
            return false;
        }
        true
    }

    /// Human-readable list of the reasons a candidate was skipped, for use in
    /// the `route_skipped_capability` tracing event.
    pub fn skip_reasons(&self, info: &ModelInfo, estimated_tokens: u64) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if self.vision && !info.capabilities.contains(&Capability::Vision) {
            reasons.push("vision_not_supported");
        }
        if self.tools && !info.capabilities.contains(&Capability::Tools) {
            reasons.push("tools_not_supported");
        }
        if self.json_mode && !info.capabilities.contains(&Capability::JsonMode) {
            reasons.push("json_mode_not_supported");
        }
        if estimated_tokens > 0 && info.max_input_tokens < estimated_tokens {
            reasons.push("context_window_too_small");
        }
        reasons
    }
}

/// Concatenate all message text parts from a request for token estimation.
///
/// Image/audio bytes are excluded — the result is passed to the caller's
/// tokenizer (e.g. `tt_tokenize::estimate_tokens`) so that `tt-shared` does
/// not need to depend on `tt-tokenize`.
pub fn message_text_for_estimation(req: &ChatCompletionRequest) -> String {
    req.messages
        .iter()
        .map(|m| match m {
            Message::User { content, .. } | Message::System { content } => extract_text(content),
            Message::Assistant { content, .. } => {
                content.as_ref().map(extract_text).unwrap_or_default()
            }
            Message::Tool { content, .. } => extract_text(content),
        })
        .collect()
}

fn extract_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// True when any message carries an image (`ContentPart::ImageUrl`) content part.
///
/// Distinct from [`RequiredCapabilities`], which collapses image **and** audio
/// into a single `vision` flag; routing needs to tell the two modalities apart.
pub fn request_has_images(req: &ChatCompletionRequest) -> bool {
    req.messages
        .iter()
        .any(|m| content_of(m).is_some_and(has_image_part))
}

/// True when any message carries an audio (`ContentPart::InputAudio`) content part.
pub fn request_has_audio(req: &ChatCompletionRequest) -> bool {
    req.messages
        .iter()
        .any(|m| content_of(m).is_some_and(has_audio_part))
}

/// The content of a message, if it has any (Assistant content is optional).
fn content_of(m: &Message) -> Option<&MessageContent> {
    match m {
        Message::User { content, .. }
        | Message::System { content }
        | Message::Tool { content, .. } => Some(content),
        Message::Assistant { content, .. } => content.as_ref(),
    }
}

fn has_image_part(c: &MessageContent) -> bool {
    matches!(c, MessageContent::Parts(parts)
        if parts.iter().any(|p| matches!(p, ContentPart::ImageUrl { .. })))
}

fn has_audio_part(c: &MessageContent) -> bool {
    matches!(c, MessageContent::Parts(parts)
        if parts.iter().any(|p| matches!(p, ContentPart::InputAudio { .. })))
}

/// Concatenated text of the **user + system** messages — the caller-controlled
/// input, used for content/topic routing. Assistant/tool turns are excluded so a
/// model's own output can't spuriously trigger a topic route.
pub fn request_input_text(req: &ChatCompletionRequest) -> String {
    req.messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content, .. } | Message::System { content } => {
                Some(extract_text(content))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{
        messages::{
            ImageUrl, InputAudio, ResponseFormat, Tool, ToolCall, ToolCallFunction, ToolFunction,
        },
        pricing::Capability,
        ModelInfo,
    };

    fn text_model() -> ModelInfo {
        ModelInfo {
            id: "text-only".into(),
            provider: "mock".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 1024,
        }
    }

    fn vision_model() -> ModelInfo {
        ModelInfo {
            id: "vision-model".into(),
            provider: "mock".into(),
            capabilities: vec![Capability::Text, Capability::Vision, Capability::Tools],
            max_input_tokens: 128_000,
            max_output_tokens: 4096,
        }
    }

    fn small_model() -> ModelInfo {
        ModelInfo {
            id: "small-ctx".into(),
            provider: "mock".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 100,
            max_output_tokens: 100,
        }
    }

    fn base_req() -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![],
            temperature: None,
            top_p: None,
            max_tokens: None,
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
        }
    }

    #[test]
    fn plain_text_request_has_no_required_caps() {
        let req = base_req();
        let caps = RequiredCapabilities::from_request(&req);
        assert!(!caps.vision);
        assert!(!caps.tools);
        assert!(!caps.json_mode);
    }

    #[test]
    fn image_url_part_sets_vision() {
        let mut req = base_req();
        req.messages = vec![Message::User {
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "describe this".into(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,abc".into(),
                        detail: None,
                    },
                },
            ]),
            name: None,
        }];
        let caps = RequiredCapabilities::from_request(&req);
        assert!(caps.vision);
        assert!(!caps.tools);
    }

    #[test]
    fn tools_field_sets_tools_cap() {
        let mut req = base_req();
        req.tools = vec![Tool {
            r#type: "function".into(),
            function: ToolFunction {
                name: "get_weather".into(),
                description: None,
                parameters: serde_json::json!({}),
            },
        }];
        let caps = RequiredCapabilities::from_request(&req);
        assert!(caps.tools);
    }

    #[test]
    fn assistant_tool_calls_in_history_sets_tools_cap() {
        let mut req = base_req();
        req.messages = vec![Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: "get_weather".into(),
                    arguments: "{}".into(),
                },
            }],
            name: None,
        }];
        let caps = RequiredCapabilities::from_request(&req);
        assert!(caps.tools);
    }

    #[test]
    fn json_object_response_format_sets_json_mode() {
        let mut req = base_req();
        req.response_format = Some(ResponseFormat {
            r#type: "json_object".into(),
            json_schema: None,
        });
        let caps = RequiredCapabilities::from_request(&req);
        assert!(caps.json_mode);
    }

    #[test]
    fn vision_request_not_satisfied_by_text_model() {
        let mut req = base_req();
        req.messages = vec![Message::User {
            content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,abc".into(),
                    detail: None,
                },
            }]),
            name: None,
        }];
        let caps = RequiredCapabilities::from_request(&req);
        assert!(!caps.satisfied_by(&text_model(), 0));
    }

    #[test]
    fn vision_request_satisfied_by_vision_model() {
        let mut req = base_req();
        req.messages = vec![Message::User {
            content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,abc".into(),
                    detail: None,
                },
            }]),
            name: None,
        }];
        let caps = RequiredCapabilities::from_request(&req);
        assert!(caps.satisfied_by(&vision_model(), 0));
    }

    #[test]
    fn exceeds_context_window_not_satisfied() {
        let caps = RequiredCapabilities::default();
        assert!(!caps.satisfied_by(&small_model(), 200));
    }

    #[test]
    fn within_context_window_satisfied() {
        let caps = RequiredCapabilities::default();
        assert!(caps.satisfied_by(&small_model(), 50));
    }

    #[test]
    fn zero_estimated_tokens_skips_window_check() {
        let caps = RequiredCapabilities::default();
        assert!(caps.satisfied_by(&small_model(), 0));
    }

    #[test]
    fn skip_reasons_lists_all_failures() {
        let caps = RequiredCapabilities {
            vision: true,
            tools: true,
            ..Default::default()
        };
        let reasons = caps.skip_reasons(&text_model(), 9999);
        assert!(reasons.contains(&"vision_not_supported"));
        assert!(reasons.contains(&"tools_not_supported"));
        assert!(reasons.contains(&"context_window_too_small"));
    }

    #[test]
    fn request_has_images_detects_image_part() {
        let mut req = base_req();
        req.messages = vec![Message::User {
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "look".into(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,abc".into(),
                        detail: None,
                    },
                },
            ]),
            name: None,
        }];
        assert!(request_has_images(&req));
        assert!(!request_has_audio(&req));
    }

    #[test]
    fn request_has_audio_detects_audio_part() {
        let mut req = base_req();
        req.messages = vec![Message::User {
            content: MessageContent::Parts(vec![ContentPart::InputAudio {
                input_audio: InputAudio {
                    data: "abc".into(),
                    format: "wav".into(),
                },
            }]),
            name: None,
        }];
        assert!(request_has_audio(&req));
        assert!(!request_has_images(&req));
    }

    #[test]
    fn plain_text_request_has_no_modality() {
        let req = base_req();
        assert!(!request_has_images(&req));
        assert!(!request_has_audio(&req));
    }

    #[test]
    fn request_input_text_user_and_system_only() {
        let mut req = base_req();
        req.messages = vec![
            Message::System {
                content: MessageContent::Text("sys ctx".into()),
            },
            Message::User {
                content: MessageContent::Text("Confidential matter".into()),
                name: None,
            },
            Message::Assistant {
                content: Some(MessageContent::Text("legal advice".into())),
                tool_calls: vec![],
                name: None,
            },
        ];
        let t = request_input_text(&req);
        assert!(t.contains("sys ctx"));
        assert!(t.contains("Confidential matter"));
        assert!(
            !t.contains("legal advice"),
            "assistant output must be excluded"
        );
    }
}
