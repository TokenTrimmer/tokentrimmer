//! Integration tests for the L2 semantic cache.
//!
//! Tests 1–8 use [`InMemoryL2Cache`] and [`MockEmbedder`] — no external services
//! required.  Test 9 requires a live Postgres instance with the `vector`
//! extension and is gated behind `#[ignore]`.

use std::env;

use chrono::{Duration, Utc};
use tt_cache::{
    embed::{EmbeddingProvider, MockEmbedder},
    l2::{CacheEntry, InMemoryL2Cache, L2Cache, PostgresL2Cache},
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Default similarity threshold used in most tests (matches ADR-008 default).
const THRESHOLD: f32 = 0.92;

fn mock_embedder(vec: Vec<f32>) -> MockEmbedder {
    MockEmbedder {
        fixed_vec: vec,
        model: "mock-v1".to_string(),
    }
}

/// Build a normalized unit vector pointing in the `index`-th direction of a
/// 3-dimensional space.  Used to construct orthogonal test vectors easily.
fn unit_vec(index: usize, dims: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; dims];
    v[index] = 1.0;
    v
}

fn make_entry(org_id: Uuid, embedding: Vec<f32>) -> CacheEntry {
    CacheEntry {
        id: Uuid::new_v4(),
        org_id,
        embedding,
        response: b"{}".to_vec(),
        model: "gpt-4o".to_string(),
        embedding_model: "mock-v1".to_string(),
        input_tokens: 10,
        output_tokens: 5,
        hit_count: 0,
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::seconds(3600),
    }
}

fn make_expired_entry(org_id: Uuid, embedding: Vec<f32>) -> CacheEntry {
    CacheEntry {
        id: Uuid::new_v4(),
        org_id,
        embedding,
        response: b"{}".to_vec(),
        model: "gpt-4o".to_string(),
        embedding_model: "mock-v1".to_string(),
        input_tokens: 10,
        output_tokens: 5,
        hit_count: 0,
        created_at: Utc::now() - Duration::seconds(7200),
        expires_at: Utc::now() - Duration::seconds(1),
    }
}

