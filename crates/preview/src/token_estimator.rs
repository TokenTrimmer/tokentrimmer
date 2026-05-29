//! Per-model token estimation.
//!
//! - OpenAI / Anthropic → tiktoken-rs `cl100k_base` (close enough; final
//!   billing uses provider report).
//! - Gemini / local → char-count / 4.0 heuristic.

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
    let (input, confidence) = match provider {
        "openai" | "anthropic" => {
            match tiktoken_rs::cl100k_base() {
                Ok(bpe) => (bpe.encode_with_special_tokens(&text).len() as u32, EstimationConfidence::High),
                Err(_) => (char_count_estimate(&text), EstimationConfidence::Low),
            }
        }
        _ => (char_count_estimate(&text), EstimationConfidence::Medium),
    };
    let output = max_tokens_hint.unwrap_or(512).min(4096);
    EstimateResult { input_tokens: input, output_tokens: output, confidence }
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

fn char_count_estimate(s: &str) -> u32 {
    ((s.chars().count() as f64) / 4.0).ceil() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message { role: "user".into(), content: json!(text) }
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
