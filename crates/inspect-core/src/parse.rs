//! Thin wrapper around tree-sitter that provides [`parse`] and the memoized
//! [`parse_cached`].
//!
//! A single source file is fed to every applicable rule during a scan. Rules
//! that walk the AST therefore call [`parse_cached`] rather than [`parse`], so
//! the file is parsed **once** and the resulting [`Tree`] is shared (as an
//! `Arc`) across all rules — the rule-level AST cache.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tree_sitter::{Parser, Tree};

use crate::Language;

/// Errors that can occur while setting up the parser or parsing source text.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The parser could not be configured for the requested language.
    #[error("parser setup: {0}")]
    Setup(String),
    /// tree-sitter returned `None` instead of a tree (typically means the
    /// parser was cancelled or the source contains NUL bytes).
    #[error("parse returned no tree")]
    NoTree,
}

/// Parse `source` as the given `language` and return the resulting
/// [`Tree`][tree_sitter::Tree].
///
/// The tree always covers the entire input even when the file contains syntax
/// errors — tree-sitter performs error recovery. Callers can check
/// [`Tree::root_node().has_error()`][tree_sitter::Node::has_error] if they
/// need to gate on a clean parse.
pub fn parse(source: &str, language: Language) -> Result<Tree, ParseError> {
    let mut parser = Parser::new();
    let ts_lang: tree_sitter::Language = match language {
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        // .ts and .tsx both map to Language::Typescript; we use the TypeScript
        // grammar here. Rules that need TSX-specific AST structure can call
        // parse_tsx() instead (not yet implemented; add when a rule requires it).
        Language::Typescript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Javascript => tree_sitter_javascript::LANGUAGE.into(),
        // Markdown files do not have a tree-sitter grammar bundled in this
        // workspace. Rules that operate on Markdown (e.g.
        // `config-agents-md-contains-secrets`) use regex-only detection and
        // do not call `parse()`. Reaching this arm is a programming error.
        Language::Markdown => {
            return Err(ParseError::Setup(
                "tree-sitter parsing is not supported for Markdown; \
                 use regex detection instead"
                    .into(),
            ))
        }
    };
    parser
        .set_language(&ts_lang)
        .map_err(|e| ParseError::Setup(e.to_string()))?;
    parser.parse(source, None).ok_or(ParseError::NoTree)
}

/// Process-wide parse cache: `hash(language, source) -> Arc<Tree>`. Bounded to
/// avoid unbounded growth in a long-lived process (the gateway parses
/// per-request temp files); when the cap is exceeded the whole cache is
/// cleared, which is acceptable for a best-effort memo.
fn parse_cache() -> &'static Mutex<HashMap<u64, Arc<Tree>>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Arc<Tree>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const PARSE_CACHE_CAP: usize = 512;

fn cache_key(source: &str, language: Language) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (language as u8).hash(&mut h);
    source.hash(&mut h);
    h.finish()
}

/// Parse `source` once and memoize the [`Tree`] keyed by `(language, source)`.
///
/// Repeated calls for the same file — e.g. every AST-backed rule in a single
/// scan — return the cached `Arc<Tree>` instead of re-parsing. This is the
/// rule-level AST cache: parse cost is paid once per unique source per process.
pub fn parse_cached(source: &str, language: Language) -> Result<Arc<Tree>, ParseError> {
    let key = cache_key(source, language);
    {
        let guard = parse_cache().lock().expect("parse cache poisoned");
        if let Some(tree) = guard.get(&key) {
            return Ok(Arc::clone(tree));
        }
    }
    let tree = Arc::new(parse(source, language)?);
    let mut guard = parse_cache().lock().expect("parse cache poisoned");
    if guard.len() >= PARSE_CACHE_CAP {
        guard.clear();
    }
    guard.insert(key, Arc::clone(&tree));
    Ok(tree)
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn parse_cached_returns_same_tree_for_same_source() {
        let src = "x = client.chat.completions.create(model='gpt-4o')\n";
        let a = parse_cached(src, Language::Python).unwrap();
        let b = parse_cached(src, Language::Python).unwrap();
        // Same Arc allocation → the file was parsed once and shared.
        assert!(Arc::ptr_eq(&a, &b), "expected the cached tree to be reused");
    }

    #[test]
    fn parse_cached_distinguishes_sources_and_languages() {
        let a = parse_cached("a = 1\n", Language::Python).unwrap();
        let b = parse_cached("b = 2\n", Language::Python).unwrap();
        assert!(!Arc::ptr_eq(&a, &b), "different sources must not alias");
        // Same text, different language → distinct entries.
        let c = parse_cached("const x = 1\n", Language::Typescript).unwrap();
        let d = parse_cached("const x = 1\n", Language::Javascript).unwrap();
        assert!(!Arc::ptr_eq(&c, &d));
    }

    #[test]
    fn parse_cached_tree_covers_input() {
        let tree = parse_cached("y = 1\n", Language::Python).unwrap();
        assert!(!tree.root_node().has_error());
    }
}
