//! S11: strict hosted read policy, bounded envelope recognition and actual
//! cache-store behavior. All keys/content/services are disposable fixtures.
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use tt_cache::memory::InMemoryL1Cache;
use tt_cache::{
    CacheEntry, InMemoryL2Cache, L1Cache, L1Open, L2Cache, L2Open, PostgresL2Cache, ResponseCodec,
};
use uuid::Uuid;

const KEY: [u8; 32] = [3; 32];
const PAYLOAD: &[u8] = br#"{"choices":[{"message":{"content":"private-cache-fixture"}}]}"#;

// Each environment configuration runs in its own process, never by mutating
// the process-global environment of parallel Rust tests.
fn configured_child(name: &str) -> bool {
    if std::env::var("TT_CACHE_POLICY_TEST_CHILD").as_deref() == Ok(name) {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            name,
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("TT_CACHE_POLICY_TEST_CHILD", name)
        .env("TT_MASTER_KEY", hex::encode(KEY))
        .env("TT_REQUIRE_ENCRYPTED_CACHE", "true")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}
fn strict_from_env() -> ResponseCodec {
    ResponseCodec::from_env().unwrap().unwrap()
}
fn entry(org: Uuid) -> CacheEntry {
    let mut vector = vec![0.0; 1536];
    vector[0] = 1.0;
    CacheEntry {
        id: Uuid::new_v4(),
        org_id: org,
        embedding: vector,
        response: PAYLOAD.to_vec(),
        model: "fixture-model".into(),
        embedding_model: "fixture-embedding".into(),
        input_tokens: 10,
        output_tokens: 5,
        baseline_cost_usd: Some(0.01),
        request_delta_evidence_state: Default::default(),
        hit_count: 0,
        quality_score: None,
        judge_verdict: None,
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::hours(1),
        lexical_sig: None,
    }
}

#[test]
fn marked_l2_envelopes_must_be_complete_and_versioned_not_legacy_plaintext() {
    let codec = ResponseCodec::new(KEY);
    let org = Uuid::new_v4();
    let id = Uuid::new_v4();
    let valid = codec.seal_response_json(org, id, PAYLOAD).unwrap();
    let mut cases = vec![
        json!({"__tt_cache_enc__":1}),
        json!({"__tt_cache_enc__":1,"blob":null}),
        json!({"__tt_cache_enc__":1,"blob":123}),
        json!({"__tt_cache_enc__":1,"blob":"not-hex"}),
    ];
    for version in [
        json!(2),
        json!(0),
        json!("1"),
        json!(null),
        json!(true),
        json!(1.0),
    ] {
        let mut malformed = valid.clone();
        malformed["__tt_cache_enc__"] = version;
        cases.push(malformed);
    }
    let mut extra = valid.clone();
    extra["plaintext"] = json!("unexpected");
    cases.push(extra);
    for value in cases {
        assert!(ResponseCodec::is_encrypted_json(&value));
        assert_eq!(
            codec.open_response_json(org, id, &value),
            L2Open::Undecryptable,
            "{value}"
        );
    }
    assert_eq!(
        codec.open_response_json(org, id, &valid),
        L2Open::Decrypted(PAYLOAD.to_vec())
    );
    assert_eq!(
        codec.open_response_json(org, id, &json!({"choices":[]})),
        L2Open::Plaintext
    );
}

#[test]
fn unknown_l1_envelope_versions_are_not_legacy_values() {
    let codec = ResponseCodec::new(KEY);
    for bytes in [
        b"\0tt-l1-enc:v2\0fixture".as_slice(),
        b"\0tt-l1-enc:v1".as_slice(),
    ] {
        assert_eq!(
            codec.open_l1_value(Uuid::nil(), "key", bytes),
            L1Open::Undecryptable
        );
        assert!(ResponseCodec::is_encrypted_l1(bytes));
    }
}

#[test]
fn environment_policy_is_explicit_and_preserves_self_hosted_compatibility() {
    const PROBE: &str = "TT_CACHE_ENV_MATRIX";
    if let Ok(mode) = std::env::var(PROBE) {
        let configured = ResponseCodec::from_env();
        if mode == "missing" {
            assert!(configured.unwrap().is_none());
            return;
        }
        if mode == "invalid" {
            assert!(configured.is_err());
            return;
        }
        let codec = configured.unwrap().unwrap();
        let org = Uuid::new_v4();
        let id = Uuid::new_v4();
        let legacy = json!({"choices":[]});
        if mode == "strict" {
            assert_eq!(
                codec.open_response_json(org, id, &legacy),
                L2Open::Undecryptable
            );
            assert_eq!(
                codec.open_l1_value(org, "key", PAYLOAD),
                L1Open::Undecryptable
            );
        } else {
            assert_eq!(
                codec.open_response_json(org, id, &legacy),
                L2Open::Plaintext
            );
            assert_eq!(codec.open_l1_value(org, "key", PAYLOAD), L1Open::Plaintext);
        }
        let encrypted = codec.seal_response_json(org, id, PAYLOAD).unwrap();
        assert_eq!(
            codec.open_response_json(org, id, &encrypted),
            L2Open::Decrypted(PAYLOAD.to_vec())
        );
        assert_eq!(
            ResponseCodec::new(KEY).open_response_json(org, id, &legacy),
            L2Open::Plaintext
        );
        assert!(!format!("{codec:?}").contains(&hex::encode(KEY)));
        return;
    }
    let mut failures = Vec::new();
    for (flag, key, mode) in [
        (Some("true"), Some(hex::encode(KEY)), "strict"),
        (Some("1"), Some(hex::encode(KEY)), "strict"),
        (Some("TRUE"), Some(hex::encode(KEY)), "strict"),
        (Some("false"), Some(hex::encode(KEY)), "legacy"),
        (None, Some(hex::encode(KEY)), "legacy"),
        (Some("true"), None, "missing"),
        (Some("true"), Some("invalid-key".into()), "invalid"),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "environment_policy_is_explicit_and_preserves_self_hosted_compatibility",
                "--nocapture",
            ])
            .env_remove("TT_MASTER_KEY")
            .env_remove("TT_REQUIRE_ENCRYPTED_CACHE")
            .env(PROBE, mode);
        if let Some(flag) = flag {
            command.env("TT_REQUIRE_ENCRYPTED_CACHE", flag);
        }
        if let Some(key) = key {
            command.env("TT_MASTER_KEY", key);
        }
        let output = command.output().unwrap();
        if !output.status.success() {
            failures.push(format!(
                "{flag:?}/{mode}: {}",
                String::from_utf8_lossy(&output.stdout)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[cfg(unix)]
#[test]
fn non_unicode_master_key_is_an_error_not_plaintext_configuration() {
    use std::os::unix::ffi::OsStringExt;
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "environment_policy_is_explicit_and_preserves_self_hosted_compatibility",
            "--nocapture",
        ])
        .env("TT_CACHE_ENV_MATRIX", "invalid")
        .env_remove("TT_REQUIRE_ENCRYPTED_CACHE")
        .env("TT_MASTER_KEY", std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn memory_l1_rejects_legacy_in_hosted_mode_but_can_refill_with_encrypted_values() {
    if !configured_child(
        "memory_l1_rejects_legacy_in_hosted_mode_but_can_refill_with_encrypted_values",
    ) {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let plain = InMemoryL1Cache::new();
        let org = Uuid::new_v4();
        let key = format!("{org}:fixture");
        plain.set(&key, PAYLOAD, 60).await.unwrap();
        let legacy_reader = plain.clone().with_response_codec(ResponseCodec::new(KEY));
        assert_eq!(
            legacy_reader.get(&key).await.unwrap(),
            Some(PAYLOAD.to_vec())
        );
        let strict = plain.clone().with_response_codec(strict_from_env());
        assert_eq!(strict.get(&key).await.unwrap(), None);
        strict.set(&key, PAYLOAD, 60).await.unwrap();
        assert_eq!(strict.get(&key).await.unwrap(), Some(PAYLOAD.to_vec()));
        assert_eq!(
            plain.get(&key).await.unwrap(),
            None,
            "codec-disabled reader must not return ciphertext"
        );
        let wrong = plain.with_response_codec(ResponseCodec::new([4; 32]));
        assert_eq!(wrong.get(&key).await.unwrap(), None);
    });
}

#[test]
fn memory_l1_without_codec_never_returns_reserved_envelope_bytes() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let plain = InMemoryL1Cache::new();
        let org = Uuid::new_v4();
        let key = format!("{org}:fixture");
        for value in [
            ResponseCodec::new(KEY)
                .seal_l1_value(org, &key, PAYLOAD)
                .unwrap(),
            b"\0tt-l1-enc:v2\0future".to_vec(),
        ] {
            plain.set(&key, &value, 60).await.unwrap();
            assert_eq!(plain.get(&key).await.unwrap(), None);
        }
    });
}