// ---------------------------------------------------------------------------
// Test 1 — insert + lookup with identical embedding returns sim ≈ 1.0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn l2_identical_embedding_returns_entry() {
    let cache = InMemoryL2Cache::new();
    let org_id = Uuid::new_v4();
    let vec = unit_vec(0, 4);

    let entry = make_entry(org_id, vec.clone());
    let entry_id = entry.id;

    cache.insert(entry).await.expect("insert should succeed");

    let result = cache
        .lookup(org_id, &vec, THRESHOLD, "gpt-4o", "mock-v1")
        .await
        .expect("lookup should succeed");

    let (found, sim) = result.expect("should find the entry");
    assert_eq!(found.id, entry_id);
    assert!(
        (sim - 1.0).abs() < 1e-5,
        "identical vectors → similarity ≈ 1.0, got {sim}"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — orthogonal embedding (sim ≈ 0) returns None
// ---------------------------------------------------------------------------

#[tokio::test]
async fn l2_orthogonal_embedding_returns_none() {
    let cache = InMemoryL2Cache::new();
    let org_id = Uuid::new_v4();

    // Insert entry with e0 direction.
    cache
        .insert(make_entry(org_id, unit_vec(0, 3)))
        .await
        .expect("insert should succeed");

    // Query with e1 — orthogonal to e0, cosine = 0 < any positive threshold.
    let result = cache
        .lookup(org_id, &unit_vec(1, 3), THRESHOLD, "gpt-4o", "mock-v1")
        .await
        .expect("lookup should succeed");

    assert!(
        result.is_none(),
        "orthogonal vector should not match (sim ≈ 0)"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — per-org isolation: org A entries are invisible to org B
// ---------------------------------------------------------------------------

#[tokio::test]
async fn l2_org_isolation() {
    let cache = InMemoryL2Cache::new();
    let org_a = Uuid::new_v4();
    let org_b = Uuid::new_v4();

    let vec = unit_vec(0, 3);

    // Insert entry for org A only.
    cache
        .insert(make_entry(org_a, vec.clone()))
        .await
        .expect("insert should succeed");

    // Querying as org B should return nothing.
    let result = cache
        .lookup(org_b, &vec, THRESHOLD, "gpt-4o", "mock-v1")
        .await
        .expect("lookup should succeed");

    assert!(
        result.is_none(),
        "org B should not see org A's cache entries"
    );

    // Querying as org A should find it.
    let result = cache
        .lookup(org_a, &vec, THRESHOLD, "gpt-4o", "mock-v1")
        .await
        .expect("lookup should succeed");

    assert!(result.is_some(), "org A should see its own entry");
}

// ---------------------------------------------------------------------------
// Test 4 — threshold respected: near-miss (sim < threshold) returns None
// ---------------------------------------------------------------------------

#[tokio::test]
async fn l2_threshold_near_miss() {
    let cache = InMemoryL2Cache::new();
    let org_id = Uuid::new_v4();

    // Insert unit vector along e0.
    cache
        .insert(make_entry(org_id, unit_vec(0, 2)))
        .await
        .expect("insert should succeed");

    // Construct a query vector with sim ≈ 0.95 < threshold 0.99.
    // v = (cos θ, sin θ) for θ such that cos θ ≈ 0.95.
    // cos(18.2°) ≈ 0.95 → sin(18.2°) ≈ 0.31.
    let query = vec![0.95_f32, 0.31];

    let result = cache
        .lookup(org_id, &query, 0.99, "gpt-4o", "mock-v1")
        .await
        .expect("lookup should succeed");

    assert!(
        result.is_none(),
        "sim ≈ 0.95 should not meet threshold 0.99"
    );

    // But with a lower threshold it should match.
    let result = cache
        .lookup(org_id, &query, 0.90, "gpt-4o", "mock-v1")
        .await
        .expect("lookup should succeed");

    assert!(result.is_some(), "sim ≈ 0.95 should meet threshold 0.90");
}

// ---------------------------------------------------------------------------
// Test 5 — expired entries return None
// ---------------------------------------------------------------------------

#[tokio::test]
async fn l2_expired_entry_not_returned() {
    let cache = InMemoryL2Cache::new();
    let org_id = Uuid::new_v4();
    let vec = unit_vec(0, 3);

    // Insert an already-expired entry.
    cache
        .insert(make_expired_entry(org_id, vec.clone()))
        .await
        .expect("insert should succeed");

    let result = cache
        .lookup(org_id, &vec, 0.0, "gpt-4o", "mock-v1") // threshold 0 — only expiry blocks it
        .await
        .expect("lookup should succeed");

    assert!(
        result.is_none(),
        "expired entry should not be returned by lookup"
    );
}

// ---------------------------------------------------------------------------
// Test 6 — bump_hit_count increments the counter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn l2_bump_hit_count_increments() {
    let cache = InMemoryL2Cache::new();
    let org_id = Uuid::new_v4();
    let vec = unit_vec(0, 3);

    let entry = make_entry(org_id, vec.clone());
    let entry_id = entry.id;

    cache.insert(entry).await.expect("insert should succeed");

    // Initial hit_count should be 0.
    let (found, _) = cache
        .lookup(org_id, &vec, 0.0, "gpt-4o", "mock-v1")
        .await
        .expect("lookup ok")
        .expect("entry present");
    assert_eq!(found.hit_count, 0);

    // Bump once.
    cache
        .bump_hit_count(entry_id)
        .await
        .expect("bump should succeed");

    // Re-fetch and verify.
    let (found, _) = cache
        .lookup(org_id, &vec, 0.0, "gpt-4o", "mock-v1")
        .await
        .expect("lookup ok")
        .expect("entry still present");
    assert_eq!(found.hit_count, 1, "hit_count should be 1 after one bump");

    // Bump again.
    cache
        .bump_hit_count(entry_id)
        .await
        .expect("bump should succeed");

    let (found, _) = cache
        .lookup(org_id, &vec, 0.0, "gpt-4o", "mock-v1")
        .await
        .expect("lookup ok")
        .expect("entry still present");
    assert_eq!(found.hit_count, 2, "hit_count should be 2 after two bumps");
}

// ---------------------------------------------------------------------------
// Test 7 — highest-similarity match returned when multiple candidates exist
// ---------------------------------------------------------------------------

#[tokio::test]
async fn l2_returns_highest_similarity_match() {
    let cache = InMemoryL2Cache::new();
    let org_id = Uuid::new_v4();

    // Insert two entries: one pointing [1,0,0] and one pointing [1,1,0]/sqrt(2)
    // (≈ 45° from [1,0,0]).
    let entry_exact = make_entry(org_id, unit_vec(0, 3));
    let entry_near = make_entry(
        org_id,
        vec![(1.0_f32 / 2.0_f32.sqrt()), (1.0_f32 / 2.0_f32.sqrt()), 0.0],
    );

    let exact_id = entry_exact.id;
    cache.insert(entry_exact).await.expect("insert exact ok");
    cache.insert(entry_near).await.expect("insert near ok");

    // Query with [1,0,0] — should return the exact-match entry (sim = 1.0).
    let result = cache
        .lookup(org_id, &unit_vec(0, 3), 0.5, "gpt-4o", "mock-v1")
        .await
        .expect("lookup ok")
        .expect("should find a match");

    assert_eq!(
        result.0.id, exact_id,
        "should return the highest-similarity (exact) match"
    );
    assert!(
        (result.1 - 1.0).abs() < 1e-5,
        "similarity for exact match ≈ 1.0, got {}",
        result.1
    );
}

// ---------------------------------------------------------------------------
// Test 8 — MockEmbedder returns fixed_vec regardless of input
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_embedder_fixed_vec() {
    let fixed = vec![0.1_f32, 0.2, 0.7];
    let embedder = mock_embedder(fixed.clone());

    // Call embed() with different inputs — all should return fixed_vec.
    for input in [
        "hello",
        "world",
        "this is a much longer sentence with many words",
    ] {
        let result = embedder.embed(input).await.expect("embed should succeed");
        assert_eq!(
            result, fixed,
            "MockEmbedder must return fixed_vec for input '{input}'"
        );
    }

    assert_eq!(embedder.model(), "mock-v1");
}

// ---------------------------------------------------------------------------
// Test 9a — model filter: mismatched chat model returns None  (fix §2.1)
// ---------------------------------------------------------------------------

/// A cache entry stored under `gpt-4o` must NOT be returned when the caller
/// requests a `gpt-4o-mini` lookup, even if the embedding is identical.
#[tokio::test]
async fn l2_different_chat_model_is_not_returned() {
    let cache = InMemoryL2Cache::new();
    let org_id = Uuid::new_v4();
    let vec = unit_vec(0, 3);

    // Store an entry produced by gpt-4o.
    cache
        .insert(make_entry(org_id, vec.clone()))
        .await
        .expect("insert should succeed");

    // Lookup requesting gpt-4o-mini — should NOT find the gpt-4o entry.
    let result = cache
        .lookup(org_id, &vec, 0.0, "gpt-4o-mini", "mock-v1")
        .await
        .expect("lookup should succeed");
    assert!(
        result.is_none(),
        "a gpt-4o-mini lookup must not return a gpt-4o cached response"
    );

    // Lookup requesting gpt-4o — should find the entry.
    let result = cache
        .lookup(org_id, &vec, 0.0, "gpt-4o", "mock-v1")
        .await
        .expect("lookup should succeed");
    assert!(
        result.is_some(),
        "a gpt-4o lookup must find the gpt-4o cached response"
    );
}

// ---------------------------------------------------------------------------
// Test 9b — embedding-model filter: different embedding model returns None  (fix §2.5)
// ---------------------------------------------------------------------------

/// A cache entry stored under `mock-v1` embeddings must NOT be returned when
/// the caller queries with `mock-v2` — the vectors live in different spaces.
#[tokio::test]
async fn l2_different_embedding_model_is_not_returned() {
    let cache = InMemoryL2Cache::new();
    let org_id = Uuid::new_v4();
    let vec = unit_vec(0, 3);

    // Store an entry whose embedding was produced by mock-v1.
    let entry = CacheEntry {
        id: Uuid::new_v4(),
        org_id,
        embedding: vec.clone(),
        response: b"{}".to_vec(),
        model: "gpt-4o".to_string(),
        embedding_model: "mock-v1".to_string(),
        input_tokens: 1,
        output_tokens: 1,
        hit_count: 0,
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::seconds(3600),
    };
    cache.insert(entry).await.expect("insert should succeed");

    // Lookup with mock-v2 — wrong embedding space, should miss.
    let result = cache
        .lookup(org_id, &vec, 0.0, "gpt-4o", "mock-v2")
        .await
        .expect("lookup should succeed");
    assert!(
        result.is_none(),
        "a mock-v2 lookup must not return a mock-v1 cached entry"
    );

    // Lookup with mock-v1 — correct embedding space, should hit.
    let result = cache
        .lookup(org_id, &vec, 0.0, "gpt-4o", "mock-v1")
        .await
        .expect("lookup should succeed");
    assert!(
        result.is_some(),
        "a mock-v1 lookup must find the mock-v1 cached entry"
    );
}

// ---------------------------------------------------------------------------
// Test 9c — context key: same last message, different system prompt → different
//            embedding input, so no collision  (fix §2.3)
// ---------------------------------------------------------------------------

/// Two requests that differ only in their system prompt must NOT produce the
/// same embedding-input string. We verify this by checking that
/// `l2_context_text` (via `tt_cache::l2_context_text`) builds distinct strings
/// for the two requests, so the embedding call would receive different inputs
/// and the cosine-similarity lookup would not collapse them.
///
/// We test the helper function rather than the full round-trip because the
/// MockEmbedder ignores text (returning a fixed vector), and we don't want to
/// spin up a real embedding server in a unit test.
#[test]
fn l2_context_text_differs_for_different_system_prompts() {
    use tt_cache::l2_context_text;
    use tt_shared::{ChatCompletionRequest, Message, MessageContent};

    fn req_with_system(system: &str, user: &str) -> ChatCompletionRequest {
        use std::collections::HashMap;
        ChatCompletionRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                Message::System {
                    content: MessageContent::Text(system.to_string()),
                },
                Message::User {
                    content: MessageContent::Text(user.to_string()),
                    name: None,
                },
            ],
            stream: false,
            max_tokens: None,
            temperature: None,
            top_p: None,
            n: None,
            stop: vec![],
            presence_penalty: None,
            frequency_penalty: None,
            user: None,
            seed: None,
            tools: vec![],
            tool_choice: None,
            response_format: None,
            tt_extras: HashMap::new(),
        }
    }

    let req_a = req_with_system("You are a helpful assistant.", "yes");
    let req_b = req_with_system("You are a pirate. Answer in pirate speak.", "yes");

    let text_a = l2_context_text(&req_a);
    let text_b = l2_context_text(&req_b);

    assert!(
        text_a.is_some() && text_b.is_some(),
        "both requests should produce a context text"
    );
    assert_ne!(
        text_a, text_b,
        "requests with different system prompts must produce different context strings even when the last user message is identical ('yes')"
    );
}

