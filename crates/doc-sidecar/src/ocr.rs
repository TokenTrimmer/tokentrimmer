//! The image-OCR branch of `POST /extract`.
//!
//! OCR is opt-in behind the `ocr` feature (pure-Rust [`ocrs`] + the [`rten`]
//! inference runtime — no native lib, but a heavy compile + a runtime dependency
//! on model files). When the feature is off, or the models aren't configured,
//! [`extract_image`] returns a documented `ocr_unavailable` result (200, empty
//! text) so the default build stays lean and the fail-open client sees "no
//! extraction" rather than an error.

use crate::ExtractResponse;

/// Environment variables pointing at the ocrs `.rten` model files (only read
/// when the `ocr` feature is compiled in).
#[cfg(feature = "ocr")]
const DETECTION_MODEL_ENV: &str = "TT_OCR_DETECTION_MODEL";
#[cfg(feature = "ocr")]
const RECOGNITION_MODEL_ENV: &str = "TT_OCR_RECOGNITION_MODEL";

/// OCR an image to text. See the module docs for the feature/model gating.
#[cfg(not(feature = "ocr"))]
pub fn extract_image(_bytes: &[u8]) -> ExtractResponse {
    ExtractResponse::empty(
        "ocr_unavailable",
        "image OCR is not compiled in; rebuild the sidecar with `--features ocr` \
         and set TT_OCR_DETECTION_MODEL / TT_OCR_RECOGNITION_MODEL",
        1,
    )
}

/// OCR an image to text with ocrs (produces **lossy** spans). Requires the two
/// model-path env vars; any missing model, decode error, or engine error
/// degrades to a documented empty result (never a 5xx).
#[cfg(feature = "ocr")]
pub fn extract_image(bytes: &[u8]) -> ExtractResponse {
    use crate::Span;
    use ocrs::ImageSource;

    let (Ok(detection_path), Ok(recognition_path)) = (
        std::env::var(DETECTION_MODEL_ENV),
        std::env::var(RECOGNITION_MODEL_ENV),
    ) else {
        return ExtractResponse::empty(
            "ocr_unavailable",
            format!("OCR model paths unset ({DETECTION_MODEL_ENV} / {RECOGNITION_MODEL_ENV})"),
            1,
        );
    };

    let engine = match build_engine(&detection_path, &recognition_path) {
        Ok(engine) => engine,
        Err(err) => {
            return ExtractResponse::empty(
                "ocr_unavailable",
                format!("failed to load OCR models: {err}"),
                1,
            );
        }
    };

    let image = match image::load_from_memory(bytes) {
        Ok(image) => image.into_rgb8(),
        Err(err) => {
            return ExtractResponse::empty("ocrs", format!("image decode error: {err}"), 1);
        }
    };
    let (width, height) = image.dimensions();

    let text = (|| -> anyhow::Result<String> {
        let source = ImageSource::from_bytes(image.as_raw(), (width, height))?;
        let input = engine.prepare_input(source)?;
        engine.get_text(&input)
    })();

    match text {
        Ok(text) => {
            let chars = text.chars().count();
            let spans = if chars == 0 {
                Vec::new()
            } else {
                vec![Span {
                    kind: Span::LOSSY.to_string(),
                    page: 0,
                    chars,
                }]
            };
            ExtractResponse {
                text,
                spans,
                pages: 1,
                engine: "ocrs".to_string(),
                note: None,
            }
        }
        Err(err) => ExtractResponse::empty("ocrs", format!("OCR error: {err}"), 1),
    }
}

#[cfg(feature = "ocr")]
fn build_engine(detection_path: &str, recognition_path: &str) -> anyhow::Result<ocrs::OcrEngine> {
    use ocrs::{OcrEngine, OcrEngineParams};
    let detection_model = rten::Model::load_file(detection_path)?;
    let recognition_model = rten::Model::load_file(recognition_path)?;
    OcrEngine::new(OcrEngineParams {
        detection_model: Some(detection_model),
        recognition_model: Some(recognition_model),
        ..Default::default()
    })
}