#[test]
fn memory_l2_rejects_legacy_until_explicit_eviction_and_encrypted_refill() {
    if !configured_child("memory_l2_rejects_legacy_until_explicit_eviction_and_encrypted_refill") {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let org = Uuid::new_v4();
        let old = entry(org);
        let vector = old.embedding.clone();
        let old_id = old.id;
        let plain = InMemoryL2Cache::new();
        plain.insert(old).await.unwrap();
        let strict = plain.with_response_codec(strict_from_env());
        assert!(strict
            .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
            .await
            .unwrap()
            .is_none());
        strict.evict(old_id).await.unwrap();
        strict.insert(entry(org)).await.unwrap();
        assert_eq!(
            strict
                .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
                .await
                .unwrap()
                .unwrap()
                .0
                .response,
            PAYLOAD
        );
        assert!(strict
            .lookup(
                Uuid::new_v4(),
                &vector,
                0.99,
                "fixture-model",
                "fixture-embedding"
            )
            .await
            .unwrap()
            .is_none());
        let wrong = strict.with_response_codec(ResponseCodec::new([4; 32]));
        assert!(wrong
            .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
            .await
            .unwrap()
            .is_none());
    });
}

#[test]
fn memory_l2_policy_covers_non_json_legacy_and_codec_disabled_envelopes() {
    if !configured_child("memory_l2_policy_covers_non_json_legacy_and_codec_disabled_envelopes") {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let org = Uuid::new_v4();
        let mut raw = entry(org);
        let vector = raw.embedding.clone();
        raw.response = b"unsealed-raw-private-fixture".to_vec();
        let plain = InMemoryL2Cache::new();
        plain.insert(raw).await.unwrap();
        let strict = plain.with_response_codec(strict_from_env());
        assert!(strict
            .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
            .await
            .unwrap()
            .is_none());
        let plain = InMemoryL2Cache::new();
        let mut sealed = entry(org);
        sealed.response = serde_json::to_vec(
            &ResponseCodec::new(KEY)
                .seal_response_json(org, sealed.id, PAYLOAD)
                .unwrap(),
        )
        .unwrap();
        plain.insert(sealed).await.unwrap();
        assert!(plain
            .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
            .await
            .unwrap()
            .is_none());
    });
}

