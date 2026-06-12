//! End-to-end tests for the L2 false-positive verify gate + volatility-class
//! TTL (research Phase 2.2) and the L2-hit judge rate/cap knobs.
//!
//! Covers:
//! 1. OFF BY DEFAULT: without `with_l2_verify`, an ambiguous-band hit with a
//!    mismatching lexical signature still serves from cache (today's behavior,
//!    byte-identical).
//! 2. Gate on: a rejected in-band hit falls through to a live provider
//!    dispatch — no `hit-l2` header, no hit-count bump, no cache savings.
//! 3. Gate on: an agreeing in-band hit serves from cache; a legacy (no-sig)
//!    entry fails open.
//! 4. Adaptive ratchet: after a breaching FP batch, a similarity that used to
//!    hit now misses — and the effective threshold never drops below 0.92.
//! 5. Volatility TTL: opt-in shorten-only L2 expiry for volatile prompts, on
//!    both the non-streaming and streaming insert paths (L1 untouched);
//!    off-by-default keeps the base TTL.
//! 6. L2-hit judge: dedicated `l2_hit_sample_rate` / `l2_band_sample_rate`
//!    override the shared rate; the hourly cap (`l2_hit_max_per_hour = 0`)
//!    stops L2-hit judging entirely.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::ApiKeyContext;
use tt_cache::{
    CacheEntry, EmbeddingProvider, FpGateTuning, InMemoryL2Cache, L1Cache, L2Cache, TaskClass,
};
use tt_core::state::L2VolatilityTtl;
use tt_core::{build_router, AppState, InMemoryJudgeBandStore, JudgeConfig, ProviderRegistry};
use tt_shared::{
    messages::{Choice, ChunkChoice, ChunkDelta, Message, MessageContent},
    pricing::Capability,
    CallerTier, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext, Usage,
};
use uuid::Uuid;

// ─── fixtures ────────────────────────────────────────────────────────────────

/// The user message every cached-entry test sends; its canonicalized L2
/// context text is `"[user] hello there"`.
const USER_MSG: &str = "hello there";
const QUERY_CONTEXT: &str = "[user] hello there";

/// Provider that counts dispatches (cache hits must bypass it).
struct CountingProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for CountingProvider {
    fn id(&self) -> &'static str {
        "counting"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "counting-1".into(),
            provider: "counting".into(),
            capabilities: vec![Capability::Text, Capability::Streaming],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "chatcmpl-live".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("live from provider".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // Clean 2-chunk stream: content, then finish + authoritative usage —
        // the shape the streaming cache-write path requires.
        let chunk = |content: Option<&str>, finish: Option<&str>, usage: Option<Usage>| {
            Ok(ChatCompletionChunk {
                id: "c1".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: "counting-1".into(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: content.map(|_| "assistant".to_string()),
                        content: content.map(str::to_string),
                        tool_calls: vec![],
                        extra: Default::default(),
                    },
                    finish_reason: finish.map(str::to_string),
                    extra: Default::default(),
                }],
                usage,
                extra: Default::default(),
            })
        };
        let chunks = vec![
            chunk(Some("hi"), None, None),
            chunk(
                None,
                Some("stop"),
                Some(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    total_tokens: 14,
                    cached_tokens: 0,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
            ),
        ];
        Ok(futures::stream::iter(chunks).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// Deterministic embedder returning a fixed vector for every text.
struct FixedEmbedder {
    vec: Vec<f32>,
}

#[async_trait]
impl EmbeddingProvider for FixedEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, tt_cache::EmbedError> {
        Ok(self.vec.clone())
    }
    fn model(&self) -> &str {
        "fixed-embed"
    }
}

/// 1536-dim unit vector along e0 (the primed entry's embedding).
fn e0() -> Vec<f32> {
    let mut v = vec![0.0_f32; 1536];
    v[0] = 1.0;
    v
}

/// A query vector at cosine ≈ `cos_x / |(cos_x, sin_x)|` to e0 — used to land
/// inside the ambiguous band [0.92, 0.94).
fn query_at(cos_x: f32, sin_x: f32) -> Vec<f32> {
    let mut v = vec![0.0_f32; 1536];
    v[0] = cos_x;
    v[1] = sin_x;
    v
}

fn primed_entry(lexical_sig: Option<i64>) -> CacheEntry {
    let primed = ChatCompletionResponse {
        id: "chatcmpl-cached".into(),
        object: "chat.completion".into(),
        created: 0,
        model: "counting-1".into(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text("from cache".into())),
                tool_calls: vec![],
                name: None,
            },
            finish_reason: Some("stop".into()),
        }],
        usage: Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        },
    };
    CacheEntry {
        id: Uuid::now_v7(),
        org_id: Uuid::nil(),
        embedding: e0(),
        response: serde_json::to_vec(&primed).unwrap(),
        model: "counting-1".into(),
        embedding_model: "fixed-embed".into(),
        input_tokens: 100,
        output_tokens: 50,
        baseline_cost_usd: Some(0.0006),
        hit_count: 0,
        quality_score: None,
        judge_verdict: None,
        created_at: Utc::now(),
        expires_at: Utc::now() + ChronoDuration::hours(1),
        lexical_sig,
    }
}

