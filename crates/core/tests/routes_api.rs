//! End-to-end tests for the user-facing /v1/routes CRUD.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
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
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

struct RecordingProvider {
    served: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["gpt-4o", "gpt-4o-mini"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "recording".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let (i, o) = if model == "gpt-4o" {
            (5.0, 15.0)
        } else {
            (0.15, 0.6)
        };
        Some(ModelPricing {
            input_per_million: i,
            output_per_million: o,
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
        req: ChatCompletionRequest,
        _c: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.served.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
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
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _r: ChatCompletionRequest,
        _c: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        _r: EmbeddingsRequest,
        _c: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System)
        .await
        .unwrap()
        .plaintext
}

async fn app_with_key() -> (axum::Router, String, Arc<Mutex<Vec<String>>>) {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served: Arc::clone(&served),
    }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);
    let routing = Arc::new(CachingRoutingStore::new(
        Arc::new(InMemoryRoutingStore::new()) as Arc<dyn RoutingStore>,
    ));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );
    (app, key, served)
}

/// Build an app whose backing store contains a route injected outside the
/// normal write path. Production Postgres can contain the equivalent from a
/// legacy migration or direct/manual edit; management reads must expose it
/// even though canonical validation says it cannot activate.
async fn app_with_manual_route(route: Route) -> (axum::Router, String) {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served: Arc::new(Mutex::new(Vec::new())),
    }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, vec![route]);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );
    (app, key)
}

