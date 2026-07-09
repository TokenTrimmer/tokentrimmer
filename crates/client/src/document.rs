//! D3 — client-side document distillation helpers (the `tt docprep` mirror).
//!
//! `user_with_document(path)` reads a local document, pulls its text layer
//! (PDFs, lossless — pure-Rust `pdf_extract`) + returns a `Message::User`
//! carrying the distilled text. The request arrives PRE-distilled → no gateway
//! sidecar round-trip + routes to a text model. Mirrors `tt docprep`
//! (`crates/cli/src/docprep.rs`) + the gateway's Document Lane seam
//! (`crates/core/src/document_lane/seam.rs`), reusing the same extraction so the
//! client + the gateway agree on what a doc distills to.
//!
//! # v1 scope (honest)
//! PDF text layers (lossless) only. Images (`image/*`) → an `unsupported`
//! result (OCR is out of scope for the SDK v1, matching the CLI's off-by-default
//! OCR feature gate — a future slice). Remote URLs are not fetched.
//!
//! # The `doc_distill` feature
//! Distillation is feature-gated (`doc_distill`, default OFF) to keep the
//! default client dep-lean. Without the feature, the distill helpers return
//! [`DocumentError::FeatureDisabled`]; the raw-attach helper
//! ([`user_with_document_raw`]) is available regardless (file read + base64).

use std::path::Path;

use tt_shared::messages::{DocumentPart, DocumentSource, Message, MessageContent};

use crate::user;

/// A client-side distillation result — mirrors `doc_sidecar::ExtractResponse`
/// (sans spans; the SDK caller doesn't need per-page fidelity). The `engine`
/// doubles as a status tag: `"pdf-extract"` (success); `DocumentError::Unsupported`
/// covers non-PDF types + `DocumentError::FeatureDisabled` the off-feature path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistilledDocument {
    /// The extracted text (all pages joined). Empty only on a failed extraction
    /// (which surfaces as a [`DocumentError::Empty`]).
    pub text: String,
    /// Number of pages the extractor saw.
    pub pages: u32,
    /// The engine / status tag (`"pdf-extract"` on success).
    pub engine: String,
    /// A human-readable note (why a result is empty). `None` on success.
    pub note: Option<String>,
}

/// Errors a [`distill_document`] / [`user_with_document`] call can return.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DocumentError {
    /// The `doc_distill` cargo feature is off → distillation is unavailable.
    /// Use [`user_with_document_raw`] to attach the bytes instead.
    #[error("the `doc_distill` cargo feature is off — enable it, or use `user_with_document_raw`")]
    FeatureDisabled,
    /// The file could not be read.
    #[error("read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The media type is unsupported (non-PDF, or an unrecognized extension).
    /// `engine` is `"unsupported"`; the caller may attach the raw bytes instead.
    #[error("unsupported media type ({media_type}) — {note}")]
    Unsupported { media_type: String, note: String },
    /// The extraction succeeded but produced no text (a scan / an empty layer).
    #[error("no text extracted (engine: {engine}, note: {note})")]
    Empty { engine: String, note: String },
}

/// Infer the media type from the file extension (the extraction dispatches on
/// it: `application/pdf` → text layer). A future slice could read magic bytes;
/// v1 is extension-based (the common case). Mirrors `tt docprep`'s
/// `media_type_for`.
fn media_type_for(path: &Path) -> Result<String, DocumentError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let media = match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        _ => {
            return Err(DocumentError::Unsupported {
                media_type: format!(".{ext}"),
                note: "unrecognized file extension (expected pdf/png/jpg/gif/webp/bmp/tiff)"
                    .to_string(),
            });
        }
    };
    Ok(media.to_string())
}

/// Distill a local document to text (the client-side mirror of the gateway
/// seam). Reads the file, pulls its text layer (PDFs only in v1; images return
/// [`DocumentError::Unsupported`]). Returns the [`DistilledDocument`] so the
/// caller can inspect the engine/note before building a message.
///
/// Requires the `doc_distill` cargo feature; without it returns
/// [`DocumentError::FeatureDisabled`].
pub fn distill_document(path: &Path) -> Result<DistilledDocument, DocumentError> {
    let bytes = std::fs::read(path).map_err(|source| DocumentError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let media_type = media_type_for(path)?;
    distill_document_bytes(&media_type, &bytes)
}

/// Distill in-memory document bytes by media type (the bytes-level entry point
/// [`distill_document`] delegates to). See [`distill_document`] for the scope +
/// the `doc_distill` feature gate.
pub fn distill_document_bytes(
    media_type: &str,
    bytes: &[u8],
) -> Result<DistilledDocument, DocumentError> {
    let media_type = media_type.trim();
    if !media_type.eq_ignore_ascii_case("application/pdf") {
        return Err(DocumentError::Unsupported {
            media_type: media_type.to_string(),
            note: "images are OCR, out of scope for the SDK v1".to_string(),
        });
    }
    #[cfg(not(feature = "doc_distill"))]
    {
        let _ = bytes;
        let _ = media_type;
        Err(DocumentError::FeatureDisabled)
    }
    #[cfg(feature = "doc_distill")]
    {
        let _ = media_type; // already dispatched on
        distill_pdf(bytes)
    }
}

#[cfg(feature = "doc_distill")]
fn distill_pdf(bytes: &[u8]) -> Result<DistilledDocument, DocumentError> {
    // `pdf_extract` can panic on malformed PDFs — catch it so the client never
    // crashes on a bad input (mirrors `doc-sidecar::extract_pdf`).
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem_by_pages(bytes)
    }));
    let distilled = match parsed {
        Ok(Ok(pages_text)) => DistilledDocument {
            text: pages_text.join("\n\n"),
            pages: u32::try_from(pages_text.len()).unwrap_or(u32::MAX),
            engine: "pdf-extract".to_string(),
            note: None,
        },
        Ok(Err(err)) => DistilledDocument {
            text: String::new(),
            pages: 0,
            engine: "pdf-extract".to_string(),
            note: Some(format!("pdf parse error: {err}")),
        },
        Err(_) => DistilledDocument {
            text: String::new(),
            pages: 0,
            engine: "pdf-extract".to_string(),
            note: Some("pdf extraction panicked".to_string()),
        },
    };
    if distilled.text.is_empty() {
        return Err(DocumentError::Empty {
            engine: distilled.engine,
            note: distilled.note.unwrap_or_else(|| "(none)".to_string()),
        });
    }
    Ok(distilled)
}

