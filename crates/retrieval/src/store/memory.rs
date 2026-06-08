//! In-process Vec-backed store for tests + local dev.

use std::sync::Mutex;
use uuid::Uuid;

use crate::error::RetrievalError;
use crate::store::RetrievalStore;
use crate::types::{Chunk, RetrievalResult};

pub struct MemoryStore {
    chunks: Mutex<Vec<Chunk>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            chunks: Mutex::new(Vec::new()),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[async_trait::async_trait]
impl RetrievalStore for MemoryStore {
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError> {
        if !crate::embedding_is_finite(&chunk.embedding) {
            return Err(RetrievalError::InvalidEmbedding);
        }
        self.chunks.lock().unwrap().push(chunk);
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
        let snap: Vec<_> = self.chunks.lock().unwrap().clone();
        let mut scored: Vec<(f32, &Chunk)> = snap
            .iter()
            .filter(|c| {
                c.org_id == org_id && c.corpus == corpus && c.embedding_model == embedding_model
            })
            .map(|c| (cosine(q, &c.embedding), c))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let out = scored
            .into_iter()
            .take(k)
            .map(|(s, c)| RetrievalResult {
                chunk_id: c.id,
                doc_id: c.doc_id,
                chunk_idx: c.chunk_idx,
                text: c.text.clone(),
                similarity: s,
            })
            .collect();
        Ok(out)
    }

    async fn delete_corpus(&self, org_id: Uuid, corpus: &str) -> Result<u64, RetrievalError> {
        let mut g = self.chunks.lock().unwrap();
        let before = g.len();
        g.retain(|c| !(c.org_id == org_id && c.corpus == corpus));
        Ok((before - g.len()) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(org: Uuid, corpus: &str, emb: Vec<f32>, text: &str, model: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(),
            org_id: org,
            corpus: corpus.into(),
            doc_id: Uuid::new_v4(),
            chunk_idx: 0,
            text: text.into(),
            embedding: emb,
            embedding_model: model.into(),
            metadata: json!({}),
        }
    }

    #[tokio::test]
    async fn search_returns_highest_similarity_first() {
        let s = MemoryStore::new();
        let org = Uuid::new_v4();
        s.insert(chunk(org, "x", vec![1.0, 0.0], "first", "m"))
            .await
            .unwrap();
        s.insert(chunk(org, "x", vec![0.0, 1.0], "second", "m"))
            .await
            .unwrap();
        s.insert(chunk(org, "x", vec![0.9, 0.1], "third", "m"))
            .await
            .unwrap();
        let r = s.search(org, "x", &[1.0, 0.0], 2, "m").await.unwrap();
        assert_eq!(r[0].text, "first");
        assert_eq!(r[1].text, "third");
    }

    #[tokio::test]
    async fn search_isolates_by_org_and_corpus() {
        let s = MemoryStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        s.insert(chunk(a, "x", vec![1.0], "a-x", "m"))
            .await
            .unwrap();
        s.insert(chunk(b, "x", vec![1.0], "b-x", "m"))
            .await
            .unwrap();
        s.insert(chunk(a, "y", vec![1.0], "a-y", "m"))
            .await
            .unwrap();
        let r = s.search(a, "x", &[1.0], 10, "m").await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "a-x");
    }

    #[tokio::test]
    async fn delete_corpus_returns_removed_count() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        s.insert(chunk(o, "x", vec![1.0], "1", "m")).await.unwrap();
        s.insert(chunk(o, "x", vec![1.0], "2", "m")).await.unwrap();
        s.insert(chunk(o, "y", vec![1.0], "y", "m")).await.unwrap();
        let removed = s.delete_corpus(o, "x").await.unwrap();
        assert_eq!(removed, 2);
    }

    #[tokio::test]
    async fn search_partitions_by_embedding_model() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        s.insert(chunk(o, "x", vec![1.0, 0.0], "from-a", "m-a"))
            .await
            .unwrap();
        s.insert(chunk(o, "x", vec![1.0, 0.0], "from-b", "m-b"))
            .await
            .unwrap();
        let r = s.search(o, "x", &[1.0, 0.0], 10, "m-a").await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "from-a");
    }

    #[tokio::test]
    async fn insert_rejects_non_finite_embedding() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        let err = s
            .insert(chunk(o, "x", vec![f32::NAN], "bad", "m"))
            .await
            .unwrap_err();
        assert!(matches!(err, RetrievalError::InvalidEmbedding));
        // A finite embedding still inserts.
        s.insert(chunk(o, "x", vec![1.0], "ok", "m")).await.unwrap();
    }
}
