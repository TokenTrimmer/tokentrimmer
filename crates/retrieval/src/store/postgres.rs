//! Postgres + pgvector store. Schema lives in cloud-repo migrations.
//! Body intentionally `todo!()` — wired in a follow-up plan once the cloud
//! migration ships.

use uuid::Uuid;

use crate::error::RetrievalError;
use crate::store::RetrievalStore;
use crate::types::{Chunk, RetrievalResult};

pub struct PostgresStore {
    pub pool: sqlx::PgPool,
}

#[async_trait::async_trait]
impl RetrievalStore for PostgresStore {
    async fn insert(&self, _chunk: Chunk) -> Result<(), RetrievalError> {
        todo!("wire after cloud-repo migration ships retrieval_chunks table")
    }

    async fn search(
        &self,
        _o: Uuid,
        _c: &str,
        _q: &[f32],
        _k: usize,
    ) -> Result<Vec<RetrievalResult>, RetrievalError> {
        todo!()
    }

    async fn delete_corpus(&self, _o: Uuid, _c: &str) -> Result<u64, RetrievalError> {
        todo!()
    }
}
