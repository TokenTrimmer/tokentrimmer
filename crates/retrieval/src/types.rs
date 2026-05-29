//! Data shapes shared across the crate.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Corpus {
    pub org_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub created_at: String, // ISO-8601
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub doc_id: Uuid,
    pub corpus: String,
    pub org_id: Uuid,
    pub source_path: String,
    pub bytes_indexed: u64,
    pub chunks: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: Uuid,
    pub org_id: Uuid,
    pub corpus: String,
    pub doc_id: Uuid,
    pub chunk_idx: u32,
    pub text: String,
    pub embedding: Vec<f32>, // 1536-dim
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetrievalResult {
    pub chunk_id: Uuid,
    pub doc_id: Uuid,
    pub chunk_idx: u32,
    pub text: String,
    pub similarity: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievableTag {
    pub corpus: String,
    pub k: u32,
    /// Span in the original message text (start_byte_idx, end_byte_idx_exclusive).
    pub span: (usize, usize),
}
