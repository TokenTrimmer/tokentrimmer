//! Thin wrapper around tree-sitter that provides a single [`parse`] function.
//!
//! Rules that need an AST call [`parse`] themselves; the engine does not parse
//! eagerly (a rule-level AST cache is a future perf win tracked in the backlog).

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
