//! Postgres + pgvector store. Schema:
//! `crates/core/migrations/0005_retrieval_chunks.{up,down}.sql`.
//!
//! Mirrors the L2 semantic cache (`tt_cache::PostgresL2Cache`): HNSW cosine
//! (`<=>`) over org-scoped vectors, with a per-transaction `hnsw.ef_search`
//! raise so the `(org_id, corpus)` filter doesn't starve recall under
//! multi-tenant load.

use uuid::Uuid;

use crate::error::RetrievalError;
use crate::store::RetrievalStore;
use crate::types::{Chunk, RetrievalResult};

/// Raised HNSW candidate-list size. pgvector's default (40) lets other tenants'
/// vectors crowd out the querying org's neighbours once the `(org_id, corpus)`
/// filter is applied; 100 restores recall. Matches `PostgresL2Cache`.
const DEFAULT_EF_SEARCH: i64 = 100;

pub struct PostgresStore {
    pub pool: sqlx::PgPool,
    ef_search: i64,
}

impl PostgresStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
            ef_search: DEFAULT_EF_SEARCH,
        }
    }

    /// Override the HNSW `ef_search` candidate-list size (min 1).
    #[must_use]
    pub fn with_ef_search(mut self, ef_search: i64) -> Self {
        self.ef_search = ef_search.max(1);
        self
    }
}

fn store_err(e: impl std::fmt::Display) -> RetrievalError {
    RetrievalError::Store(e.to_string())
}

#[async_trait::async_trait]
impl RetrievalStore for PostgresStore {
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError> {
        let embedding = pgvector::Vector::from(chunk.embedding);
        sqlx::query(
            r#"INSERT INTO retrieval_chunks
                 (id, org_id, corpus, doc_id, chunk_idx, text, embedding, embedding_model, metadata)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
               ON CONFLICT (id) DO NOTHING"#,
        )
        .bind(chunk.id)
        .bind(chunk.org_id)
        .bind(&chunk.corpus)
        .bind(chunk.doc_id)
        .bind(i32::try_from(chunk.chunk_idx).unwrap_or(i32::MAX))
        .bind(&chunk.text)
        .bind(embedding)
        .bind(&chunk.embedding_model)
        .bind(&chunk.metadata)
        .execute(&self.pool)
        .await
        .map_err(store_err)?;
        Ok(())
    }

    async fn search(
        &self,
        org_id: Uuid,
        corpus: &str,
        q: &[f32],
        k: usize,
        embedding_model: &str,
    ) -> Result<Vec<RetrievalResult>, RetrievalError> {
        let query_vec = pgvector::Vector::from(q.to_vec());
        // Raise ef_search for THIS query only (SET LOCAL is transaction-scoped),
        // so the org/corpus-filtered HNSW search keeps high recall.
        let mut tx = self.pool.begin().await.map_err(store_err)?;
        sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", self.ef_search))
            .execute(&mut *tx)
            .await
            .map_err(store_err)?;
        let rows: Vec<(Uuid, Uuid, i32, String, f32)> = sqlx::query_as(
            r#"SELECT id, doc_id, chunk_idx, text,
                      CAST(1.0 - (embedding <=> $3) AS REAL) AS similarity
                 FROM retrieval_chunks
                WHERE org_id = $1 AND corpus = $2 AND embedding_model = $5
                ORDER BY embedding <=> $3
                LIMIT $4"#,
        )
        .bind(org_id)
        .bind(corpus)
        .bind(query_vec)
        .bind(i64::try_from(k).unwrap_or(i64::MAX))
        .bind(embedding_model)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_err)?;
        tx.commit().await.map_err(store_err)?;

        Ok(rows
            .into_iter()
            .map(
                |(chunk_id, doc_id, chunk_idx, text, similarity)| RetrievalResult {
                    chunk_id,
                    doc_id,
                    chunk_idx: chunk_idx.max(0) as u32,
                    text,
                    similarity,
                },
            )
            .collect())
    }

    async fn delete_corpus(&self, org_id: Uuid, corpus: &str) -> Result<u64, RetrievalError> {
        let res = sqlx::query("DELETE FROM retrieval_chunks WHERE org_id = $1 AND corpus = $2")
            .bind(org_id)
            .bind(corpus)
            .execute(&self.pool)
            .await
            .map_err(store_err)?;
        Ok(res.rows_affected())
    }
}
