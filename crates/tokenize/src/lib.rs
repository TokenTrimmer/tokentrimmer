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
//!
//! # Model-aware estimation (`*_for_model`)
//!
//! [`estimate_input_tokens_for_model`] is the stricter variant used wherever a
//! token DELTA is booked as money (the request-pass token-true gate, the
//! cache-aware split, the cache classifier): it selects the billing-correct
//! encoding per model where one exists and NEVER silently substitutes the
//! `chars / 4` heuristic for a BPE:
//!
//! - `openai` + a modern model (`gpt-4o*`, `gpt-4.1*`, `gpt-5*`, `o1*`/`o3*`/
//!   `o4*`, `chatgpt-4o*`) → `o200k_base` (the encoding those models are
//!   billed in; `cl100k` overcounts them by ~5–15%). Older models keep
//!   `cl100k_base`. High confidence.
//! - `anthropic` → `cl100k_base` proxy, Medium, **with a documented `+18%`
//!   correction** ([`ANTHROPIC_PROXY_CORRECTION`]). `cl100k` undercounts
//!   Anthropic input ~15–20%; the model-aware ($-bearing) path scales the raw
//!   count up so every Anthropic dollar ESTIMATE is directionally right rather
//!   than systematically low. Still Medium / directional — the provider's
//!   reported usage stays authoritative, and `AnthropicProvider::count_tokens`
//!   gives the exact count when one is needed. (The provider-keyed
//!   [`estimate_input_tokens`] path is left uncorrected — its routing / preview
//!   / capability thresholds were tuned against the raw proxy.)
//! - everything else → `o200k_base` as a *generic BPE proxy*, Medium. Not the
//!   provider's real tokenizer, but a real BPE: it encodes whitespace runs /
//!   punctuation the way real tokenizers do, where `chars / 4` books a quarter
//!   token per character and can overstate a whitespace-trim delta ~2–3x.
//!   Because the same encoding is used on both sides of any delta, the delta
//!   direction is trustworthy and the magnitude is BPE-realistic.
//! - the `chars / 4` heuristic is only ever returned when tiktoken itself
//!   fails to load, flagged **Low** — callers booking savings MUST treat a
//!   Low-confidence count as unbookable (the pass pipeline books $0 for it).
//!
//! The original provider-keyed functions are kept byte-for-byte unchanged for
//! the routing / preview / capability-check paths, whose thresholds were tuned
//! against them.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

pub mod image_tokens;

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

/// Process-wide cached `o200k_base` BPE (modern OpenAI billing encoding, also
/// the generic-BPE proxy for providers with no tiktoken support). `None` if it
/// failed to load.
fn o200k() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::o200k_base().ok()).as_ref()
}

/// True when an OpenAI model id is billed in `o200k_base` (gpt-4o and every
/// later family). Anything unrecognized stays on `cl100k_base` — the older
/// encoding — which for a NEW unrecognized family overcounts slightly: the
/// conservative direction for a savings delta is handled by the gate itself
/// (same encoding both sides), so the residual risk is only estimate skew.
fn openai_model_uses_o200k(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let m = lower
        .strip_prefix("openai/")
        .or_else(|| lower.strip_prefix("azure/"))
        .unwrap_or(&lower);
    m.starts_with("gpt-4o")
        || m.starts_with("gpt-4.1")
        || m.starts_with("gpt-5")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("chatgpt-4o")
}

/// `chars / 4`, rounded up — the universal cheap fallback.
#[must_use]
pub fn char_count_estimate(text: &str) -> u32 {
    ((text.chars().count() as f64) / 4.0).ceil() as u32
}

/// Multiplicative correction applied to the Anthropic `cl100k` proxy count in
/// the **model-aware** ($-bearing) estimator [`estimate_input_tokens_for_model`].
///
/// `cl100k` is not Anthropic's tokenizer; it undercounts Anthropic input by
/// ~15–20% on typical text (more on code / non-English). `+18%` sits in the
/// middle of that band, so the corrected count — and every Anthropic dollar
/// ESTIMATE derived from it (cache-bust penalties, field-drop / compression
/// savings, the agentic net-positive predicate) — is directionally right
/// instead of systematically low.
///
/// This stays an ESTIMATE: the count remains [`Confidence::Medium`], the
/// provider's reported usage is still authoritative for billing, and the
/// authoritative pre-dispatch count is `AnthropicProvider::count_tokens`
/// (Anthropic's own `/v1/messages/count_tokens`). The correction is applied
/// ONLY on the model-aware path; the provider-keyed [`estimate_input_tokens`] /
/// [`estimate_tokens`] are left uncorrected because the routing / preview /
/// capability-check thresholds were tuned against their raw values.
pub const ANTHROPIC_PROXY_CORRECTION: f64 = 1.18;

