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

/// Assumed output length when the caller gives no `max_tokens` — a typical
/// short completion. Clamped to the model's real catalog max-output when known.
const DEFAULT_OUTPUT_TOKENS: u32 = 512;

pub struct EstimateResult {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub confidence: EstimationConfidence,
}

pub fn estimate(
    provider: &str,
    messages: &[Message],
    max_tokens_hint: Option<u32>,
    model_max_output: Option<u32>,
) -> EstimateResult {
    let text = concat_message_text(messages);
    let est = tt_tokenize::estimate_input_tokens(provider, &text);
    let output = output_tokens(max_tokens_hint, model_max_output);
    EstimateResult {
        input_tokens: est.tokens,
        output_tokens: output,
        confidence: map_confidence(est.confidence),
    }
}

/// Project the output-token count used for cost.
///
/// - An explicit `max_tokens` hint is the caller's stated ceiling and is
///   honored. (It was previously capped at a hardcoded 4096, silently halving
///   the projected output cost — the more expensive side, ~5× input — for any
///   larger generation, which biased the headline preview number low.)
/// - With no hint, assume [`DEFAULT_OUTPUT_TOKENS`].
/// - Either way, clamp to the model's catalog `max_output_tokens` when known so
///   the estimate never projects beyond what the model can actually emit. When
///   the model is not in the catalog, the hint/default passes through uncapped
///   (more honest than a fictitious fixed cap).
fn output_tokens(max_tokens_hint: Option<u32>, model_max_output: Option<u32>) -> u32 {
    let assumed = max_tokens_hint.unwrap_or(DEFAULT_OUTPUT_TOKENS);
    match model_max_output {
        Some(max) => assumed.min(max),
        None => assumed,
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
        let est = estimate("openai", &[user("Hello, world.")], Some(100), None);
        assert!(est.input_tokens >= 1);
        assert!(matches!(est.confidence, EstimationConfidence::High));
        assert_eq!(est.output_tokens, 100);
    }

    #[test]
    fn anthropic_uses_tiktoken_proxy_medium() {
        // cl100k tokenizes Anthropic input but only as a proxy that undercounts
        // ~15–20%, so the surfaced confidence is Medium, not High.
        let est = estimate("anthropic", &[user("Hello, world.")], None, None);
        assert!(est.input_tokens >= 1);
        assert!(matches!(est.confidence, EstimationConfidence::Medium));
        assert_eq!(est.output_tokens, 512); // default
    }

    #[test]
    fn unknown_provider_uses_heuristic_medium() {
        let est = estimate("gemini", &[user("abcdefgh")], None, None);
        // "abcdefgh\n" = 9 chars, ceil(9/4) = 3 tokens // fixup: \n appended per message
        assert_eq!(est.input_tokens, 3);
        assert!(matches!(est.confidence, EstimationConfidence::Medium));
    }

    #[test]
    fn explicit_hint_is_honored_when_model_max_unknown() {
        // No catalog entry → the caller's stated ceiling passes through; the
        // old hardcoded 4096 cap that silently halved this is gone.
        let est = estimate("openai", &[user("hi")], Some(99999), None);
        assert_eq!(est.output_tokens, 99999);
    }

    #[test]
    fn output_is_clamped_to_model_max_when_known() {
        // An over-large hint is clamped to what the model can actually emit.
        let est = estimate("openai", &[user("hi")], Some(99999), Some(8192));
        assert_eq!(est.output_tokens, 8192);
    }

    #[test]
    fn default_output_is_clamped_to_small_model_max() {
        // The 512 default is clamped down for a model whose max-output is below it.
        let est = estimate("openai", &[user("hi")], None, Some(256));
        assert_eq!(est.output_tokens, 256);
    }

    #[test]
    fn structured_content_extracts_text_parts() {
        let m = Message {
            role: "user".into(),
            content: json!([{"type": "text", "text": "Hello"}, {"type": "text", "text": " world"}]),
        };
        let est = estimate("gemini", &[m], None, None);
        assert!(est.input_tokens >= 2);
    }
}
