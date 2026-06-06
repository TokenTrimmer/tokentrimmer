//! Per-model token estimation.
//!
//! The actual counting lives in the shared [`tt_tokenize`] crate, so `/v1/preview`,
//! live dispatch, and routing all use the same tokenizer. The text they feed it is
//! nearly identical, but this module's `concat_message_text` inserts a newline
//! after every message and every text part, so preview's count can differ by ~1
//! char per message from the dispatch/routing estimate
//! (`tt_shared::message_text_for_estimation`, which uses no separators). This
//! module adapts preview's [`Message`] shape into text and maps the shared
//! confidence onto preview's public [`EstimationConfidence`].

use crate::types::{EstimationConfidence, Message};

pub struct EstimateResult {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub confidence: EstimationConfidence,
}

pub fn estimate(
    provider: &str,
    messages: &[Message],
    max_tokens_hint: Option<u32>,
) -> EstimateResult {
    let text = concat_message_text(messages);
    let est = tt_tokenize::estimate_input_tokens(provider, &text);
    let output = max_tokens_hint.unwrap_or(512).min(4096);
    EstimateResult {
        input_tokens: est.tokens,
        output_tokens: output,
        confidence: map_confidence(est.confidence),
    }
}

fn map_confidence(c: tt_tokenize::Confidence) -> EstimationConfidence {
    match c {
        tt_tokenize::Confidence::High => EstimationConfidence::High,
        tt_tokenize::Confidence::Medium => EstimationConfidence::Medium,
        tt_tokenize::Confidence::Low => EstimationConfidence::Low,
    }
}

fn concat_message_text(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        if let Some(s) = m.content.as_str() {
            out.push_str(s);
            out.push('\n');
        } else if let Some(parts) = m.content.as_array() {
            for p in parts {
                if let Some(s) = p.get("text").and_then(|v| v.as_str()) {
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message {
            role: "user".into(),
            content: json!(text),
        }
    }

    #[test]
    fn openai_uses_tiktoken_high_confidence() {
        let est = estimate("openai", &[user("Hello, world.")], Some(100));
        assert!(est.input_tokens >= 1);
        assert!(matches!(est.confidence, EstimationConfidence::High));
        assert_eq!(est.output_tokens, 100);
    }

    #[test]
    fn anthropic_uses_tiktoken() {
        let est = estimate("anthropic", &[user("Hello, world.")], None);
        assert!(est.input_tokens >= 1);
        assert!(matches!(est.confidence, EstimationConfidence::High));
        assert_eq!(est.output_tokens, 512); // default
    }

    #[test]
    fn unknown_provider_uses_heuristic_medium() {
        let est = estimate("gemini", &[user("abcdefgh")], None);
        // "abcdefgh\n" = 9 chars, ceil(9/4) = 3 tokens // fixup: \n appended per message
        assert_eq!(est.input_tokens, 3);
        assert!(matches!(est.confidence, EstimationConfidence::Medium));
    }

    #[test]
    fn max_tokens_caps_output_at_4096() {
        let est = estimate("openai", &[user("hi")], Some(99999));
        assert_eq!(est.output_tokens, 4096);
    }

    #[test]
    fn structured_content_extracts_text_parts() {
        let m = Message {
            role: "user".into(),
            content: json!([{"type": "text", "text": "Hello"}, {"type": "text", "text": " world"}]),
        };
        let est = estimate("gemini", &[m], None);
        assert!(est.input_tokens >= 2);
    }
}
