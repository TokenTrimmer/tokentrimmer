//! Integration tests for `tt-cache`.
//!
//! Tests 1–7 use [`InMemoryL1Cache`] and run without any external services.
//! Test 8 requires a live Redis instance and is gated behind `#[ignore]`.

use std::env;

use tt_cache::key::cache_key;
use tt_cache::memory::InMemoryL1Cache;
use tt_cache::L1Cache;
use tt_shared::messages::{ChatCompletionRequest, Message, MessageContent};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn base_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "gpt-4o".into(),
        messages: vec![Message::User {
            content: MessageContent::Text("hello world".into()),
            name: None,
        }],
        temperature: Some(0.7),
        top_p: None,
        max_tokens: Some(512),
        stream: false,
        tools: vec![],
        tool_choice: None,
        response_format: None,
        stop: vec![],
        presence_penalty: None,
        frequency_penalty: None,
        n: None,
        seed: None,
        user: None,
        tt_extras: Default::default(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Test 1 — round-trip: set then get returns the same bytes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_set_get_round_trip() {
    let cache = InMemoryL1Cache::new();
    let key = "test:round-trip";
    let value = b"hello, cache!";

    cache.set(key, value, 60).await.expect("set should succeed");
    let result = cache.get(key).await.expect("get should succeed");

    assert_eq!(result, Some(value.to_vec()));
}

// ---------------------------------------------------------------------------
// Test 2 — missing key returns Ok(None)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_missing_key_returns_none() {
    let cache = InMemoryL1Cache::new();
    let result = cache.get("no-such-key").await.expect("get should succeed");
    assert_eq!(result, None);
}

// ---------------------------------------------------------------------------
// Test 3 — delete removes the key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_removes_key() {
    let cache = InMemoryL1Cache::new();
    let key = "test:delete";
    cache
        .set(key, b"to be deleted", 60)
        .await
        .expect("set should succeed");

    // Verify it's there first.
    assert!(cache.get(key).await.unwrap().is_some());

    cache.delete(key).await.expect("delete should succeed");
    let result = cache
        .get(key)
        .await
        .expect("get after delete should succeed");
    assert_eq!(result, None);
}

// ---------------------------------------------------------------------------
// Test 4 — TTL eviction: expired entries return None
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ttl_eviction() {
    let cache = InMemoryL1Cache::new();
    let key = "test:ttl";

    // TTL = 1 second.
    cache
        .set(key, b"temporary", 1)
        .await
        .expect("set should succeed");

    // Should still be present immediately.
    assert!(cache.get(key).await.unwrap().is_some());

    // Sleep slightly longer than the TTL.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let result = cache
        .get(key)
        .await
        .expect("get after expiry should succeed");
    assert_eq!(result, None, "entry should have expired");
}

// ---------------------------------------------------------------------------
// Test 5 — key stability: `stream` does not affect the cache key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_key_stream_field_ignored() {
    let mut req_a = base_request();
    let mut req_b = base_request();
    req_a.stream = false;
    req_b.stream = true;

    assert_eq!(
        cache_key(&req_a),
        cache_key(&req_b),
        "stream field must not affect the cache key"
    );
}

// ---------------------------------------------------------------------------
// Test 6 — key differentiation: different message content → different key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_key_differs_on_message_content() {
    let req_a = base_request();
    let mut req_b = base_request();
    req_b.messages = vec![Message::User {
        content: MessageContent::Text("completely different message".into()),
        name: None,
    }];

    assert_ne!(
        cache_key(&req_a),
        cache_key(&req_b),
        "different message content must produce different keys"
    );
}

// ---------------------------------------------------------------------------
// Test 7 — key stability: `stop` order does not affect the key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_key_stop_order_independent() {
    let mut req_a = base_request();
    let mut req_b = base_request();
    req_a.stop = vec!["alpha".into(), "beta".into(), "gamma".into()];
    req_b.stop = vec!["gamma".into(), "alpha".into(), "beta".into()];

    assert_eq!(
        cache_key(&req_a),
        cache_key(&req_b),
        "stop sequences in different orders must produce the same key"
    );
}