/// Build a `user` message carrying the document's distilled text (the
/// client-side distillation — no gateway sidecar round-trip). The request
/// arrives pre-distilled + routes to a text model.
///
/// Requires the `doc_distill` cargo feature; without it returns
/// [`DocumentError::FeatureDisabled`] (use [`user_with_document_raw`] to attach
/// the raw bytes for the gateway seam to distill).
///
/// ```
/// # use tt_client::user_with_document;
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let msg = user_with_document(std::path::Path::new("report.pdf"))?;
/// # Ok(()) }
/// ```
pub fn user_with_document(path: &Path) -> Result<Message, DocumentError> {
    let distilled = distill_document(path)?;
    Ok(user(distilled.text))
}

/// Build a `user` message that attaches the document as a `ContentPart::Document`
/// (raw bytes, base64-inlined) — for callers who want the gateway's Document
/// Lane seam to distill it server-side (route opted in via
/// `RouteAction::document_lane`). No `doc_distill` feature required.
pub fn user_with_document_raw(path: &Path) -> Result<Message, DocumentError> {
    let bytes = std::fs::read(path).map_err(|source| DocumentError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let media_type = media_type_for(path)?;
    // A non-PDF image is still attachable as a document part (the gateway seam
    // handles both); only an unrecognized extension is an error here.
    let data = STANDARD.encode(&bytes);
    let part = DocumentPart {
        source: DocumentSource::Base64 { media_type, data },
        filename: path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string),
    };
    let content = MessageContent::Parts(vec![tt_shared::messages::ContentPart::Document {
        document: part,
    }]);
    Ok(Message::User {
        content,
        name: None,
    })
}

// `user_with_document_raw` base64-encodes (a non-feature dep, always available).
use base64::Engine as _;
const STANDARD: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid single-page PDF with a text layer that reads
    /// "Hello TokenTrimmer" — using lopdf so the xref/offsets are correct.
    /// Mirrors `doc-sidecar`'s `text_layer_pdf` fixture.
    fn text_layer_pdf() -> Vec<u8> {
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
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
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

    #[test]
    fn media_type_for_known_extensions() {
        let mk = |s: &str| std::path::PathBuf::from(s);
        assert_eq!(media_type_for(&mk("a.pdf")).unwrap(), "application/pdf");
        assert_eq!(media_type_for(&mk("a.PNG")).unwrap(), "image/png");
        assert_eq!(media_type_for(&mk("a.jpg")).unwrap(), "image/jpeg");
    }

    #[test]
    fn media_type_for_unknown_extension_errors() {
        let mk = |s: &str| std::path::PathBuf::from(s);
        assert!(matches!(
            media_type_for(&mk("a.xyz")),
            Err(DocumentError::Unsupported { .. })
        ));
        assert!(media_type_for(&mk("noext")).is_err());
    }

    #[test]
    fn distill_missing_file_errors_read() {
        let err = distill_document(std::path::Path::new("/nonexistent/doc.pdf")).unwrap_err();
        assert!(matches!(err, DocumentError::Read { .. }));
    }

    #[cfg(feature = "doc_distill")]
    #[test]
    fn distill_text_layer_pdf_extracts_the_text() {
        let bytes = text_layer_pdf();
        let distilled =
            distill_document_bytes("application/pdf", &bytes).expect("a text-layer PDF distills");
        assert_eq!(distilled.engine, "pdf-extract");
        assert_eq!(distilled.pages, 1);
        assert!(
            distilled.text.contains("Hello TokenTrimmer"),
            "the distilled text should contain the PDF's text layer; got: {:?}",
            distilled.text
        );
        assert!(distilled.note.is_none());
    }

    #[cfg(feature = "doc_distill")]
    #[test]
    fn distill_image_bytes_returns_unsupported() {
        let png = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        ];
        let err = distill_document_bytes("image/png", &png).unwrap_err();
        assert!(matches!(err, DocumentError::Unsupported { .. }));
    }

    #[cfg(not(feature = "doc_distill"))]
    #[test]
    fn distill_without_feature_returns_disabled() {
        let bytes = text_layer_pdf();
        let err = distill_document_bytes("application/pdf", &bytes).unwrap_err();
        assert!(matches!(err, DocumentError::FeatureDisabled));
    }

    #[test]
    fn user_with_document_raw_attaches_a_document_part() {
        // Any readable file works for the raw-attach path (it doesn't parse the
        // content). A text file with a .pdf extension round-trips the media type.
        let tmp = std::env::temp_dir().join("tt-client-d3-raw-test.pdf");
        std::fs::write(&tmp, b"%PDF-1.5 fake").unwrap();
        let msg = user_with_document_raw(&tmp).unwrap();
        let Message::User {
            content: MessageContent::Parts(parts),
            ..
        } = msg
        else {
            panic!("expected a user message with parts");
        };
        assert_eq!(parts.len(), 1);
        assert!(matches!(
            parts[0],
            tt_shared::messages::ContentPart::Document { .. }
        ));
        std::fs::remove_file(&tmp).ok();
    }
}
