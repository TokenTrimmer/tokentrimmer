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
    /// At least one message contains an image_url content part.
    pub vision: bool,
    /// At least one message contains an input_audio content part.
    pub audio: bool,
    /// The request has non-empty `tools`, or any assistant message contains
    /// `tool_calls`.
    pub tools: bool,
    /// `response_format.type` is `"json_object"` or `"json_schema"`.
    pub json_mode: bool,
    /// `response_format.type` is `"json_schema"` AND the envelope carries
    /// `strict: true` (S02). A strict structured-output request must not be
    /// rewritten to a model that only promises the loose `json_object` shape:
    /// the dispatch-time downgrade warning fires AFTER the rewrite committed,
    /// which is exactly the silently-changed-output-contract failure the
    /// review named. Suppressed at selection instead; unknown models stay
    /// permissive per the module's unknown-metadata policy (the downgrade
    /// warning remains the backstop there).
    pub strict_json_schema: bool,
    /// `stream` is true; models explicitly lacking Streaming cannot serve.
    pub streaming: bool,
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
            // S02: a STRICT structured-output request (json_schema envelope
            // with strict: true) additionally requires the strict capability
            // so selection cannot rewrite it onto a json_object-only model.
            if rf.r#type == "json_schema" && schema_envelope_is_strict(&rf.json_schema) {
                caps.strict_json_schema = true;
            }
        }

        // streaming
        if req.stream {
            caps.streaming = true;
        }

        // scan messages for vision/audio content and tool_calls
        for msg in &req.messages {
            match msg {
                Message::User { content, .. } | Message::System { content } => {
                    if let MessageContent::Parts(parts) = content {
                        for part in parts {
                            match part {
                                ContentPart::ImageUrl { .. } => {
                                    caps.vision = true;
                                }
                                ContentPart::InputAudio { .. } => {
                                    caps.audio = true;
                                }
                                // A Document part does NOT require Vision: the
                                // Document Lane's target is a TEXT model (the
                                // pre-routing seam distills it to text). Leaving
                                // `vision` unset is what lets a document route
                                // downgrade to a non-Vision model.
                                ContentPart::Document { .. } | ContentPart::Text { .. } => {}
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
        if self.audio && !info.capabilities.contains(&Capability::Audio) {
            return false;
        }
        if self.tools && !info.capabilities.contains(&Capability::Tools) {
            return false;
        }
        if self.json_mode && !info.capabilities.contains(&Capability::JsonMode) {
            return false;
        }
        if self.strict_json_schema && !info.capabilities.contains(&Capability::StrictJsonSchema) {
            return false;
        }
        if self.streaming && !info.capabilities.contains(&Capability::Streaming) {
            return false;
        }
        if estimated_tokens > 0 && info.max_input_tokens < estimated_tokens {
            return false;
        }
        true
    }

    /// S02 evidence policy for the guard: what was checked against positive
    /// catalog knowledge vs. what was assumed. Unknown models are PERMISSIVE
    /// (dispatch may still work; the dispatch-time downgrade warnings remain
    /// the backstop), and token/media counts are ESTIMATES — this enum lets
    /// callers label route decisions with that uncertainty instead of
    /// presenting an approximate guard as an exact one.
    #[must_use]
    pub fn evidence_policy(info_known: bool) -> &'static str {
        if info_known {
            "catalog_verified"
        } else {
            "unknown_model_permissive"
        }
    }

    /// Human-readable list of the reasons a candidate was skipped, for use in
    /// the `route_skipped_capability` tracing event.
    pub fn skip_reasons(&self, info: &ModelInfo, estimated_tokens: u64) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if self.vision && !info.capabilities.contains(&Capability::Vision) {
            reasons.push("vision_not_supported");
        }
        if self.audio && !info.capabilities.contains(&Capability::Audio) {
            reasons.push("audio_not_supported");
        }
        if self.tools && !info.capabilities.contains(&Capability::Tools) {
            reasons.push("tools_not_supported");
        }
        if self.json_mode && !info.capabilities.contains(&Capability::JsonMode) {
            reasons.push("json_mode_not_supported");
        }
        if self.strict_json_schema && !info.capabilities.contains(&Capability::StrictJsonSchema) {
            reasons.push("strict_json_schema_not_supported");
        }
        if self.streaming && !info.capabilities.contains(&Capability::Streaming) {
            reasons.push("streaming_not_supported");
        }
        if estimated_tokens > 0 && info.max_input_tokens < estimated_tokens {
            reasons.push("context_window_too_small");
        }
        reasons
    }
}

/// True when the OpenAI `response_format.json_schema` envelope carries
/// `strict: true`. Mirrors `unwrap_schema_envelope` in the core shaping
/// module (`{"name", "strict", "schema"}` envelope, accepting a bare
/// schema as non-strict) without a cross-crate dependency — keep the two in
/// sync. A bare schema without the envelope is deliberately NOT strict: the
/// absence of the flag is not evidence of a grammar-locked output contract.
fn schema_envelope_is_strict(raw: &Option<serde_json::Value>) -> bool {
    raw.as_ref()
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get("schema").filter(|s| s.is_object()))
        .is_some_and(|_| {
            raw.as_ref()
                .and_then(|v| v.as_object())
                .and_then(|obj| obj.get("strict"))
                .and_then(|s| s.as_bool())
                .unwrap_or(false)
        })
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
/// Distinct from [`RequiredCapabilities`], which separates image and audio
/// into independent required capabilities.
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

/// True when any message carries a document (`ContentPart::Document`) content
/// part — the Document Lane routing signal (D4a).
///
/// Distinct from [`request_has_images`]/[`request_has_audio`]: a document part
/// does NOT imply the `vision` capability (its route target is a TEXT model),
/// so it gets its own detector rather than folding into the vision flag.
pub fn request_has_documents(req: &ChatCompletionRequest) -> bool {
    req.messages
        .iter()
        .any(|m| content_of(m).is_some_and(has_document_part))
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

fn has_document_part(c: &MessageContent) -> bool {
    matches!(c, MessageContent::Parts(parts)
        if parts.iter().any(|p| matches!(p, ContentPart::Document { .. })))
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

/// The [`ContentKind`](crate::content_kind::ContentKind) of the LARGEST text
/// block in the request — the request's "dominant content kind" — backing the
/// `content_type` routing condition (P1a). Scans every message's text content
/// (all roles), then classifies the single largest block; `None` when no block
/// is large enough to classify. Allocation-light: it borrows each block and
/// runs the classifier once, on the largest.
pub fn request_dominant_content_kind(
    req: &ChatCompletionRequest,
) -> Option<crate::content_kind::ContentKind> {
    let mut best: Option<&str> = None;
    for m in &req.messages {
        if let Some(content) = content_of(m) {
            match content {
                MessageContent::Text(s) => update_largest(&mut best, s),
                MessageContent::Parts(parts) => {
                    for p in parts {
                        if let ContentPart::Text { text } = p {
                            update_largest(&mut best, text);
                        }
                    }
                }
            }
        }
    }
    best.and_then(crate::content_kind::classify)
}

fn update_largest<'a>(best: &mut Option<&'a str>, s: &'a str) {
    if best.is_none_or(|cur| s.len() > cur.len()) {
        *best = Some(s);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{
        messages::{
            DocumentPart, DocumentSource, ImageUrl, InputAudio, ResponseFormat, Tool, ToolCall,
            ToolCallFunction, ToolFunction,
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
            ..Default::default()
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
    fn audio_part_sets_audio_not_vision() {
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
        let caps = RequiredCapabilities::from_request(&req);
        assert!(caps.audio);
        assert!(!caps.vision, "audio must not be treated as vision");
    }

    #[test]
    fn audio_cannot_route_to_vision_only_model() {
        let vision_only = ModelInfo {
            id: "vision-only".into(),
            provider: "mock".into(),
            capabilities: vec![Capability::Text, Capability::Vision, Capability::Tools],
            max_input_tokens: 128_000,
            max_output_tokens: 4096,
        };
        let req_with_audio = RequiredCapabilities {
            audio: true,
            ..Default::default()
        };
        assert!(
            !req_with_audio.satisfied_by(&vision_only, 0),
            "a vision-only model without Audio cannot serve audio"
        );
        let reasons = req_with_audio.skip_reasons(&vision_only, 0);
        assert!(reasons.contains(&"audio_not_supported"));
    }

    #[test]
    fn streaming_request_requires_streaming_capability() {
        let mut req = base_req();
        req.stream = true;
        let caps = RequiredCapabilities::from_request(&req);
        assert!(caps.streaming);
        let no_streaming = ModelInfo {
            id: "no-streaming".into(),
            provider: "mock".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 1024,
        };
        assert!(!caps.satisfied_by(&no_streaming, 0));
        assert!(caps
            .skip_reasons(&no_streaming, 0)
            .contains(&"streaming_not_supported"));

        let with_streaming = ModelInfo {
            capabilities: vec![Capability::Text, Capability::Streaming],
            ..no_streaming
        };
        assert!(caps.satisfied_by(&with_streaming, 0));
    }

    #[test]
    fn plain_text_request_has_no_modality() {
        let req = base_req();
        assert!(!request_has_images(&req));
        assert!(!request_has_audio(&req));
        assert!(!request_has_documents(&req));
    }

    #[test]
    fn request_has_documents_detects_document_part() {
        let mut req = base_req();
        req.messages = vec![Message::User {
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "summarize".into(),
                },
                ContentPart::Document {
                    document: DocumentPart {
                        source: DocumentSource::Base64 {
                            media_type: "application/pdf".into(),
                            data: "JVBERi0=".into(),
                        },
                        filename: Some("a.pdf".into()),
                    },
                },
            ]),
            name: None,
        }];
        assert!(request_has_documents(&req));
        // A document is NOT an image or audio modality.
        assert!(!request_has_images(&req));
        assert!(!request_has_audio(&req));
        // A document does NOT require the Vision capability.
        assert!(!RequiredCapabilities::from_request(&req).vision);
    }

    #[test]
    fn image_only_request_has_no_documents() {
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
        assert!(!request_has_documents(&req));
        assert!(request_has_images(&req));
    }

    #[test]
    fn strict_schema_request_requires_the_strict_capability() {
        let mut req = base_req();
        req.response_format = Some(ResponseFormat {
            r#type: "json_schema".into(),
            json_schema: Some(serde_json::json!({
                "name": "receipt",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {"total": {"type": "number"}},
                    "required": ["total"],
                    "additionalProperties": false
                }
            })),
        });
        let caps = RequiredCapabilities::from_request(&req);
        assert!(
            caps.json_mode,
            "a json_schema request still needs json_mode"
        );
        assert!(
            caps.strict_json_schema,
            "strict envelope must set the strict cap"
        );

        // A json_object-only model (JsonMode but no StrictJsonSchema) cannot serve.
        let json_object_only = ModelInfo {
            id: "json-object-only".into(),
            provider: "mock".into(),
            capabilities: vec![Capability::Text, Capability::JsonMode],
            max_input_tokens: 8192,
            max_output_tokens: 1024,
        };
        assert!(
            !caps.satisfied_by(&json_object_only, 0),
            "a strict-schema request must not be rewritten onto a json_object-only model"
        );
        assert!(caps
            .skip_reasons(&json_object_only, 0)
            .contains(&"strict_json_schema_not_supported"));

        // A model with the strict capability can serve.
        let strict_capable = ModelInfo {
            id: "strict-capable".into(),
            provider: "mock".into(),
            capabilities: vec![
                Capability::Text,
                Capability::JsonMode,
                Capability::StrictJsonSchema,
            ],
            max_input_tokens: 8192,
            max_output_tokens: 1024,
        };
        assert!(caps.satisfied_by(&strict_capable, 0));
    }

    #[test]
    fn non_strict_schema_request_only_needs_json_mode() {
        // A json_schema envelope WITHOUT strict:true (or a bare schema) must
        // only demand json_mode — the loose-shape contract downgrades safely
        // at dispatch with the existing warning.
        for envelope in [
            serde_json::json!({
                "name": "receipt",
                "strict": false,
                "schema": {"type": "object", "properties": {}}
            }),
            serde_json::json!({"schema": {"type": "object"}}),
            serde_json::json!({"type": "object"}),
        ] {
            let mut req = base_req();
            req.response_format = Some(ResponseFormat {
                r#type: "json_schema".into(),
                json_schema: Some(envelope.clone()),
            });
            let caps = RequiredCapabilities::from_request(&req);
            assert!(caps.json_mode);
            assert!(
                !caps.strict_json_schema,
                "envelope without strict:true must not demand the strict cap: {envelope}"
            );

            let json_object_only = ModelInfo {
                id: "json-object-only".into(),
                provider: "mock".into(),
                capabilities: vec![Capability::Text, Capability::JsonMode],
                max_input_tokens: 8192,
                max_output_tokens: 1024,
            };
            assert!(
                caps.satisfied_by(&json_object_only, 0),
                "a non-strict schema request may still route to a json_object model"
            );
        }
    }

    #[test]
    fn json_object_request_never_demands_strict_schema() {
        let mut req = base_req();
        req.response_format = Some(ResponseFormat {
            r#type: "json_object".into(),
            json_schema: None,
        });
        let caps = RequiredCapabilities::from_request(&req);
        assert!(caps.json_mode);
        assert!(!caps.strict_json_schema);
    }

    #[test]
    fn evidence_policy_names_the_guard_basis_honestly() {
        assert_eq!(
            RequiredCapabilities::evidence_policy(true),
            "catalog_verified"
        );
        assert_eq!(
            RequiredCapabilities::evidence_policy(false),
            "unknown_model_permissive"
        );
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
