//! Orchestrator: take a message body, parse retrievable tags, embed the rest,
//! retrieve top-k above a similarity floor, splice the retrieved chunks into
//! the tag spans.
//!
//! When no chunk clears the similarity floor for a given span the original
//! payload is left intact and the span is counted in
//! `SubstitutionReport::low_confidence_skips`.

use serde_json::Value;
use uuid::Uuid;

use crate::embed::EmbeddingClient;
use crate::error::RetrievalError;
use crate::search::top_k;
use crate::store::RetrievalStore;
use crate::tags;

/// Minimum cosine-similarity a chunk must reach to be spliced into the prompt.
/// Chunks below this threshold are treated as irrelevant and the original
/// `<retrievable>` payload is kept unchanged.
pub const DEFAULT_MIN_SIMILARITY: f32 = 0.6;

pub struct SubstitutionReport {
    /// Number of `<retrievable>` spans that were replaced with retrieved chunks.
    pub substitutions: u32,
    /// Spans where every candidate chunk fell below the similarity floor and
    /// the original payload was therefore left intact.
    pub low_confidence_skips: u32,
    /// Rough token-savings estimate (char-delta ÷ 4) across **substituted**
    /// spans only. Skipped spans contribute nothing.
    pub tokens_saved_estimate: i64,
}

