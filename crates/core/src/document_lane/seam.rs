//! D4c — the post-route-match document-lane distillation seam.
//!
//! Invoked in `prepare()` after route/canary selection (so the matched route's
//! `RouteAction::document_lane` opt-in and candidate target are known), but
//! before target-provider rebind, pinning, panel admission, failover, and
//! `SplitRequest::compute` (so cache keys derive from the converted request).
//! When the route opted in + the request carries document/image content parts,
//! the sidecar distills each to text; the parts are swapped for `ContentPart::Text`
//! only after every conversion succeeds, so a route can safely retain its
//! text-model downgrade. The isolated `doc_vision_saved_est_usd`
//! saving (D4c-v2) is booked from the [`DistillBookkeeping`] the seam returns —
//! the raw image tokens the request WOULD have sent vs the distilled text tokens
//! it now sends, priced at the served model's input rate via D0's
//! `document_projection::project`.
//!
//! # Fail-open (the production posture)
//! Mirrors the sidecar client's fail-open: sidecar disabled (`TT_DOC_SIDECAR_URL`
//! unset) / connection error / timeout / malformed response → the request stays
//! VERBATIM (no distillation and no saving booked). The seam is a pure
//! optimization — it must never be able to break a request. Error blobs are
//! never distilled (`should_distill` stays false). The default-CLOSED
//! [`DocDistillGate`] means lossy substitution never happens unless an operator
//! opts in. A failure to distill *any* lane-targeted document/image part aborts
//! the whole transaction: all earlier candidate substitutions are discarded and
//! booking is zero, so the request can never contain a misleading mixture of
//! raw and distilled media.
//!
//! # The v1 scope (honest)
//! v1 distills INLINE base64 document/image bytes (URL parts are NOT fetched —
//! a future slice that resolves URLs to bytes, with the SSRF posture the `http`
//! workflow node already enforces). A remote or malformed URL therefore aborts
//! the transaction rather than allowing a partial conversion. Lossless
//! extractions (PDF text layers) substitute without the judge; LOSSY
//! extractions (OCR images) require the [`DocDistillGate`] + the 0.90 floor.
//! **Gemini direction guard:** the booked saving is $0 for Gemini-targeted
//! downgrades (Gemini prices page-images flat and cheaper than distilled text;
//! never claim a saving the provider's invoice would contradict — the guard
//! lives in `document_projection::project`).

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
/// Cheap to construct (an `Arc<dyn>` + a bool). With no sidecar URL, the seam
/// performs only a small content scan so callers can distinguish text-only
/// traffic from an incomplete media conversion; it never makes a network call.
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

/// The bookkeeping a completed [`distill_request_parts`] call accrues so the
/// handler can book the isolated `doc_vision_saved_est_usd` (D4c-v2). Only a
/// fully completed request transaction exposes nonzero bookkeeping; an
/// incomplete conversion contributes nothing (the saving is never over-booked).
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

/// The request-level outcome for a document-lane transaction. This makes a
/// zero [`DistillBookkeeping`] unambiguous to callers that need to decide
/// whether raw media is still present for routing/capability purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestDistillOutcome {
    /// The request has no lane-targeted [`ContentPart::Document`] or
    /// [`ContentPart::ImageUrl`] part.
    NoEligibleParts,
    /// Every lane-targeted part was converted and the candidate request was
    /// committed atomically.
    Complete { booking: DistillBookkeeping },
    /// At least one lane-targeted part could not be converted (disabled
    /// sidecar, remote/malformed input, sidecar response, or gate rejection).
    /// The original request is unchanged and booking is zero.
    Incomplete,
}

