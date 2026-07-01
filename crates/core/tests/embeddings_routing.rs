//! Embeddings routing: a `prompt_contains` route rewrites the embedding model to
//! a cheaper one. Proves the synthetic-request adapter feeds the embedding input
//! text into the routing engine and that savings are reported.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{EmbeddingData, EmbeddingsRequest, EmbeddingsResponse},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Provider serving the two OpenAI embedding models with their real-ish input
/// rates; records the served model from each embeddings call.
struct RecordingEmbedder {
    served: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for RecordingEmbedder {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["text-embedding-3-large", "text-embedding-3-small"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "recording".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 0,
            })
            .collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let input_per_million = match model {
            "text-embedding-3-large" => 0.13,
            "text-embedding-3-small" => 0.02,
            _ => 1.0,
        };
        Some(ModelPricing {
            input_per_million,
            output_per_million: 0.0,
            cached_input_per_million: None,
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
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported("chat not used".into()))
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        self.served.lock().unwrap().push(req.model.clone());
        Ok(EmbeddingsResponse {
            object: "list".into(),
            data: vec![EmbeddingData {
                object: "embedding".into(),
                index: 0,
                embedding: vec![0.1, 0.2],
            }],
            model: req.model,
            usage: Usage {
                prompt_tokens: 1000,
                completion_tokens: 0,
                total_tokens: 1000,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org_id, "k", Environment::Live, Actor::System)
        .await
        .expect("issue tt_live_ key")
        .plaintext
}

fn embed_req(model: &str, bearer: &str, input: &str) -> Request<Body> {
    let body = json!({ "model": model, "input": input });
    Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn embeddings_prompt_route_downgrades_and_reports_savings() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingEmbedder {
        served: Arc::clone(&served),
    }));

    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    // Route: when the input text contains "downgrade me", rewrite large → small.
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![Route {
            paused: false,
            id: Uuid::now_v7(),
            name: "embed-downgrade".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                prompt_contains_any_of: vec!["downgrade me".into()],
                ..Default::default()
            },
            then: RouteAction {
                format_switch: None,
                diff: false,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
                agentic_budget: None,
                target_model: Some("text-embedding-3-small".into()),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: false,
                doc_compaction: false,
                document_lane: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                panel: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );

    // Matching input → downgrade to -small, savings reported (baseline = large).
    let r1 = app
        .clone()
        .oneshot(embed_req(
            "text-embedding-3-large",
            &key,
            "please downgrade me now",
        ))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(
        r1.headers()["x-tokentrimmer-model-used"].to_str().unwrap(),
        "text-embedding-3-small",
        "matching input should downgrade the embedding model"
    );
    let saved: f64 = r1.headers()["x-tokentrimmer-saved-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    // baseline 1000×0.13/M − cost 1000×0.02/M = 0.00013 − 0.00002 = 0.00011.
    assert!(saved > 0.0, "expected positive savings, got {saved}");

    // Non-matching input → no route, served unchanged.
    let r2 = app
        .oneshot(embed_req("text-embedding-3-large", &key, "unrelated text"))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers()["x-tokentrimmer-model-used"].to_str().unwrap(),
        "text-embedding-3-large",
        "non-matching input should pass through"
    );

    let served = served.lock().unwrap().clone();
    assert_eq!(
        served,
        vec!["text-embedding-3-small", "text-embedding-3-large"]
    );
}
