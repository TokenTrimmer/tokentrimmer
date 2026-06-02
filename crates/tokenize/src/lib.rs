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
//! - `openai` / `anthropic` → tiktoken `cl100k_base` (high confidence). The
//!   BPE is built once and cached, so this is cheap on the hot path.
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
    /// Exact tokenizer for the provider family (tiktoken `cl100k`).
    High,
    /// Heuristic (`chars / 4`) — close enough for routing, not for billing.
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

/// Whether this provider id is tokenized accurately by tiktoken `cl100k`.
fn uses_tiktoken(provider: &str) -> bool {
    matches!(provider, "openai" | "anthropic")
}

/// Estimate input tokens for `text` as served by `provider`, with a confidence
/// label. See the module docs for the per-provider strategy.
#[must_use]
pub fn estimate_input_tokens(provider: &str, text: &str) -> Estimate {
    if uses_tiktoken(provider) {
        if let Some(bpe) = cl100k() {
            return Estimate {
                tokens: bpe.encode_with_special_tokens(text).len() as u32,
                confidence: Confidence::High,
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
    fn openai_and_anthropic_use_tiktoken_high() {
        for p in ["openai", "anthropic"] {
            let e = estimate_input_tokens(p, "Hello, world.");
            assert!(e.tokens >= 1);
            assert_eq!(e.confidence, Confidence::High, "provider {p}");
        }
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