impl RequestDistillOutcome {
    /// Return booking only for a complete transaction. This is a compatibility
    /// bridge for callers that only book savings; routing should inspect the
    /// enum itself to distinguish `NoEligibleParts` from `Incomplete`.
    #[must_use]
    pub(crate) fn booking(&self) -> DistillBookkeeping {
        match self {
            Self::Complete { booking } => booking.clone(),
            Self::NoEligibleParts | Self::Incomplete => DistillBookkeeping::default(),
        }
    }
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

/// Compatibility wrapper for callers that only need savings booking. Use
/// [`distill_request_parts_with_outcome`] when behavior depends on whether a
/// request lacked media or failed to convert it.
pub(crate) async fn distill_request_parts(
    harness: &DistillHarness,
    model: &str,
    req: &mut ChatCompletionRequest,
) -> DistillBookkeeping {
    distill_request_parts_with_outcome(harness, model, req)
        .await
        .booking()
}

/// Walk the request's messages + atomically swap every lane-targeted inline
/// base64 Document / data-URL Image part for a `ContentPart::Text` carrying the
/// sidecar's distilled text. A remote Document/Image URL, malformed data URL,
/// sidecar failure, empty response, or gate rejection returns
/// [`RequestDistillOutcome::Incomplete`]: the original request remains intact
/// and booking is zero. Text and audio parts are ignored, not blockers.
///
/// `model` is the SERVED model (the post-routing `target_model` rewrite) — used
/// to (a) pick the per-provider image-token formula for the raw image tokens +
/// (b) BPE-encode the distilled text. v1 distills INLINE base64 document bytes +
/// data-URL image bytes ONLY. A [`RequestDistillOutcome::Complete`] commits a
/// candidate containing only text in the converted positions, so the caller can
/// safely recompute media capability after inspecting the outcome.
pub(crate) async fn distill_request_parts_with_outcome(
    harness: &DistillHarness,
    model: &str,
    req: &mut ChatCompletionRequest,
) -> RequestDistillOutcome {
    if !request_has_lane_targeted_parts(req) {
        return RequestDistillOutcome::NoEligibleParts;
    }

    // With a disabled sidecar, media is present but no request conversion is
    // possible. Do not mutate or contact the network.
    if harness.sidecar_url.is_none() {
        return RequestDistillOutcome::Incomplete;
    }

    // Work on a candidate and assign it only after every Document/Image part
    // succeeds. This is the transaction boundary that prevents a later failure
    // from leaving the request partly raw and partly distilled.
    let mut candidate = req.clone();
    let mut booking = DistillBookkeeping::default();
    for message in &mut candidate.messages {
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
                    // v1 does not fetch remote/file-id document sources. A raw
                    // lane target must block the whole transaction.
                    DocumentSource::Url { .. } => return RequestDistillOutcome::Incomplete,
                },
                ContentPart::ImageUrl { image_url } => {
                    // Remote, non-base64, or malformed data URLs are all
                    // unsupported in v1 and therefore block the transaction.
                    let Some((media, b64)) = parse_data_url(&image_url.url) else {
                        return RequestDistillOutcome::Incomplete;
                    };
                    (media, b64, true)
                }
                _ => continue, // Text, audio — not lane targets or blockers.
            };
            let DistillOutcome::Distilled { text, .. } =
                distill_part(harness, &media_type, &data_b64).await
            else {
                return RequestDistillOutcome::Incomplete;
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

    *req = candidate;
    RequestDistillOutcome::Complete { booking }
}

/// True when a request contains a content part that this lane must either
/// convert or preserve verbatim as part of an incomplete transaction. Text and
/// audio intentionally do not count.
pub(crate) fn request_has_lane_targeted_parts(req: &ChatCompletionRequest) -> bool {
    req.messages.iter().any(|message| match message {
        Message::User {
            content: MessageContent::Parts(parts),
            ..
        }
        | Message::System {
            content: MessageContent::Parts(parts),
        }
        | Message::Tool {
            content: MessageContent::Parts(parts),
            ..
        }
        | Message::Assistant {
            content: Some(MessageContent::Parts(parts)),
            ..
        } => parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::Document { .. } | ContentPart::ImageUrl { .. }
            )
        }),
        _ => false,
    })
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

    fn test_harness(sidecar_url: Option<String>) -> DistillHarness {
        DistillHarness {
            client: reqwest::Client::new(),
            sidecar_url,
            gate: DocDistillGate::default(),
        }
    }

    fn request_with_parts(parts: Vec<ContentPart>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            messages: vec![Message::User {
                content: MessageContent::Parts(parts),
                name: None,
            }],
            ..default_request()
        }
    }

    fn inline_pdf(data: &str) -> ContentPart {
        ContentPart::Document {
            document: tt_shared::messages::DocumentPart {
                source: DocumentSource::Base64 {
                    media_type: "application/pdf".into(),
                    data: data.into(),
                },
                filename: None,
            },
        }
    }

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
    async fn disabled_sidecar_with_media_is_incomplete_and_verbatim() {
        // No sidecar URL with media present is distinct from a text-only
        // request: routing must retain the raw capability requirements.
        let h = test_harness(None);
        let mut req = request_with_parts(vec![inline_pdf("JVBERi0=")]);
        let before = serde_json::to_value(&req).expect("request serializes");

        let outcome = distill_request_parts_with_outcome(&h, "gpt-4o", &mut req).await;

        assert_eq!(outcome, RequestDistillOutcome::Incomplete);
        assert_eq!(outcome.booking(), DistillBookkeeping::default());
        assert_eq!(
            serde_json::to_value(&req).expect("request serializes"),
            before,
            "disabled conversion must not mutate media"
        );
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
        let server = MockServer::start();
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
        let h = test_harness(Some(server.base_url()));
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
        let outcome = distill_request_parts_with_outcome(&h, "gpt-4o", &mut req).await;
        mock.assert();
        let RequestDistillOutcome::Complete { booking } = outcome else {
            panic!("a successful one-part conversion must complete atomically");
        };
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
    }

    #[tokio::test]
    async fn failed_extraction_is_incomplete_and_verbatim() {
        // A sidecar 500 → fail-open: the part stays verbatim AND the bookkeeping
        // stays all-zero (never book a saving for a part we did not replace).
        use httpmock::prelude::*;
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/extract");
            then.status(500);
        });
        const PNG_1X1_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAWJcCq0AAAAASUVORK5CYII=";
        let h = test_harness(Some(server.base_url()));
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
        let before = serde_json::to_value(&req).expect("request serializes");

        let outcome = distill_request_parts_with_outcome(&h, "gpt-4o", &mut req).await;

        mock.assert();
        assert_eq!(outcome, RequestDistillOutcome::Incomplete);
        assert_eq!(outcome.booking(), DistillBookkeeping::default());
        assert_eq!(
            serde_json::to_value(&req).expect("request serializes"),
            before
        );
    }

    #[tokio::test]
    async fn text_and_audio_are_no_eligible_parts() {
        let h = test_harness(None);
        let mut req = request_with_parts(vec![
            ContentPart::Text {
                text: "keep this text".into(),
            },
            ContentPart::InputAudio {
                input_audio: tt_shared::messages::InputAudio {
                    data: "AAAA".into(),
                    format: "wav".into(),
                },
            },
        ]);
        let before = serde_json::to_value(&req).expect("request serializes");

        assert!(!request_has_lane_targeted_parts(&req));
        let outcome = distill_request_parts_with_outcome(&h, "gpt-4o", &mut req).await;

        assert_eq!(outcome, RequestDistillOutcome::NoEligibleParts);
        assert_eq!(outcome.booking(), DistillBookkeeping::default());
        assert_eq!(
            serde_json::to_value(&req).expect("request serializes"),
            before
        );
    }

    #[tokio::test]
    async fn partial_multi_part_failure_rolls_back_all_conversions() {
        use httpmock::prelude::*;

        const FIRST_PDF_B64: &str = "RklSU1Q=";
        const SECOND_PDF_B64: &str = "U0VDT05E";
        let server = MockServer::start();
        let successful_first = server.mock(|when, then| {
            when.method(POST)
                .path("/extract")
                .body_includes(FIRST_PDF_B64);
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    serde_json::json!({
                        "text": "first document text",
                        "pages": 1,
                        "spans": [{ "kind": "lossless", "page": 0, "chars": 19 }]
                    })
                    .to_string(),
                );
        });
        let failed_second = server.mock(|when, then| {
            when.method(POST)
                .path("/extract")
                .body_includes(SECOND_PDF_B64);
            then.status(500);
        });
        let h = test_harness(Some(server.base_url()));
        let mut req =
            request_with_parts(vec![inline_pdf(FIRST_PDF_B64), inline_pdf(SECOND_PDF_B64)]);
        let before = serde_json::to_value(&req).expect("request serializes");

        let outcome = distill_request_parts_with_outcome(&h, "gpt-4o", &mut req).await;

        successful_first.assert_calls(1);
        failed_second.assert_calls(1);
        assert_eq!(outcome, RequestDistillOutcome::Incomplete);
        assert_eq!(outcome.booking(), DistillBookkeeping::default());
        assert_eq!(
            serde_json::to_value(&req).expect("request serializes"),
            before,
            "the successful first candidate conversion must be discarded"
        );
    }

    #[tokio::test]
    async fn empty_200_keeps_raw_part_and_returns_incomplete() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/extract");
            then.status(200)
                .header("content-type", "application/json")
                .body("{}");
        });
        let h = test_harness(Some(server.base_url()));
        let mut req = request_with_parts(vec![inline_pdf("RU1QVFk=")]);
        let before = serde_json::to_value(&req).expect("request serializes");

        let outcome = distill_request_parts_with_outcome(&h, "gpt-4o", &mut req).await;

        mock.assert();
        assert_eq!(outcome, RequestDistillOutcome::Incomplete);
        assert_eq!(outcome.booking(), DistillBookkeeping::default());
        assert_eq!(
            serde_json::to_value(&req).expect("request serializes"),
            before
        );
        let Message::User {
            content: MessageContent::Parts(parts),
            ..
        } = &req.messages[0]
        else {
            panic!("raw media message must remain present");
        };
        assert!(matches!(parts[0], ContentPart::Document { .. }));
    }

    #[tokio::test]
    async fn remote_or_malformed_url_blocks_the_transaction() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        let no_sidecar_request = server.mock(|when, then| {
            when.method(POST).path("/extract");
            then.status(500);
        });
        let h = test_harness(Some(server.base_url()));
        let unsupported_parts = vec![
            ContentPart::Document {
                document: tt_shared::messages::DocumentPart {
                    source: DocumentSource::Url {
                        url: "https://example.test/document.pdf".into(),
                    },
                    filename: None,
                },
            },
            ContentPart::ImageUrl {
                image_url: tt_shared::messages::ImageUrl {
                    url: "https://example.test/image.png".into(),
                    detail: None,
                },
            },
            ContentPart::ImageUrl {
                image_url: tt_shared::messages::ImageUrl {
                    url: "data:image/png,not-base64".into(),
                    detail: None,
                },
            },
        ];

        for unsupported_part in unsupported_parts {
            let mut req = request_with_parts(vec![
                ContentPart::Text {
                    text: "do not mutate surrounding text".into(),
                },
                unsupported_part,
            ]);
            let before = serde_json::to_value(&req).expect("request serializes");

            let outcome = distill_request_parts_with_outcome(&h, "gpt-4o", &mut req).await;

            assert_eq!(outcome, RequestDistillOutcome::Incomplete);
            assert_eq!(outcome.booking(), DistillBookkeeping::default());
            assert_eq!(
                serde_json::to_value(&req).expect("request serializes"),
                before
            );
        }
        no_sidecar_request.assert_calls(0);
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
