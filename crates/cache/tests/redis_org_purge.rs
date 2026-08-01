use redis::AsyncCommands;
use tt_cache::{redis_impl::RedisL1Cache, L1Cache};
use uuid::Uuid;

async fn cleanup_namespace(url: &str, namespace: &str) {
    let client = redis::Client::open(url).expect("test Redis URL");
    let mut connection = redis::aio::ConnectionManager::new(client)
        .await
        .expect("test Redis connection");
    let mut cursor = 0_u64;
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(format!("{namespace}:*"))
            .arg("COUNT")
            .arg(1_000)
            .query_async(&mut connection)
            .await
            .expect("scan exact test namespace");
        if !keys.is_empty() {
            let _: usize = connection
                .del(keys)
                .await
                .expect("delete exact test namespace keys");
        }
        if next == 0 {
            break;
        }
        cursor = next;
    }
}

#[tokio::test]
#[ignore = "requires TEST_REDIS_URL"]
async fn indexed_org_purge_is_scoped_and_fences_future_writes() {
    let url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL");
    let namespace = format!("tt:test:org-purge:{}", Uuid::new_v4());
    let cache = RedisL1Cache::connect(&url, &namespace)
        .await
        .expect("connect cache");
    let erased = Uuid::new_v4();
    let retained = Uuid::new_v4();
    let erased_request = format!("{erased}:request");
    // This is the exact key emitted by tt_core::routes::agent_run::run_key.
    let erased_transcript = format!("tt:runs:{erased}:{}", Uuid::new_v4());
    let retained_request = format!("{retained}:request");

    cache.set(&erased_request, b"a", 60).await.unwrap();
    cache
        .set(&erased_transcript, b"transcript", 60)
        .await
        .unwrap();
    cache.set(&retained_request, b"b", 60).await.unwrap();
    assert_eq!(
        cache.get(&erased_request).await.unwrap(),
        Some(b"a".to_vec())
    );

    let progress = cache.purge_org(erased).await.unwrap();
    assert!(progress.complete);
    assert_eq!(progress.deleted, 2);
    assert_eq!(cache.get(&erased_request).await.unwrap(), None);
    assert_eq!(cache.get(&erased_transcript).await.unwrap(), None);
    assert_eq!(
        cache.get(&retained_request).await.unwrap(),
        Some(b"b".to_vec())
    );

    cache.set(&erased_request, b"late", 60).await.unwrap();
    assert_eq!(cache.get(&erased_request).await.unwrap(), None);

    cleanup_namespace(&url, &namespace).await;
}
