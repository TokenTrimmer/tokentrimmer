//! Rule: `raw-document-to-vision-model`
//!
//! Detects a **raw image / base64-image / base64-PDF fed straight to a
//! vision-capable model** inside an LLM `…create(...)` / `generate_content(...)`
//! call — an `image_url` / `input_image` part, or an inline base64 image/PDF
//! block. Sending the document as pixels re-bills the whole page as image input
//! tokens on *every* call; distilling/offloading it to compact text (OCR, a
//! caption, or extracted fields) cuts that to a fraction. This is the static
//! Tier-1 half of the Document Lane — it quotes the deterministic
//! [`tt_preview::document_projection`] projector (D0) so the finding carries a
//! directional, clearly-labelled projected saving.
//!
//! AST-backed, mirroring [`super::inline_data_offload_candidate`]: the
//! tree-sitter harness yields real `call` nodes, the rule keeps only LLM
//! create-calls, and the image-part signal is scoped to each call's own
//! argument span — so an `image_url` mentioned in a comment or an unrelated
//! object literal does not trip it.
//!
//! **Vision gate & provider direction.** The catalog `Capability::Vision`
//! signal (`tt_shared::pricing`) is not carried by the static pricing table —
//! model capabilities live in the runtime provider registry — so, like the
//! sibling cost rules ([`super::model_flagship_for_classification`]), the vision
//! decision is a self-contained model-family heuristic. Clearly text-only
//! families (embeddings, audio, `gpt-3.5*`) are treated as *not* vision and skip
//! the finding. The model id is resolved from the call's `model=` argument when
//! present, else inferred from the callee's SDK family (`generate_content` →
//! Gemini, `messages.create` → Anthropic, else OpenAI). The projection is then
//! run through D0's [`project`], whose **Gemini direction guard** books a $0
//! saving (page-images price flat and cheaper than distilled text there); the
//! rule **suppresses any finding whose projected saving is 0** — so Gemini
//! traffic (identified explicitly or inferred from the Google SDK callee) never
//! fires.

use tt_inspect_core::ast::{call_sites, is_llm_create_callee};
use tt_inspect_core::parse::parse_cached;
use tt_inspect_core::{Finding, Language, Rule, Severity};
use tt_preview::document_projection::project;
use tt_tokenize::image_tokens::{estimate_image_tokens, ImageDetail};

/// Fires when a raw image / base64-PDF part is passed into a vision-capable
/// model's LLM call, quoting D0's projected offload saving.
pub struct RawDocumentToVisionModelRule;

impl RawDocumentToVisionModelRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for RawDocumentToVisionModelRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Nominal square-image dimension used for the static projection. A real static
/// scan has no image dims, so we price a representative full-size page image
/// (`1024x1024`, high detail) — the precise figure comes from the runtime cost
/// preview, which reads the actual dims (D0 wired there).
const NOMINAL_IMAGE_DIM: u32 = 1024;

/// Nominal fraction of the raw image tokens a distilled/offloaded text version
/// is projected to occupy (~a page distilled to a compact extract/caption).
/// Directional only — labelled a projection in the finding.
const NOMINAL_DISTILLED_RATIO: f64 = 0.25;

/// Representative flagship input price ($/M tokens) for the rough, clearly
/// labelled projection. Matches the sibling [`super::inline_data_offload_candidate`]'s
/// self-contained constant — the Tier-1 rules do not depend on the live pricing
/// catalog, and the headline of the finding is the offload action, not a precise
/// dollar figure.
const REPRESENTATIVE_INPUT_USD_PER_MTOK: f64 = 3.00;

/// Volume anchor used to express the projected saving at scale (per 1,000
/// images), alongside the per-image figure. Clearly labelled a projection.
const VOLUME_ANCHOR_IMAGES: f64 = 1000.0;

/// Model-family signals that are NOT vision-capable — an image part addressed to
/// one of these is out of scope (mirrors the negative side of the catalog
/// `Capability::Vision` set for the common text-only / embedding / audio
/// families). Conservative: only clearly non-vision families are listed.
const NON_VISION_SIGNALS: &[&str] = &[
    "gpt-3.5",
    "text-embedding",
    "embedding",
    "text-davinci",
    "davinci",
    "babbage",
    "whisper",
    "tts-",
    "text-moderation",
    "moderation",
];

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

