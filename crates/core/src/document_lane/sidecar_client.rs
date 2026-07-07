//! Fail-open client for the out-of-process document `doc-sidecar` service.
//!
//! The gateway calls [`extract`] to turn an image/document part into text before
//! routing (the D4c seam). Everything here is **fail-open**: if the sidecar is
//! not configured, unreachable, slow, errors, or returns anything unexpected,
//! [`extract`] yields `None` and the caller keeps the request verbatim. A
//! document distillation is a pure *optimization* — it must never be able to
//! drop, corrupt, or stall a request.
//!
//! - Disabled by default: the sidecar URL comes from `TT_DOC_SIDECAR_URL`
//!   ([`sidecar_url_from_env`]); unset/blank means "no sidecar" → `None`.
//! - Short timeout (5s) so a wedged sidecar can't add latency to the hot path.
//! - The wire span `kind` (`"lossless"`/`"lossy"`) maps onto D4a's
//!   [`SpanFidelity`] so the D4c gate can branch on it.

use std::time::Duration;

use serde::Deserialize;

use super::SpanFidelity;

/// Environment variable holding the sidecar base URL (e.g.
/// `http://127.0.0.1:8088`). Unset or blank disables the Document Lane sidecar.
pub const SIDECAR_URL_ENV: &str = "TT_DOC_SIDECAR_URL";

/// Hard ceiling on a single sidecar round-trip. Kept short: the seam is on the
/// pre-routing hot path, and a distillation is optional, so a slow sidecar must
/// fail open rather than delay the request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// One extracted span, with its fidelity resolved onto D4a's [`SpanFidelity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractSpan {
    /// Lossless (a PDF text layer) or lossy (OCR) — governs the gate.
    pub fidelity: SpanFidelity,
    /// Zero-based page index this span came from.
    pub page: u32,
    /// Number of Unicode scalar values in the span's text.
    pub chars: usize,
}

/// A successful extraction from the sidecar. Mirrors the sidecar's response
/// shape (minus the `engine`/`note` diagnostics the seam doesn't need).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extraction {
    /// The extracted text (all pages joined).
    pub text: String,
    /// The per-page / per-image spans.
    pub spans: Vec<ExtractSpan>,
    /// Number of pages the extractor saw.
    pub pages: u32,
}

impl Extraction {
    /// Whether every span is lossless (a text layer). Lossless extractions skip
    /// the D4c quality judge; a single lossy span forces the gate.
    #[must_use]
    pub fn is_lossless(&self) -> bool {
        self.spans
            .iter()
            .all(|s| s.fidelity == SpanFidelity::Lossless)
    }
}

