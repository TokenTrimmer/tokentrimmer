//! Deterministic relevance ranking over a `RepoIndex` for a task description.
use std::path::PathBuf;

use serde::Serialize;

use crate::index::RepoIndex;

#[derive(Debug, Clone, Serialize)]
pub struct RankedFile {
    pub path: PathBuf,
    pub score: f64,
    pub reasons: Vec<String>,
}

const W_SYMBOL: f64 = 3.0;
const W_CENTRAL: f64 = 1.0;
const SIZE_PENALTY: f64 = 0.0008;

#[must_use]
pub fn rank(index: &RepoIndex, task: &str) -> Vec<RankedFile> {
    let keywords = tokenize(&task.to_lowercase());
    let mut ranked: Vec<RankedFile> = index
        .files()
        .iter()
        .map(|f| {
            let mut score = 0.0;
            let mut reasons = Vec::new();
            let names: Vec<String> = f
                .symbols
                .functions
                .iter()
                .chain(f.symbols.classes.iter())
                .map(|s| s.name.to_lowercase())
                .chain(path_tokens(&f.path))
                .collect();
            let hits = keywords
                .iter()
                .filter(|k| names.iter().any(|n| n.contains(*k)))
                .count();
            if hits > 0 {
                score += W_SYMBOL * hits as f64;
                reasons.push(format!("matches {hits} task term(s)"));
            }
            let indeg = f.importers.len();
            if indeg > 0 {
                score += W_CENTRAL * indeg as f64;
                reasons.push(format!("imported by {indeg} file(s)"));
            }
            score -= SIZE_PENALTY * f.loc as f64;
            RankedFile {
                path: f.path.clone(),
                score,
                reasons,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    ranked
}

fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(str::to_string)
        .collect()
}

fn path_tokens(p: &std::path::Path) -> Vec<String> {
    p.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::RepoIndex;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn ranks_symbol_match_and_centrality_first_deterministically() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("auth.py"),
            "def authenticate(user):\n    return True\n",
        )
        .unwrap();
        fs::write(root.join("api.py"), "from auth import authenticate\n").unwrap();
        fs::write(root.join("web.py"), "from auth import authenticate\n").unwrap();
        fs::write(root.join("unrelated.py"), "def widget():\n    return 0\n").unwrap();
        let idx = RepoIndex::build(root);
        let ranked = rank(&idx, "add a new authenticate handler");
        assert!(
            ranked.first().unwrap().path.ends_with("auth.py"),
            "auth.py should rank first; got {:?}",
            ranked.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
        let ranked2 = rank(&idx, "add a new authenticate handler");
        assert_eq!(
            ranked.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
            ranked2.iter().map(|r| r.path.clone()).collect::<Vec<_>>()
        );
    }
}
