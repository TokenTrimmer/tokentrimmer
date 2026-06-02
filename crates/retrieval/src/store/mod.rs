use async_trait::async_trait;
use uuid::Uuid;

use crate::error::RetrievalError;
use crate::types::{Chunk, RetrievalResult};

#[async_trait]
pub trait RetrievalStore: Send + Sync {
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError>;
    async fn search(
        &self,
        org_id: Uuid,
        corpus: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<RetrievalResult>, RetrievalError>;
    async fn delete_corpus(&self, org_id: Uuid, corpus: &str) -> Result<u64, RetrievalError>;
}

pub mod memory;
#[cfg(feature = "postgres")]
pub mod postgres;
