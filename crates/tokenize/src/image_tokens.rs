//! Deterministic, model-family-keyed image-token estimator (no LLM, no network).
//!
//! Formulas verified vs provider docs (June 2026); they shift by model version,
//! so the family is resolved from the model id, never a per-call constant
//! (Claude Opus 4.7 roughly tripled its image-token cost across a version bump).
//! This is a directional ESTIMATE — the provider's reported usage stays
//! authoritative for billing. It exists so image-heavy requests stop pricing as
//! ~0 tokens on every preview surface.

/// OpenAI-style detail hint. `Auto`/`High` price the full tiling; `Low` is flat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageDetail {
    Low,
    High,
    Auto,
}

/// The per-provider image-token formula family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormula {
    /// Claude: `ceil(w/28) * ceil(h/28)` (≈ w·h/750).
    Claude,
    /// OpenAI GPT-4o high-detail: `85 + 170 * tiles` over 512px tiles.
    OpenAiTile,
    /// OpenAI GPT-5.x / GPT-4.1 / o-series: `ceil(w/32) * ceil(h/32)` patches,
    /// capped at 1536.
    OpenAiPatch,
    /// Gemini: `258` flat for small images, else `ceil(w/768) * ceil(h/768) * 258`.
    GeminiTiled,
}

/// Classify a model id into its image-token formula family (case-insensitive).
///
/// Prefix rules (verified against provider docs, June 2026):
/// - `claude*` / `anthropic*` → [`ImageFormula::Claude`]
/// - `gemini*` → [`ImageFormula::GeminiTiled`]
/// - `gpt-4o*` / `chatgpt-4o*` → [`ImageFormula::OpenAiTile`]
/// - `gpt-5*` / `gpt-4.1*` / `o1*` / `o3*` / `o4*` → [`ImageFormula::OpenAiPatch`]
/// - anything else → [`ImageFormula::OpenAiTile`] (the conservative middle default)
#[must_use]
pub fn formula_for_model(model: &str) -> ImageFormula {
    // Tolerate `provider/model` namespacing (e.g. `openai/gpt-4o`).
    let m = model.to_ascii_lowercase();
    let m = m.rsplit('/').next().unwrap_or(m.as_str());
    if m.starts_with("claude") || m.starts_with("anthropic") {
        ImageFormula::Claude
    } else if m.starts_with("gemini") {
        ImageFormula::GeminiTiled
    } else if m.starts_with("gpt-4o") || m.starts_with("chatgpt-4o") {
        ImageFormula::OpenAiTile
    } else if m.starts_with("gpt-5")
        || m.starts_with("gpt-4.1")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
    {
        ImageFormula::OpenAiPatch
    } else {
        // Unknown model — GPT-4o-style tiling is the conservative middle estimate.
        ImageFormula::OpenAiTile
    }
}

#[inline]
fn ceil_div(a: u32, b: u32) -> u32 {
    a.div_ceil(b)
}