/// `true` when `model` clearly names a non-vision (text-only / audio /
/// embedding) family, so an image part on it is out of this rule's scope.
fn is_non_vision_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    NON_VISION_SIGNALS.iter().any(|s| lower.contains(s))
}

/// The nominal model id to project with when the call carries no explicit
/// `model=` argument, inferred from the SDK the callee belongs to. This makes
/// the Gemini direction guard fire for the native Google SDK (`generate_content`)
/// path, where the model id lives on the client object, not in the call.
fn nominal_model_for_callee(callee: &str) -> &'static str {
    if callee.ends_with("generate_content") || callee.ends_with("generateContent") {
        "gemini-2.5-flash"
    } else if callee.ends_with("messages.create") {
        "claude-3-5-sonnet"
    } else {
        "gpt-4o"
    }
}

/// Detect a raw image / base64-PDF part inside a call's argument span, returning
/// a human-readable kind label. Conservative: requires an explicit image/PDF
/// indicator (an OpenAI `image_url`/`input_image` key, an inline `data:` URL, or
/// a base64 image/PDF block with its mime/media type).
fn image_part_signal(text: &str) -> Option<&'static str> {
    if text.contains("data:application/pdf") {
        return Some("inline base64 PDF");
    }
    if text.contains("data:image/") {
        return Some("inline base64 image");
    }
    if text.contains("input_image") {
        return Some("input_image part");
    }
    if text.contains("image_url") {
        return Some("image_url part");
    }
    let has_mime =
        text.contains("media_type") || text.contains("mime_type") || text.contains("mimeType");
    if has_mime && text.contains("application/pdf") {
        return Some("inline base64 PDF block");
    }
    if has_mime
        && (text.contains("image/png")
            || text.contains("image/jpeg")
            || text.contains("image/jpg")
            || text.contains("image/webp")
            || text.contains("image/gif"))
    {
        return Some("inline base64 image block");
    }
    None
}

