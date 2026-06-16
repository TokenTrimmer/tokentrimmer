//! Assemble a ranked file list into a token-budgeted context pack.
use std::path::PathBuf;

use serde::Serialize;

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
            if spent + cost <= token_budget {
                spent += cost;
                content = Some(src);
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
        let big_inlined = pack
            .files
            .iter()
            .find(|f| f.path.ends_with("big.py"))
            .and_then(|f| f.content.as_ref());
        assert!(
            big_inlined.is_none(),
            "big.py should not be inlined under a 50-token budget"
        );
    }
}
