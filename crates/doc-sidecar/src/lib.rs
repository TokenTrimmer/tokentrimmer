//! `doc-sidecar` — the Document Lane's out-of-process OCR/parse service.
//!
//! A tiny axum HTTP service exposing a single endpoint, `POST /extract`, that
//! turns an inline document (PDF / image) into text plus per-page
//! [`Span`]s tagged with their fidelity:
//!
//! - `application/pdf` → a **pure-Rust** text-layer pull ([`pdf_extract`]). The
//!   text came from the document's own text layer, so the spans are
//!   **`lossless`** (structurally safe to substitute for the image).
//! - `image/*` → OCR via [`ocrs`] (pure-Rust, opt-in `ocr` feature). OCR is a
//!   lossy transform, so the spans are **`lossy`** and must clear the Document
//!   Lane gate before a caller may substitute them. Without the `ocr` feature —
//!   or without model files — the handler returns a documented `ocr_unavailable`
//!   response (200, empty text) rather than failing.
//!
//! Everything is out-of-process on purpose: the gateway's fail-open
//! [`sidecar_client`](../../core/src/document_lane/sidecar_client.rs) treats any
//! error/timeout as "no extraction" and keeps the request verbatim. No native
//! libraries are linked in the default build; the OCR path is feature-gated so
//! constrained CI can build the sidecar without the ML graph.

use axum::{
    extract::{DefaultBodyLimit, Json},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::Semaphore;
use tt_shared::{
    validate_document_bytes, MAX_DOCUMENT_EXTRACTED_TEXT_BYTES, MAX_DOCUMENT_EXTRACTED_TEXT_CHARS,
    MAX_DOCUMENT_PAGES, MAX_INLINE_DOCUMENT_BYTES, SUPPORTED_DOCUMENT_MEDIA_TYPES,
};

mod ocr;

const MAX_SIDECAR_BASE64_CHARS: usize = MAX_INLINE_DOCUMENT_BYTES.div_ceil(3) * 4;
const MAX_EXTRACT_REQUEST_BODY_BYTES: usize = MAX_SIDECAR_BASE64_CHARS + 1024;
const MAX_CONCURRENT_EXTRACTIONS: usize = 2;
const EXTRACTION_QUEUE_TIMEOUT: Duration = Duration::from_millis(100);
const EXTRACTION_TIMEOUT: Duration = Duration::from_secs(4);
static EXTRACTION_PERMITS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_EXTRACTIONS);

/// Request body for `POST /extract`: a document's media type + its bytes as
/// standard base64 (no `data:` prefix).
#[derive(Debug, Clone, Deserialize)]
pub struct ExtractRequest {
    /// The document's MIME type, e.g. `application/pdf` or `image/png`.
    pub media_type: String,
    /// The document bytes, standard-base64 encoded.
    pub data_base64: String,
}

/// A distilled span of a document — one per page for PDFs, one per image for
/// OCR. `kind` is the fidelity vocabulary the Document Lane gate branches on:
/// `"lossless"` (a text layer) or `"lossy"` (OCR / a scanned page).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    /// `"lossless"` (PDF text layer) or `"lossy"` (OCR).
    pub kind: String,
    /// Zero-based page index this span was extracted from.
    pub page: u32,
    /// Number of Unicode scalar values in this span's text.
    pub chars: usize,
}

impl Span {
    /// The `"lossless"` kind string — a PDF text-layer span.
    pub const LOSSLESS: &'static str = "lossless";
    /// The `"lossy"` kind string — an OCR span.
    pub const LOSSY: &'static str = "lossy";
}

/// Response body for `POST /extract`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractResponse {
    /// The extracted text (all pages joined).
    pub text: String,
    /// Per-page / per-image spans with their fidelity `kind`.
    pub spans: Vec<Span>,
    /// Number of pages the extractor saw (1 for a single image).
    pub pages: u32,
    /// Which engine produced this result — doubles as a machine-readable status
    /// tag: `"pdf-extract"`, `"ocrs"`, `"ocr_unavailable"`, or `"unsupported"`.
    pub engine: String,
    /// An optional human-readable note (why a result is empty, a parse error,
    /// etc.). Omitted when there's nothing to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ExtractResponse {
    /// An empty result carrying an `engine` status tag + an explanatory note.
    fn empty(engine: &str, note: impl Into<String>, pages: u32) -> Self {
        Self {
            text: String::new(),
            spans: Vec::new(),
            pages,
            engine: engine.to_string(),
            note: Some(note.into()),
        }
    }
}

