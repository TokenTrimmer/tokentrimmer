//! D4c — the pre-routing document-lane distillation seam.
//!
//! Invoked in `prepare()` AFTER routing (so the matched route's
//! `RouteAction::document_lane` opt-in is known) but BEFORE `SplitRequest::compute`
//! (so the cache-stable prefix + L1/L2 keys are derived from the DISTILLED
//! request). When the route opted in + the request carries document/image
//! content parts, the sidecar distills each to text; the parts are swapped for
//! `ContentPart::Text` so routing can downgrade to a text model + the request
//! is billed at text-model rates. The isolated `doc_vision_saved_est_usd`
//! saving (D4c-v2) is booked from the [`DistillBookkeeping`] the seam returns —
//! the raw image tokens the request WOULD have sent vs the distilled text tokens
//! it now sends, priced at the served model's input rate via D0's
//! `document_projection::project`.
//!
//! # Fail-open (the production posture)
//! Mirrors the sidecar client's fail-open: sidecar disabled (`TT_DOC_SIDECAR_URL`
//! unset) / connection error / timeout / malformed response → the request stays
//! VERBATIM (no distillation, no downgrade, no saving booked) + routes to the
//! vision model as before. The seam is a pure optimization — it must never be
//! able to break a request. Error blobs are never distilled (`should_distill`
//! stays false). The default-CLOSED [`DocDistillGate`] means lossy substitution
//! never happens unless an operator opts in. A part whose extraction fails is
//! left verbatim AND contributes nothing to the bookkeeping — the saving is
//! never over-booked for a part we did not actually replace.
//!
//! # The v1 scope (honest)
//! v1 distills INLINE base64 document/image bytes (URL parts are NOT fetched —
//! a future slice that resolves URLs to bytes, with the SSRF posture the `http`
//! workflow node already enforces). Lossless extractions (PDF text layers)
//! substitute without the judge; LOSSY extractions (OCR images) require the
//! [`DocDistillGate`] + the 0.90 floor. **Gemini direction guard:** the booked
//! saving is $0 for Gemini-targeted downgrades (Gemini prices page-images flat
//! and cheaper than distilled text; never claim a saving the provider's invoice
//! would contradict — the guard lives in `document_projection::project`).

use base64::Engine as _;

use tt_shared::messages::{
    ChatCompletionRequest, ContentPart, DocumentSource, Message, MessageContent,
};
use tt_tokenize::estimate_tokens_for_model;
use tt_tokenize::image_tokens::{
    estimate_image_tokens, image_dims_from_bytes, ImageDetail, FALLBACK_IMAGE_DIM,
};

use super::{sidecar_client, DocDistillGate, SpanFidelity};

/// The sidecar client + gate the seam uses. Resolved ONCE in `prepare()` (the
/// sidecar URL off env, the gate default-CLOSED) + reused per distilled part.
/// Cheap to construct (an `Arc<dyn>` + a bool); the seam early-returns when the
/// sidecar URL is unset (the common case — zero added latency for text traffic).
pub(crate) struct DistillHarness {
    pub client: reqwest::Client,
    pub sidecar_url: Option<String>,
    pub gate: DocDistillGate,
}

impl DistillHarness {
    /// Build the harness from the env. `None` sidecar URL → disabled (the seam
    /// early-returns). The gate is default-CLOSED (the operator opts into lossy
    /// substitution via `DocDistillGate::with_lossy_opt_in`).
    pub(crate) fn from_env() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(sidecar_client::REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            sidecar_url: sidecar_client::sidecar_url_from_env(),
            // Default-closed: `should_distill` always returns false until the
            // operator opts in (D4c v1 keeps the gate closed for lossy spans;
            // lossless PDF-text-layer extractions substitute unconditionally
            // since they skip the judge).
            gate: DocDistillGate::default(),
        }
    }
}

/// The outcome of distilling one content part. The caller filters non-document
/// parts first, so `distill_part` returns `Disabled` (sidecar unset, the common
/// case), `ExtractFailed` (fail-open on error), or `Distilled`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DistillOutcome {
    /// The sidecar is disabled (no `TT_DOC_SIDECAR_URL`) — leave verbatim.
    Disabled,
    /// The part was a document/image but extraction failed (fail-open) — leave
    /// verbatim. Carries no error string (fail-open: we do not surface sidecar
    /// errors to the request path; they're logged at debug level).
    ExtractFailed,
    /// The part was distilled to text. `lossless` governs whether the gate (the
    /// judge) was required (Lossless skips it; Lossy requires the gate).
    Distilled { text: String, lossless: bool },
}

