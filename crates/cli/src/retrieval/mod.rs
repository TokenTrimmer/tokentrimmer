//! `tt retrieval` — corpus + doc + ad-hoc search.
//!
//! v1 ships against an in-process MemoryStore so devs can prototype
//! locally. Wiring to the cloud HTTP endpoints is a follow-up.

use anyhow::Result;
use uuid::Uuid;

use tt_retrieval::{
    chunking::chunk,
    embed::EmbeddingClient,
    search::top_k,
    store::{memory::MemoryStore, RetrievalStore},
    types::Chunk,
};

pub async fn add_doc(corpus: &str, path: &std::path::Path, openai_key: &str) -> Result<()> {
    let body = std::fs::read_to_string(path)?;
    let chunks = chunk(&body);
    println!("chunked {} into {} window(s)", path.display(), chunks.len());
    let store = MemoryStore::new();
    let embedder = EmbeddingClient::openai(openai_key);
    let org = Uuid::nil();
    let doc_id = Uuid::new_v4();
    for (i, c) in chunks.iter().enumerate() {
        let emb = embedder.embed(&c.text).await?;
        store
            .insert(Chunk {
                id: Uuid::new_v4(),
                org_id: org,
                corpus: corpus.into(),
                doc_id,
                chunk_idx: i as u32,
                text: c.text.clone(),
                embedding: emb,
                embedding_model: embedder.model.clone(),
                metadata: serde_json::json!({}),
            })
            .await?;
    }
    println!("indexed (in-process). NOTE: not persisted; cloud-API wiring is a follow-up.");
    Ok(())
}

pub async fn search(corpus: &str, query: &str, k: usize, openai_key: &str) -> Result<()> {
    let store = MemoryStore::new();
    let embedder = EmbeddingClient::openai(openai_key);
    let q = embedder.embed(query).await?;
    let r = top_k(&store, Uuid::nil(), corpus, &q, k, 0.0, &embedder.model).await?;
    for hit in r {
        println!(
            "{:.3}  {}",
            hit.similarity,
            hit.text.chars().take(120).collect::<String>()
        );
    }
    Ok(())
}