fn chat_request_pro(content: &str, stream: bool) -> Request<Body> {
    let body = json!({
        "model": "counting-1",
        "messages": [{ "role": "user", "content": content }],
        "stream": stream,
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut().insert(ApiKeyContext {
        key_id: Uuid::nil(),
        org_id: Uuid::nil(),
        tier: Some(CallerTier::Pro),
    });
    req
}

fn cache_header(resp: &axum::response::Response) -> String {
    resp.headers()
        .get("x-tokentrimmer-cache")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// A counting+in-band gateway: primed entry at e0, embedder returning a
/// query vector with cosine ≈ 0.9292 (inside [0.92, 0.94)).
struct Gateway {
    calls: Arc<AtomicUsize>,
    cache: Arc<InMemoryL2Cache>,
    state: AppState,
    entry_id: Uuid,
}

async fn in_band_gateway(entry_sig: Option<i64>) -> Gateway {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());
    let entry = primed_entry(entry_sig);
    let entry_id = entry.id;
    cache.insert(entry).await.unwrap();
    // cosine(e0, (0.93, 0.37)) ≈ 0.9292 — in the band [0.92, 0.94).
    let embedder = Arc::new(FixedEmbedder {
        vec: query_at(0.93, 0.37),
    });
    let state = AppState::new(registry).with_l2(cache.clone(), embedder, None);
    Gateway {
        calls,
        cache,
        state,
        entry_id,
    }
}

// ─── 1. off by default ───────────────────────────────────────────────────────

/// Without `with_l2_verify`, an in-band hit with a MISMATCHING signature
/// still serves from cache — the gate changes nothing until opted into.
#[tokio::test]
async fn verify_gate_off_by_default_no_behavior_change() {
    let mismatch = tt_cache::lexical_sig("[user] a completely different subject entirely");
    let g = in_band_gateway(Some(mismatch)).await;
    let app = build_router(g.state);

    let resp = app
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        cache_header(&resp),
        "hit-l2",
        "gate off must serve as today"
    );
    assert_eq!(
        g.calls.load(Ordering::Relaxed),
        0,
        "gate off: provider must NOT be called on the hit"
    );
}

// ─── 2. rejected in-band hit ─────────────────────────────────────────────────

/// Gate on + mismatching signature: the in-band hit is REJECTED — the request
/// dispatches live (one provider call, live body, no hit-l2 header) and the
/// entry's hit_count is NOT bumped (no savings booked for a rejected hit).
#[tokio::test]
async fn rejected_in_band_hit_dispatches_provider_and_books_no_cache_saving() {
    let mismatch = tt_cache::lexical_sig("[user] a completely different subject entirely");
    let g = in_band_gateway(Some(mismatch)).await;
    let state = g
        .state
        .with_l2_verify(0.02, 0.75, 1.0, FpGateTuning::default());
    let app = build_router(state);

    let resp = app
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        cache_header(&resp),
        "miss",
        "a rejected hit must surface as a miss, never hit-l2"
    );
    assert_eq!(
        g.calls.load(Ordering::Relaxed),
        1,
        "a rejected hit must fall through to ONE live dispatch"
    );
    let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["id"], "chatcmpl-live", "the live body is served");

    // The rejected entry was not bumped (lookup at threshold 0 sees it raw).
    let (entry, _) = g
        .cache
        .lookup(Uuid::nil(), &e0(), 0.0, "counting-1", "fixed-embed")
        .await
        .unwrap()
        .expect("the entry itself is never evicted by a reject");
    assert_eq!(entry.id, g.entry_id);
    assert_eq!(
        entry.hit_count, 0,
        "a rejected hit must not bump hit_count (no phantom cache hit)"
    );
}