/// The bookkeeping a [`distill_request_parts`] call accrues so the handler can
/// book the isolated `doc_vision_saved_est_usd` (D4c-v2). Only parts that were
/// ACTUALLY substituted contribute — a part that failed extraction / the sidecar
/// is disabled contributes nothing (the saving is never over-booked).
///
/// - `raw_image_tokens` — the input tokens the distilled-away IMAGE parts WOULD
///   have spent at the served model (`estimate_image_tokens` on each image's
///   decoded dims; a PDF/audio/other document part has no pixel-token analogue
///   and contributes 0 here — its saving is the text tokens it displaced, below).
/// - `distilled_text_tokens` — the input tokens the substituted `ContentPart::Text`
///   now spends (real BPE, `estimate_tokens_for_model`).
///
/// The handler prices both at the served model's input rate via D0's
/// `document_projection::project` (which applies the Gemini guard + clamps
/// negatives to 0). All-zero when the sidecar is disabled or nothing distilled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DistillBookkeeping {
    /// Number of content parts substituted for text (0 when none).
    pub distilled_parts: usize,
    /// Raw image input tokens the substituted image parts would have spent.
    pub raw_image_tokens: u32,
    /// Distilled-text input tokens the substituted parts now spend.
    pub distilled_text_tokens: u32,
}

/// Distill ONE content part's document/image payload to text via the sidecar,
/// applying the gate. Pure of the request mutation — returns the outcome so the
/// caller can swap the part.
///
/// `media_type` is the document's MIME (`application/pdf`, `image/png`, …) +
/// `data_base64` is its standard-base64 bytes. Returns `DistillOutcome::Disabled`
/// when the sidecar URL is unset (the common no-sidecar case), `ExtractFailed`
/// on any sidecar error (fail-open), or `Distilled` on success.
pub(crate) async fn distill_part(
    harness: &DistillHarness,
    media_type: &str,
    data_base64: &str,
) -> DistillOutcome {
    let Some(url) = harness.sidecar_url.as_deref() else {
        return DistillOutcome::Disabled;
    };
    let Some(extraction) =
        sidecar_client::extract(&harness.client, Some(url), media_type, data_base64).await
    else {
        // Fail-open: sidecar error / timeout / malformed body → verbatim.
        tracing::debug!(
            target: "tokentrimmer.document_lane",
            media_type,
            "document-lane sidecar extraction failed; leaving part verbatim (fail-open)"
        );
        return DistillOutcome::ExtractFailed;
    };
    let lossless = extraction.is_lossless();
    // The gate: lossless extractions substitute unconditionally; lossy require
    // the gate (which is default-CLOSED → lossy never substitutes unless the
    // operator opted in). `should_distill` is the contract — a closed gate +
    // a lossy span stays verbatim (fail-open to the vision model, no behavior
    // change). A single lossy span that fails the gate blocks the WHOLE
    // extraction's substitution (the request stays verbatim — partial
    // substitution would mix distilled + raw parts, a misleading shape).
    if !lossless
        && !harness.gate.should_distill(
            SpanFidelity::Lossy,
            1.0, /* TODO: judge recall — the lossy-substitution quality gate
                  * (DocDistillGate::should_distill) is still the D4a default-CLOSED
                  * scaffold; wiring the real recall-of-baseline judge + the 0.90
                  * floor is a separate slice from the cost-booking work (which is
                  * done). Lossy spans stay verbatim until then; lossless PDF-text
                  * layers substitute unconditionally + book their saving now. */
        )
    {
        tracing::debug!(
            target: "tokentrimmer.document_lane",
            media_type,
            "document-lane lossy extraction rejected by the gate (closed / judge-failed); verbatim"
        );
        return DistillOutcome::ExtractFailed;
    }
    DistillOutcome::Distilled {
        text: extraction.text,
        lossless,
    }
}