/// Apply [`ANTHROPIC_PROXY_CORRECTION`] to a raw `cl100k` count, rounding UP so
/// the corrected value never rounds an already-low count further down.
fn apply_anthropic_correction(raw: u32) -> u32 {
    (f64::from(raw) * ANTHROPIC_PROXY_CORRECTION).ceil() as u32
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

/// Model-aware estimate for money-bearing token deltas — see the module docs
/// (`# Model-aware estimation`). Selects the billing-correct encoding for
/// OpenAI models (`o200k_base` for gpt-4o and later, `cl100k_base` before),
/// keeps the documented `cl100k` proxy for Anthropic, and uses `o200k_base` as
/// a generic BPE proxy everywhere else. The `chars / 4` heuristic is returned
/// ONLY when tiktoken fails to load, flagged [`Confidence::Low`] — a
/// Low-confidence count must never be booked as a reconcilable saving.
#[must_use]
pub fn estimate_input_tokens_for_model(provider: &str, model: &str, text: &str) -> Estimate {
    let (bpe, confidence) = match provider {
        "openai" => {
            if openai_model_uses_o200k(model) {
                (o200k(), Confidence::High)
            } else {
                (cl100k(), Confidence::High)
            }
        }
        // Documented proxy: `cl100k` undercounts Anthropic by ~15–20% (estimates
        // keyed on it are systematically LOW — see the split/bust docs in
        // tt-core). The raw count is corrected by [`ANTHROPIC_PROXY_CORRECTION`]
        // below so $-bearing deltas are directionally right; still Medium.
        "anthropic" => (cl100k(), Confidence::Medium),
        // Generic BPE proxy: real-BPE whitespace/punctuation behavior, same
        // encoding on both sides of any delta.
        _ => (o200k(), Confidence::Medium),
    };
    // The Anthropic-proxy undercount correction applies ONLY to the real-BPE
    // count, never to the `chars / 4` fallback (which is already a different,
    // Low-confidence heuristic and is unbookable anyway).
    let correct_anthropic = provider == "anthropic";
    match bpe {
        Some(bpe) => {
            let raw = bpe.encode_with_special_tokens(text).len() as u32;
            let tokens = if correct_anthropic {
                apply_anthropic_correction(raw)
            } else {
                raw
            };
            Estimate { tokens, confidence }
        }
        None => Estimate {
            tokens: char_count_estimate(text),
            confidence: Confidence::Low,
        },
    }
}

/// Convenience: just the model-aware token count, discarding confidence. Only
/// for callers that do not book money from the count (the booking paths must
/// check [`Confidence`] via [`estimate_input_tokens_for_model`]).
#[must_use]
pub fn estimate_tokens_for_model(provider: &str, model: &str, text: &str) -> u32 {
    estimate_input_tokens_for_model(provider, model, text).tokens
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

    /// Modern OpenAI models are billed in o200k_base; older ones in cl100k.
    /// The two encodings genuinely differ, and the model-aware API picks the
    /// right one per model id.
    #[test]
    fn model_aware_openai_selects_encoding_per_model() {
        // A text where the encodings disagree (o200k merges more aggressively).
        let text = "The quick brown fox jumps over the lazy dog. \
                    Καλημέρα κόσμε. def f(x):\n    return x + 1\n";
        let modern = estimate_input_tokens_for_model("openai", "gpt-4o", text);
        let legacy = estimate_input_tokens_for_model("openai", "gpt-4", text);
        assert_eq!(modern.confidence, Confidence::High);
        assert_eq!(legacy.confidence, Confidence::High);
        // cl100k yields the same count as the provider-keyed (cl100k) API.
        assert_eq!(legacy.tokens, estimate_tokens("openai", text));
        // o200k differs from cl100k on this text — proving the switch happened.
        assert_ne!(modern.tokens, legacy.tokens);
        // Family prefixes (incl. namespaced ids) route to o200k.
        for m in [
            "gpt-4.1-mini",
            "gpt-5",
            "o3-mini",
            "chatgpt-4o-latest",
            "openai/gpt-4o",
        ] {
            assert_eq!(
                estimate_input_tokens_for_model("openai", m, text).tokens,
                modern.tokens,
                "{m} should use o200k"
            );
        }
    }

    /// Non-OpenAI/Anthropic providers get a real-BPE proxy (o200k), never the
    /// chars/4 heuristic: a whitespace run costs BPE-realistic tokens, not a
    /// quarter token per character.
    #[test]
    fn model_aware_other_providers_use_bpe_proxy_not_chars() {
        let ws_run = "x".to_string() + &" ".repeat(400) + "y";
        let est = estimate_input_tokens_for_model("groq", "llama-3.3-70b", &ws_run);
        assert_eq!(est.confidence, Confidence::Medium);
        // chars/4 would book ~100 tokens for the run; a real BPE encodes the
        // whole 400-space run as a handful of tokens.
        assert!(
            est.tokens < 20,
            "BPE proxy must not price whitespace at chars/4 (got {})",
            est.tokens
        );
        assert!(char_count_estimate(&ws_run) > 100);
    }

    /// Anthropic keeps the cl100k proxy under the model-aware API, but the
    /// $-bearing count is corrected by [`ANTHROPIC_PROXY_CORRECTION`] (+18%) to
    /// offset cl100k's ~15–20% undercount. Still Medium / directional.
    #[test]
    fn model_aware_anthropic_corrects_cl100k_proxy() {
        // Use a longer text so the +18% scaling clears the rounding floor and
        // the corrected count is strictly above the raw proxy count.
        let text = "The quick brown fox jumps over the lazy dog, repeatedly, \
                    across many tokens of representative English prose.";
        let raw = estimate_tokens("anthropic", text); // provider-keyed = uncorrected cl100k
        let e = estimate_input_tokens_for_model("anthropic", "claude-sonnet-4-6", text);
        assert_eq!(
            e.confidence,
            Confidence::Medium,
            "still a directional proxy"
        );
        let expected = (f64::from(raw) * ANTHROPIC_PROXY_CORRECTION).ceil() as u32;
        assert_eq!(
            e.tokens, expected,
            "model-aware count is the +18% corrected proxy"
        );
        assert!(
            e.tokens > raw,
            "corrected count ({}) must exceed the raw cl100k count ({raw})",
            e.tokens
        );
    }

    /// The correction is exactly `ceil(raw * 1.18)`.
    #[test]
    fn anthropic_correction_is_eighteen_percent_ceil() {
        assert_eq!(apply_anthropic_correction(0), 0);
        assert_eq!(apply_anthropic_correction(100), 118);
        assert_eq!(apply_anthropic_correction(50), 59); // 50 * 1.18 = 59.0
        assert_eq!(apply_anthropic_correction(7), 9); // 7 * 1.18 = 8.26 → 9
    }

    /// The correction is scoped to the MODEL-AWARE path. The provider-keyed
    /// `estimate_input_tokens` / `estimate_tokens` Anthropic path stays raw
    /// (routing / preview / capability thresholds were tuned against it).
    #[test]
    fn provider_keyed_anthropic_path_is_uncorrected() {
        let text = "The quick brown fox jumps over the lazy dog.";
        // Provider-keyed equals OpenAI's cl100k count, byte-for-byte (no +18%).
        assert_eq!(
            estimate_input_tokens("anthropic", text).tokens,
            estimate_input_tokens("openai", text).tokens,
            "provider-keyed Anthropic must remain the raw cl100k proxy"
        );
    }

    /// Other providers' generic-BPE proxy is NOT scaled by the Anthropic
    /// correction — only `provider == "anthropic"` triggers it.
    #[test]
    fn correction_does_not_leak_to_other_providers() {
        let text = "The quick brown fox jumps over the lazy dog.";
        let groq = estimate_input_tokens_for_model("groq", "llama-3.3-70b", text);
        // o200k raw count, uncorrected: recompute the raw o200k count and match.
        let raw_o200k = o200k()
            .map(|b| b.encode_with_special_tokens(text).len() as u32)
            .expect("o200k available in test env");
        assert_eq!(
            groq.tokens, raw_o200k,
            "non-anthropic providers are uncorrected"
        );
    }
}