/// Read the sidecar base URL from `TT_DOC_SIDECAR_URL`. Returns `None` when the
/// var is unset or blank (the Document Lane sidecar is disabled).
#[must_use]
pub fn sidecar_url_from_env() -> Option<String> {
    std::env::var(SIDECAR_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Extract text from a document via the sidecar. **Fail-open:** returns `None`
/// when `sidecar_url` is `None` (disabled) or on ANY error — connection
/// refused, non-2xx, timeout, malformed body. Never returns `Err`.
///
/// `media_type` is the document's MIME type (`application/pdf`, `image/png`, …)
/// and `data_base64` is its standard-base64 bytes.
pub async fn extract(
    client: &reqwest::Client,
    sidecar_url: Option<&str>,
    media_type: &str,
    data_base64: &str,
) -> Option<Extraction> {
    let base = sidecar_url?.trim();
    if base.is_empty() {
        return None;
    }
    request_extraction(client, base, media_type, data_base64, REQUEST_TIMEOUT).await
}

/// The actual round-trip, with the timeout as a parameter so tests can exercise
/// the timeout path without waiting the full production budget.
async fn request_extraction(
    client: &reqwest::Client,
    base: &str,
    media_type: &str,
    data_base64: &str,
    timeout: Duration,
) -> Option<Extraction> {
    let url = format!("{}/extract", base.trim_end_matches('/'));
    let body = serde_json::json!({
        "media_type": media_type,
        "data_base64": data_base64,
    });

    // `.ok()?` at every step is the fail-open contract: any transport error,
    // non-success status, or decode failure collapses to `None`.
    let response = client
        .post(&url)
        .timeout(timeout)
        .json(&body)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let wire = response.json::<WireResponse>().await.ok()?;
    Some(wire.into_extraction())
}

/// The sidecar's on-the-wire response. Deserialized leniently (unknown fields
/// ignored, missing numeric fields default) so a forward-compatible sidecar
/// never breaks the client.
#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    text: String,
    #[serde(default)]
    spans: Vec<WireSpan>,
    #[serde(default)]
    pages: u32,
}

#[derive(Debug, Deserialize)]
struct WireSpan {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    page: u32,
    #[serde(default)]
    chars: usize,
}

impl WireResponse {
    fn into_extraction(self) -> Extraction {
        Extraction {
            text: self.text,
            pages: self.pages,
            spans: self
                .spans
                .into_iter()
                .map(|s| ExtractSpan {
                    fidelity: fidelity_from_kind(&s.kind),
                    page: s.page,
                    chars: s.chars,
                })
                .collect(),
        }
    }
}

/// Map the wire `kind` string onto [`SpanFidelity`]. Only an explicit
/// `"lossless"` is trusted as a text layer; every other value (including an
/// unknown kind) defaults to **lossy** so an unrecognized span must clear the
/// gate rather than being silently trusted as safe-to-substitute.
fn fidelity_from_kind(kind: &str) -> SpanFidelity {
    match kind {
        "lossless" => SpanFidelity::Lossless,
        _ => SpanFidelity::Lossy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn disabled_when_url_is_none() {
        let got = extract(&client(), None, "application/pdf", "AAAA").await;
        assert!(got.is_none(), "no sidecar URL => disabled => None");
    }

    #[tokio::test]
    async fn blank_url_is_disabled() {
        let got = extract(&client(), Some("   "), "application/pdf", "AAAA").await;
        assert!(got.is_none(), "blank URL => disabled => None");
    }

    #[tokio::test]
    async fn ok_200_parses_into_some() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/extract");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    serde_json::json!({
                        "text": "Hello TokenTrimmer",
                        "pages": 2,
                        "engine": "pdf-extract",
                        "spans": [
                            { "kind": "lossless", "page": 0, "chars": 11 },
                            { "kind": "lossy", "page": 1, "chars": 7 }
                        ]
                    })
                    .to_string(),
                );
        });

        let got = extract(
            &client(),
            Some(&server.base_url()),
            "application/pdf",
            "AAAA",
        )
        .await;

        mock.assert();
        let extraction = got.expect("a 200 must parse into Some");
        assert_eq!(extraction.text, "Hello TokenTrimmer");
        assert_eq!(extraction.pages, 2);
        assert_eq!(extraction.spans.len(), 2);
        assert_eq!(extraction.spans[0].fidelity, SpanFidelity::Lossless);
        assert_eq!(extraction.spans[0].chars, 11);
        assert_eq!(extraction.spans[1].fidelity, SpanFidelity::Lossy);
        assert!(!extraction.is_lossless(), "one lossy span => not lossless");
    }

    #[tokio::test]
    async fn server_500_fails_open_to_none() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/extract");
            then.status(500);
        });

        let got = extract(
            &client(),
            Some(&server.base_url()),
            "application/pdf",
            "AAAA",
        )
        .await;
        assert!(got.is_none(), "a 5xx must fail open to None");
    }

    #[tokio::test]
    async fn connection_refused_fails_open_to_none() {
        // Port 1 on loopback has nothing listening -> connection refused.
        let got = extract(
            &client(),
            Some("http://127.0.0.1:1"),
            "application/pdf",
            "AAAA",
        )
        .await;
        assert!(got.is_none(), "a refused connection must fail open to None");
    }

    #[tokio::test]
    async fn timeout_fails_open_to_none() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/extract");
            // Delay well past the (tiny, test-only) timeout below.
            then.status(200)
                .delay(Duration::from_millis(400))
                .header("content-type", "application/json")
                .body(r#"{"text":"late","pages":1,"spans":[]}"#);
        });

        // Drive the inner request with a 50ms timeout so the delayed response
        // trips it deterministically without waiting the 5s production budget.
        let got = request_extraction(
            &client(),
            &server.base_url(),
            "application/pdf",
            "AAAA",
            Duration::from_millis(50),
        )
        .await;
        assert!(
            got.is_none(),
            "a response slower than the timeout must fail open to None"
        );
    }

    #[tokio::test]
    async fn malformed_body_fails_open_to_none() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/extract");
            then.status(200)
                .header("content-type", "application/json")
                .body("this is not json");
        });

        let got = extract(
            &client(),
            Some(&server.base_url()),
            "application/pdf",
            "AAAA",
        )
        .await;
        assert!(got.is_none(), "an undecodable body must fail open to None");
    }

    #[test]
    fn unknown_span_kind_defaults_to_lossy() {
        assert_eq!(fidelity_from_kind("lossless"), SpanFidelity::Lossless);
        assert_eq!(fidelity_from_kind("lossy"), SpanFidelity::Lossy);
        assert_eq!(fidelity_from_kind("mystery"), SpanFidelity::Lossy);
    }
}
