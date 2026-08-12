use std::time::Duration;

use tt_cache::{RateLimitDecision, RedisRateLimiter};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires TEST_REDIS_URL"]
async fn replicas_share_one_atomic_rate_window() {
    let url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL");
    let namespace = format!("tt:test:rate-limit:{}", Uuid::new_v4());
    let first = RedisRateLimiter::connect(&url, namespace.clone())
        .await
        .expect("connect first limiter");
    let second = RedisRateLimiter::connect(&url, namespace)
        .await
        .expect("connect second limiter");

    assert_eq!(
        first
            .check("argon2", "203.0.113.7", 2, Duration::from_secs(60))
            .await
            .unwrap(),
        RateLimitDecision::Allow
    );
    assert_eq!(
        second
            .check("argon2", "203.0.113.7", 2, Duration::from_secs(60))
            .await
            .unwrap(),
        RateLimitDecision::Allow
    );
    assert!(matches!(
        first
            .check("argon2", "203.0.113.7", 2, Duration::from_secs(60))
            .await
            .unwrap(),
        RateLimitDecision::Reject {
            retry_after_secs: 1..
        }
    ));

    assert_eq!(
        second
            .check("argon2", "198.51.100.9", 2, Duration::from_secs(60))
            .await
            .unwrap(),
        RateLimitDecision::Allow
    );
}