// ---------------------------------------------------------------------------
// Test 8 — Content normalization: trailing whitespace produces the same key
// ---------------------------------------------------------------------------

/// Two requests that differ only in trailing whitespace on a message must
/// produce the SAME cache key (normalization collapses them).
#[tokio::test]
async fn test_key_trailing_whitespace_same_key() {
    let req_clean = base_request(); // "hello world"
    let mut req_trailing = base_request();
    req_trailing.messages = vec![Message::User {
        content: MessageContent::Text("hello world   \n\t".into()),
        name: None,
    }];

    assert_eq!(
        cache_key(&req_clean),
        cache_key(&req_trailing),
        "trailing whitespace must not produce a different cache key"
    );
}

/// A request with semantically-different content (not just whitespace) must
/// still produce a DIFFERENT key — normalization must not over-collapse.
#[tokio::test]
async fn test_key_different_semantic_content_differs() {
    let req_a = base_request(); // "hello world"
    let mut req_b = base_request();
    req_b.messages = vec![Message::User {
        content: MessageContent::Text("goodbye world".into()),
        name: None,
    }];

    assert_ne!(
        cache_key(&req_a),
        cache_key(&req_b),
        "semantically different messages must produce different keys even after normalization"
    );
}

/// Two requests that differ only in NFC vs NFD Unicode form must produce the
/// SAME cache key.  'é' as U+00E9 (NFC, single code point) vs U+0065 U+0301
/// (NFD, base + combining accent) encode the same character.
#[tokio::test]
async fn test_key_nfc_nfd_same_key() {
    // U+00E9 — precomposed é (NFC form)
    let nfc_text = "\u{00E9}";
    // U+0065 U+0301 — e + combining acute accent (NFD form)
    let nfd_text = "\u{0065}\u{0301}";
    assert_ne!(nfc_text, nfd_text, "sanity: raw strings must differ");

    let mut req_nfc = base_request();
    req_nfc.messages = vec![Message::User {
        content: MessageContent::Text(nfc_text.into()),
        name: None,
    }];
    let mut req_nfd = base_request();
    req_nfd.messages = vec![Message::User {
        content: MessageContent::Text(nfd_text.into()),
        name: None,
    }];

    assert_eq!(
        cache_key(&req_nfc),
        cache_key(&req_nfd),
        "NFC and NFD forms of the same character must produce the same cache key"
    );
}

/// Normalization applies only to the KEY derivation — the original request
/// object is not mutated.  Verify the request's message text is unchanged.
#[tokio::test]
async fn test_key_normalization_does_not_mutate_request() {
    let original_text = "hello world   \n";
    let mut req = base_request();
    req.messages = vec![Message::User {
        content: MessageContent::Text(original_text.into()),
        name: None,
    }];

    let _key = cache_key(&req);

    // The request must be unchanged after key derivation.
    match &req.messages[0] {
        Message::User { content, .. } => match content {
            MessageContent::Text(s) => assert_eq!(
                s.as_str(),
                original_text,
                "cache_key must not mutate the request"
            ),
            _ => panic!("expected Text content"),
        },
        _ => panic!("expected User message"),
    }
}

// ---------------------------------------------------------------------------
// Test 9 — Redis round-trip (requires REDIS_URL env var; skipped otherwise)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a live Redis instance (set REDIS_URL env var)"]
async fn test_redis_round_trip() {
    let url = match env::var("REDIS_URL") {
        Ok(u) => u,
        Err(_) => {
            // Skip gracefully when REDIS_URL is not set.
            return;
        }
    };

    let cache = tt_cache::redis_impl::RedisL1Cache::connect(&url, "tt:test")
        .await
        .expect("should connect to Redis");

    let key = "integration:round-trip";
    let value = b"redis cache value";

    cache
        .set(key, value, 30)
        .await
        .expect("Redis set should succeed");

    let result = cache.get(key).await.expect("Redis get should succeed");
    assert_eq!(result, Some(value.to_vec()));

    cache
        .delete(key)
        .await
        .expect("Redis delete should succeed");
    let after_delete = cache
        .get(key)
        .await
        .expect("get after delete should succeed");
    assert_eq!(after_delete, None);
}
