//! Run `tt inspect` on the target repo after install and write the result
//! to `.claude/inspect-baseline.json`. If --skip-baseline is set, write a
//! stub so future comparison logic doesn't crash.

use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BaselineError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub fn run_baseline(root: &Path) -> Result<usize, BaselineError> {
    // Mirrors the invocation in crates/cli/src/main.rs::run_inspect.
    let mut engine = tt_inspect_core::Engine::new();
    for rule in tt_inspect_rules_tier1::all_rules() {
        engine.add_rule(rule);
    }
    let findings = engine.scan(root);
    let dest = root.join(".claude").join("inspect-baseline.json");
    std::fs::create_dir_all(dest.parent().unwrap())?;
    let body = serde_json::json!({
        "findings": findings,
        "generated_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(&dest, serde_json::to_string_pretty(&body).unwrap())?;
    Ok(findings.len())
}

pub fn write_skipped_baseline(root: &Path) -> Result<(), BaselineError> {
    let dest = root.join(".claude").join("inspect-baseline.json");
    std::fs::create_dir_all(dest.parent().unwrap())?;
    let body = serde_json::json!({
        "findings": [],
        "skipped": true,
        "generated_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(&dest, serde_json::to_string_pretty(&body).unwrap())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_writes_stub_with_skipped_flag() {
        let d = tempfile::tempdir().unwrap();
        write_skipped_baseline(d.path()).unwrap();
        let body = std::fs::read_to_string(d.path().join(".claude").join("inspect-baseline.json")).unwrap();
        assert!(body.contains("\"skipped\": true"));
        assert!(body.contains("\"findings\": []"));
    }
}