/// Walk the request's messages + swap each inline-base64 Document / data-URL
/// Image part for a `ContentPart::Text` carrying the sidecar's distilled text.
/// Returns a [`DistillBookkeeping`] accrues the raw-image-tokens vs
/// distilled-text-tokens so the handler can book the isolated `doc_vision_saved_est_usd`
/// (D4c-v2) — all-zero when none distilled / the seam early-returned.
///
/// `model` is the SERVED model (the post-routing `target_model` rewrite) — used
/// to (a) pick the per-provider image-token formula for the raw image tokens +
/// (b) BPE-encode the distilled text. v1 distills INLINE base64 document bytes +
/// data-URL image bytes ONLY (remote URLs are not fetched — a future slice).
/// After substitution, the caller recomputes `request_has_images`/
/// `request_has_documents` (now false → the route can downgrade to a text model).
pub(crate) async fn distill_request_parts(
    harness: &DistillHarness,
    model: &str,
    req: &mut ChatCompletionRequest,
) -> DistillBookkeeping {
    // Early-return: when the sidecar is disabled (the common case), skip the
    // message walk entirely — zero added latency for text traffic.
    if harness.sidecar_url.is_none() {
        return DistillBookkeeping::default();
    }
    let mut booking = DistillBookkeeping::default();
    for message in &mut req.messages {
        // Only User / System / Tool messages carry a `Parts` content the seam
        // mutates (Assistant content is optional + treated as verbatim).
        let parts: &mut Vec<ContentPart> = match message {
            Message::User { content, .. }
            | Message::System { content }
            | Message::Tool { content, .. } => {
                if let MessageContent::Parts(p) = content {
                    p
                } else {
                    continue;
                }
            }
            Message::Assistant { content, .. } => {
                if let Some(MessageContent::Parts(p)) = content.as_mut() {
                    p
                } else {
                    continue;
                }
            }
        };
        for part in parts.iter_mut() {
            // `is_image` flags the image parts whose raw pixel tokens we accrue
            // to `raw_image_tokens` (a PDF/audio document part has no pixel-token
            // analogue — its saving comes entirely from the displaced text tokens).
            let (media_type, data_b64, is_image) = match part {
                ContentPart::Document { document } => match &document.source {
                    DocumentSource::Base64 { media_type, data } => {
                        (media_type.clone(), data.clone(), false)
                    }
                    // v1: URLs are not fetched — leave verbatim.
                    DocumentSource::Url { .. } => continue,
                },
                // v1: data-URL images (base64-inline). Remote image URLs are
                // left verbatim (a future fetch slice).
                ContentPart::ImageUrl { image_url } => {
                    if let Some((media, b64)) = parse_data_url(&image_url.url) {
                        (media, b64, true)
                    } else {
                        continue;
                    }
                }
                _ => continue, // Text, audio — not distilled.
            };
            let DistillOutcome::Distilled { text, .. } =
                distill_part(harness, &media_type, &data_b64).await
            else {
                // NotADocument / Disabled / ExtractFailed → leave verbatim, accrue
                // nothing (never book a saving for a part we did not replace).
                continue;
            };
            // Accrue the raw image tokens the substituted image WOULD have spent.
            // Only image parts contribute (a PDF document's pixels are not billed
            // as image tokens — the sidecar extracted its text layer). Un-decodable
            // dims → the nominal square (mirrors tt-preview's fallback).
            if is_image {
                let dims = image_dims_from_decoded_b64(&data_b64)
                    .unwrap_or((FALLBACK_IMAGE_DIM, FALLBACK_IMAGE_DIM));
                booking.raw_image_tokens =
                    booking
                        .raw_image_tokens
                        .saturating_add(estimate_image_tokens(
                            model,
                            dims.0,
                            dims.1,
                            ImageDetail::Auto,
                        ));
            }
            // Accrue the distilled text tokens the substituted part now spends
            // (real BPE; provider "" → o200k proxy, a sound directional estimate).
            booking.distilled_text_tokens = booking
                .distilled_text_tokens
                .saturating_add(estimate_tokens_for_model("", model, &text));
            *part = ContentPart::Text { text };
            booking.distilled_parts += 1;
        }
    }
    booking
}

/// Parse a `data:<media>;base64,<payload>` URL into `(media_type, base64_payload)`.
/// Returns `None` for a non-data URL (remote) or a malformed data URL.
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    // The header is `<media>;base64` (or just `<media>` for a non-base64 data
    // URL — we only distill base64).
    if !header.ends_with(";base64") {
        return None;
    }
    let media_type = header.strip_suffix(";base64")?;
    let media_type = if media_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        media_type.to_string()
    };
    Some((media_type, payload.to_string()))
}