/// Build the sidecar's axum router. Kept public so tests can drive it via
/// [`tower::ServiceExt::oneshot`] without binding a socket.
pub fn app() -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/extract", post(extract_handler))
        .layer(DefaultBodyLimit::max(MAX_EXTRACT_REQUEST_BODY_BYTES))
}

/// `POST /extract` handler. Malformed base64 → `400`; everything else → `200`
/// with a (possibly empty) [`ExtractResponse`] (fail-soft, so the fail-open
/// client never has to distinguish "couldn't extract" from "no text").
async fn extract_handler(Json(req): Json<ExtractRequest>) -> Response {
    let bytes = match decode_request_data(&req.data_base64, MAX_INLINE_DOCUMENT_BYTES) {
        Ok(bytes) => bytes,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": message,
                })),
            )
                .into_response();
        }
    };

    let media_type = req.media_type.trim().to_ascii_lowercase();
    if SUPPORTED_DOCUMENT_MEDIA_TYPES.contains(&media_type.as_str()) {
        if validate_document_bytes(&media_type, &bytes).is_err() {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "document bytes failed bounded media validation",
                })),
            )
                .into_response();
        }
    } else {
        return (StatusCode::OK, Json(extract(&media_type, &bytes))).into_response();
    }

    let permit =
        match tokio::time::timeout(EXTRACTION_QUEUE_TIMEOUT, EXTRACTION_PERMITS.acquire()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) | Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ExtractResponse::empty(
                        "isolation_unavailable",
                        "document extraction capacity is unavailable",
                        0,
                    )),
                )
                    .into_response();
            }
        };
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            extract(&media_type, &bytes)
        }))
    });
    let response = match tokio::time::timeout(EXTRACTION_TIMEOUT, task).await {
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(_))) => {
            ExtractResponse::empty("isolation_error", "document extraction panicked", 0)
        }
        Ok(Err(_)) => {
            ExtractResponse::empty("isolation_error", "document extraction task failed", 0)
        }
        Err(_) => ExtractResponse::empty(
            "isolation_timeout",
            "document extraction exceeded the bounded runtime",
            0,
        ),
    };
    (StatusCode::OK, Json(response)).into_response()
}

fn decode_request_data(data: &str, max_decoded_bytes: usize) -> Result<Vec<u8>, &'static str> {
    let data = data.trim();
    let max_base64_chars = max_decoded_bytes.div_ceil(3) * 4;
    if data.len() > max_base64_chars {
        return Err("decoded document bytes exceed the sidecar limit");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "invalid standard base64 in `data_base64`")?;
    if bytes.len() > max_decoded_bytes {
        return Err("decoded document bytes exceed the sidecar limit");
    }
    Ok(bytes)
}

/// Dispatch extraction on the media type. PDFs → lossless text-layer pull;
/// images → (feature-gated) OCR; anything else → an empty `unsupported` result.
#[must_use]
pub fn extract(media_type: &str, bytes: &[u8]) -> ExtractResponse {
    let media_type = media_type.trim();
    let response = if media_type.eq_ignore_ascii_case("application/pdf") {
        extract_pdf(bytes)
    } else if media_type.to_ascii_lowercase().starts_with("image/") {
        ocr::extract_image(bytes)
    } else {
        ExtractResponse::empty(
            "unsupported",
            format!("unsupported media_type `{media_type}` (expected application/pdf or image/*)"),
            0,
        )
    };
    bound_extraction_response(response)
}

fn bound_extraction_response(response: ExtractResponse) -> ExtractResponse {
    let too_large = response.pages > MAX_DOCUMENT_PAGES
        || response.spans.len() > usize::try_from(MAX_DOCUMENT_PAGES).unwrap_or(usize::MAX)
        || response.text.len() > MAX_DOCUMENT_EXTRACTED_TEXT_BYTES
        || response.text.chars().count() > MAX_DOCUMENT_EXTRACTED_TEXT_CHARS;
    if too_large && (!response.text.is_empty() || !response.spans.is_empty()) {
        ExtractResponse::empty(
            "output_limit",
            "document extraction exceeded the bounded output",
            response.pages,
        )
    } else {
        response
    }
}

