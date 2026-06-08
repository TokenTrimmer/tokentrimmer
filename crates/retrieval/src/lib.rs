//! `tt-retrieval` — RAG / context-compression engine.
//!
//! See `docs/superpowers/specs/2026-05-28-trackE-rag-context-compression-design.md`.

#[cfg(feature = "postgres")]
pub mod audit;
pub mod chunking;
pub mod embed;
pub mod error;
pub mod search;
pub mod store;
pub mod substitute;
pub mod tags;
pub mod types;

pub use error::RetrievalError;
pub use store::RetrievalStore;
pub use substitute::{substitute_in_messages, SubstitutionReport, DEFAULT_MIN_SIMILARITY};
pub use types::{Chunk, Corpus, Document, RetrievableTag, RetrievalResult};

/// True if every component is finite (no NaN/Inf). A non-finite embedding makes
/// cosine distance NaN and corrupts top-k ranking, so it is rejected at the
/// embed chokepoint and at store insert (mirrors the L2 cache guard).
pub(crate) fn embedding_is_finite(v: &[f32]) -> bool {
    v.iter().all(|x| x.is_finite())
}
