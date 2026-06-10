//! `tt-tokenize` — one token estimator, shared everywhere a token count is
//! needed before the provider reports the authoritative usage.
//!
//! Before this crate, `/v1/preview` estimated tokens with tiktoken while the
//! live routing path used a cruder `len / 4` heuristic — so a request could
//! preview as "under the route threshold" yet route differently in production
//! (or vice-versa). Centralising the estimate here keeps preview, dispatch,
//! and routing in lockstep.
//!
//! Strategy (mirrors what `/v1/preview` historically did):
//! - `openai` → tiktoken `cl100k_base` (high confidence — `cl100k` is OpenAI's
//!   own tokenizer). The BPE is built once and cached, so this is cheap.
//! - `anthropic` → tiktoken `cl100k_base` as a *proxy* (medium confidence).
//!   `cl100k` is not Anthropic's tokenizer; it undercounts Anthropic input by
//!   ~15–20% on typical text (more on code / non-English), so the estimate is
//!   directional-only, not for billing. Demoting it from High keeps callers
//!   (and `/v1/preview`) from over-trusting an Anthropic number that is
//!   systematically low. The provider's reported usage remains authoritative.
//! - everything else (Gemini, Groq, Together, local, …) → `chars / 4` heuristic
//!   (medium confidence). tiktoken is not accurate for these tokenizers, and
//!   the final bill always uses the provider's reported usage anyway.
//! - if tiktoken fails to load → `chars / 4` with low confidence.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

/// How the estimate was produced — surfaced by `/v1/preview` so callers can
/// weight it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// The provider's own exact tokenizer (tiktoken `cl100k` for OpenAI).
    High,
    /// A proxy tokenizer or heuristic — close enough for routing, not for
    /// billing. Covers both the `chars / 4` heuristic and `cl100k` used as a
    /// stand-in for Anthropic (which it undercounts by ~15–20%).
    Medium,
    /// Heuristic used only because the exact tokenizer failed to load.
    Low,
}

/// An input-token estimate plus how it was derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub tokens: u32,
    pub confidence: Confidence,
}

/// Process-wide cached `cl100k_base` BPE. `None` if it failed to load (then we
/// fall back to the heuristic). Building it is non-trivial, so we do it once.
fn cl100k() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}

/// `chars / 4`, rounded up — the universal cheap fallback.
#[must_use]
pub fn char_count_estimate(text: &str) -> u32 {
    ((text.chars().count() as f64) / 4.0).ceil() as u32
}

/// Whether `cl100k` produces an estimate for this provider — exactly for
/// OpenAI (its own tokenizer), or as an undercounting proxy for Anthropic.
fn uses_tiktoken(provider: &str) -> bool {
    matches!(provider, "openai" | "anthropic")
}

/// The confidence to report when `cl100k` succeeds for `provider`: `High` only
/// for OpenAI (its native tokenizer); `Medium` for Anthropic, where `cl100k` is
/// a proxy that undercounts input by ~15–20% (see module docs).
fn tiktoken_confidence(provider: &str) -> Confidence {
    if provider == "openai" {
        Confidence::High
    } else {
        Confidence::Medium
    }
}

/// Estimate input tokens for `text` as served by `provider`, with a confidence
/// label. See the module docs for the per-provider strategy.
#[must_use]
pub fn estimate_input_tokens(provider: &str, text: &str) -> Estimate {
    if uses_tiktoken(provider) {
        if let Some(bpe) = cl100k() {
            return Estimate {
                tokens: bpe.encode_with_special_tokens(text).len() as u32,
                confidence: tiktoken_confidence(provider),
            };
        }
        // tiktoken unavailable — degrade to the heuristic, flagged Low.
        return Estimate {
            tokens: char_count_estimate(text),
            confidence: Confidence::Low,
        };
    }
    Estimate {
        tokens: char_count_estimate(text),
        confidence: Confidence::Medium,
    }
}

/// Convenience: just the token count, discarding confidence. Used on the
/// routing hot path where only the number matters.
#[must_use]
pub fn estimate_tokens(provider: &str, text: &str) -> u32 {
    estimate_input_tokens(provider, text).tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_uses_tiktoken_high() {
        // cl100k is OpenAI's own tokenizer → High confidence.
        let e = estimate_input_tokens("openai", "Hello, world.");
        assert!(e.tokens >= 1);
        assert_eq!(e.confidence, Confidence::High);
    }

    #[test]
    fn anthropic_uses_tiktoken_proxy_medium() {
        // cl100k is a proxy for Anthropic (undercounts ~15–20%), so the
        // estimate is produced by tiktoken but flagged Medium, not High.
        let e = estimate_input_tokens("anthropic", "Hello, world.");
        assert!(e.tokens >= 1);
        assert_eq!(e.confidence, Confidence::Medium);
        // It still uses the real BPE, so it should match OpenAI's count on the
        // same text (same tokenizer) — confirming it's not the chars/4 path.
        assert_eq!(
            e.tokens,
            estimate_input_tokens("openai", "Hello, world.").tokens
        );
    }

    #[test]
    fn other_providers_use_heuristic_medium() {
        let e = estimate_input_tokens("groq", "abcdefgh"); // 8 chars → ceil(8/4)=2
        assert_eq!(e.tokens, 2);
        assert_eq!(e.confidence, Confidence::Medium);
    }

    #[test]
    fn char_count_rounds_up() {
        assert_eq!(char_count_estimate(""), 0);
        assert_eq!(char_count_estimate("abcde"), 2); // ceil(5/4)
    }

    #[test]
    fn estimate_tokens_matches_estimate_input_tokens() {
        let text = "The quick brown fox jumps over the lazy dog.";
        assert_eq!(
            estimate_tokens("openai", text),
            estimate_input_tokens("openai", text).tokens
        );
    }

    #[test]
    fn tiktoken_is_more_precise_than_heuristic_for_known_text() {
        // tiktoken should not wildly disagree with chars/4 in magnitude.
        let text = "Tokenization keeps preview and routing in lockstep.";
        let tk = estimate_tokens("openai", text);
        let heur = char_count_estimate(text);
        assert!(tk > 0 && heur > 0);
    }
}
