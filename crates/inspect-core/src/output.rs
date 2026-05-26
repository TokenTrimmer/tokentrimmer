//! Formatting helpers that turn a slice of [`Finding`]s into a human-readable
//! markdown report or a machine-readable JSON array.

use crate::{Finding, Severity};

/// Render `findings` as a pretty-printed JSON array.
///
/// Returns `"[]"` on serialisation failure (which should never occur given
/// the types involved).
pub fn format_json(findings: &[Finding]) -> String {
    serde_json::to_string_pretty(findings).unwrap_or_else(|_| "[]".into())
}

/// Render `findings` as a markdown report grouped by severity (descending).
///
/// When `findings` is empty the output is a short "No findings." section so
/// that CI logs are unambiguous.
pub fn format_markdown(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "# TokenTrimmer Inspect\n\nNo findings.\n".into();
    }

    let mut out = String::new();
    out.push_str("# TokenTrimmer Inspect\n\n");
    out.push_str(&format!("Found **{}** finding(s).\n\n", findings.len()));

    // Emit groups in descending severity order.
    for sev in [Severity::Critical, Severity::High, Severity::Medium, Severity::Low] {
        let bucket: Vec<&Finding> = findings.iter().filter(|f| f.severity == sev).collect();
        if bucket.is_empty() {
            continue;
        }
        out.push_str(&format!("## {:?} ({})\n\n", sev, bucket.len()));
        for f in bucket {
            out.push_str(&format!(
                "- **{}** `{}:{}` — {} _(confidence {:.0}%)_\n",
                f.rule_id,
                f.file,
                f.line,
                f.message,
                f.confidence * 100.0
            ));
            if let Some(hint) = &f.fix_hint {
                out.push_str(&format!("    Fix: {hint}\n"));
            }
        }
        out.push('\n');
    }
    out
}
