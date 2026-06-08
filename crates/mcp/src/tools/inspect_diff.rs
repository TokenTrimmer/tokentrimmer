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
        let suffix = if ext.is_empty() {
            String::new()
        } else {
            format!(".{ext}")
        };
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
        Ok(json!({ "findings": findings }))
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_ext;

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
