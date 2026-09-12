//! Request requirements shared by route selection and buffered/stream failover.
use crate::{
    messages::{ContentPart, Message, MessageContent},
    pricing::{Capability, ModelInfo},
    ChatCompletionRequest,
};

/// Capabilities and explicit output limit that a candidate must support.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequiredCapabilities {
    /// Input contains an image.
    pub vision: bool,
    /// Input contains audio (not interchangeable with vision).
    pub audio: bool,
    /// Tools are offered or used in conversation history.
    pub tools: bool,
    /// JSON object/schema output is requested.
    pub json_mode: bool,
    /// A json_schema envelope specifically requires strict: true.
    pub strict_json_schema: bool,
    /// Streaming output is requested.
    pub streaming: bool,
    /// Caller-supplied cap, not predicted output length. Like native adapters,
    /// max_completion_tokens takes precedence over max_tokens. Omission does
    /// not invent a cap or guarantee the provider's default is admissible.
    pub requested_max_output_tokens: Option<u32>,
}

impl RequiredCapabilities {
    /// Derive requirements without changing the request or supplying defaults.
    pub fn from_request(req: &ChatCompletionRequest) -> Self {
        let mut caps = Self {
            tools: !req.tools.is_empty(),
            streaming: req.stream,
            requested_max_output_tokens: req.max_completion_tokens.or(req.max_tokens),
            ..Self::default()
        };
        if let Some(rf) = &req.response_format {
            caps.json_mode = rf.r#type == "json_object" || rf.r#type == "json_schema";
            caps.strict_json_schema = rf.r#type == "json_schema"
                && rf.json_schema.as_ref().is_some_and(|v| {
                    v.get("schema").is_some_and(serde_json::Value::is_object)
                        && v.get("strict").and_then(serde_json::Value::as_bool) == Some(true)
                });
        }
        for msg in &req.messages {
            match msg {
                Message::User { content, .. } | Message::System { content } => {
                    if let MessageContent::Parts(parts) = content {
                        for part in parts {
                            match part {
                                ContentPart::ImageUrl { .. } => caps.vision = true,
                                ContentPart::InputAudio { .. } => caps.audio = true,
                                // The Document Lane distills to text; a document
                                // alone does not require a vision target.
                                ContentPart::Document { .. } | ContentPart::Text { .. } => {}
                            }
                        }
                    }
                }
                Message::Assistant { tool_calls, .. } => caps.tools |= !tool_calls.is_empty(),
                Message::Tool { .. } => caps.tools = true,
            }
        }
        caps
    }

    /// Known-catalog checks only. estimated_tokens=0 skips the approximate input
    /// check, NOT the explicit output check. Unknown models are handled by the
    /// caller's metadata policy; this does not validate credentials or predict
    /// output length, quality, combined-context capacity or final spend.
    #[must_use]
    pub fn satisfied_by(&self, info: &ModelInfo, estimated_tokens: u64) -> bool {
        self.failures(info, estimated_tokens).next().is_none()
    }

