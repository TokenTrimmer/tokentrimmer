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
pub use substitute::{substitute_in_messages, SubstitutionReport};
pub use types::{Chunk, Corpus, Document, RetrievableTag, RetrievalResult};
