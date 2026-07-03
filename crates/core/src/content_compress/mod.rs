//! Content-aware compression engine (Phase 1).
//!
//! A live [`ContentClassifier`](classify::classify) tags each large request
//! block by kind ([`ContentKind`]) and a `content_compress` dispatcher routes it
//! to a specialized backend, all wrapped in TT's moat — the pass token-true gate
//! (reject any token-growing transform → verbatim), the quality judge + 0.90
//! auto-pause (for the lossy backends), and isolated/attested savings.
//!
//! # Slices
//! - **P1a (this):** the classifier + the opt-in [`ContentCompressPass`]
//!   dispatcher + a JSON/CSV/log CONTENT-PRESERVING structural backend + the
//!   isolated `content_compress_saved_est_usd` cost field + flywheel telemetry
//!   ([`capture`]). Code/Prose blocks are classified but LEFT UNTOUCHED here.
//! - **P1b:** a prose extractive backend (lossy, judge-gated).
//! - **P1c:** an AST code backend (reuses `inspect-core` tree-sitter).
//! - **P1d:** the flywheel dataset export.
//!
//! The classifier itself lives in `tt-shared` ([`tt_shared::content_kind`]) so
//! the routing engine's `content_type` condition can reuse it without a
//! dependency cycle; this module re-exports it so the spec-named
//! `content_compress::classify` path holds.

pub mod classify;

pub use classify::{classify, ContentKind, MIN_BLOB_CHARS};