// ─── 3. verified / unverifiable in-band hits ─────────────────────────────────

/// Gate on + agreeing signature: the in-band hit serves from cache.
#[tokio::test]
async fn verified_in_band_hit_serves_from_cache() {
    let g = in_band_gateway(Some(tt_cache::lexical_sig(QUERY_CONTEXT))).await;
    let state = g
        .state
        .with_l2_verify(0.02, 0.75, 1.0, FpGateTuning::default());
    let app = build_router(state);

    let resp = app
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(cache_header(&resp), "hit-l2");
    assert_eq!(g.calls.load(Ordering::Relaxed), 0);
}

/// Gate on + a legacy entry without a signature (pre-0018): fails OPEN.
#[tokio::test]
async fn unverifiable_legacy_entry_fails_open_and_serves() {
    let g = in_band_gateway(None).await;
    let state = g
        .state
        .with_l2_verify(0.02, 0.75, 1.0, FpGateTuning::default());
    let app = build_router(state);

    let resp = app
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        cache_header(&resp),
        "hit-l2",
        "a pre-0018 row must fail open (serve), never 5xx or reject"
    );
    assert_eq!(g.calls.load(Ordering::Relaxed), 0);
}

// ─── 4. adaptive ratchet ─────────────────────────────────────────────────────

/// After feeding the gate a breaching FP batch, a similarity that previously
/// hit now misses — and the effective threshold stays >= the 0.92 floor.
#[tokio::test]
async fn adapted_threshold_turns_former_band_hit_into_miss() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());
    cache
        .insert(primed_entry(Some(tt_cache::lexical_sig(QUERY_CONTEXT))))
        .await
        .unwrap();
    // cosine(e0, (0.922, 0.387)) ≈ 0.9221 — above 0.92, below 0.925.
    let embedder = Arc::new(FixedEmbedder {
        vec: query_at(0.922, 0.387),
    });
    let state = AppState::new(registry)
        .with_l2(cache.clone(), embedder, None)
        .with_l2_verify(0.02, 0.75, 1.0, FpGateTuning::new(20, 0.005));
    let gate = state
        .l2
        .as_ref()
        .unwrap()
        .verify
        .as_ref()
        .unwrap()
        .gate
        .clone();
    let app = build_router(state);

    // Before adaptation: the ~0.9221 hit serves from cache.
    let resp = app
        .clone()
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(cache_header(&resp), "hit-l2", "pre-adaptation: a hit");
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    // Feed a breaching batch: 20 judged in-band FPs at 1% tolerance.
    let class = Some(TaskClass::ChatCompletions);
    for _ in 0..20 {
        gate.record_judged_band_hit(class, Some(true), 1.0);
    }
    let effective = gate.effective_threshold(class);
    assert!(
        effective > 0.9221,
        "the raise must clear the fixture similarity (and, a fortiori, the \
         0.92 floor); got {effective}"
    );

    // After adaptation: the same request now misses and dispatches live.
    let resp = app
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(
        cache_header(&resp),
        "miss",
        "post-adaptation: the former band hit must miss"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1, "one live dispatch");
}

// ─── 5. volatility-class TTL ─────────────────────────────────────────────────