/// Extract the `model` argument value from a call's (possibly multi-line) span,
/// matching `model="…"` (Python kwarg) or `"model": "…"` / `model: "…"` (JS/TS
/// object). Returns `None` when no literal model value is present.
fn extract_model_from_call(span: &str) -> Option<String> {
    for line in span.lines() {
        if !line.contains("model=")
            && !line.contains("\"model\"")
            && !line.contains("'model'")
            && !line.contains("model:")
        {
            continue;
        }
        let pos = line.find("model")?;
        let after = &line[pos + "model".len()..];
        let rest = after.trim_start_matches(|c: char| {
            c == '=' || c == ':' || c.is_whitespace() || c == '"' || c == '\''
        });
        let value: String = rest
            .chars()
            .take_while(|c| *c != '"' && *c != '\'' && *c != ',' && *c != ')' && *c != '}')
            .collect();
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

impl Rule for RawDocumentToVisionModelRule {
    fn id(&self) -> &'static str {
        "raw-document-to-vision-model"
    }

    fn severity(&self) -> Severity {
        Severity::Medium
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Python, Language::Typescript, Language::Javascript]
    }

    fn check(&self, source: &str, language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        // Cheap two-part reject before parsing (mirrors the sibling's pre-filter):
        // the file must contain an LLM create-call token AND an image-part signal.
        if !source.contains(".create")
            && !source.contains("generate_content")
            && !source.contains("generateContent")
        {
            return vec![];
        }
        let has_image_signal = source.contains("image_url")
            || source.contains("input_image")
            || source.contains("data:image/")
            || source.contains("data:application/pdf")
            || source.contains("media_type")
            || source.contains("mime_type")
            || source.contains("mimeType");
        if !has_image_signal {
            return vec![];
        }

        let Ok(tree) = parse_cached(source, language) else {
            return vec![];
        };

        let mut findings = Vec::new();
        for call in call_sites(&tree, source) {
            if !is_llm_create_callee(&call.callee) {
                continue;
            }

            // Image/PDF part scoped to THIS call's argument span.
            let Some(kind) = image_part_signal(&call.text) else {
                continue;
            };

            // Resolve the target model: explicit `model=` arg, else infer from
            // the callee's SDK family (so native Gemini `generate_content` is
            // treated as Gemini and correctly suppressed by the guard below).
            let (model_id, model_label) = match extract_model_from_call(&call.text) {
                Some(m) => {
                    let label = format!("'{m}'");
                    (m, label)
                }
                None => (
                    nominal_model_for_callee(&call.callee).to_string(),
                    "an unspecified vision model".to_string(),
                ),
            };

            // Text-only / embedding / audio families are out of scope.
            if is_non_vision_model(&model_id) {
                continue;
            }

            // Quote D0: nominal full-page image tokens → projected distilled
            // tokens, priced at the input rate. The Gemini direction guard (and
            // any negative-saving clamp) books $0 — suppress those findings.
            let raw_image_tokens = estimate_image_tokens(
                &model_id,
                NOMINAL_IMAGE_DIM,
                NOMINAL_IMAGE_DIM,
                ImageDetail::High,
            );
            let projected_distilled_tokens =
                (f64::from(raw_image_tokens) * NOMINAL_DISTILLED_RATIO).round() as u32;
            let proj = project(
                raw_image_tokens,
                projected_distilled_tokens,
                REPRESENTATIVE_INPUT_USD_PER_MTOK,
                &model_id,
            );
            if proj.projected_savings_usd <= 0.0 {
                continue;
            }

            let per_image = proj.projected_savings_usd;
            let per_1k = per_image * VOLUME_ANCHOR_IMAGES;

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: call.line,
                message: format!(
                    "Raw document ({kind}) is fed straight to vision model {model_label}. Sending \
                     the page as pixels bills ~{raw_image_tokens} image input tokens every call; \
                     distilling/offloading it to compact text (~{projected_distilled_tokens} \
                     tokens) projects a saving of ≈${per_image:.5}/image (≈${per_1k:.2} per \
                     {VOLUME_ANCHOR_IMAGES:.0} images) at a representative \
                     ${REPRESENTATIVE_INPUT_USD_PER_MTOK:.2}/M input rate (projected — basis: \
                     {basis}; the precise figure comes from the runtime cost preview).",
                    basis = proj.basis,
                ),
                confidence: 0.70,
                fix_hint: Some(
                    "Offload the document before the call instead of sending raw pixels: OCR / \
                     extract just the fields you need (or a compact caption) and send that text; \
                     cache the distilled text when the same document recurs; or route image-only \
                     steps to a cheaper vision tier. (Gemini prices page-images flat, so no \
                     saving is projected there.)"
                        .to_string(),
                ),
            });
        }

        findings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> RawDocumentToVisionModelRule {
        RawDocumentToVisionModelRule::new()
    }

    #[test]
    fn fires_on_openai_image_url_in_python_create_call() {
        let src = "import openai\n\
             client = openai.OpenAI()\n\
             resp = client.chat.completions.create(\n\
               model='gpt-4o',\n\
               messages=[{'role':'user','content':[\n\
                 {'type':'text','text':'What is in this receipt?'},\n\
                 {'type':'image_url','image_url':{'url':'data:image/png;base64,iVBORw0KGgoAAAA'}}\n\
               ]}],\n\
             )\n";
        let f = rule().check(src, Language::Python, "/src/app.py");
        assert_eq!(f.len(), 1, "image_url to gpt-4o should fire once");
        assert!(
            f[0].message.contains("projected"),
            "message must label the figure a projection"
        );
        assert!(f[0].message.contains("image input tokens"));
        assert!(f[0].message.contains("$"));
    }

    #[test]
    fn fires_on_anthropic_base64_image_block_python() {
        let src = "import anthropic\n\
             client = anthropic.Anthropic()\n\
             resp = client.messages.create(\n\
               model='claude-3-5-sonnet',\n\
               max_tokens=300,\n\
               messages=[{'role':'user','content':[\n\
                 {'type':'image','source':{'type':'base64','media_type':'image/png','data':'iVBORw0KGgo'}},\n\
                 {'type':'text','text':'Describe the chart'}\n\
               ]}],\n\
             )\n";
        let f = rule().check(src, Language::Python, "/src/anthropic_app.py");
        assert_eq!(f.len(), 1, "base64 image block to claude should fire once");
    }

    #[test]
    fn does_not_fire_on_text_only_call() {
        let src = "import openai\n\
             resp = openai.chat.completions.create(model='gpt-4o', \
             messages=[{'role':'user','content':'summarize this quarter'}])\n";
        let f = rule().check(src, Language::Python, "/src/text.py");
        assert!(f.is_empty(), "text-only call must not fire");
    }

    #[test]
    fn does_not_fire_when_image_not_in_llm_call() {
        // An image_url object built + logged, never passed to a create call.
        let src =
            "part = {'type':'image_url','image_url':{'url':'data:image/png;base64,iVBORw0'}}\n\
             print(part)\n";
        let f = rule().check(src, Language::Python, "/src/build.py");
        assert!(f.is_empty(), "image not in an LLM call must not fire");
    }

    #[test]
    fn does_not_fire_for_gemini_openai_compat_explicit_id() {
        // Gemini via the OpenAI-compatible endpoint: model id is explicit →
        // D0's direction guard books $0 → suppressed.
        let src = "resp = client.chat.completions.create(\n\
               model='gemini-2.5-flash',\n\
               messages=[{'role':'user','content':[\n\
                 {'type':'image_url','image_url':{'url':'data:image/png;base64,iVBORw0'}}\n\
               ]}],\n\
             )\n";
        let f = rule().check(src, Language::Python, "/src/gemini_compat.py");
        assert!(
            f.is_empty(),
            "Gemini projects $0 saving → must be suppressed"
        );
    }

    #[test]
    fn does_not_fire_for_native_gemini_generate_content() {
        // Native Google SDK: model id is on the client object, not the call.
        // The callee family (`generate_content`) infers Gemini → suppressed.
        let src = "import google.generativeai as genai\n\
             model = genai.GenerativeModel('gemini-2.5-flash')\n\
             resp = model.generate_content([\n\
               {'inline_data':{'mime_type':'image/png','data':'iVBORw0KGgo'}},\n\
               'Describe this receipt',\n\
             ])\n";
        let f = rule().check(src, Language::Python, "/src/gemini_native.py");
        assert!(
            f.is_empty(),
            "native Gemini generate_content must be suppressed"
        );
    }

    #[test]
    fn does_not_fire_for_non_vision_model() {
        // gpt-3.5-turbo is text-only → out of scope even with an image part.
        let src = "resp = client.chat.completions.create(\n\
               model='gpt-3.5-turbo',\n\
               messages=[{'role':'user','content':[\n\
                 {'type':'image_url','image_url':{'url':'data:image/png;base64,iVBORw0'}}\n\
               ]}],\n\
             )\n";
        let f = rule().check(src, Language::Python, "/src/nonvision.py");
        assert!(f.is_empty(), "non-vision model must not fire");
    }

    #[test]
    fn fires_on_openai_responses_input_image_ts() {
        let src = "const r = await client.responses.create({\n\
               model: 'gpt-4o',\n\
               input: [{ role: 'user', content: [\n\
                 { type: 'input_text', text: 'What is this?' },\n\
                 { type: 'input_image', image_url: 'data:image/jpeg;base64,/9j/4AAQ' }\n\
               ]}],\n\
             });\n";
        let f = rule().check(src, Language::Typescript, "/src/app.ts");
        assert_eq!(f.len(), 1, "input_image to gpt-4o (Responses) should fire");
    }

    #[test]
    fn test_fixture_paths_are_skipped() {
        let src = "resp = client.chat.completions.create(model='gpt-4o', \
             messages=[{'role':'user','content':[{'type':'image_url',\
             'image_url':{'url':'data:image/png;base64,iVBORw0'}}]}])\n";
        assert!(rule()
            .check(src, Language::Python, "crates/x/tests/fixture.py")
            .is_empty());
    }
}