/// Estimate the input-token cost of a single image at `width`x`height` for
/// `model`. Deterministic, pure, no I/O. Dimensions are clamped to a minimum of
/// 1px so a degenerate/unknown image still prices at the family minimum rather
/// than zero.
#[must_use]
pub fn estimate_image_tokens(model: &str, width: u32, height: u32, detail: ImageDetail) -> u32 {
    let (w, h) = (width.max(1), height.max(1));
    match formula_for_model(model) {
        ImageFormula::Claude => ceil_div(w, 28) * ceil_div(h, 28),
        ImageFormula::OpenAiTile => {
            if detail == ImageDetail::Low {
                return 85;
            }
            // Scale into 2048x2048, then shortest side to 768, then 512px tiles.
            let (mut w2, mut h2) = (w, h);
            let longest = w2.max(h2);
            if longest > 2048 {
                w2 = (u64::from(w2) * 2048 / u64::from(longest)) as u32;
                h2 = (u64::from(h2) * 2048 / u64::from(longest)) as u32;
            }
            let shortest = w2.min(h2).max(1);
            if shortest > 768 {
                w2 = (u64::from(w2) * 768 / u64::from(shortest)) as u32;
                h2 = (u64::from(h2) * 768 / u64::from(shortest)) as u32;
            }
            let tiles = ceil_div(w2.max(1), 512) * ceil_div(h2.max(1), 512);
            85 + 170 * tiles
        }
        ImageFormula::OpenAiPatch => (ceil_div(w, 32) * ceil_div(h, 32)).min(1536),
        ImageFormula::GeminiTiled => {
            if w <= 384 && h <= 384 {
                258
            } else {
                ceil_div(w, 768) * ceil_div(h, 768) * 258
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_formula_ceil_div_28() {
        // 1024x1024 → ceil(1024/28)=37 ; 37*37 = 1369
        assert_eq!(
            estimate_image_tokens("claude-opus-4-8", 1024, 1024, ImageDetail::High),
            1369
        );
    }

    #[test]
    fn openai_4o_low_detail_is_flat_85() {
        assert_eq!(
            estimate_image_tokens("gpt-4o", 2048, 2048, ImageDetail::Low),
            85
        );
    }

    #[test]
    fn openai_4o_high_detail_tiles() {
        // 1024x1024 → within 2048, shortest side to 768 → 768x768 → 2x2 tiles = 4
        // 85 + 170*4 = 765
        assert_eq!(
            estimate_image_tokens("gpt-4o", 1024, 1024, ImageDetail::High),
            765
        );
    }

    #[test]
    fn openai_patch_capped_at_1536() {
        // Huge image → ceil(w/32)*ceil(h/32) clamped to 1536.
        assert_eq!(
            estimate_image_tokens("gpt-5", 100_000, 100_000, ImageDetail::High),
            1536
        );
    }

    #[test]
    fn gemini_small_is_flat_258() {
        assert_eq!(
            estimate_image_tokens("gemini-2.5-flash", 300, 300, ImageDetail::High),
            258
        );
    }

    #[test]
    fn gemini_large_is_tiled_258() {
        // 1000x1000 → ceil(1000/768)=2 → 2*2*258 = 1032
        assert_eq!(
            estimate_image_tokens("gemini-2.5-pro", 1000, 1000, ImageDetail::High),
            1032
        );
    }

    #[test]
    fn unknown_model_defaults_to_openai_tile() {
        assert!(matches!(
            formula_for_model("mystery-model"),
            ImageFormula::OpenAiTile
        ));
    }

    #[test]
    fn formula_classification_prefixes() {
        assert!(matches!(
            formula_for_model("claude-sonnet-4-6"),
            ImageFormula::Claude
        ));
        assert!(matches!(
            formula_for_model("anthropic/claude-3"),
            ImageFormula::Claude
        ));
        assert!(matches!(
            formula_for_model("GPT-4o-mini"),
            ImageFormula::OpenAiTile
        ));
        assert!(matches!(
            formula_for_model("gpt-4.1-mini"),
            ImageFormula::OpenAiPatch
        ));
        assert!(matches!(
            formula_for_model("o3-mini"),
            ImageFormula::OpenAiPatch
        ));
        assert!(matches!(
            formula_for_model("gemini-2.5-flash"),
            ImageFormula::GeminiTiled
        ));
        // Namespaced ids resolve on the trailing model segment.
        assert!(matches!(
            formula_for_model("openai/gpt-4o"),
            ImageFormula::OpenAiTile
        ));
    }

    #[test]
    fn degenerate_dims_still_nonzero() {
        // A 0x0 (or 1x1) image must still price at the family minimum, never 0.
        assert!(estimate_image_tokens("claude-opus-4-8", 0, 0, ImageDetail::High) >= 1);
        assert_eq!(
            estimate_image_tokens("gpt-4o", 0, 0, ImageDetail::High),
            85 + 170
        );
    }
}
