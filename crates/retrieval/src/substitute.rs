//! Orchestrator: take a message body, parse retrievable tags, embed the rest,
//! retrieve top-k above a similarity floor, splice the retrieved chunks into
//! the tag spans.
//!
//! When no chunk clears the similarity floor for a given span the original
//! payload is left intact and the span is counted in
//! `SubstitutionReport::low_confidence_skips`.
//!
//! When the retrieved replacement is LARGER than the original payload for a
//! given span the substitution is skipped (original payload kept) so we never
//! splice in content that increases the token count.
//!
//! `tokens_saved_estimate` is NET: gross token delta (original − replacement,
//! using the tiktoken tokenizer) minus the query-embedding token cost per
//! message. The embedding model is `text-embedding-3-small` (OpenAI, cl100k).

use serde_json::Value;
use tt_tokenize::estimate_tokens;
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

/// Provider id used when counting tokens. The embedding model is OpenAI
/// (`text-embedding-3-small`), which uses cl100k — the same tokenizer used for
/// chat models. Passing "openai" picks the high-confidence tiktoken path.
const EMBEDDING_PROVIDER: &str = "openai";

pub struct SubstitutionReport {
    /// Number of `<retrievable>` spans that were replaced with retrieved chunks.
    pub substitutions: u32,
    /// Spans where every candidate chunk fell below the similarity floor and
    /// the original payload was therefore left intact.
    pub low_confidence_skips: u32,
    /// Spans where the retrieved replacement was *larger* than the original
    /// payload (substituting would increase the token count), so the original
    /// was kept intact.
    pub size_increase_skips: u32,
    /// Gross tokens saved by substitutions (original token count − replacement
    /// token count, summed over substituted spans only, before embedding cost).
    pub gross_tokens_saved: i64,
    /// Tokens consumed by embedding calls (one query embedding per message that
    /// has at least one retrievable tag). Charged against gross savings.
    pub embedding_tokens_cost: i64,
    /// NET token-savings estimate: `gross_tokens_saved − embedding_tokens_cost`.
    /// May be zero or negative if the embedding overhead dominates. A negative
    /// value means RAG cost MORE tokens than it saved for this request.
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
    let mut size_increase_skips = 0u32;
    let mut gross_saved: i64 = 0;
    let mut embedding_cost: i64 = 0;

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

        let query_text = without_tags.trim();
        let fallback_query = if query_text.is_empty() {
            tags.first().map(|t| &text[t.span.0..t.span.1]).unwrap_or("")
        } else {
            query_text
        };

        if fallback_query.trim().is_empty() {
            continue;
        }

        let query_emb = embedder.embed(fallback_query).await?;
        // One embedding request was made for this message — deduct its token
        // cost from the gross savings.
        let query_tokens = estimate_tokens(EMBEDDING_PROVIDER, &without_tags) as i64;
        embedding_cost += query_tokens;

