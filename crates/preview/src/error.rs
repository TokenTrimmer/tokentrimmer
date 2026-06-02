//! Preview engine errors. Designed so the HTTP handler can always emit a
//! valid `PreviewResponse` with `warnings[]`, never a 5xx.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PreviewError {
    #[error("unknown model `{0}` — no pricing table available")]
    UnknownModel(String),
    #[error("malformed request: {0}")]
    Malformed(String),
    #[error("tokenizer failure: {0}")]
    Tokenizer(String),
}

impl PreviewError {
    pub fn as_warning(&self) -> String {
        match self {
            Self::UnknownModel(m) => {
                format!("model {m} not in pricing table; falling back to heuristic")
            }
            Self::Malformed(s) => format!("malformed input: {s}"),
            Self::Tokenizer(s) => format!("tokenizer failed: {s} — using char-count heuristic"),
        }
    }
}
