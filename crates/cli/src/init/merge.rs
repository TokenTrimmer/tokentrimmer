//! Per-file-type merge strategy: settings.json is JSON-merge, .gitignore is
//! append-with-dedupe, other files are skip-or-overwrite.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MergeError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Append lines from `additions` to `existing`, skipping any line already
/// present (whole-line match). Preserves order; adds a blank line before
/// new content if existing doesn't end in one.
pub fn append_gitignore(existing: &str, additions: &str) -> String {
    let existing_lines: std::collections::HashSet<&str> = existing.lines().collect();
    let mut new_lines = Vec::new();
    for line in additions.lines() {
        if !existing_lines.contains(line) && !line.trim().is_empty() {
            new_lines.push(line);
        }
    }
    if new_lines.is_empty() {
        return existing.to_string();
    }
    let mut out = existing.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&new_lines.join("\n"));
    out.push('\n');
    out
}

/// Merge JSON `additions` into `existing`. Deep merge for objects.
/// Existing values win on type mismatch (we never silently change a user
/// non-object key into an object).
pub fn merge_settings_json(existing: &str, additions: &str) -> Result<String, MergeError> {
    let mut a: serde_json::Value = serde_json::from_str(existing)?;
    let b: serde_json::Value = serde_json::from_str(additions)?;
    deep_merge(&mut a, &b);
    Ok(serde_json::to_string_pretty(&a)?)
}

fn deep_merge(into: &mut serde_json::Value, from: &serde_json::Value) {
    if let (serde_json::Value::Object(into_map), serde_json::Value::Object(from_map)) = (into, from)
    {
        for (k, v) in from_map {
            if let Some(existing_v) = into_map.get_mut(k) {
                deep_merge(existing_v, v);
            } else {
                into_map.insert(k.clone(), v.clone());
            }
        }
    }
    // type mismatch — existing wins (no-op)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_gitignore_dedupes() {
        let existing = "node_modules/\n.env\n";
        let additions = ".env\n.tt-init.lock\n.claude/PAUSED\n";
        let result = append_gitignore(existing, additions);
        assert!(result.contains("node_modules/"));
        assert!(result.contains(".tt-init.lock"));
        assert!(result.contains(".claude/PAUSED"));
        // .env appears exactly once
        assert_eq!(result.matches(".env").count(), 1);
    }

    #[test]
    fn append_gitignore_noop_when_all_present() {
        let existing = "a\nb\nc\n";
        let result = append_gitignore(existing, "a\nb\n");
        assert_eq!(result, "a\nb\nc\n");
    }

    #[test]
    fn merge_json_deep_merge_objects() {
        let a = r#"{"hooks": {"PreToolUse": [1]}, "other": "x"}"#;
        let b = r#"{"hooks": {"PostToolUse": [2]}}"#;
        let merged = merge_settings_json(a, b).unwrap();
        assert!(merged.contains("\"PreToolUse\""));
        assert!(merged.contains("\"PostToolUse\""));
        assert!(merged.contains("\"other\""));
    }

    #[test]
    fn merge_json_existing_wins_on_type_mismatch() {
        let a = r#"{"key": "string"}"#;
        let b = r#"{"key": {"nested": "x"}}"#;
        let merged = merge_settings_json(a, b).unwrap();
        assert!(merged.contains("\"string\""));
    }
}