        // Reassemble — replace each tag with retrieved chunks (joined by ---),
        // or leave the original payload when no chunk clears the floor, or when
        // the replacement would be larger than the original.
        let mut new_text = String::new();
        let mut cursor = 0;
        for t in &tags {
            new_text.push_str(&text[cursor..t.span.0]);

            let floor = t.min_similarity.unwrap_or(DEFAULT_MIN_SIMILARITY);
            let hits = top_k(
                store,
                org_id,
                &t.corpus,
                &query_emb,
                t.k as usize,
                floor,
                &embedder.model,
            )
            .await?;

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

                let orig_tokens = estimate_tokens(EMBEDDING_PROVIDER, original_payload) as i64;
                let repl_tokens = estimate_tokens(EMBEDDING_PROVIDER, &replacement) as i64;
                let delta = orig_tokens - repl_tokens;

                if delta <= 0 {
                    // Replacement is the same size or larger — skip to avoid
                    // inflating the prompt.
                    new_text.push_str(original_payload);
                    size_increase_skips += 1;
                } else {
                    gross_saved += delta;
                    new_text.push_str(&replacement);
                    substitutions += 1;
                }
            }
            cursor = t.span.1;
        }
        new_text.push_str(&text[cursor..]);
        *content = Value::String(new_text);
    }

    let net = gross_saved - embedding_cost;
    Ok(SubstitutionReport {
        substitutions,
        low_confidence_skips,
        size_increase_skips,
        gross_tokens_saved: gross_saved,
        embedding_tokens_cost: embedding_cost,
        tokens_saved_estimate: net,
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
            embedding_model: "x".into(),
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

        let original =
            r#"Hello <retrievable corpus="docs" k="1">original payload</retrievable> world"#;
        let mut messages = vec![json!({ "role": "user", "content": original })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        assert_eq!(report.substitutions, 0);
        assert_eq!(report.low_confidence_skips, 1);
        // No gross savings — all spans skipped. Embedding cost was still incurred,
        // so net savings (tokens_saved_estimate) is <= 0.
        assert_eq!(report.gross_tokens_saved, 0);
        assert!(
            report.tokens_saved_estimate <= 0,
            "net savings must be <= 0 when nothing was substituted (embedding cost > 0)"
        );
        // Payload must be intact (the entire original string is preserved).
        let content = messages[0]["content"].as_str().unwrap();
        assert_eq!(
            content, original,
            "content must be unchanged when no chunk clears the floor"
        );
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
        assert!(
            content.contains("Retrieved-A"),
            "retrieved chunk must appear in content"
        );
        assert!(
            !content.contains("raw payload"),
            "original payload must be replaced"
        );
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
        let original =
            r#"Q: <retrievable corpus="docs" k="1" min_similarity="0.8">fallback</retrievable>"#;
        let mut messages = vec![json!({ "role": "user", "content": original })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        assert_eq!(
            report.substitutions, 0,
            "chunk below per-tag floor must not substitute"
        );
        assert_eq!(report.low_confidence_skips, 1);
        let content = messages[0]["content"].as_str().unwrap();
        assert_eq!(
            content, original,
            "content must be unchanged when per-tag floor is not met"
        );
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
        // gross must be positive (original payload was longer than "Short")
        assert!(
            report.gross_tokens_saved > 0,
            "expected positive gross token savings from substituted span"
        );
        // skipped span contributes nothing to gross
    }

    // TDD (a): net savings subtracts embedding cost — a tiny payload nets ~0 or negative.
    #[tokio::test]
    async fn net_savings_subtracts_embedding_cost() {
        let server = MockServer::start_async().await;
        let embedder = mock_embedder(&server, vec![1.0, 0.0]).await;
        let store = MemoryStore::new();
        let org = Uuid::new_v4();

        // The original payload is tiny (a few tokens) and the replacement is
        // also short — gross savings will be small. The query text is
        // substantive enough that embedding_tokens_cost > gross_saved,
        // so net (tokens_saved_estimate) must be <= 0.
        store
            .insert(chunk(org, "docs", vec![1.0, 0.0], "ok"))
            .await
            .unwrap();

        // Payload: "x" (1 token) → replacement: "ok" (1 token) → gross delta = 0.
        // Query is a long sentence → embedding cost > 0.
        // Net must be <= 0.
        let long_query = "This is a fairly long surrounding context sentence to ensure the embedding query has a non-trivial token cost that will exceed any tiny gross savings.";
        let content = format!(r#"{long_query} <retrievable corpus="docs" k="1">x</retrievable>"#);
        let mut messages = vec![json!({ "role": "user", "content": content })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        assert!(
            report.embedding_tokens_cost > 0,
            "embedding cost must be tracked (got {})",
            report.embedding_tokens_cost
        );
        // gross_saved may be 0 since "x" and "ok" are both ~1 token;
        // net must be <= 0 when embedding cost dominates
        assert!(
            report.tokens_saved_estimate <= 0,
            "net savings must be <= 0 when embedding cost dominates (got {})",
            report.tokens_saved_estimate
        );
    }

    // TDD (b): a replacement larger than the payload is SKIPPED, payload unchanged.
    #[tokio::test]
    async fn larger_replacement_is_skipped() {
        let server = MockServer::start_async().await;
        let embedder = mock_embedder(&server, vec![1.0, 0.0]).await;
        let store = MemoryStore::new();
        let org = Uuid::new_v4();

        // The replacement chunk is much larger than the tiny original payload.
        let big_chunk = "This is a very long retrieved chunk that contains many many tokens and is definitely much larger than the tiny original placeholder text.";
        store
            .insert(chunk(org, "docs", vec![1.0, 0.0], big_chunk))
            .await
            .unwrap();

        let original = r#"Q: <retrievable corpus="docs" k="1">tiny</retrievable>"#;
        let mut messages = vec![json!({ "role": "user", "content": original })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        // Substitution must be skipped because replacement > original.
        assert_eq!(
            report.substitutions, 0,
            "larger replacement must not be counted as a substitution"
        );
        assert_eq!(
            report.size_increase_skips, 1,
            "larger replacement must be counted in size_increase_skips"
        );
        assert_eq!(
            report.gross_tokens_saved, 0,
            "no gross savings when replacement is larger"
        );
        // Content must be unchanged (original payload preserved).
        let content = messages[0]["content"].as_str().unwrap();
        assert!(
            content.contains("tiny"),
            "original payload must be preserved when replacement is larger"
        );
        assert!(
            !content.contains(big_chunk),
            "large replacement must not be spliced in"
        );
    }

    // TDD (c): the estimate uses the tokenizer (different from chars/4 heuristic for
    // multi-byte strings with known tokenizer behaviour).
    #[test]
    fn estimate_uses_tokenizer_not_char_div_4() {
        // "café" has 4 Unicode chars → chars/4 heuristic = ceil(4/4) = 1.
        // tiktoken cl100k tokenizes "café" as 2 tokens ("caf" + "é" or similar),
        // so estimate_tokens("openai", "café") returns 2 ≠ 1.
        let text = "café";
        let tokenizer_estimate = tt_tokenize::estimate_tokens(EMBEDDING_PROVIDER, text);
        let char_div_4 = tt_tokenize::char_count_estimate(text);
        // tiktoken must give a different count than the chars/4 heuristic for "café".
        assert_ne!(
            tokenizer_estimate, char_div_4,
            "tiktoken estimate ({tokenizer_estimate}) must differ from chars/4 heuristic ({char_div_4}) for \"café\""
        );
        // Also confirm the tokenizer gives a positive, plausible estimate.
        assert!(
            tokenizer_estimate > 0,
            "tokenizer must return > 0 for non-empty text"
        );
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
                embedding_model: "x".into(),
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

    #[tokio::test]
    async fn cross_model_chunk_is_not_retrieved() {
        let server = MockServer::start_async().await;
        let embedder = mock_embedder(&server, vec![1.0, 0.0]).await; // model "x"
        let store = MemoryStore::new();
        let org = uuid::Uuid::new_v4();
        // Chunk indexed under a DIFFERENT embedding model than the query embedder.
        store
            .insert(Chunk {
                id: uuid::Uuid::new_v4(),
                org_id: org,
                corpus: "docs".into(),
                doc_id: uuid::Uuid::new_v4(),
                chunk_idx: 0,
                text: "would-be-retrieved".into(),
                embedding: vec![1.0, 0.0],
                embedding_model: "other".into(),
                metadata: json!({}),
            })
            .await
            .unwrap();

        let mut messages = vec![json!({
            "role": "user",
            "content": r#"<retrievable corpus="docs">original-payload</retrievable>"#
        })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        // The cross-model chunk is invisible → nothing clears the floor → the
        // original payload is left intact and counted as a low-confidence skip.
        assert_eq!(report.substitutions, 0);
        assert_eq!(report.low_confidence_skips, 1);
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("original-payload"));
    }
}