fn req(method: &str, uri: &str, key: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    b.body(
        body.map(|v| Body::from(v.to_string()))
            .unwrap_or(Body::empty()),
    )
    .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_list_get_delete_round_trip() {
    let (app, key, _served) = app_with_key().await;
    let spec = json!({ "name": "downgrade", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let created = body_json(r).await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["schema_version"], json!(1));
    assert!(created["canonical_hash"]
        .as_str()
        .is_some_and(|hash| hash.starts_with("sha256:")));
    assert_eq!(created["activation"]["state"], json!("active"));

    let r = app
        .clone()
        .oneshot(req("GET", "/v1/routes", Some(&key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let listed = body_json(r).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["id"], json!(id.as_str()));
    assert_eq!(listed[0]["schema_version"], json!(1));
    assert!(listed[0]["canonical_hash"]
        .as_str()
        .is_some_and(|hash| hash.starts_with("sha256:")));
    assert_eq!(listed[0]["activation"]["state"], json!("active"));

    let r = app
        .clone()
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let fetched = body_json(r).await;
    let revision = fetched["revision"]
        .as_i64()
        .filter(|revision| *revision >= 1)
        .expect("management read must carry a positive revision");

    let r = app
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/routes/{id}?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = app
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_requires_a_current_positive_route_revision() {
    let (app, key, _served) = app_with_key().await;
    let spec = json!({
        "name": "revision-fenced-delete",
        "when": {"model_in": ["gpt-4o"]},
        "then": {"target_model": "gpt-4o-mini"},
    });
    let created = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let id = body_json(created).await["id"].as_str().unwrap().to_string();

    let current = app
        .clone()
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(current.status(), StatusCode::OK);
    let revision = body_json(current).await["revision"]
        .as_i64()
        .filter(|revision| *revision >= 1)
        .expect("route read supplies revision evidence");

    for uri in [
        format!("/v1/routes/{id}"),
        format!("/v1/routes/{id}?expected_revision=0"),
        format!("/v1/routes/{id}?expected_revision=not-a-number"),
    ] {
        let response = app
            .clone()
            .oneshot(req("DELETE", &uri, Some(&key), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    }

    let stale = app
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/routes/{id}?expected_revision={}", revision + 1),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let current = app
        .clone()
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(
        current.status(),
        StatusCode::OK,
        "stale delete must preserve the route"
    );

    let deleted = app
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/v1/routes/{id}?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
}

#[tokio::test]
async fn management_reads_keep_invalid_persisted_rows_visible_for_repair() {
    let id = Uuid::now_v7();
    let route = Route {
        id,
        name: "legacy/manual route".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions::default(),
        // Bypasses the normal route-write contract like a legacy/manual
        // persisted row. The runtime must not activate traffic_pct > 100, but
        // management reads must retain the row and its raw repair data.
        then: RouteAction {
            target_model: Some("gpt-4o-mini".into()),
            traffic_pct: Some(101),
            ..Default::default()
        },
        paused: false,
    };
    let (app, key) = app_with_manual_route(route).await;

    let list = app
        .clone()
        .oneshot(req("GET", "/v1/routes", Some(&key), None))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let listed = body_json(list).await;
    assert_eq!(
        listed.as_array().unwrap().len(),
        1,
        "invalid row must not disappear"
    );
    assert_eq!(listed[0]["id"], json!(id.to_string()));
    assert_eq!(listed[0]["priority"], json!(100));
    assert_eq!(listed[0]["then"]["traffic_pct"], json!(101));
    assert_eq!(listed[0]["schema_version"], json!(1));
    assert!(listed[0]["canonical_hash"].is_null());
    assert_eq!(listed[0]["activation"]["state"], json!("invalid"));
    assert_eq!(
        listed[0]["activation"]["issues"][0]["field"],
        json!("then.traffic_pct")
    );

    let get = app
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(
        get.status(),
        StatusCode::OK,
        "invalid row remains addressable"
    );
    let fetched = body_json(get).await;
    assert_eq!(fetched, listed[0]);
}

#[tokio::test]
async fn route_writes_reject_unknown_and_future_fields_with_addressable_422s() {
    let (app, key, _) = app_with_key().await;
    let unknown = json!({
        "name": "unknown-field",
        "when": { "model_in": ["gpt-4o"], "future_match": true },
        "then": { "target_model": "gpt-4o-mini" }
    });
    let response = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(unknown)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], json!("route_validation_failed"));
    assert_eq!(body["error"]["param"], json!("when.future_match"));
    assert_eq!(
        body["error"]["issues"][0]["field"],
        json!("when.future_match")
    );

    let future = json!({
        "schema_version": 2,
        "name": "future-version",
        "then": { "target_model": "gpt-4o-mini" }
    });
    let response = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(future)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(response).await;
    assert_eq!(
        body["error"]["issues"][0]["code"],
        json!("future_schema_version")
    );

    // The shared control-plane table stores priorities as `INT`. A route
    // beyond that range must be rejected before the store can clamp it.
    let out_of_range_priority = json!({
        "name": "out-of-range-priority",
        "priority": i32::MAX as u64 + 1,
        "then": { "target_model": "gpt-4o-mini" }
    });
    let response = app
        .clone()
        .oneshot(req(
            "POST",
            "/v1/routes",
            Some(&key),
            Some(out_of_range_priority),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(response).await;
    assert_eq!(body["error"]["param"], json!("priority"));
    assert_eq!(body["error"]["issues"][0]["code"], json!("invalid_field"));

    let list = app
        .oneshot(req("GET", "/v1/routes", Some(&key), None))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    assert!(
        body_json(list).await.as_array().unwrap().is_empty(),
        "rejected canonical writes must not create a clamped route"
    );
}

#[tokio::test]
async fn unauthenticated_is_rejected() {
    let (app, _key, _) = app_with_key().await;
    let r = app
        .oneshot(req("GET", "/v1/routes", None, None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cross_provider_target_accepted() {
    // V3d-1: cross-provider routes are allowed. A gpt-4o -> claude-haiku-4-5
    // route creates successfully (capability guard is permissive on the
    // unknown target; the same-provider gate is gone).
    let (app, key, _) = app_with_key().await;
    let spec = json!({ "name": "x", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"claude-haiku-4-5"} });
    let r = app
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn has_images_non_vision_target_rejected() {
    let (app, key, _) = app_with_key().await;
    // gpt-4o-mini in this test registry is Text-only → must reject.
    let spec = json!({ "name": "x", "when": {"has_images": true}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// POST /v1/routes/:id/pause → paused:true; GET /v1/routes shows paused;
/// POST resume → was_paused:true and the list drops the paused key (false is
/// serde-omitted); unknown id → 404; other org's key → 404; anonymous → 401.
#[tokio::test]
async fn pause_resume_endpoints_round_trip() {
    let (app, key, _served) = app_with_key().await;
    let spec = json!({ "name": "downgrade", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let id = body_json(r).await["id"].as_str().unwrap().to_string();
    let r = app
        .clone()
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let revision = body_json(r).await["revision"]
        .as_i64()
        .filter(|revision| *revision >= 1)
        .expect("management read supplies a route revision");

    // Pause.
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/pause?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = body_json(r).await;
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["paused"], json!(true));

    // The list + get surfaces paused: true.
    let r = app
        .clone()
        .oneshot(req("GET", "/v1/routes", Some(&key), None))
        .await
        .unwrap();
    let list = body_json(r).await;
    assert_eq!(list[0]["paused"], json!(true), "{list}");

    // Idempotent: a second pause is still 200 + paused:true.
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/pause?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body_json(r).await["paused"], json!(true));

    // Resume: was_paused = true, then the list omits the paused key entirely
    // (false-omitted serde — fixture stability).
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/resume?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = body_json(r).await;
    assert_eq!(body["paused"], json!(false));
    assert_eq!(body["was_paused"], json!(true));
    let r = app
        .clone()
        .oneshot(req("GET", "/v1/routes", Some(&key), None))
        .await
        .unwrap();
    let list = body_json(r).await;
    assert!(
        list[0].get("paused").is_none(),
        "unpaused route must omit the paused key: {list}"
    );

    // Resume of an unpaused route: 200, was_paused = false.
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/resume?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body_json(r).await["was_paused"], json!(false));

    // Unknown id → 404.
    let bogus = Uuid::now_v7();
    for verb in ["pause", "resume"] {
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/v1/routes/{bogus}/{verb}?expected_revision={revision}"),
                Some(&key),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{verb} unknown id");
    }

    // Another org's key → 404 (no cross-org pause control).
    let (other_app, other_key, _) = app_with_key().await;
    drop(other_app); // key from a different org against OUR app:
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/pause?expected_revision={revision}"),
            Some(&other_key),
            None,
        ))
        .await
        .unwrap();
    // The foreign key fails verification against this app's key store → 401;
    // a same-store foreign org would 404 via the get_route guard. Both deny.
    assert!(
        r.status() == StatusCode::NOT_FOUND || r.status() == StatusCode::UNAUTHORIZED,
        "foreign org must be denied, got {}",
        r.status()
    );

    // Anonymous → 401.
    let r = app
        .oneshot(req("POST", &format!("/v1/routes/{id}/pause"), None, None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

/// A catalog route can still be paused through the generic tenant-key path:
/// suppression is the quality-safe direction. It must not be resumed there,
/// because that would reactivate a catalog-managed automatic transformation
/// without the catalog flow's fresh owner/admin confirmation.
#[tokio::test]
async fn generic_resume_cannot_reactivate_a_catalog_route() {
    let id = Uuid::now_v7();
    let route = Route {
        id,
        name: "catalog:openai->gpt-4o-mini".into(),
        priority: 10,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["gpt-4o".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: Some("gpt-4o-mini".into()),
            auto_pause: true,
            ..Default::default()
        },
        paused: false,
    };
    let (app, key) = app_with_manual_route(route).await;

    let current = app
        .clone()
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(current.status(), StatusCode::OK);
    let revision = body_json(current).await["revision"]
        .as_i64()
        .filter(|revision| *revision >= 1)
        .expect("manual catalog row has a management revision");

    let paused = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/pause?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    assert_eq!(body_json(paused).await["paused"], json!(true));

    let resumed = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/resume?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let rejection = body_json(resumed).await;
    assert_eq!(rejection["error"]["code"], json!("route_validation_failed"));
    assert_eq!(rejection["error"]["issues"][0]["field"], json!("name"));
    assert_eq!(
        rejection["error"]["issues"][0]["code"],
        json!("reserved_name")
    );

    let after = app
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::OK);
    assert_eq!(body_json(after).await["paused"], json!(true));
}

#[tokio::test]
async fn pause_resume_require_a_current_positive_route_revision() {
    let (app, key, _served) = app_with_key().await;
    let created = app
        .clone()
        .oneshot(req(
            "POST",
            "/v1/routes",
            Some(&key),
            Some(json!({
                "name": "revision-fenced-pause",
                "when": {"model_in": ["gpt-4o"]},
                "then": {"target_model": "gpt-4o-mini"},
            })),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let id = body_json(created).await["id"].as_str().unwrap().to_string();
    let current = app
        .clone()
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    let revision = body_json(current).await["revision"]
        .as_i64()
        .filter(|revision| *revision >= 1)
        .expect("management read supplies a route revision");

    for uri in [
        format!("/v1/routes/{id}/pause"),
        format!("/v1/routes/{id}/resume?expected_revision=0"),
        format!("/v1/routes/{id}/pause?expected_revision=not-a-number"),
    ] {
        let response = app
            .clone()
            .oneshot(req("POST", &uri, Some(&key), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    }

    let stale_pause = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/pause?expected_revision={}", revision + 1),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(stale_pause.status(), StatusCode::CONFLICT);
    let still_unpaused = app
        .clone()
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert!(
        body_json(still_unpaused).await.get("paused").is_none(),
        "a stale pause must not change the current route"
    );

    let paused = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/pause?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    let stale_resume = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/resume?expected_revision={}", revision + 1),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(stale_resume.status(), StatusCode::CONFLICT);
    let still_paused = app
        .clone()
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(body_json(still_paused).await["paused"], json!(true));

    let resumed = app
        .oneshot(req(
            "POST",
            &format!("/v1/routes/{id}/resume?expected_revision={revision}"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
}

/// POST /v1/routes with pause_floor_pass_rate out of (0, 1] → field-addressed 422.
#[tokio::test]
async fn create_rejects_invalid_auto_pause_config() {
    let (app, key, _) = app_with_key().await;
    let spec = json!({
        "name": "bad-floor",
        "when": {"model_in":["gpt-4o"]},
        "then": {"target_model":"gpt-4o-mini", "auto_pause": true, "pause_floor_pass_rate": 1.5}
    });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let spec = json!({
        "name": "bad-min",
        "when": {"model_in":["gpt-4o"]},
        "then": {"target_model":"gpt-4o-mini", "pause_min_verdicts": 0}
    });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Valid auto-pause config is accepted.
    let spec = json!({
        "name": "good",
        "when": {"model_in":["gpt-4o"]},
        "then": {"target_model":"gpt-4o-mini", "auto_pause": true,
                  "pause_floor_pass_rate": 0.85, "pause_min_verdicts": 10}
    });
    let r = app
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
}

/// POST /v1/routes with a malformed output-shaping cap → 422; valid caps and
/// the minify flag are accepted (research Phase 3.1/3.2 config validation).
#[tokio::test]
async fn create_rejects_invalid_output_shaping_config() {
    let (app, key, _) = app_with_key().await;
    // A "high" effort cap is a no-op lie → rejected.
    let spec = json!({
        "name": "bad-effort-cap",
        "when": {"model_in":["gpt-4o"]},
        "then": {"target_model":"gpt-4o-mini", "reasoning_max_effort": "high"}
    });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // A thinking budget below Anthropic's 1024 floor → rejected.
    let spec = json!({
        "name": "bad-budget-cap",
        "when": {"model_in":["gpt-4o"]},
        "then": {"target_model":"gpt-4o-mini", "reasoning_budget_tokens": 512}
    });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Valid output-shaping config is accepted.
    let spec = json!({
        "name": "good-shaping",
        "when": {"model_in":["gpt-4o"]},
        "then": {"target_model":"gpt-4o-mini", "minify_json": true,
                  "reasoning_max_effort": "low", "reasoning_budget_tokens": 8192}
    });
    let r = app
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
}

/// GET /v1/routes/:id/savings returns every tax line as its own field (the
/// tax is never silently subtracted), plus the paused flag; an unconfigured
/// source → 503; a route with no in-window traffic → an honest all-zero body
/// (not 404); anonymous → 401; unknown route → 404.
#[tokio::test]
async fn savings_endpoint_shape() {
    use tt_core::route_savings::{assemble, InMemoryRouteSavingsSource, RouteSavingsSource};

    // Unconfigured source → 503.
    let (app, key, _) = app_with_key().await;
    let spec = json!({ "name": "downgrade", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec.clone())))
        .await
        .unwrap();
    let id = body_json(r).await["id"].as_str().unwrap().to_string();
    let r = app
        .oneshot(req(
            "GET",
            &format!("/v1/routes/{id}/savings"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "no source wired → 503"
    );

    // Configured source: seed legacy and signed values independently. The
    // signed fields deliberately retain a row-level regression that the
    // legacy positive-only gross cannot represent.
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served: Arc::clone(&served),
    }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);
    let routing = Arc::new(CachingRoutingStore::new(
        Arc::new(InMemoryRoutingStore::new()) as Arc<dyn RoutingStore>,
    ));
    let source = InMemoryRouteSavingsSource::new();
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing)
            .with_route_savings(Arc::new(source.clone()) as Arc<dyn RouteSavingsSource>),
    );
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    let created = body_json(r).await;
    let id = created["id"].as_str().unwrap().to_string();
    let route_id: Uuid = id.parse().unwrap();
    let mut seeded = assemble(route_id, 42, 0.10, 0.25, 0.05, 3, 30, 20, 8, 2);
    seeded.cache_bust_usd = 0.04;
    seeded.positive_estimated_savings_usd = 0.10;
    seeded.estimated_regressions_usd = 0.25;
    seeded.summarizer_tax_usd = 0.07;
    seeded.net_estimated_savings_usd = -0.45;
    source.set_for_org(org, vec![seeded]);

    let r = app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/routes/{id}/savings?hours=24"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = body_json(r).await;
    assert_eq!(body["route_id"].as_str().unwrap(), id);
    assert_eq!(body["paused"], json!(false));
    assert_eq!(body["requests"], json!(42));
    // Every tax line is its OWN field — never silently subtracted.
    assert!((body["gross_saved_usd"].as_f64().unwrap() - 0.10).abs() < 1e-12);
    assert!((body["judge_tax_usd"].as_f64().unwrap() - 0.25).abs() < 1e-12);
    assert!((body["shadow_tax_usd"].as_f64().unwrap() - 0.05).abs() < 1e-12);
    assert!((body["cache_bust_usd"].as_f64().unwrap() - 0.04).abs() < 1e-12);
    // Net may be NEGATIVE at the aggregate level (a regressing route must show it).
    assert!(
        (body["net_saved_usd"].as_f64().unwrap() - (-0.20)).abs() < 1e-12,
        "negative net must survive: {body}"
    );
    // The new signed estimate retains a request-level regression and records
    // known row taxes as transparent components rather than losing them to a
    // gross floor.
    assert!((body["positive_estimated_savings_usd"].as_f64().unwrap() - 0.10).abs() < 1e-12);
    assert!((body["estimated_regressions_usd"].as_f64().unwrap() - 0.25).abs() < 1e-12);
    assert!((body["summarizer_tax_usd"].as_f64().unwrap() - 0.07).abs() < 1e-12);
    assert!(
        (body["net_estimated_savings_usd"].as_f64().unwrap() - (-0.45)).abs() < 1e-12,
        "signed regression must survive: {body}"
    );
    assert_eq!(body["unmetered_tax_rows"], json!(3));
    assert!((body["verdicts"]["pass_rate"].as_f64().unwrap() - (20.0 / 28.0)).abs() < 1e-12);
    assert!(body["window_start"].is_string() && body["window_end"].is_string());

    // A route with NO in-window rows → honest all-zero body, not 404.
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            "/v1/routes",
            Some(&key),
            Some(json!({ "name": "idle", "when": {}, "then": {"target_model":"gpt-4o-mini"} })),
        ))
        .await
        .unwrap();
    let idle_id = body_json(r).await["id"].as_str().unwrap().to_string();
    let r = app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/routes/{idle_id}/savings"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = body_json(r).await;
    assert_eq!(body["requests"], json!(0));
    assert_eq!(body["net_saved_usd"].as_f64().unwrap(), 0.0);
    assert_eq!(body["net_estimated_savings_usd"].as_f64().unwrap(), 0.0);
    assert_eq!(
        body["positive_estimated_savings_usd"].as_f64().unwrap(),
        0.0
    );
    assert_eq!(body["estimated_regressions_usd"].as_f64().unwrap(), 0.0);
    assert_eq!(body["summarizer_tax_usd"].as_f64().unwrap(), 0.0);
    assert!(body["verdicts"]["pass_rate"].is_null());

    // Unknown route id → 404; anonymous → 401.
    let bogus = Uuid::now_v7();
    let r = app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/routes/{bogus}/savings"),
            Some(&key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = app
        .oneshot(req("GET", &format!("/v1/routes/{id}/savings"), None, None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn created_route_applies_immediately_without_ttl_wait() {
    let (app, key, served) = app_with_key().await;
    let spec = json!({ "name": "downgrade", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let chat =
        json!({ "model": "gpt-4o", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let r = app
        .oneshot(req("POST", "/v1/chat/completions", Some(&key), Some(chat)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // Cache was invalidated on create → the brand-new route applied on the very next request.
    assert_eq!(
        served.lock().unwrap().clone(),
        vec!["gpt-4o-mini".to_string()]
    );
}