#[test]
#[ignore = "requires disposable loopback TEST_REDIS_URL"]
fn redis_enforces_hosted_policy_and_stores_only_ciphertext_after_refill() {
    if !configured_child("redis_enforces_hosted_policy_and_stores_only_ciphertext_after_refill") {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        use redis::AsyncCommands;
        use tt_cache::redis_impl::RedisL1Cache;
        let url = std::env::var("TEST_REDIS_URL").expect("disposable TEST_REDIS_URL");
        assert!(
            url.starts_with("redis://127.0.0.1:"),
            "fixture requires explicit loopback Redis"
        );
        let ns = format!("tt:review46:{}", Uuid::new_v4());
        let org = Uuid::new_v4();
        let key = format!("{org}:fixture");
        let plain = RedisL1Cache::connect(&url, &ns).await.unwrap();
        let strict = RedisL1Cache::connect(&url, &ns)
            .await
            .unwrap()
            .with_response_codec(strict_from_env());
        plain.set(&key, PAYLOAD, 60).await.unwrap();
        assert_eq!(strict.get(&key).await.unwrap(), None);
        strict.set(&key, PAYLOAD, 60).await.unwrap();
        let mut raw = plain.connection_manager();
        let stored: Vec<u8> = raw.get(format!("{ns}:{{{org}}}:{key}")).await.unwrap();
        assert!(ResponseCodec::is_encrypted_l1(&stored));
        assert!(!stored.windows(21).any(|w| w == b"private-cache-fixture"));
        assert_eq!(strict.get(&key).await.unwrap(), Some(PAYLOAD.to_vec()));
        assert_eq!(plain.get(&key).await.unwrap(), None);
        let wrong = RedisL1Cache::connect(&url, &ns)
            .await
            .unwrap()
            .with_response_codec(ResponseCodec::new([4; 32]));
        assert_eq!(wrong.get(&key).await.unwrap(), None);
        assert!(strict.purge_org(org).await.unwrap().complete);
        assert_eq!(strict.get(&key).await.unwrap(), None);
        // This test owns its namespace; never FLUSHDB/FLUSHALL a shared service.
        let mut cursor = 0_u64;
        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(format!("{ns}:*"))
                .arg("COUNT")
                .arg(100)
                .query_async(&mut raw)
                .await
                .unwrap();
            if !keys.is_empty() {
                let _: usize = raw.del(keys).await.unwrap();
            }
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
    });
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../core/migrations");
#[test]
#[ignore = "requires disposable loopback TEST_DATABASE_URL with pgvector"]
fn postgres_enforces_policy_and_rejects_malformed_or_rebound_envelopes() {
    if !configured_child("postgres_enforces_policy_and_rejects_malformed_or_rebound_envelopes") {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let url = std::env::var("TEST_DATABASE_URL").expect("disposable TEST_DATABASE_URL");
        assert!(
            url.starts_with("postgresql://127.0.0.1:"),
            "fixture requires explicit loopback Postgres"
        );
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let plain = PostgresL2Cache::new(pool.clone());
        let strict = PostgresL2Cache::new(pool.clone()).with_response_codec(strict_from_env());
        let org = Uuid::new_v4();
        let old = entry(org);
        let vector = old.embedding.clone();
        let old_id = old.id;
        plain.insert(old).await.unwrap();
        assert!(strict
            .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
            .await
            .unwrap()
            .is_none());
        assert!(plain
            .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
            .await
            .unwrap()
            .is_some());
        strict.evict(old_id).await.unwrap();
        let fresh = entry(org);
        let id = fresh.id;
        strict.insert(fresh).await.unwrap();
        let sealed: Value = sqlx::query_scalar("SELECT response FROM cache_entries WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(ResponseCodec::is_encrypted_json(&sealed));
        assert!(!sealed.to_string().contains("private-cache-fixture"));
        assert_eq!(
            strict
                .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
                .await
                .unwrap()
                .unwrap()
                .0
                .response,
            PAYLOAD
        );
        assert!(plain
            .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
            .await
            .unwrap()
            .is_none());
        let compatibility =
            PostgresL2Cache::new(pool.clone()).with_response_codec(ResponseCodec::new(KEY));
        for invalid in [
            json!({"__tt_cache_enc__":1}),
            json!({"__tt_cache_enc__":1,"blob":null}),
            ResponseCodec::new(KEY)
                .seal_response_json(Uuid::new_v4(), id, PAYLOAD)
                .unwrap(),
            ResponseCodec::new(KEY)
                .seal_response_json(org, Uuid::new_v4(), PAYLOAD)
                .unwrap(),
        ] {
            sqlx::query("UPDATE cache_entries SET response=$1 WHERE id=$2")
                .bind(invalid)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            assert!(strict
                .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
                .await
                .unwrap()
                .is_none());
            assert!(compatibility
                .lookup(org, &vector, 0.99, "fixture-model", "fixture-embedding")
                .await
                .unwrap()
                .is_none());
        }
        strict.evict(id).await.unwrap();
        pool.close().await;
    });
}
