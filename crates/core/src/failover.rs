//! Provider failover: try an ordered list of model candidates in turn,
//! skipping providers whose circuit breaker is open, until one succeeds.
//!
//! Combined with the per-candidate [`crate::retry`] layer this turns the
//! gateway into a "cost + reliability" layer: a route's `fallbacks` are tried
//! when the primary fails with a fallback-eligible error (provider down / 5xx /
//! timeout). A non-fallback-eligible error (e.g. a bad request) short-circuits
//! — there's no point retrying a different model for a malformed request.
//!
//! The clock is injected via `now` so the breaker is deterministic in tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::BoxStream;

use tt_shared::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Provider, ProviderError,
    RequestContext,
};

use crate::registry::ProviderRegistry;
use crate::retry::{with_retry, RetryPolicy};

/// Per-provider circuit breaker. After `failure_threshold` consecutive
/// failures a provider's circuit OPENS for `cooldown`; while open, failover
/// skips it. A success closes it.
pub struct CircuitBreaker {
    failure_threshold: u32,
    cooldown: Duration,
    state: Mutex<HashMap<String, BreakerState>>,
}

#[derive(Default)]
struct BreakerState {
    consecutive_failures: u32,
    opened_at: Option<DateTime<Utc>>,
}

impl CircuitBreaker {
    #[must_use]
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            failure_threshold,
            cooldown,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// `true` if `provider_id`'s circuit is open at `now` (still within cooldown).
    #[must_use]
    pub fn is_open(&self, provider_id: &str, now: DateTime<Utc>) -> bool {
        let guard = self.state.lock().expect("breaker poisoned");
        match guard.get(provider_id).and_then(|s| s.opened_at) {
            Some(opened) => {
                let cooldown = chrono::Duration::from_std(self.cooldown).unwrap_or_default();
                now.signed_duration_since(opened) < cooldown
            }
            None => false,
        }
    }

    /// Record a success — closes the circuit and resets the failure count.
    pub fn record_success(&self, provider_id: &str) {
        let mut guard = self.state.lock().expect("breaker poisoned");
        let s = guard.entry(provider_id.to_string()).or_default();
        s.consecutive_failures = 0;
        s.opened_at = None;
    }

    /// Record a failure — opens the circuit once the threshold is reached.
    pub fn record_failure(&self, provider_id: &str, now: DateTime<Utc>) {
        let mut guard = self.state.lock().expect("breaker poisoned");
        let s = guard.entry(provider_id.to_string()).or_default();
        s.consecutive_failures += 1;
        if s.consecutive_failures >= self.failure_threshold {
            s.opened_at = Some(now);
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(30))
    }
}

