//! Top-k cosine over a RetrievalStore.

use uuid::Uuid;

use crate::error::RetrievalError;
use crate::store::RetrievalStore;
use crate::types::RetrievalResult;

pub async fn top_k(
    store: &dyn RetrievalStore,
    org_id: Uuid,
    corpus: &str,
    query_embedding: &[f32],
    k: usize,
    min_similarity: f32,
) -> Result<Vec<RetrievalResult>, RetrievalError> {
    let raw = store.search(org_id, corpus, query_embedding, k).await?;
    Ok(raw
        .into_iter()
        .filter(|r| r.similarity >= min_similarity)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use crate::types::Chunk;
    use serde_json::json;

    fn c(org: Uuid, emb: Vec<f32>, text: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(),
            org_id: org,
            corpus: "x".into(),
            doc_id: Uuid::new_v4(),
            chunk_idx: 0,
            text: text.into(),
            embedding: emb,
            metadata: json!({}),
        }
    }

    #[tokio::test]
    async fn min_similarity_filter() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        s.insert(c(o, vec![1.0, 0.0], "hi-sim")).await.unwrap();
        s.insert(c(o, vec![0.0, 1.0], "low-sim")).await.unwrap();
        let r = top_k(&s, o, "x", &[1.0, 0.0], 5, 0.5).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "hi-sim");
    }
}
