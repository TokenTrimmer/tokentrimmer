//! Lightweight model → provider lookup.
//!
//! Used by cloud-side validation (PATCH `/v1/admin/routes`) and the
//! gateway's defensive logging to detect routes that would cross provider
//! boundaries. The runtime registry in `tt-core` is the source of truth;
//! this helper is a cheap heuristic so callers that can't depend on
//! the full provider registry still get the answer right for the
//! prefixes we actually ship.
//!
//! Conservative semantics: returns `Some(&'static str)` only for prefixes
//! we know for certain belong to a single provider. Ambiguous names
//! (e.g. `llama-3-*` which could be Groq, Together, or OpenRouter) return
//! `None` so the caller doesn't reject a legitimate use of an aggregator.

/// Returns the provider id for `model` when it's a well-known
/// single-provider prefix; otherwise `None`.
///
/// Prefix table (extend in lockstep with the provider crates):
///
/// | Prefix(es)                                                              | Provider     |
/// |-------------------------------------------------------------------------|--------------|
/// | `gpt-*`, `chatgpt-*`, `o3`, `o3-*`, `o4-*`, `o5-*`                       | `openai`     |
/// | `claude-*`                                                              | `anthropic`  |
/// | `gemini-*`                                                              | `gemini`     |
/// | `mistral-*`, `mixtral-*`, `pixtral-*`, `codestral-*`, `ministral-*`      | `mistral`    |
pub fn infer_provider(model: &str) -> Option<&'static str> {
    if model.is_empty() {
        return None;
    }

    // OpenAI — gpt-*, chatgpt-*, o3, o3-*, o4-*, o5-*.
    if model.starts_with("gpt-") || model.starts_with("chatgpt-") {
        return Some("openai");
    }
    if model == "o3" || model.starts_with("o3-") {
        return Some("openai");
    }
    if model.starts_with("o4-") || model.starts_with("o5-") {
        return Some("openai");
    }

    // Anthropic
    if model.starts_with("claude-") {
        return Some("anthropic");
    }

    // Gemini
    if model.starts_with("gemini-") {
        return Some("gemini");
    }

    // Mistral family
    if model.starts_with("mistral-")
        || model.starts_with("mixtral-")
        || model.starts_with("pixtral-")
        || model.starts_with("codestral-")
        || model.starts_with("ministral-")
    {
        return Some("mistral");
    }

    None
}

/// Returns `true` iff both `a` and `b` resolve to known providers AND
/// those providers differ. `false` when either side is unknown (we don't
/// block routes that aggregate through `openrouter` / `together` /
/// `groq` whose model names overlap across providers).
pub fn known_to_differ(a: &str, b: &str) -> bool {
    match (infer_provider(a), infer_provider(b)) {
        (Some(x), Some(y)) => x != y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_prefixes() {
        assert_eq!(infer_provider("gpt-4o"), Some("openai"));
        assert_eq!(infer_provider("gpt-4o-mini"), Some("openai"));
        assert_eq!(infer_provider("gpt-5.5"), Some("openai"));
        assert_eq!(infer_provider("chatgpt-4o-latest"), Some("openai"));
        assert_eq!(infer_provider("o3"), Some("openai"));
        assert_eq!(infer_provider("o3-mini"), Some("openai"));
        assert_eq!(infer_provider("o4-mini"), Some("openai"));
    }

    #[test]
    fn anthropic_prefix() {
        assert_eq!(infer_provider("claude-opus-4-7"), Some("anthropic"));
        assert_eq!(infer_provider("claude-sonnet-4-6"), Some("anthropic"));
        assert_eq!(infer_provider("claude-haiku-4-5"), Some("anthropic"));
    }

    #[test]
    fn gemini_prefix() {
        assert_eq!(infer_provider("gemini-2.5-pro"), Some("gemini"));
        assert_eq!(infer_provider("gemini-1.5-flash"), Some("gemini"));
    }

    #[test]
    fn mistral_family_prefixes() {
        assert_eq!(infer_provider("mistral-large-2407"), Some("mistral"));
        assert_eq!(infer_provider("mixtral-8x22b"), Some("mistral"));
        assert_eq!(infer_provider("pixtral-12b"), Some("mistral"));
        assert_eq!(infer_provider("codestral-22b"), Some("mistral"));
        assert_eq!(infer_provider("ministral-8b"), Some("mistral"));
    }

    #[test]
    fn unknown_returns_none() {
        // Aggregator-routed names overlap across providers — must NOT be
        // assigned to a single provider.
        assert_eq!(infer_provider("llama-3.3-70b"), None);
        assert_eq!(infer_provider("qwen2.5-72b"), None);
        assert_eq!(infer_provider("deepseek-r1"), None);
        assert_eq!(infer_provider("totally-custom-model"), None);
        assert_eq!(infer_provider(""), None);
    }

    #[test]
    fn known_to_differ_only_blocks_known_pairs() {
        // Same provider — not differing.
        assert!(!known_to_differ("gpt-4o", "gpt-4o-mini"));
        assert!(!known_to_differ("claude-sonnet-4-6", "claude-haiku-4-5"));
        // Cross provider — differs, blocks.
        assert!(known_to_differ("gpt-4o", "claude-sonnet-4-6"));
        assert!(known_to_differ("claude-haiku-4-5", "gemini-2.5-pro"));
        // Unknowns — pass through (don't block).
        assert!(!known_to_differ("gpt-4o", "llama-3.3-70b"));
        assert!(!known_to_differ("custom-1", "custom-2"));
        assert!(!known_to_differ("custom-1", "gpt-4o"));
    }
}
