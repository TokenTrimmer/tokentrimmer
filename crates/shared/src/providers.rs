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
/// | `azure/<deployment>`                                                     | `azure`      |
pub fn infer_provider(model: &str) -> Option<&'static str> {
    if model.is_empty() {
        return None;
    }

    // Azure OpenAI — explicit `azure/<deployment>` prefix. The deployment name
    // after the slash is customer-chosen (need not match an OpenAI model id), so
    // the prefix is the only reliable signal — checked before the OpenAI
    // gpt-*/o3-* prefixes so `azure/gpt-4o-prod` resolves to `azure`, not
    // `openai`.
    if azure_deployment(model).is_some() {
        return Some("azure");
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

/// If `model` is an Azure-prefixed id (`azure/<deployment>`) with a non-empty
/// deployment name, return the deployment (the part after `azure/`); else None.
///
/// Single source of truth for the Azure prefix — used by [`infer_provider`] for
/// routing and by the Azure adapter to strip the prefix down to the bare
/// deployment name it targets on the wire.
pub fn azure_deployment(model: &str) -> Option<&str> {
    model.strip_prefix("azure/").filter(|rest| !rest.is_empty())
}

/// If `model` is a local-backend-prefixed id (`ollama/…`, `vllm/…`,
/// `lmstudio/…`) with a non-empty model name, return the backend id; else None.
/// Single source of truth for local routing — used by the registry resolver,
/// the same-provider exemption, and `LocalProvider`'s prefix strip.
pub fn local_backend(model: &str) -> Option<&'static str> {
    for id in [
        "ollama", "vllm", "lmstudio", "llamacpp", "mlx", "tgi", "sglang",
    ] {
        if let Some(rest) = model.strip_prefix(id).and_then(|r| r.strip_prefix('/')) {
            if !rest.is_empty() {
                return Some(id);
            }
        }
    }
    if let Some(rest) = model.strip_prefix("local/") {
        if let Some((profile, upstream_model)) = rest.split_once('/') {
            if !profile.is_empty() && !upstream_model.is_empty() {
                return Some("local");
            }
        }
    }
    None
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
    fn azure_prefix() {
        // `azure/<deployment>` resolves to azure, taking precedence over the
        // OpenAI gpt-*/o3-* prefixes even when the deployment is named after an
        // OpenAI model.
        assert_eq!(infer_provider("azure/gpt-4o-prod"), Some("azure"));
        assert_eq!(infer_provider("azure/gpt-4o"), Some("azure"));
        assert_eq!(infer_provider("azure/o3"), Some("azure"));
        assert_eq!(infer_provider("azure/my-custom-deployment"), Some("azure"));
        // Bare prefix with no deployment name is not Azure.
        assert_eq!(infer_provider("azure/"), None);
    }

    #[test]
    fn azure_deployment_strips_prefix() {
        assert_eq!(azure_deployment("azure/gpt-4o-prod"), Some("gpt-4o-prod"));
        assert_eq!(azure_deployment("azure/o3"), Some("o3"));
        assert_eq!(azure_deployment("azure/"), None);
        assert_eq!(azure_deployment("gpt-4o"), None);
        assert_eq!(azure_deployment(""), None);
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

    #[test]
    fn local_backend_recognizes_prefixes() {
        assert_eq!(local_backend("ollama/llama3.1:8b"), Some("ollama"));
        assert_eq!(local_backend("vllm/Qwen2.5-7B"), Some("vllm"));
        assert_eq!(local_backend("lmstudio/phi-4"), Some("lmstudio"));
        assert_eq!(local_backend("llamacpp/llama3"), Some("llamacpp"));
        assert_eq!(local_backend("mlx/mistral-7b"), Some("mlx"));
        assert_eq!(local_backend("tgi/falcon"), Some("tgi"));
        assert_eq!(local_backend("sglang/deepseek"), Some("sglang"));
        assert_eq!(local_backend("ollama"), None);
        assert_eq!(local_backend("local/gpu-a/Qwen/Qwen3-8B"), Some("local"));
        assert_eq!(local_backend("local/gpu-a"), None);
        assert_eq!(local_backend("local//model"), None);
        assert_eq!(local_backend("ollama/"), None);
        assert_eq!(local_backend("gpt-4o"), None);
        assert_eq!(local_backend(""), None);
    }
}
