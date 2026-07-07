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

use base64::Engine as _;

use crate::types::{EstimationConfidence, Message};
use tt_tokenize::image_tokens::{estimate_image_tokens, ImageDetail};

/// Assumed output length when the caller gives no `max_tokens` — a typical
/// short completion. Clamped to the model's real catalog max-output when known.
const DEFAULT_OUTPUT_TOKENS: u32 = 512;

/// Nominal square dimension assumed for an image whose real dimensions can't be
/// read (remote `http(s)` URL, unknown/unsupported format, or an unreadable
/// header). Paired with [`ImageDetail::High`] so an un-inspectable image still
/// prices at a realistic, non-trivial token cost rather than ~0.
const FALLBACK_IMAGE_DIM: u32 = 1024;

/// Upper bound on the number of base64 header chars we decode to read image
/// dimensions. Enough for a PNG IHDR (always in the first 24 bytes) and to scan
/// past typical JPEG APP0/APP1 (EXIF) segments to the SOF marker, without
/// decoding a multi-megabyte inline payload just to read a width/height. A
/// multiple of 4 so any prefix slice decodes as whole base64 groups.
const HEADER_B64_CHARS: usize = 65_536;

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

/// Sum the estimated input tokens of every inline `image_url` part across all
/// messages, using the model's per-provider image-token formula.
///
/// This is deliberately SEPARATE from [`concat_message_text`]: image tokens are
/// a numeric estimate, not text that can be concatenated and re-tokenized. The
/// caller adds this total to the text-token estimate so an image-heavy request
/// stops previewing as ~0 tokens (the historical bug — `concat_message_text`
/// only reads `p["text"]`). Dimensions are read from an inline `data:` URL's
/// PNG/JPEG header; a remote/unreadable image falls back to a documented nominal
/// size ([`FALLBACK_IMAGE_DIM`]²) — it is never dropped and never errors.
pub fn sum_image_tokens(messages: &[Message], model: &str) -> u32 {
    let mut total: u32 = 0;
    for m in messages {
        let Some(parts) = m.content.as_array() else {
            continue;
        };
        for p in parts {
            if p.get("type").and_then(|v| v.as_str()) != Some("image_url") {
                continue;
            }
            let img = p.get("image_url");
            let url = img.and_then(|v| v.get("url")).and_then(|v| v.as_str());
            let detail_hint = img.and_then(|v| v.get("detail")).and_then(|v| v.as_str());
            let hinted = parse_detail(detail_hint);

            let (w, h, detail) = match url.and_then(image_dims_from_data_url) {
                Some((w, h)) => (w, h, hinted),
                // Remote/unreadable → nominal square @ High (unless the caller
                // explicitly asked for Low, which is flat regardless of dims).
                None => {
                    let d = if hinted == ImageDetail::Low {
                        ImageDetail::Low
                    } else {
                        ImageDetail::High
                    };
                    (FALLBACK_IMAGE_DIM, FALLBACK_IMAGE_DIM, d)
                }
            };
            total = total.saturating_add(estimate_image_tokens(model, w, h, detail));
        }
    }
    total
}

/// Map an OpenAI `detail` hint to [`ImageDetail`]; absent/unknown → `Auto`
/// (which the estimator prices as full tiling, same as `High`).
fn parse_detail(hint: Option<&str>) -> ImageDetail {
    match hint.map(str::to_ascii_lowercase).as_deref() {
        Some("low") => ImageDetail::Low,
        Some("high") => ImageDetail::High,
        _ => ImageDetail::Auto,
    }
}

/// Read `(width, height)` from an inline base64 `data:` image URL by parsing the
/// image header only — no full decode, no heavy image crate. Returns `None` for
/// remote URLs, non-`data:` URLs, or formats/headers we don't recognise (PNG and
/// JPEG are supported); the caller then falls back to a nominal size.
fn image_dims_from_data_url(url: &str) -> Option<(u32, u32)> {
    let (_media_type, b64) = tt_shared::messages::parse_data_url(url)?;
    // Decode only a bounded header prefix. Truncate to a whole number of base64
    // groups (multiple of 4) so the prefix decodes without padding errors.
    let take = b64.len().min(HEADER_B64_CHARS);
    let take = take - (take % 4);
    let prefix = b64.as_bytes().get(..take)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(prefix)
        .ok()?;
    png_dims(&bytes).or_else(|| jpeg_dims(&bytes))
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

    /// Smallest valid PNG (1x1). Its IHDR reports width=1, height=1.
    const PNG_1X1: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAWJcCq0AAAAASUVORK5CYII=";

    #[test]
    fn image_part_adds_tokens() {
        // A 1x1 PNG data URL reads as 1x1 → gpt-4o tiling (Auto≈High) = one 512px
        // tile → 85 + 170 = 255. The key regression: NOT zero.
        let msgs = vec![Message {
            role: "user".into(),
            content: json!([{"type": "image_url", "image_url": {"url": PNG_1X1}}]),
        }];
        let n = sum_image_tokens(&msgs, "gpt-4o");
        assert_eq!(n, 255, "image-only request must not estimate 0 tokens");
    }

    #[test]
    fn text_only_has_no_image_tokens() {
        let msgs = vec![user("just some text")];
        assert_eq!(sum_image_tokens(&msgs, "gpt-4o"), 0);
        let msgs = vec![Message {
            role: "user".into(),
            content: json!([{"type": "text", "text": "hello"}]),
        }];
        assert_eq!(sum_image_tokens(&msgs, "gpt-4o"), 0);
    }

    #[test]
    fn remote_image_falls_back_to_nominal_size() {
        // A remote URL can't be inspected → nominal 1024x1024 @ High → gpt-4o
        // 765 tokens (2x2 tiles). Never 0, never a panic.
        let msgs = vec![Message {
            role: "user".into(),
            content: json!([{"type": "image_url", "image_url": {"url": "https://example.com/cat.png"}}]),
        }];
        assert_eq!(sum_image_tokens(&msgs, "gpt-4o"), 765);
    }

    #[test]
    fn low_detail_hint_is_flat_for_openai_tile() {
        // detail:low is flat 85 for the OpenAI tile family regardless of dims.
        let msgs = vec![Message {
            role: "user".into(),
            content: json!([{"type": "image_url", "image_url": {"url": PNG_1X1, "detail": "low"}}]),
        }];
        assert_eq!(sum_image_tokens(&msgs, "gpt-4o"), 85);
    }

    #[test]
    fn multiple_images_sum() {
        let msgs = vec![Message {
            role: "user".into(),
            content: json!([
                {"type": "text", "text": "compare these"},
                {"type": "image_url", "image_url": {"url": PNG_1X1}},
                {"type": "image_url", "image_url": {"url": PNG_1X1}},
            ]),
        }];
        assert_eq!(sum_image_tokens(&msgs, "gpt-4o"), 510); // 255 * 2
    }

    #[test]
    fn png_dims_reads_ihdr() {
        // Decode the 1x1 PNG through the real data-URL path.
        assert_eq!(image_dims_from_data_url(PNG_1X1), Some((1, 1)));
    }
}