/// Decode a bounded prefix of a standard-base64 image payload + read its
/// `(width, height)` via the shared header-only parser. Returns `None` when the
/// base64 is malformed or the header is an unsupported/truncated format — the
/// caller then falls back to the nominal square. Bounds the decode to a header
/// prefix (PNG/JPEG dims live in the first bytes) so a multi-megabyte inline
/// image is not fully decoded just to read its dimensions. Mirrors the bounded
/// decode in `tt-preview::token_estimator::image_dims_from_data_url`.
fn image_dims_from_decoded_b64(b64: &str) -> Option<(u32, u32)> {
    /// Upper bound on the base64 header chars we decode. Enough for a PNG IHDR
    /// (first 24 bytes) + scanning past typical JPEG APP0/APP1 (EXIF) segments
    /// to the SOF marker. A multiple of 4 so any prefix slice decodes as whole
    /// base64 groups (no padding error).
    const HEADER_B64_CHARS: usize = 65_536;
    let take = b64.len().min(HEADER_B64_CHARS);
    let take = take - (take % 4);
    let prefix = b64.as_bytes().get(..take)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(prefix)
        .ok()?;
    image_dims_from_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Parallel workspace-test exec races env reads/writes across these
    // env-touching tests (they set/remove TT_DOC_SIDECAR_URL). The lock
    // serializes them so each sees a consistent env across the `await` that
    // reads it (mirrors the ml-scoring ENV_LOCK fix from #302, but async-aware
    // so the guard may legitimately span the mocked-sidecar round-trip).
    // stdlib `serial_test` would do this too; a local Mutex avoids the dep.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn parse_data_url_extracts_media_and_payload() {
        let (media, data) = parse_data_url("data:application/pdf;base64,JVBERi0=").unwrap();
        assert_eq!(media, "application/pdf");
        assert_eq!(data, "JVBERi0=");
    }

    #[test]
    fn parse_data_url_rejects_non_base64() {
        // A non-base64 data URL → not distilled (v1).
        assert!(parse_data_url("data:text/plain,hello").is_none());
    }

    #[test]
    fn parse_data_url_rejects_remote_url() {
        assert!(parse_data_url("https://example.com/doc.pdf").is_none());
        assert!(parse_data_url("not a url").is_none());
    }

    #[test]
    fn parse_data_url_defaults_media_when_empty() {
        let (media, _) = parse_data_url("data:;base64,Zm9v").unwrap();
        assert_eq!(media, "application/octet-stream");
    }

    #[tokio::test]
    async fn harness_from_env_is_disabled_without_sidecar_url() {
        // No env var set → disabled (the common no-sidecar case) + the gate is
        // default-CLOSED (lossy substitution never fires unless opted in).
        let _guard = ENV_LOCK.lock().await;
        std::env::remove_var("TT_DOC_SIDECAR_URL");
        let h = DistillHarness::from_env();
        assert!(h.sidecar_url.is_none());
        assert!(!h.gate.lossy_opt_in());
    }

    #[tokio::test]
    async fn distill_part_returns_disabled_without_sidecar_url() {
        let _guard = ENV_LOCK.lock().await;
        std::env::remove_var("TT_DOC_SIDECAR_URL");
        let h = DistillHarness::from_env();
        // Disabled path early-returns BEFORE any network — safe to await here.
        assert_eq!(
            distill_part(&h, "application/pdf", "JVBERi0=").await,
            DistillOutcome::Disabled
        );
    }

    #[tokio::test]
    async fn distill_request_parts_disabled_sidecar_is_zero_bookkeeping() {
        // No sidecar URL → the seam early-returns with all-zero bookkeeping — no
        // mutation, no network, no saving booked (the common no-sidecar case).
        let _guard = ENV_LOCK.lock().await;
        std::env::remove_var("TT_DOC_SIDECAR_URL");
        let h = DistillHarness::from_env();
        let mut req = ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(vec![ContentPart::Document {
                    document: tt_shared::messages::DocumentPart {
                        source: DocumentSource::Base64 {
                            media_type: "application/pdf".into(),
                            data: "JVBERi0=".into(),
                        },
                        filename: None,
                    },
                }]),
                name: None,
            }],
            ..default_request()
        };
        let booking = distill_request_parts(&h, "gpt-4o", &mut req).await;
        assert_eq!(booking, DistillBookkeeping::default());
        // The document part is left verbatim (fail-open).
        let Message::User {
            content: MessageContent::Parts(p),
            ..
        } = &req.messages[0]
        else {
            panic!("user message parts preserved");
        };
        assert!(matches!(p[0], ContentPart::Document { .. }));
    }

    #[tokio::test]
    async fn distill_request_parts_books_tokens_for_a_distilled_image() {
        // A mocked sidecar returns distilled text for the inline 1×1 PNG. The
        // seam substitutes the image part for text + accrues BOTH the raw image
        // tokens the image would have spent AND the distilled text tokens.
        use httpmock::prelude::*;
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var("TT_DOC_SIDECAR_URL", "http://unused-set-per-mock");
        let server = MockServer::start();
        // Point the harness's client at the mock by overriding the env URL.
        std::env::set_var("TT_DOC_SIDECAR_URL", server.base_url());
        let mock = server.mock(|when, then| {
            when.method(POST).path("/extract");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    serde_json::json!({
                        "text": "redacted-text",
                        "pages": 1,
                        "spans": [{ "kind": "lossless", "page": 0, "chars": 13 }]
                    })
                    .to_string(),
                );
        });
        // A real 1×1 PNG header (decodes to dims (1,1)).
        const PNG_1X1_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAWJcCq0AAAAASUVORK5CYII=";
        let h = DistillHarness::from_env();
        let mut req = ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                    image_url: tt_shared::messages::ImageUrl {
                        url: format!("data:image/png;base64,{PNG_1X1_B64}"),
                        detail: None,
                    },
                }]),
                name: None,
            }],
            ..default_request()
        };
        let booking = distill_request_parts(&h, "gpt-4o", &mut req).await;
        mock.assert();
        assert_eq!(
            booking.distilled_parts, 1,
            "the one image part was distilled"
        );
        assert!(
            booking.raw_image_tokens > 0,
            "the substituted image accrued its raw image tokens"
        );
        assert!(
            booking.distilled_text_tokens > 0,
            "the distilled text accrued its token count"
        );
        // The image part is now a Text part.
        let Message::User {
            content: MessageContent::Parts(p),
            ..
        } = &req.messages[0]
        else {
            panic!("user message parts preserved");
        };
        assert!(matches!(p[0], ContentPart::Text { .. }));
        std::env::remove_var("TT_DOC_SIDECAR_URL");
    }

    #[tokio::test]
    async fn distill_request_parts_does_not_book_a_failed_extraction() {
        // A sidecar 500 → fail-open: the part stays verbatim AND the bookkeeping
        // stays all-zero (never book a saving for a part we did not replace).
        use httpmock::prelude::*;
        let _guard = ENV_LOCK.lock().await;
        let server = MockServer::start();
        std::env::set_var("TT_DOC_SIDECAR_URL", server.base_url());
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/extract");
            then.status(500);
        });
        const PNG_1X1_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAWJcCq0AAAAASUVORK5CYII=";
        let h = DistillHarness::from_env();
        let mut req = ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                    image_url: tt_shared::messages::ImageUrl {
                        url: format!("data:image/png;base64,{PNG_1X1_B64}"),
                        detail: None,
                    },
                }]),
                name: None,
            }],
            ..default_request()
        };
        let booking = distill_request_parts(&h, "gpt-4o", &mut req).await;
        assert_eq!(booking, DistillBookkeeping::default());
        std::env::remove_var("TT_DOC_SIDECAR_URL");
    }

    /// The handler's D4c-v2 override prices the seam's bookkeeping via D0's
    /// `document_projection::project` at the served model's input rate. This pins
    /// the booking math + the fail-open postures the override relies on (NOT a
    /// re-test of `project`, which has its own suite): a real distilled image
    /// books a positive isolated saving; a Gemini-targeted downgrade books $0
    /// (the direction guard); a no-distill / no-pricing path leaves the field at 0.
    #[test]
    fn booking_to_projection_books_isolated_saving_with_gemini_guard() {
        use tt_shared::pricing::ModelPricing;
        // A 1024×1024 gpt-4o image (~765 raw image tokens) distilled to ~50 text
        // tokens @ $5/Mtok input → a positive saving, NEVER folded into tt_saved.
        let booking = DistillBookkeeping {
            distilled_parts: 1,
            raw_image_tokens: 765,
            distilled_text_tokens: 50,
        };
        let pricing = ModelPricing {
            input_per_million: 5.0,
            output_per_million: 15.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let proj = tt_preview::document_projection::project(
            booking.raw_image_tokens,
            booking.distilled_text_tokens,
            pricing.input_per_million,
            "gpt-4o",
        );
        assert!(
            proj.projected_savings_usd > 0.0,
            "an image-heavy gpt-4o distillation books a positive isolated saving"
        );
        // Gemini direction guard: page-images are priced flat + cheaper than
        // distilled text → the override books $0 for a Gemini-targeted downgrade.
        let gemini_proj = tt_preview::document_projection::project(
            booking.raw_image_tokens,
            booking.distilled_text_tokens,
            1.0,
            "gemini-2.5-flash",
        );
        assert_eq!(gemini_proj.projected_savings_usd, 0.0);
        // Fail-open: nothing distilled → the handler's `distilled_parts > 0` gate
        // skips the override entirely (the field stays at compute_cost_full's 0.0).
        assert_eq!(DistillBookkeeping::default().distilled_parts, 0);
    }

    /// A minimal `ChatCompletionRequest` for the seam tests (the seam only walks
    /// `messages`; the other fields are the default).
    fn default_request() -> ChatCompletionRequest {
        serde_json::from_str(r#"{"model":"gpt-4o","messages":[]}"#)
            .expect("a minimal request deserializes")
    }
}
