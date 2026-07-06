//! Content-aware compression engine (Phase 1).
//!
//! A live [`ContentClassifier`](classify::classify) tags each large request
//! block by kind ([`ContentKind`]) and a `content_compress` dispatcher routes it
//! to a specialized backend, all wrapped in TT's moat — the pass token-true gate
//! (reject any token-growing transform → verbatim), the quality judge + 0.90
//! auto-pause (for the lossy backends), and isolated/attested savings.
//!
//! # Slices
//! - **P1a:** the classifier + the opt-in [`ContentCompressPass`]
//!   dispatcher + a JSON/CSV/log CONTENT-PRESERVING structural backend + the
//!   isolated `content_compress_saved_est_usd` cost field + flywheel telemetry
//!   ([`capture`]). Code blocks are classified but LEFT UNTOUCHED there.
//! - **P1b (this):** a prose EXTRACTIVE backend ([`prose`]) — LOSSY, so it
//!   commits only behind the shared judge gate + 0.90 auto-pause (fail-open to
//!   verbatim), routed by the dispatcher when a route sets `content_compress`
//!   AND the `"prose"` class is judge-trusted.
//! - **P1c (this):** an AST code backend ([`code`]) — LOSSY (it truncates
//!   function bodies), reusing `inspect-core`'s tree-sitter harness. It KEEPS
//!   imports/signatures/type declarations and RE-PARSE-VERIFIES its output
//!   ("never serve broken code"), committed only behind the shared judge gate for
//!   the `"code"` class (fail-open to verbatim), routed by the dispatcher when a
//!   route sets `content_compress`.
//! - **P1d:** the flywheel dataset export.
//!
//! The classifier itself lives in `tt-shared` ([`tt_shared::content_kind`]) so
//! the routing engine's `content_type` condition can reuse it without a
//! dependency cycle; this module re-exports it so the spec-named
//! `content_compress::classify` path holds.

pub mod capture;
pub mod classify;
pub mod code;
pub mod prose;
pub mod structural;

pub use classify::{classify, ContentKind, MIN_BLOB_CHARS};
pub use code::CODE_CLASS;
pub use prose::PROSE_CLASS;
pub use structural::{compactable_kind, dominant_compactable_kind, ContentCompressPass};
