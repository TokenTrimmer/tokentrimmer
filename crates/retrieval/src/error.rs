use thiserror::Error;

#[derive(Debug, Error)]
pub enum RetrievalError {
    #[error("embedding HTTP: {0}")]
    Embedding(String),
    #[error("store: {0}")]
    Store(String),
    #[error("tag parse: {0}")]
    Tag(String),
    #[error("malformed: {0}")]
    Malformed(String),
    #[error("non-finite embedding (NaN/Inf)")]
    InvalidEmbedding,
}