/// Pull the text layer from a PDF with the pure-Rust [`pdf_extract`] parser.
/// Produces one **lossless** span per page. A parse error or an internal panic
/// (some malformed PDFs make the parser panic) degrades to an empty result with
/// a note rather than a 500 — the client stays fail-open either way.
fn extract_pdf(bytes: &[u8]) -> ExtractResponse {
    let page_count = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lopdf::Document::load_mem(bytes).map(|document| document.get_pages().len())
    }));
    let page_count = match page_count {
        Ok(Ok(page_count)) => page_count,
        Ok(Err(_)) => {
            return ExtractResponse::empty("pdf-extract", "pdf page tree parse failed", 0);
        }
        Err(_) => {
            return ExtractResponse::empty("pdf-extract", "pdf page tree parse panicked", 0);
        }
    };
    let pages = u32::try_from(page_count).unwrap_or(u32::MAX);
    if pages > MAX_DOCUMENT_PAGES {
        return ExtractResponse::empty(
            "pdf_limit",
            "pdf exceeds the 100-page extraction limit",
            pages,
        );
    }

    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem_by_pages(bytes)
    }));

    match parsed {
        Ok(Ok(pages_text)) => {
            if pages_text.len() != page_count {
                return ExtractResponse::empty(
                    "pdf-extract",
                    "pdf extractor page evidence is inconsistent",
                    pages,
                );
            }
            let text_bytes = pages_text.iter().try_fold(0usize, |total, text| {
                total
                    .checked_add(text.len())
                    .and_then(|value| value.checked_add(2))
            });
            let text_chars = pages_text.iter().try_fold(0usize, |total, text| {
                total
                    .checked_add(text.chars().count())
                    .and_then(|value| value.checked_add(2))
            });
            if text_bytes.is_none_or(|count| count > MAX_DOCUMENT_EXTRACTED_TEXT_BYTES + 2)
                || text_chars.is_none_or(|count| count > MAX_DOCUMENT_EXTRACTED_TEXT_CHARS + 2)
            {
                return ExtractResponse::empty(
                    "output_limit",
                    "pdf extraction exceeded the bounded text output",
                    pages,
                );
            }
            let spans = pages_text
                .iter()
                .enumerate()
                .map(|(idx, page_text)| Span {
                    kind: Span::LOSSLESS.to_string(),
                    page: u32::try_from(idx).unwrap_or(u32::MAX),
                    chars: page_text.chars().count(),
                })
                .collect();
            ExtractResponse {
                text: pages_text.join("\n\n"),
                spans,
                pages,
                engine: "pdf-extract".to_string(),
                note: None,
            }
        }
        Ok(Err(_)) => ExtractResponse::empty("pdf-extract", "pdf text extraction failed", 0),
        Err(_) => ExtractResponse::empty("pdf-extract", "pdf extraction panicked", 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    /// A 1x1 transparent PNG (same fixture the preview crate uses).
    const PNG_1X1_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAWJcCq0AAAAASUVORK5CYII=";

    /// Build a valid single-page PDF with a text layer that reads
    /// "Hello TokenTrimmer" — using lopdf so the xref/offsets are correct.
    fn text_layer_pdf_with_pages(page_count: u32) -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{dictionary, Document, Object, Stream};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello TokenTrimmer")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_ids = (0..page_count)
            .map(|_| {
                doc.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "Contents" => content_id,
                    "Resources" => resources_id,
                    "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                })
            })
            .collect::<Vec<_>>();
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
            "Count" => page_count,
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    fn text_layer_pdf() -> Vec<u8> {
        text_layer_pdf_with_pages(1)
    }

    async fn post_extract(media_type: &str, data_base64: &str) -> (StatusCode, ExtractResponse) {
        let body = serde_json::json!({
            "media_type": media_type,
            "data_base64": data_base64,
        })
        .to_string();
        let resp = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/extract")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        // 400 responses aren't ExtractResponse-shaped; callers check status first.
        let parsed = serde_json::from_slice::<ExtractResponse>(&bytes).unwrap_or(ExtractResponse {
            text: String::new(),
            spans: Vec::new(),
            pages: 0,
            engine: "<non-extract-response>".to_string(),
            note: None,
        });
        (status, parsed)
    }

    #[tokio::test]
    async fn pdf_text_layer_yields_lossless_spans() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text_layer_pdf());
        let (status, resp) = post_extract("application/pdf", &b64).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp.engine, "pdf-extract");
        assert_eq!(resp.pages, 1, "one-page fixture");
        assert!(!resp.text.trim().is_empty(), "text layer must be non-empty");
        assert!(!resp.spans.is_empty(), "must emit at least one span");
        assert!(
            resp.spans.iter().all(|s| s.kind == Span::LOSSLESS),
            "text-layer spans are lossless, got {:?}",
            resp.spans
        );
        // The parser may or may not preserve inter-word spacing; compare with all
        // whitespace stripped so the assertion is robust.
        let squished: String = resp.text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squished.contains("TokenTrimmer"),
            "expected the embedded text, got {:?}",
            resp.text
        );
    }

    #[tokio::test]
    async fn image_without_ocr_feature_returns_documented_empty_result() {
        let (status, resp) = post_extract("image/png", PNG_1X1_B64).await;

        assert_eq!(status, StatusCode::OK, "image path is fail-soft, never 5xx");
        // Default build (no `ocr` feature): a documented empty result.
        #[cfg(not(feature = "ocr"))]
        {
            assert_eq!(resp.engine, "ocr_unavailable");
            assert!(resp.text.is_empty());
            assert!(resp.spans.is_empty());
            assert!(resp.note.is_some(), "empty OCR result must explain why");
        }
        // With the `ocr` feature but no model files, the same documented empty
        // result; with models present it would be lossy OCR text.
        #[cfg(feature = "ocr")]
        {
            assert!(resp.engine == "ocr_unavailable" || resp.engine == "ocrs");
            assert!(resp.spans.iter().all(|s| s.kind == Span::LOSSY));
        }
    }

    #[tokio::test]
    async fn malformed_base64_is_400() {
        let (status, _resp) = post_extract("application/pdf", "@@@ this is not base64 @@@").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn decoded_request_limit_preflights_before_allocation() {
        assert_eq!(
            decode_request_data("QUJD", 3).expect("three decoded bytes"),
            b"ABC"
        );
        assert_eq!(
            decode_request_data("QUJD", 2),
            Err("decoded document bytes exceed the sidecar limit")
        );
    }

    #[tokio::test]
    async fn supported_media_with_invalid_container_is_422() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"not a pdf");
        let (status, _resp) = post_extract("application/pdf", &b64).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn pdf_page_count_is_bounded_inside_the_isolated_parser() {
        let response = extract(
            "application/pdf",
            &text_layer_pdf_with_pages(MAX_DOCUMENT_PAGES + 1),
        );
        assert_eq!(response.engine, "pdf_limit");
        assert_eq!(response.pages, MAX_DOCUMENT_PAGES + 1);
        assert!(response.text.is_empty());
        assert!(response.spans.is_empty());
    }

    #[test]
    fn extracted_output_is_bounded_for_every_engine() {
        let response = bound_extraction_response(ExtractResponse {
            text: "x".repeat(MAX_DOCUMENT_EXTRACTED_TEXT_BYTES + 1),
            spans: vec![Span {
                kind: Span::LOSSY.into(),
                page: 0,
                chars: 1,
            }],
            pages: 1,
            engine: "test".into(),
            note: None,
        });
        assert_eq!(response.engine, "output_limit");
        assert!(response.text.is_empty());
        assert!(response.spans.is_empty());
    }

    #[tokio::test]
    async fn unsupported_media_type_is_empty_200() {
        // Valid base64, but a media type we don't handle.
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"plain text");
        let (status, resp) = post_extract("text/plain", &b64).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp.engine, "unsupported");
        assert!(resp.text.is_empty());
        assert!(resp.spans.is_empty());
    }
}
