//! Assemble a ranked file list into a token-budgeted context pack.
use std::path::{Path, PathBuf};

use serde::Serialize;
use tt_inspect_core::Language;

use crate::index::RepoIndex;
use crate::rank::RankedFile;

#[derive(Debug, Clone, Serialize)]
pub struct ContextFile {
    pub path: PathBuf,
    pub summary: String,
    pub symbols: Vec<String>,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextPack {
    pub files: Vec<ContextFile>,
    pub token_estimate: u32,
    pub note: String,
}

#[must_use]
pub fn assemble(
    ranked: &[RankedFile],
    index: &RepoIndex,
    max_files: usize,
    token_budget: u32,
) -> ContextPack {
    let mut files = Vec::new();
    let mut spent: u32 = 0;
    for r in ranked.iter().take(max_files) {
        let entry = index.files().iter().find(|f| f.path == r.path);
        let symbols: Vec<String> = entry
            .map(|e| {
                e.symbols
                    .functions
                    .iter()
                    .chain(e.symbols.classes.iter())
                    .map(|s| s.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let summary = if symbols.is_empty() {
            "(no top-level symbols)".to_string()
        } else {
            format!("symbols: {}", symbols.join(", "))
        };
        let mut content = None;
        if let Ok(src) = std::fs::read_to_string(&r.path) {
            let cost = tt_tokenize::estimate_tokens("openai", &src);
            if spent.saturating_add(cost) <= token_budget {
                spent = spent.saturating_add(cost);
                content = Some(src);
            } else if let Some(language) = language_for_path(&r.path) {
                // Required file doesn't fit the remaining budget — inline its
                // AST signature skeleton instead of dropping it outright
                // (`skeletonizer.rs`). A rated-but-skipped file is invisible
                // to the consumer; a skeletonized one keeps its API surface
                // (signature) visible for near-zero tokens.
                let skeleton = crate::skeletonizer::skeletonize_source(&src, language);
                let skeleton_cost = tt_tokenize::estimate_tokens("openai", &skeleton);
                if spent.saturating_add(skeleton_cost) <= token_budget {
                    spent = spent.saturating_add(skeleton_cost);
                    content = Some(skeleton);
                }
            }
        }
        files.push(ContextFile {
            path: r.path.clone(),
            summary,
            symbols,
            reasons: r.reasons.clone(),
            content,
        });
    }
    let note = if files.is_empty() {
        "No matching files found in the repo index.".to_string()
    } else {
        format!(
            "{} files ranked; {} inlined within the {}-token budget.",
            files.len(),
            files.iter().filter(|f| f.content.is_some()).count(),
            token_budget
        )
    };
    ContextPack {
        files,
        token_estimate: spent,
        note,
    }
}

/// Extension → tree-sitter language for the skeleton fallback in
/// [`assemble`]. Files in other languages simply keep the drop-instead-of-
/// inline behavior when they don't fit the budget.
fn language_for_path(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()? {
        "py" => Some(Language::Python),
        "ts" | "tsx" | "mts" | "cts" => Some(Language::Typescript),
        "js" | "jsx" | "mjs" | "cjs" => Some(Language::Javascript),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::RepoIndex;
    use crate::rank::rank;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn assembles_outlines_and_respects_token_budget() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let big = "x = 1\n".repeat(5000);
        fs::write(root.join("big.py"), &big).unwrap();
        fs::write(root.join("small.py"), "def helper():\n    return 1\n").unwrap();
        let idx = RepoIndex::build(root);
        let ranked = rank(&idx, "helper");
        let pack = assemble(&ranked, &idx, 10, 50);
        assert!(!pack.files.is_empty());
        assert!(
            pack.token_estimate <= 50,
            "estimate {} over budget",
            pack.token_estimate
        );
        assert!(
            pack.files.iter().any(|f| f.path.ends_with("big.py")),
            "big.py must be in the pack to prove it isn't fully inlined"
        );
        let big_inlined = pack
            .files
            .iter()
            .find(|f| f.path.ends_with("big.py"))
            .and_then(|f| f.content.as_ref());
        // The full text never fits a 50-token budget; the skeleton fallback
        // may inline a tiny signature-only view instead.
        if let Some(content) = big_inlined {
            assert!(
                content.contains("skeletonized"),
                "oversized file may only inline as a skeleton, got: {content:?}"
            );
        }
    }

    /// When a ranked file does not fit the remaining budget, its AST skeleton
    /// is inlined instead of being dropped entirely.
    #[test]
    fn oversized_file_falls_back_to_ast_skeleton() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // A Python file: enough real definitions that the full text exceeds a
        // tight budget, but whose skeleton (names only) fits.
        let mut big = String::new();
        for i in 0..8 {
            big.push_str(&format!(
                "def handler_{i}(req: int, res: str, payload: dict[str, int]) -> dict[str, int]:\n    return {{}}  # no-op body padding padding padding padding\n\n"
            ));
        }
        fs::write(root.join("big.py"), &big).unwrap();
        let idx = RepoIndex::build(root);
        let ranked = rank(&idx, "handler");
        // Budget sized so the FULL file cannot inline (~310 estimated tokens
        // for 8 fully-commented handlers) but its 8-signature skeleton can
        // (~≈205 estimated tokens).
        let pack = assemble(&ranked, &idx, 10, 250);
        let content = pack
            .files
            .iter()
            .find(|f| f.path.ends_with("big.py"))
            .and_then(|f| f.content.as_ref())
            .expect("skeleton should be inlined when the full file doesn't fit");
        assert!(
            content.contains("skeletonized"),
            "inlined content must be the skeleton"
        );
        assert!(content.contains("handler_1"), "skeleton keeps definitions");
    }
}
