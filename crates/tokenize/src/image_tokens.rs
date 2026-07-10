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

/// Nominal square dimension assumed for an image whose real dimensions can't be
/// read (an un-decodable header, an unsupported format, an audio/PDF document
/// part with no pixel-token analogue). Paired with [`ImageDetail::High`] so an
/// un-inspectable image still prices at a realistic, non-trivial token cost
/// rather than ~0. Shared by `tt-tokenize` callers + `tt-preview`.
pub const FALLBACK_IMAGE_DIM: u32 = 1024;

/// Read `(width, height)` from the first bytes of a decoded image by parsing
/// the image header ONLY — no full decode, no heavy image crate. Supports PNG
/// (IHDR chunk) and JPEG (SOF marker scan). Returns `None` for any other format,
/// a truncated header, or a garbage prefix; the caller then falls back to
/// [`FALLBACK_IMAGE_DIM`].
///
/// Pure + allocation-free: the same header bytes the provider would bill on
/// (PNG dims live in the first 24 bytes; JPEG dims follow the SOF marker past
/// the EXIF/APP segments). Shared between `tt-preview`'s data-URL path + the
/// Document Lane D4c-v2 seam (which keeps the raw decoded bytes pre-distillation
/// so it can book the vision-avoided saving the substituted image represented).
#[must_use]
pub fn image_dims_from_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
    png_dims(bytes).or_else(|| jpeg_dims(bytes))
}

/// PNG dimensions from the IHDR chunk (width/height are big-endian u32 at bytes
/// 16..24, immediately after the 8-byte signature + IHDR length/type).
fn png_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 24 || bytes[..8] != SIG {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

/// JPEG dimensions by scanning segment markers for an SOF (Start-Of-Frame)
/// marker; the frame header carries height then width as big-endian u16.
fn jpeg_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    // Must start with SOI (0xFF 0xD8).
    if bytes.len() < 2 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 1 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        // Skip any 0xFF fill bytes to land on the marker code.
        let mut mp = i;
        while mp < bytes.len() && bytes[mp] == 0xFF {
            mp += 1;
        }
        if mp >= bytes.len() {
            break;
        }
        let marker = bytes[mp];
        // Standalone markers with no length payload: SOI/EOI and RSTn/TEM.
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            i = mp + 1;
            continue;
        }
        // Every other marker is followed by a 2-byte big-endian segment length
        // (which includes those 2 length bytes).
        let len_hi = *bytes.get(mp + 1)?;
        let len_lo = *bytes.get(mp + 2)?;
        let seg_len = u16::from_be_bytes([len_hi, len_lo]) as usize;
        // SOF0..SOF15 carry the frame dimensions; C4 (DHT), C8 (JPG), CC (DAC)
        // are NOT frame headers and are excluded.
        let is_sof = matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF);
        if is_sof {
            // FF Cn LenHi LenLo Precision HeightHi HeightLo WidthHi WidthLo
            let h = u16::from_be_bytes([*bytes.get(mp + 4)?, *bytes.get(mp + 5)?]);
            let w = u16::from_be_bytes([*bytes.get(mp + 6)?, *bytes.get(mp + 7)?]);
            return Some((u32::from(w), u32::from(h)));
        }
        i = mp + 1 + seg_len.max(2);
    }
    None
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

    // The header-only dimension parser. A real 1x1 PNG header → (1,1); a
    // truncated/garbage/unsupported prefix → None (the caller falls back).
    use base64::Engine as _;
    const PNG_1X1_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAWJcCq0AAAAASUVORK5CYII=";

    fn png_1x1_bytes() -> Vec<u8> {
        base64_decode_header(PNG_1X1_B64)
    }

    /// Decode the test fixture's base64 payload (the data-URL wrapper is stripped
    /// at the call site that owns it; the shared parser takes raw image bytes).
    fn base64_decode_header(b64: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap()
    }

    #[test]
    fn image_dims_from_bytes_reads_png() {
        assert_eq!(image_dims_from_bytes(&png_1x1_bytes()), Some((1, 1)));
    }

    #[test]
    fn image_dims_empty_returns_none() {
        assert_eq!(image_dims_from_bytes(&[]), None);
    }

    #[test]
    fn image_dims_garbage_returns_none() {
        // No PNG signature, no JPEG SOI.
        assert_eq!(image_dims_from_bytes(&[0x00, 0x01, 0x02, 0x03]), None);
    }

    #[test]
    fn image_dims_truncated_png_returns_none() {
        // A PNG signature but shorter than the 24-byte IHDR tail → None.
        let mut bytes = png_1x1_bytes();
        bytes.truncate(20);
        assert_eq!(image_dims_from_bytes(&bytes), None);
    }

    #[test]
    fn image_dims_unsupported_format_returns_none() {
        // A GIF89a header is a known image format the parser does not read.
        assert_eq!(image_dims_from_bytes(b"GIF89a rest of the bytes"), None);
    }
}
