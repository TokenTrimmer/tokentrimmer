//! Coding-agent context preloader. Builds a repo symbol/import index, ranks
//! files for a task, and assembles a token-budgeted context pack. Fully local
//! and deterministic — no embeddings, no network.
pub mod assemble;
pub mod cache;
pub mod index;
pub mod rank;

use std::path::Path;
use std::sync::OnceLock;

use crate::assemble::{assemble, ContextPack};
use crate::cache::IndexCache;
use crate::rank::rank;

fn global_cache() -> &'static IndexCache {
    static CACHE: OnceLock<IndexCache> = OnceLock::new();
    CACHE.get_or_init(IndexCache::new)
}

/// Build/reuse the repo index, rank for `task`, and assemble a context pack.
/// The single entry point shared by the MCP tool and the CLI.
#[must_use]
pub fn repo_context(
    repo_root: &Path,
    task: &str,
    max_files: usize,
    token_budget: u32,
) -> ContextPack {
    let index = global_cache().get_or_build(repo_root);
    let ranked = rank(&index, task);
    assemble(&ranked, &index, max_files, token_budget)
}

#[cfg(test)]
mod smoke {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn repo_context_returns_non_empty_pack() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("helper.py"), "def helper():\n    return 42\n").unwrap();
        let pack = repo_context(root, "helper", 5, 1000);
        assert!(
            !pack.files.is_empty(),
            "expected at least one file in the pack"
        );
        assert!(
            pack.token_estimate <= 1000,
            "token_estimate {} exceeds budget",
            pack.token_estimate
        );
    }
}
