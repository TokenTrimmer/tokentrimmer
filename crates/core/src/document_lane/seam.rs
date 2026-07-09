//! D4c — the pre-routing document-lane distillation seam.
//!
//! Invoked in `prepare()` AFTER routing (so the matched route's
//! `RouteAction::document_lane` opt-in is known) but BEFORE `SplitRequest::compute`
//! (so the cache-stable prefix + L1/L2 keys are derived from the DISTILLED
//! request). When the route opted in + the request carries document/image
//! content parts, the sidecar distills each to text; the parts are swapped for
//! `ContentPart::Text` so routing can downgrade to a text model + the request
//! is billed at text-model rates.
//!
//! # Fail-open (the production posture)
//! Mirrors the sidecar client's fail-open: sidecar disabled (`TT_DOC_SIDECAR_URL`
//! unset) / connection error / timeout / malformed response → the request stays
//! VERBATIM (no distillation, no downgrade, no saving booked) + routes to the
//! vision model as before. The seam is a pure optimization — it must never be
//! able to break a request. Error blobs are never distilled (`should_distill`
//! stays false). The default-CLOSED [`DocDistillGate`] means lossy substitution
//! never happens unless an operator opts in.
//!
//! # The v1 scope (honest)
//! v1 distills INLINE base64 document/image bytes (URL parts are NOT fetched —
//! a future slice that resolves URLs to bytes, with the SSRF posture the `http`
//! workflow node already enforces). Lossless extractions (PDF text layers)
//! substitute without the judge; LOSSY extractions (OCR images) require the
//! [`DocDistillGate`] + the 0.90 floor. The isolated `doc_vision_saved_est_usd`
//! saving is booked via D0's `document_projection` (the raw image tokens the
//! request WOULD have sent vs the distilled text tokens, priced at input rate).
//! **Gemini direction guard:** the saving is $0 for Gemini-targeted downgrades
//! (Gemini's multimodal pricing model is non-comparable; never claim a saving
//! the provider's invoice would contradict).

use tt_shared::messages::{
    ChatCompletionRequest, ContentPart, DocumentSource, Message, MessageContent,
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
            1.0, /* TODO: judge recall, D4c v2 */
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
/// Returns the count of parts distilled (0 when none / the seam early-returned).
///
/// v1 distills INLINE base64 document bytes + data-URL image bytes ONLY (remote
/// URLs are not fetched — a future slice). After substitution, the caller
/// recomputes `request_has_images`/`request_has_documents` (now false → the
/// route can downgrade to a text model).
pub(crate) async fn distill_request_parts(
    harness: &DistillHarness,
    req: &mut ChatCompletionRequest,
) -> usize {
    // Early-return: when the sidecar is disabled (the common case), skip the
    // message walk entirely — zero added latency for text traffic.
    if harness.sidecar_url.is_none() {
        return 0;
    }
    let mut distilled = 0usize;
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
            let (media_type, data_b64) = match part {
                ContentPart::Document { document } => match &document.source {
                    DocumentSource::Base64 { media_type, data } => {
                        (media_type.clone(), data.clone())
                    }
                    // v1: URLs are not fetched — leave verbatim.
                    DocumentSource::Url { .. } => continue,
                },
                // v1: data-URL images (base64-inline). Remote image URLs are
                // left verbatim (a future fetch slice).
                ContentPart::ImageUrl { image_url } => {
                    if let Some((media, b64)) = parse_data_url(&image_url.url) {
                        (media, b64)
                    } else {
                        continue;
                    }
                }
                _ => continue, // Text, audio — not distilled.
            };
            if let DistillOutcome::Distilled { text, .. } =
                distill_part(harness, &media_type, &data_b64).await
            {
                *part = ContentPart::Text { text };
                distilled += 1;
            }
            // NotADocument / Disabled / ExtractFailed → leave the part verbatim.
        }
    }
    distilled
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn harness_from_env_is_disabled_without_sidecar_url() {
        // No env var set → disabled (the common no-sidecar case) + the gate is
        // default-CLOSED (lossy substitution never fires unless opted in).
        std::env::remove_var("TT_DOC_SIDECAR_URL");
        let h = DistillHarness::from_env();
        assert!(h.sidecar_url.is_none());
        assert!(!h.gate.lossy_opt_in());
    }

    #[tokio::test]
    async fn distill_part_returns_disabled_without_sidecar_url() {
        std::env::remove_var("TT_DOC_SIDECAR_URL");
        let h = DistillHarness::from_env();
        // Disabled path early-returns BEFORE any network — safe to await here.
        assert_eq!(
            distill_part(&h, "application/pdf", "JVBERi0=").await,
            DistillOutcome::Disabled
        );
    }
}