    /// Same predicates as admission, so a refusal always has a reason and a
    /// passing model cannot produce a contradictory explanation.
    pub fn skip_reasons(&self, info: &ModelInfo, estimated_tokens: u64) -> Vec<&'static str> {
        self.failures(info, estimated_tokens).collect()
    }

    fn failures<'a>(
        &'a self,
        info: &'a ModelInfo,
        estimated_tokens: u64,
    ) -> impl Iterator<Item = &'static str> + 'a {
        [
            (self.vision, Capability::Vision, "vision_not_supported"),
            (self.audio, Capability::Audio, "audio_not_supported"),
            (self.tools, Capability::Tools, "tools_not_supported"),
            (
                self.json_mode,
                Capability::JsonMode,
                "json_mode_not_supported",
            ),
            (
                self.strict_json_schema,
                Capability::StrictJsonSchema,
                "strict_json_schema_not_supported",
            ),
            (
                self.streaming,
                Capability::Streaming,
                "streaming_not_supported",
            ),
        ]
        .into_iter()
        .filter_map(move |(required, capability, reason)| {
            (required && !info.capabilities.contains(&capability)).then_some(reason)
        })
        .chain(
            [
                (estimated_tokens > 0 && info.max_input_tokens < estimated_tokens)
                    .then_some("context_window_too_small"),
                self.requested_max_output_tokens
                    .is_some_and(|limit| u64::from(limit) > info.max_output_tokens)
                    .then_some("output_limit_too_large"),
            ]
            .into_iter()
            .flatten(),
        )
    }

    /// Token/media counts remain estimates even with known catalog metadata.
    #[must_use]
    pub fn evidence_policy(info_known: bool) -> &'static str {
        if info_known {
            "catalog_verified"
        } else {
            "unknown_model_permissive"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(output: u64) -> ModelInfo {
        ModelInfo {
            id: "small".into(),
            provider: "test".into(),
            capabilities: vec![Capability::Text, Capability::Vision, Capability::Streaming],
            max_input_tokens: 4096,
            max_output_tokens: output,
        }
    }

    #[test]
    fn explicit_output_limit_is_independent_of_input_estimation() {
        let req = ChatCompletionRequest {
            max_tokens: Some(129),
            ..Default::default()
        };
        let caps = RequiredCapabilities::from_request(&req);
        assert!(!caps.satisfied_by(&model(128), 0));
        assert_eq!(
            caps.skip_reasons(&model(128), 0),
            ["output_limit_too_large"]
        );
        assert!(caps.satisfied_by(&model(129), 0));
        assert!(caps.satisfied_by(&model(u64::MAX), 4096));
        assert!(!caps.satisfied_by(&model(u64::MAX), 4097));
    }

    #[test]
    fn completion_tokens_precedence_matches_native_adapters_in_both_directions() {
        for (legacy, completion, expected) in [
            (Some(8192), Some(64), Some(64)),
            (Some(64), Some(8192), Some(8192)),
            (Some(64), None, Some(64)),
            (None, None, None),
        ] {
            let req = ChatCompletionRequest {
                max_tokens: legacy,
                max_completion_tokens: completion,
                ..Default::default()
            };
            let caps = RequiredCapabilities::from_request(&req);
            assert_eq!(caps.requested_max_output_tokens, expected);
            assert_eq!(
                caps.satisfied_by(&model(128), 0),
                expected.is_none_or(|v| v <= 128)
            );
            assert_eq!(req.max_tokens, legacy);
            assert_eq!(req.max_completion_tokens, completion);
        }
    }

    #[test]
    fn omitted_caps_are_not_invented_and_known_zero_limits_are_not_unknown_models() {
        let caps = RequiredCapabilities::from_request(&ChatCompletionRequest::default());
        assert!(caps.satisfied_by(&model(0), 0));
        let caps = RequiredCapabilities::from_request(&ChatCompletionRequest {
            max_tokens: Some(1),
            ..Default::default()
        });
        assert!(!caps.satisfied_by(&model(0), 0));
        assert_eq!(
            RequiredCapabilities::evidence_policy(false),
            "unknown_model_permissive"
        );
    }

    #[test]
    fn refusal_and_explanation_agree_for_all_flag_and_limit_combinations() {
        for bits in 0..64 {
            for limit in [None, Some(0), Some(128), Some(129), Some(u32::MAX)] {
                for input in [0, 4096, 4097] {
                    let caps = RequiredCapabilities {
                        vision: bits & 1 != 0,
                        audio: bits & 2 != 0,
                        tools: bits & 4 != 0,
                        json_mode: bits & 8 != 0,
                        strict_json_schema: bits & 16 != 0,
                        streaming: bits & 32 != 0,
                        requested_max_output_tokens: limit,
                    };
                    let reasons = caps.skip_reasons(&model(128), input);
                    assert_eq!(caps.satisfied_by(&model(128), input), reasons.is_empty());
                    assert_eq!(
                        reasons.contains(&"output_limit_too_large"),
                        limit.is_some_and(|v| v > 128)
                    );
                    assert_eq!(reasons.contains(&"context_window_too_small"), input > 4096);
                }
            }
        }
    }
}