/// Poll the L2 store until the spawned insert lands and return the entry.
async fn wait_for_l2_entry(cache: &InMemoryL2Cache, query_vec: &[f32]) -> CacheEntry {
    for _ in 0..200 {
        tokio::task::yield_now().await;
        if let Some((entry, _)) = cache
            .lookup(Uuid::nil(), query_vec, 0.0, "counting-1", "fixed-embed")
            .await
            .unwrap()
        {
            return entry;
        }
    }
    panic!("the miss-path L2 insert never landed");
}

fn ttl_secs_of(entry: &CacheEntry) -> i64 {
    (entry.expires_at - entry.created_at).num_seconds()
}

/// With TT_L2_VOLATILITY_TTL wired, a volatile prompt's L2 entry expires at
/// base * multiplier; a stable prompt keeps the base (tier) TTL. The insert
/// also stamps the lexical signature unconditionally.
#[tokio::test]
async fn volatile_prompt_l2_entry_expires_sooner() {
    let base = CallerTier::Pro.ttl_secs() as i64;
    for (content, expected_ttl) in [
        ("what are today's headlines", base / 4),
        ("explain the borrow checker in rust", base),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        }));
        let cache = Arc::new(InMemoryL2Cache::new());
        let embedder = Arc::new(FixedEmbedder { vec: e0() });
        let state = AppState::new(registry)
            .with_l2(cache.clone(), embedder, None)
            .with_l2_volatility_ttl(L2VolatilityTtl {
                volatile_multiplier: 0.25,
                floor_secs: 300,
            });
        let app = build_router(state);

        let resp = app.oneshot(chat_request_pro(content, false)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let entry = wait_for_l2_entry(&cache, &e0()).await;
        assert_eq!(
            ttl_secs_of(&entry),
            expected_ttl,
            "prompt {content:?} must get TTL {expected_ttl}"
        );
        assert!(
            entry.lexical_sig.is_some(),
            "every new insert stamps the lexical signature"
        );
    }
}