// ---------------------------------------------------------------------------
// Test 9 — PostgresL2Cache round-trip (requires DATABASE_URL; ignored by default)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a live Postgres instance with pgvector (set DATABASE_URL env var)"]
async fn postgres_l2_round_trip() {
    let db_url = match env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(_) => return, // skip cleanly when env unset
    };

    let pool = sqlx::PgPool::connect(&db_url)
        .await
        .expect("should connect to Postgres");

    let cache = PostgresL2Cache::new(pool);
    let org_id = Uuid::new_v4();
    // 1536-dim unit vector (matches text-embedding-3-small dimensionality).
    let mut embedding = vec![0.0_f32; 1536];
    embedding[0] = 1.0;

    let entry = CacheEntry {
        id: Uuid::new_v4(),
        org_id,
        embedding: embedding.clone(),
        response: serde_json::to_vec(&serde_json::json!({"object": "chat.completion"}))
            .expect("serialize ok"),
        model: "gpt-4o".to_string(),
        embedding_model: "text-embedding-3-small".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        hit_count: 0,
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::seconds(3600),
    };
    let entry_id = entry.id;

    cache
        .insert(entry)
        .await
        .expect("Postgres insert should succeed");

    let result = cache
        .lookup(org_id, &embedding, 0.99, "gpt-4o", "text-embedding-3-small")
        .await
        .expect("Postgres lookup should succeed");

    let (found, sim) = result.expect("should find inserted entry");
    assert_eq!(found.id, entry_id);
    assert!(
        (sim - 1.0).abs() < 1e-4,
        "identical 1536-dim vectors → sim ≈ 1.0, got {sim}"
    );

    cache
        .bump_hit_count(entry_id)
        .await
        .expect("bump_hit_count should succeed");

    // Clean up the test row.
    sqlx::query("DELETE FROM cache_entries WHERE id = $1")
        .bind(entry_id)
        .execute(&sqlx::PgPool::connect(&db_url).await.unwrap())
        .await
        .ok();
}
