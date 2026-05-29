//! Orchestrator: take a message body, parse retrievable tags, embed the rest,
//! retrieve top-k, splice the retrieved chunks into the tag spans.

use serde_json::Value;
use uuid::Uuid;

use crate::embed::EmbeddingClient;
use crate::error::RetrievalError;
use crate::store::RetrievalStore;
use crate::tags;

pub struct SubstitutionReport {
    pub substitutions: u32,
    pub tokens_saved_estimate: i64,
}

pub async fn substitute_in_messages(
    messages: &mut [Value],
    org_id: Uuid,
    store: &dyn RetrievalStore,
    embedder: &EmbeddingClient,
) -> Result<SubstitutionReport, RetrievalError> {
    let mut substitutions = 0u32;
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

        // Strip all tag spans to form the "embed query"
        let mut without_tags = String::new();
        let mut last = 0;
        for t in &tags {
            without_tags.push_str(&text[last..t.span.0]);
            last = t.span.1;
        }
        without_tags.push_str(&text[last..]);

        let query_emb = embedder.embed(&without_tags).await?;

        // Reassemble — replace each tag with retrieved chunks (joined by ---).
        let mut new_text = String::new();
        let mut cursor = 0;
        for t in &tags {
            new_text.push_str(&text[cursor..t.span.0]);
            let hits = store
                .search(org_id, &t.corpus, &query_emb, t.k as usize)
                .await?;
            let original_payload = &text[t.span.0..t.span.1];
            let replacement = hits
                .iter()
                .map(|r| r.text.clone())
                .collect::<Vec<_>>()
                .join("\n\n---\n\n");
            saved += original_payload.len() as i64 - replacement.len() as i64;
            new_text.push_str(&replacement);
            substitutions += 1;
            cursor = t.span.1;
        }
        new_text.push_str(&text[cursor..]);
        *content = Value::String(new_text);
    }
    // Char-delta / 4 as the token-savings heuristic.
    Ok(SubstitutionReport {
        substitutions,
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

    #[tokio::test]
    async fn substitution_replaces_payload_with_top_k_chunks() {
        // Embedding mock: any /v1/embeddings returns vec![1.0]
        let emb_server = MockServer::start_async().await;
        let _m = emb_server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/embeddings");
                then.status(200)
                    .json_body(json!({ "data": [{ "embedding": [1.0, 0.0] }] }));
            })
            .await;
        let embedder = EmbeddingClient {
            api_key: "k".into(),
            base_url: emb_server.base_url(),
            model: "x".into(),
            http: reqwest::Client::new(),
        };
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