/// Without the volatility flag, even a volatile prompt keeps the base TTL.
#[tokio::test]
async fn volatility_ttl_off_by_default() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());
    let embedder = Arc::new(FixedEmbedder { vec: e0() });
    let state = AppState::new(registry).with_l2(cache.clone(), embedder, None);
    let app = build_router(state);

    let resp = app
        .oneshot(chat_request_pro("what are today's headlines", false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let entry = wait_for_l2_entry(&cache, &e0()).await;
    assert_eq!(
        ttl_secs_of(&entry),
        CallerTier::Pro.ttl_secs() as i64,
        "off by default: the base tier TTL applies even to volatile text"
    );
}

/// L1 wrapper recording every `set` TTL — proves the streaming path shortens
/// the L2 TTL while leaving the L1 TTL at the base.
struct RecordingL1 {
    inner: tt_cache::memory::InMemoryL1Cache,
    set_ttls: Arc<Mutex<Vec<u64>>>,
}

#[async_trait]
impl L1Cache for RecordingL1 {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, tt_cache::CacheError> {
        self.inner.get(key).await
    }
    async fn set(
        &self,
        key: &str,
        value: &[u8],
        ttl_secs: u64,
    ) -> Result<(), tt_cache::CacheError> {
        self.set_ttls.lock().unwrap().push(ttl_secs);
        self.inner.set(key, value, ttl_secs).await
    }
    async fn delete(&self, key: &str) -> Result<(), tt_cache::CacheError> {
        self.inner.delete(key).await
    }
}

/// Streaming miss with a volatile prompt: the reconstructed response's L2
/// entry gets the shortened TTL while the L1 insert keeps the base TTL.
#[tokio::test]
async fn streaming_volatile_insert_shortens_l2_ttl_but_not_l1() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());
    let set_ttls = Arc::new(Mutex::new(Vec::new()));
    let l1 = Arc::new(RecordingL1 {
        inner: tt_cache::memory::InMemoryL1Cache::new(),
        set_ttls: set_ttls.clone(),
    });
    let embedder = Arc::new(FixedEmbedder { vec: e0() });
    let base = CallerTier::Pro.ttl_secs();
    let state = AppState::new(registry)
        .with_l1(l1, None)
        .with_l2(cache.clone(), embedder, None)
        .with_l2_volatility_ttl(L2VolatilityTtl {
            volatile_multiplier: 0.25,
            floor_secs: 300,
        });
    let app = build_router(state);

    let resp = app
        .oneshot(chat_request_pro("what are today's headlines", true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Drain the SSE body so the DropGuard fires the cache inserts.
    let _ = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();

    let entry = wait_for_l2_entry(&cache, &e0()).await;
    assert_eq!(
        ttl_secs_of(&entry),
        (base / 4) as i64,
        "streaming volatile insert must shorten the L2 TTL"
    );
    assert!(
        entry.lexical_sig.is_some(),
        "the streaming insert stamps the lexical signature too"
    );
    // L1 got the BASE ttl — never shortened by volatility.
    for _ in 0..200 {
        tokio::task::yield_now().await;
        if !set_ttls.lock().unwrap().is_empty() {
            break;
        }
    }
    let ttls = set_ttls.lock().unwrap().clone();
    assert!(!ttls.is_empty(), "the streaming L1 insert must have fired");
    assert!(
        ttls.iter().all(|t| *t == base),
        "L1 TTLs must stay at the base ({base}); got {ttls:?}"
    );
}

// ─── 6. L2-hit judge: rates + hourly cap ─────────────────────────────────────

/// Provider serving BOTH the reference model (`counting-1`) and the judge
/// model (`judge-1`); the judge names the cached answer's blind slot as
/// missing, so the verdict maps to Degraded → eviction (the observable signal
/// that the judge FIRED).
struct JudgingProvider {
    ref_calls: Arc<AtomicUsize>,
    judge_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for JudgingProvider {
    fn id(&self) -> &'static str {
        "judging"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["counting-1", "judge-1"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "judging".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let text = if req.model == "judge-1" {
            self.judge_calls.fetch_add(1, Ordering::Relaxed);
            let user_text = req
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::User {
                        content: MessageContent::Text(s),
                        ..
                    } => Some(s.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let a_start = user_text.find("ANSWER A:").unwrap_or(0);
            let b_start = user_text.find("ANSWER B:").unwrap_or(user_text.len());
            if user_text[a_start..b_start].contains("from cache") {
                "A_MISSING — the cached answer does not address the query".to_string()
            } else {
                "B_MISSING — the cached answer does not address the query".to_string()
            }
        } else {
            self.ref_calls.fetch_add(1, Ordering::Relaxed);
            "a fresh reference answer".to_string()
        };
        Ok(ChatCompletionResponse {
            id: "chatcmpl-judge".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(text)),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Err(ProviderError::Unsupported("no stream".into()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

struct JudgeGateway {
    ref_calls: Arc<AtomicUsize>,
    cache: Arc<InMemoryL2Cache>,
    app: axum::Router,
}

/// L2-hit-judge harness: a primed exact-match entry (similarity 1.0 by
/// default; pass an in-band embedder vec for band tests) + the configured
/// judge knobs.
async fn judge_gateway(config: JudgeConfig, embed_vec: Vec<f32>, verify: bool) -> JudgeGateway {
    let ref_calls = Arc::new(AtomicUsize::new(0));
    let judge_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(JudgingProvider {
        ref_calls: Arc::clone(&ref_calls),
        judge_calls: Arc::clone(&judge_calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());
    cache
        .insert(primed_entry(Some(tt_cache::lexical_sig(QUERY_CONTEXT))))
        .await
        .unwrap();
    let embedder = Arc::new(FixedEmbedder { vec: embed_vec });
    let store = Arc::new(InMemoryJudgeBandStore::new());
    let mut state = AppState::new(registry)
        .with_l2(cache.clone(), embedder, None)
        .with_quality_judge(store, config);
    if verify {
        state = state.with_l2_verify(0.02, 0.75, 1.0, FpGateTuning::default());
    }
    JudgeGateway {
        ref_calls,
        cache,
        app: build_router(state),
    }
}

/// Wait until the served entry is evicted (the judge fired with a Degraded
/// verdict) — or fail.
async fn wait_for_eviction(cache: &InMemoryL2Cache) -> bool {
    for _ in 0..500 {
        tokio::task::yield_now().await;
        let still_there = cache
            .lookup(Uuid::nil(), &e0(), 0.0, "counting-1", "fixed-embed")
            .await
            .unwrap()
            .is_some();
        if !still_there {
            return true;
        }
    }
    false
}

/// `l2_hit_sample_rate` is the L2-hit path's own knob: a zero shared rate
/// with a 1.0 L2-hit rate still judges (and evicts); the converse never does.
#[tokio::test]
async fn l2_hit_judge_uses_dedicated_sample_rate() {
    // shared 0.0 + l2_hit 1.0 → fires.
    let g = judge_gateway(
        JudgeConfig {
            enabled: true,
            sample_rate: 0.0,
            l2_hit_sample_rate: Some(1.0),
            judge_model: "judge-1".to_string(),
            ..JudgeConfig::default()
        },
        e0(),
        false,
    )
    .await;
    let resp = g
        .app
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(cache_header(&resp), "hit-l2");
    assert!(
        wait_for_eviction(&g.cache).await,
        "l2_hit_sample_rate=1.0 must judge the hit even when the shared rate is 0"
    );

    // shared 1.0 + l2_hit 0.0 → never fires on the L2-hit path.
    let g = judge_gateway(
        JudgeConfig {
            enabled: true,
            sample_rate: 1.0,
            l2_hit_sample_rate: Some(0.0),
            judge_model: "judge-1".to_string(),
            ..JudgeConfig::default()
        },
        e0(),
        false,
    )
    .await;
    let resp = g
        .app
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(cache_header(&resp), "hit-l2");
    for _ in 0..300 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        g.ref_calls.load(Ordering::Relaxed),
        0,
        "l2_hit_sample_rate=0.0 must suppress the L2-hit judge regardless of the shared rate"
    );
}

/// For an IN-BAND hit, `l2_band_sample_rate` overrides the L2-hit rate so the
/// FP estimator can converge without judging every confident hit.
#[tokio::test]
async fn band_sample_rate_overrides_hit_rate_for_in_band_hits() {
    let g = judge_gateway(
        JudgeConfig {
            enabled: true,
            sample_rate: 0.0,
            l2_hit_sample_rate: Some(0.0),
            l2_band_sample_rate: Some(1.0),
            judge_model: "judge-1".to_string(),
            ..JudgeConfig::default()
        },
        query_at(0.93, 0.37), // in-band ≈ 0.9292
        true,                 // verify gate on → the band exists
    )
    .await;
    let resp = g
        .app
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(cache_header(&resp), "hit-l2", "verified in-band hit serves");
    assert!(
        wait_for_eviction(&g.cache).await,
        "an in-band hit must sample at the band rate (1.0) and judge"
    );
}

/// `l2_hit_max_per_hour = 0` fully caps L2-hit judging off — even at rate 1.0
/// no judge dispatch happens.
#[tokio::test]
async fn l2_hit_judge_cap_zero_disables_judging() {
    let g = judge_gateway(
        JudgeConfig {
            enabled: true,
            sample_rate: 1.0,
            l2_hit_sample_rate: Some(1.0),
            l2_hit_max_per_hour: 0,
            judge_model: "judge-1".to_string(),
            ..JudgeConfig::default()
        },
        e0(),
        false,
    )
    .await;
    let resp = g
        .app
        .oneshot(chat_request_pro(USER_MSG, false))
        .await
        .unwrap();
    assert_eq!(cache_header(&resp), "hit-l2", "the hit itself still serves");
    for _ in 0..300 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        g.ref_calls.load(Ordering::Relaxed),
        0,
        "a zero hourly cap must stop every L2-hit judge dispatch"
    );
}
