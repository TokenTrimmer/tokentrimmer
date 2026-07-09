//! `tt docprep` — client-side document distillation (BACKLOG item D3).
//!
//! The client-side mirror of the gateway's Document Lane seam (D4c): run the
//! same extraction the gateway's sidecar would, but locally — so a request can
//! arrive with text parts already (no sidecar round-trip), or an SDK can front-
//! load the distillation before attaching the doc. Mirrors the in-process
//! `doc_sidecar::extract` the gateway calls (`crates/core/src/document_lane/
//! sidecar_client.rs` `extract()`), reusing the identical extraction code so the
//! client + the gateway agree byte-for-byte on what a doc distills to.
//!
//! # Scope (v1)
//! PDF text layers (lossless, pure-Rust `pdf_extract` — always available) +
//! images (OCR, feature-gated on the `doc-sidecar` crate; off by default →
//! images return an `unsupported`/note result rather than OCR text). A future
//! slice fetches remote URLs (the SSRF-safe fetch the gateway seam also defers).

use std::path::Path;

use anyhow::{Context, Result};

/// `tt docprep <file>` — extract text from a local document/image, print it to
/// stdout (the distilled text the gateway would substitute). `--json` prints the
/// full `ExtractResponse` (spans, pages, engine) for inspection. Exits non-zero
/// on a read error or an unsupported media type (the `unsupported` result).
pub fn run(path: &Path, json: bool) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let media_type = media_type_for(path)?;
    let response = doc_sidecar::extract(&media_type, &bytes);

    if json {
        // The full ExtractResponse — the spans carry the fidelity (LOSSLESS /
        // LOSSY) the gateway gate keys on, so a customer can see what the seam
        // would do before opting a route in.
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }

    // Text-only: the distilled text (all spans joined). Empty + a non-empty
    // note → an unsupported/error result the customer must see (exit non-zero).
    if response.text.is_empty() {
        anyhow::bail!(
            "no text extracted (engine: {}, note: {})",
            response.engine,
            response.note.unwrap_or_else(|| "(none)".to_string()),
        );
    }
    print!("{}", response.text);
    eprintln!(
        "\n---\nengine: {} · pages: {} · spans: {}",
        response.engine,
        response.pages,
        response.spans.len(),
    );
    Ok(())
}

/// Infer the media type from the file extension (the `doc-sidecar` `extract`
/// dispatches on it: `application/pdf` → text layer; `image/*` → OCR). A future
/// slice could read the magic bytes; v1 is extension-based (the common case).
fn media_type_for(path: &Path) -> Result<String> {
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
        _ => anyhow::bail!(
            "unrecognized file extension `{ext}` (expected pdf/png/jpg/gif/webp/bmp/tiff)"
        ),
    };
    Ok(media.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn media_type_for_pdf() {
        assert_eq!(
            media_type_for(&PathBuf::from("doc.pdf")).unwrap(),
            "application/pdf"
        );
    }

    #[test]
    fn media_type_for_image_extensions() {
        assert_eq!(
            media_type_for(&PathBuf::from("a.png")).unwrap(),
            "image/png"
        );
        assert_eq!(
            media_type_for(&PathBuf::from("a.JPG")).unwrap(),
            "image/jpeg"
        );
        assert_eq!(
            media_type_for(&PathBuf::from("a.webp")).unwrap(),
            "image/webp"
        );
    }

    #[test]
    fn media_type_for_unknown_extension_errors() {
        assert!(media_type_for(&PathBuf::from("doc.xyz")).is_err());
        assert!(media_type_for(&PathBuf::from("noext")).is_err());
    }

    #[test]
    fn run_errors_on_missing_file() {
        // A nonexistent file → a read error (exit non-zero), not a panic.
        assert!(run(&PathBuf::from("/nonexistent/doc.pdf"), false).is_err());
    }
}