/// Dispatch `req` across `candidates` (ordered model ids) until one succeeds.
///
/// Each candidate is resolved via the registry and dispatched with `retry`;
/// providers whose circuit is open are skipped; on a fallback-eligible error
/// the next candidate is tried (and the provider's failure recorded). A
/// non-fallback-eligible error surfaces immediately. Returns the serving
/// provider + response, or the last error when every candidate is exhausted.
pub async fn dispatch_with_failover(
    registry: &ProviderRegistry,
    breaker: &CircuitBreaker,
    retry: &RetryPolicy,
    candidates: &[String],
    req: &ChatCompletionRequest,
    ctx: &RequestContext,
    now: DateTime<Utc>,
) -> Result<(Arc<dyn Provider>, ChatCompletionResponse), ProviderError> {
    let mut last_err: Option<ProviderError> = None;
    for model in candidates {
        let Some(provider) = registry.resolve(model) else {
            continue;
        };
        if breaker.is_open(provider.id(), now) {
            continue;
        }
        let mut attempt_req = req.clone();
        attempt_req.model = model.clone();
        let result = with_retry(retry, || provider.chat_completion(attempt_req.clone(), ctx)).await;
        match result {
            Ok(resp) => {
                breaker.record_success(provider.id());
                return Ok((provider, resp));
            }
            Err(e) if e.is_fallback_eligible() => {
                breaker.record_failure(provider.id(), now);
                last_err = Some(e);
            }
            // Not fallback-eligible (bad request, unsupported, …) — surface now.
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or(ProviderError::ProviderUpstream {
        status: 503,
        message: "no candidate provider available (unknown models or open circuits)".to_string(),
    }))
}

/// Streaming sibling of [`dispatch_with_failover`]: establish a chat-completion
/// stream across `candidates` in order. Failover happens only on the *initial*
/// stream establishment (before any chunk is yielded) — once bytes are
/// streaming a mid-stream error cannot be retried on another provider. Returns
/// the serving provider, the model it served, and the stream.
pub async fn dispatch_stream_with_failover(
    registry: &ProviderRegistry,
    breaker: &CircuitBreaker,
    retry: &RetryPolicy,
    candidates: &[String],
    req: &ChatCompletionRequest,
    ctx: &RequestContext,
    now: DateTime<Utc>,
) -> Result<
    (
        Arc<dyn Provider>,
        String,
        BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
    ),
    ProviderError,
> {
    let mut last_err: Option<ProviderError> = None;
    for model in candidates {
        let Some(provider) = registry.resolve(model) else {
            continue;
        };
        if breaker.is_open(provider.id(), now) {
            continue;
        }
        let mut attempt_req = req.clone();
        attempt_req.model = model.clone();
        let result = with_retry(retry, || {
            provider.chat_completion_stream(attempt_req.clone(), ctx)
        })
        .await;
        match result {
            Ok(stream) => {
                breaker.record_success(provider.id());
                return Ok((provider, model.clone(), stream));
            }
            Err(e) if e.is_fallback_eligible() => {
                breaker.record_failure(provider.id(), now);
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or(ProviderError::ProviderUpstream {
        status: 503,
        message: "no candidate provider available (unknown models or open circuits)".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::TimeZone;
    use futures::stream::BoxStream;
    use tt_shared::{
        messages::{Choice, Message, MessageContent},
        pricing::Capability,
        ChatCompletionChunk, EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Usage,
    };

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()
    }

    // ---- CircuitBreaker ----

    #[test]
    fn breaker_opens_after_threshold_and_cooldown_expires() {
        let b = CircuitBreaker::new(2, Duration::from_secs(30));
        assert!(!b.is_open("p", now()));
        b.record_failure("p", now());
        assert!(!b.is_open("p", now()), "one failure < threshold");
        b.record_failure("p", now());
        assert!(b.is_open("p", now()), "threshold reached → open");
        // After cooldown the circuit is no longer open.
        let later = now() + chrono::Duration::seconds(31);
        assert!(!b.is_open("p", later));
    }

    #[test]
    fn breaker_success_resets() {
        let b = CircuitBreaker::new(2, Duration::from_secs(30));
        b.record_failure("p", now());
        b.record_failure("p", now());
        assert!(b.is_open("p", now()));
        b.record_success("p");
        assert!(!b.is_open("p", now()), "success closes the circuit");
    }

    // ---- dispatch_with_failover ----

    enum Behavior {
        Ok,
        Fail5xx,
        Invalid,
    }

    struct MockProvider {
        id: &'static str,
        model: &'static str,
        behavior: Behavior,
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            self.id
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: self.model.to_string(),
                provider: self.id.to_string(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 100,
                max_output_tokens: 100,
            }]
        }
        fn pricing(&self, _: &str) -> Option<ModelPricing> {
            None
        }
        async fn chat_completion(
            &self,
            req: ChatCompletionRequest,
            _: &RequestContext,
        ) -> Result<ChatCompletionResponse, ProviderError> {
            match self.behavior {
                Behavior::Ok => Ok(ChatCompletionResponse {
                    id: "x".into(),
                    object: "chat.completion".into(),
                    created: 0,
                    model: req.model,
                    choices: vec![Choice {
                        index: 0,
                        message: Message::Assistant {
                            content: Some(MessageContent::Text("ok".into())),
                            tool_calls: vec![],
                            name: None,
                        },
                        finish_reason: Some("stop".into()),
                    }],
                    usage: Usage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        total_tokens: 2,
                        cached_tokens: 0,
                        cache_creation_input_tokens: None,
                    },
                }),
                Behavior::Fail5xx => Err(ProviderError::ProviderUpstream {
                    status: 503,
                    message: "down".into(),
                }),
                Behavior::Invalid => Err(ProviderError::InvalidRequest("bad".into())),
            }
        }
        async fn chat_completion_stream(
            &self,
            _: ChatCompletionRequest,
            _: &RequestContext,
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
            match self.behavior {
                Behavior::Ok => Ok(Box::pin(futures::stream::iter(Vec::<
                    Result<ChatCompletionChunk, ProviderError>,
                >::new()))),
                Behavior::Fail5xx => Err(ProviderError::ProviderUpstream {
                    status: 503,
                    message: "down".into(),
                }),
                Behavior::Invalid => Err(ProviderError::InvalidRequest("bad".into())),
            }
        }
        async fn embeddings(
            &self,
            _: EmbeddingsRequest,
            _: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Err(ProviderError::Unsupported("n/a".into()))
        }
    }

    fn ctx() -> RequestContext {
        RequestContext {
            trace_id: uuid::Uuid::nil(),
            org_id: uuid::Uuid::nil(),
            api_key_id: uuid::Uuid::nil(),
            credentials: tt_shared::context::ProviderCredentials {
                api_key: tt_shared::context::SecretString::new("k"),
                base_url: None,
                extra_headers: vec![],
            },
            tag: None,
            deadline: None,
        }
    }

    fn req(model: &str) -> ChatCompletionRequest {
        serde_json::from_str(&format!(r#"{{"model":"{model}","messages":[]}}"#)).unwrap()
    }

    fn fast() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn falls_over_to_next_candidate_on_5xx() {
        let mut reg = ProviderRegistry::new();
        reg.register(Arc::new(MockProvider {
            id: "pa",
            model: "model-a",
            behavior: Behavior::Fail5xx,
        }));
        reg.register(Arc::new(MockProvider {
            id: "pb",
            model: "model-b",
            behavior: Behavior::Ok,
        }));
        let breaker = CircuitBreaker::default();
        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let (provider, resp) = dispatch_with_failover(
            &reg,
            &breaker,
            &fast(),
            &candidates,
            &req("model-a"),
            &ctx(),
            now(),
        )
        .await
        .expect("fallback should serve");
        assert_eq!(provider.id(), "pb");
        assert_eq!(resp.model, "model-b");
    }

    #[tokio::test]
    async fn non_fallback_eligible_error_short_circuits() {
        let mut reg = ProviderRegistry::new();
        reg.register(Arc::new(MockProvider {
            id: "pa",
            model: "model-a",
            behavior: Behavior::Invalid,
        }));
        reg.register(Arc::new(MockProvider {
            id: "pb",
            model: "model-b",
            behavior: Behavior::Ok,
        }));
        let breaker = CircuitBreaker::default();
        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let r = dispatch_with_failover(
            &reg,
            &breaker,
            &fast(),
            &candidates,
            &req("model-a"),
            &ctx(),
            now(),
        )
        .await;
        assert!(
            matches!(r, Err(ProviderError::InvalidRequest(_))),
            "must not fall over on a non-fallback-eligible error"
        );
    }

    #[tokio::test]
    async fn stream_falls_over_to_next_candidate_on_5xx() {
        let mut reg = ProviderRegistry::new();
        reg.register(Arc::new(MockProvider {
            id: "pa",
            model: "model-a",
            behavior: Behavior::Fail5xx,
        }));
        reg.register(Arc::new(MockProvider {
            id: "pb",
            model: "model-b",
            behavior: Behavior::Ok,
        }));
        let breaker = CircuitBreaker::default();
        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let (provider, served, _stream) = dispatch_stream_with_failover(
            &reg,
            &breaker,
            &fast(),
            &candidates,
            &req("model-a"),
            &ctx(),
            now(),
        )
        .await
        .expect("fallback should establish the stream");
        assert_eq!(provider.id(), "pb");
        assert_eq!(served, "model-b");
    }

    #[tokio::test]
    async fn stream_non_fallback_eligible_error_short_circuits() {
        let mut reg = ProviderRegistry::new();
        reg.register(Arc::new(MockProvider {
            id: "pa",
            model: "model-a",
            behavior: Behavior::Invalid,
        }));
        reg.register(Arc::new(MockProvider {
            id: "pb",
            model: "model-b",
            behavior: Behavior::Ok,
        }));
        let breaker = CircuitBreaker::default();
        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let r = dispatch_stream_with_failover(
            &reg,
            &breaker,
            &fast(),
            &candidates,
            &req("model-a"),
            &ctx(),
            now(),
        )
        .await;
        assert!(matches!(r, Err(ProviderError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn open_circuit_is_skipped() {
        let mut reg = ProviderRegistry::new();
        reg.register(Arc::new(MockProvider {
            id: "pa",
            model: "model-a",
            behavior: Behavior::Ok, // would succeed, but circuit is forced open
        }));
        reg.register(Arc::new(MockProvider {
            id: "pb",
            model: "model-b",
            behavior: Behavior::Ok,
        }));
        let breaker = CircuitBreaker::new(1, Duration::from_secs(30));
        breaker.record_failure("pa", now()); // opens pa
        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let (provider, _) = dispatch_with_failover(
            &reg,
            &breaker,
            &fast(),
            &candidates,
            &req("model-a"),
            &ctx(),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(provider.id(), "pb", "open primary should be skipped");
    }
}