pub async fn substitute_in_messages(
    messages: &mut [Value],
    org_id: Uuid,
    store: &dyn RetrievalStore,
    embedder: &EmbeddingClient,
) -> Result<SubstitutionReport, RetrievalError> {
    let mut substitutions = 0u32;
    let mut low_confidence_skips = 0u32;
    let mut saved = 0i64;

    for msg in messages.iter_mut() {
        let Some(content) = msg.get_mut("content") else {
            continue;
        };
        let Some(text) = content.as_str() else {
            continue;
        };
        let text = text.to_string();
        let tags = tags::parse(&text)?;
        if tags.is_empty() {
            continue;
        }

        // Strip all tag spans to form the "embed query".
        let mut without_tags = String::new();
        let mut last = 0;
        for t in &tags {
            without_tags.push_str(&text[last..t.span.0]);
            last = t.span.1;
        }
        without_tags.push_str(&text[last..]);

        let query_emb = embedder.embed(&without_tags).await?;

        // Reassemble — replace each tag with retrieved chunks (joined by ---),
        // or leave the original payload when no chunk clears the floor.
        let mut new_text = String::new();
        let mut cursor = 0;
        for t in &tags {
            new_text.push_str(&text[cursor..t.span.0]);

            let floor = t.min_similarity.unwrap_or(DEFAULT_MIN_SIMILARITY);
            let hits = top_k(store, org_id, &t.corpus, &query_emb, t.k as usize, floor).await?;

            if hits.is_empty() {
                // Nothing cleared the floor — leave original payload intact.
                new_text.push_str(&text[t.span.0..t.span.1]);
                low_confidence_skips += 1;
            } else {
                let original_payload = &text[t.span.0..t.span.1];
                let replacement = hits
                    .iter()
                    .map(|r| r.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n");
                saved += original_payload.len() as i64 - replacement.len() as i64;
                new_text.push_str(&replacement);
                substitutions += 1;
            }
            cursor = t.span.1;
        }
        new_text.push_str(&text[cursor..]);
        *content = Value::String(new_text);
    }
    // Char-delta / 4 as the token-savings heuristic.
    Ok(SubstitutionReport {
        substitutions,
        low_confidence_skips,
        tokens_saved_estimate: saved / 4,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use crate::types::Chunk;
    use httpmock::prelude::*;
    use serde_json::json;

    /// Build an embedding mock that returns the given vector for any POST to
    /// /v1/embeddings, and return an `EmbeddingClient` pointed at it.
    async fn mock_embedder(server: &MockServer, emb: Vec<f64>) -> EmbeddingClient {
        server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/embeddings");
                then.status(200)
                    .json_body(json!({ "data": [{ "embedding": emb }] }));
            })
            .await;
        EmbeddingClient {
            api_key: "k".into(),
            base_url: server.base_url(),
            model: "x".into(),
            http: reqwest::Client::new(),
        }
    }

    fn chunk(org: uuid::Uuid, corpus: &str, emb: Vec<f32>, text: &str) -> Chunk {
        Chunk {
            id: uuid::Uuid::new_v4(),
            org_id: org,
            corpus: corpus.into(),
            doc_id: uuid::Uuid::new_v4(),
            chunk_idx: 0,
            text: text.into(),
            embedding: emb,
            metadata: json!({}),
        }
    }

    // (a) Chunks below the floor are NOT substituted; original payload is kept.
    #[tokio::test]
    async fn low_similarity_leaves_payload_intact() {
        let server = MockServer::start_async().await;
        // Query embedding is [1.0, 0.0]; store has only [0.0, 1.0] (sim ≈ 0.0).
        let embedder = mock_embedder(&server, vec![1.0, 0.0]).await;
        let store = MemoryStore::new();
        let org = Uuid::new_v4();
        store
            .insert(chunk(org, "docs", vec![0.0, 1.0], "IrrelevantChunk"))
            .await
            .unwrap();

        let original = r#"Hello <retrievable corpus="docs" k="1">original payload</retrievable> world"#;
        let mut messages = vec![json!({ "role": "user", "content": original })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        assert_eq!(report.substitutions, 0);
        assert_eq!(report.low_confidence_skips, 1);
        assert_eq!(report.tokens_saved_estimate, 0);
        // Payload must be intact (the entire original string is preserved).
        let content = messages[0]["content"].as_str().unwrap();
        assert_eq!(content, original, "content must be unchanged when no chunk clears the floor");
    }

    // (b) Chunks at/above the floor ARE substituted.
    #[tokio::test]
    async fn high_similarity_substitutes_payload() {
        let server = MockServer::start_async().await;
        // Query and chunk both [1.0, 0.0] → cosine sim = 1.0 ≥ 0.6.
        let embedder = mock_embedder(&server, vec![1.0, 0.0]).await;
        let store = MemoryStore::new();
        let org = Uuid::new_v4();
        store
            .insert(chunk(org, "docs", vec![1.0, 0.0], "Retrieved-A"))
            .await
            .unwrap();

        let mut messages = vec![json!({
            "role": "user",
            "content": r#"Summarize <retrievable corpus="docs" k="1">raw payload</retrievable> please."#
        })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        assert_eq!(report.substitutions, 1);
        assert_eq!(report.low_confidence_skips, 0);
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("Retrieved-A"), "retrieved chunk must appear in content");
        assert!(!content.contains("raw payload"), "original payload must be replaced");
    }

    // (c) Per-tag min_similarity override is honored over the default.
    #[tokio::test]
    async fn per_tag_min_similarity_override() {
        let server = MockServer::start_async().await;
        // Query [1.0, 0.0], chunk [0.7, 0.7] → sim ≈ 0.71.
        // Default floor is 0.6 (would pass), but the tag sets floor=0.8 (should fail).
        let embedder = mock_embedder(&server, vec![1.0, 0.0]).await;
        let store = MemoryStore::new();
        let org = Uuid::new_v4();
        // Normalise: [0.7, 0.7] / |[0.7, 0.7]| ≈ [0.707, 0.707]
        // cos([1,0],[0.707,0.707]) ≈ 0.707 — above 0.6 but below 0.8.
        let norm = 2f32.sqrt() / 2.0;
        store
            .insert(chunk(org, "docs", vec![norm, norm], "MidChunk"))
            .await
            .unwrap();

        // Tag asks for floor=0.8, so this chunk (sim≈0.707) should be skipped.
        let original = r#"Q: <retrievable corpus="docs" k="1" min_similarity="0.8">fallback</retrievable>"#;
        let mut messages = vec![json!({ "role": "user", "content": original })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        assert_eq!(report.substitutions, 0, "chunk below per-tag floor must not substitute");
        assert_eq!(report.low_confidence_skips, 1);
        let content = messages[0]["content"].as_str().unwrap();
        assert_eq!(content, original, "content must be unchanged when per-tag floor is not met");
    }

    // (d) tokens_saved_estimate reflects only actually-substituted spans.
    #[tokio::test]
    async fn tokens_saved_only_for_substituted_spans() {
        let server = MockServer::start_async().await;
        // First call returns [1.0, 0.0] (used for the embedding query).
        // The server mock responds the same for both messages, which is fine.
        let embedder = mock_embedder(&server, vec![1.0, 0.0]).await;
        let store = MemoryStore::new();
        let org = Uuid::new_v4();

        // Corpus "good" has a high-sim chunk.
        store
            .insert(chunk(org, "good", vec![1.0, 0.0], "Short"))
            .await
            .unwrap();
        // Corpus "bad" has only a low-sim chunk.
        store
            .insert(chunk(org, "bad", vec![0.0, 1.0], "IrrelevantChunk"))
            .await
            .unwrap();

        // Two tags: first substitutes (good), second skips (bad).
        let mut messages = vec![json!({
            "role": "user",
            "content": concat!(
                r#"A <retrievable corpus="good" k="1">a very long original payload text here</retrievable>"#,
                r#" and B <retrievable corpus="bad" k="1">another long payload that must stay</retrievable>."#
            )
        })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        assert_eq!(report.substitutions, 1);
        assert_eq!(report.low_confidence_skips, 1);
        // tokens_saved must be non-zero (original payload was longer than "Short")
        // but must not account for the skipped span.
        assert!(report.tokens_saved_estimate > 0, "expected positive token savings from substituted span");
    }

    // Regression: original substitution_replaces_payload_with_top_k_chunks still passes.
    #[tokio::test]
    async fn substitution_replaces_payload_with_top_k_chunks() {
        let emb_server = MockServer::start_async().await;
        let embedder = mock_embedder(&emb_server, vec![1.0, 0.0]).await;
        let store = MemoryStore::new();
        let org = Uuid::new_v4();
        store
            .insert(Chunk {
                id: Uuid::new_v4(),
                org_id: org,
                corpus: "docs".into(),
                doc_id: Uuid::new_v4(),
                chunk_idx: 0,
                text: "Retrieved-A".into(),
                embedding: vec![1.0, 0.0],
                metadata: json!({}),
            })
            .await
            .unwrap();

        let mut messages = vec![json!({
            "role": "user",
            "content": "Summarize <retrievable corpus=\"docs\" k=\"1\">raw payload that the LLM never sees</retrievable> for the team."
        })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();
        assert_eq!(report.substitutions, 1);
        let new_content = messages[0]["content"].as_str().unwrap();
        assert!(new_content.contains("Retrieved-A"));
        assert!(!new_content.contains("raw payload"));
    }
}
