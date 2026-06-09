//! inspect_diff — write proposed content to a temp file, run inspect-core, return findings.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct InspectDiffTool;

/// Sanitize a caller-supplied file extension into a short alphanumeric token.
///
/// The extension only steers language detection for the temp file, so we keep
/// it to ASCII-alphanumeric and cap its length — a caller can't inject path or
/// suffix surprises through `file_path`.
fn sanitize_ext(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect()
}

#[derive(Deserialize)]
struct Input {
    file_path: String,
    proposed_content: String,
}

#[async_trait]
impl Tool for InspectDiffTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "inspect_diff",
            description: "Run TokenTrimmer Inspect rules against a proposed file diff before writing. Returns findings (severity, rule_id, line, message).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string" },
                    "proposed_content": { "type": "string" }
                },
                "required": ["file_path", "proposed_content"]
            }),
        }
    }
    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let inp: Input =
            serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
        let raw_ext = std::path::Path::new(&inp.file_path)
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("");
        let ext = sanitize_ext(raw_ext);

        // Resolve the language the engine would assign this file up front. If
        // it's one inspect doesn't scan, return an explicit reason rather than a
        // bare empty findings list — otherwise the caller can't tell "clean"
        // from "silently skipped because .txt isn't a scanned language". Uses
        // the same source-of-truth mapping the directory walker uses.
        let Some(lang) = tt_inspect_core::Language::from_extension(&ext) else {
            return Ok(json!({
                "findings": [],
                "scanned": false,
                "detected_language": Value::Null,
                "reason": format!(
                    "inspect does not scan '{}' — supported extensions: \
                     .py, .ts/.tsx, .js/.jsx/.mjs/.cjs, .md",
                    inp.file_path
                ),
            }));
        };

        let suffix = format!(".{ext}");
        let mut tmp = tempfile::Builder::new()
            .suffix(&suffix)
            .tempfile()
            .map_err(|e| McpError::Internal(format!("tempfile: {e}")))?;
        use std::io::Write;
        write!(tmp, "{}", inp.proposed_content)
            .map_err(|e| McpError::Internal(format!("write: {e}")))?;
        let mut engine = tt_inspect_core::Engine::new();
        for rule in tt_inspect_rules_tier1::all_rules() {
            engine.add_rule(rule);
        }
        let findings = engine.scan(tmp.path());
        Ok(json!({
            "findings": findings,
            "scanned": true,
            "detected_language": lang,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_ext, InspectDiffTool};
    use crate::tools::Tool;
    use serde_json::json;

    #[tokio::test]
    async fn unsupported_language_returns_reason_not_silent_empty() {
        let out = InspectDiffTool
            .call(json!({
                "file_path": "notes.txt",
                "proposed_content": "just some prose, not a scanned language"
            }))
            .await
            .unwrap();
        assert_eq!(out["scanned"], json!(false));
        assert_eq!(out["detected_language"], serde_json::Value::Null);
        assert_eq!(out["findings"], json!([]));
        assert!(
            out["reason"].as_str().unwrap().contains("does not scan"),
            "expected an explicit reason, got: {out}"
        );
    }

    #[tokio::test]
    async fn extensionless_path_is_reported_as_unsupported() {
        let out = InspectDiffTool
            .call(json!({ "file_path": "Makefile", "proposed_content": "all:\n\techo hi" }))
            .await
            .unwrap();
        assert_eq!(out["scanned"], json!(false));
    }

    #[tokio::test]
    async fn supported_language_scans_and_echoes_detected_language() {
        let out = InspectDiffTool
            .call(json!({
                "file_path": "mod.py",
                "proposed_content": "x = 1\n"
            }))
            .await
            .unwrap();
        assert_eq!(out["scanned"], json!(true));
        assert_eq!(out["detected_language"], json!("python"));
        assert!(out["findings"].is_array());
    }

    #[test]
    fn sanitize_ext_keeps_simple_extensions() {
        assert_eq!(sanitize_ext("rs"), "rs");
        assert_eq!(sanitize_ext("py"), "py");
        assert_eq!(sanitize_ext("md"), "md");
    }

    #[test]
    fn sanitize_ext_strips_non_alnum_and_caps_length() {
        // Path/suffix-injection characters are dropped.
        assert_eq!(sanitize_ext("rs/../../etc"), "rsetc");
        assert_eq!(sanitize_ext("sh; rm -rf"), "shrmrf");
        // Capped at 16 chars.
        assert_eq!(sanitize_ext(&"a".repeat(50)).len(), 16);
        // Empty / all-junk extensions collapse to empty (no suffix).
        assert_eq!(sanitize_ext(""), "");
        assert_eq!(sanitize_ext("!@#$"), "");
    }
}
