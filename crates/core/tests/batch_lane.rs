//! End-to-end (hermetic) tests for the Batch Lane gateway endpoints (slice 2):
//! `POST /v1/files`, `POST /v1/batches`, `GET /v1/batches/{id}`,
//! `POST /v1/batches/{id}/cancel`, `GET /v1/files/{id}/content`.
//!
//! The upstream OpenAI Batch API is mocked with [`httpmock`]; the per-org
//! credential store points the concrete `OpenAiProvider` (registered via
//! `register_openai`, `allow_local`) at the mock. Persistence uses the in-memory
//! `batch_jobs` store, so no DB is required. A separate `#[ignore]`d test
//! (`batch_jobs_pg.rs`) exercises the Postgres store against the migrated schema.
//!
//! Covered:
//!   * unauthenticated → 401 (no `tt_live_*` key);
//!   * `POST /v1/batches` persists an org-scoped row + returns the `Batch`;
//!   * `GET /v1/batches/{id}` returns the stored row (with on-demand refresh);
//!   * another org's batch id → 404 (org-scoping);
//!   * `POST /v1/batches/{id}/cancel` updates the row's status;
//!   * `POST /v1/files` proxies the multipart upload → a complete OpenAI
//!     `FileObject` (`id, object, bytes, created_at, filename, purpose, status`);
//!   * `GET /v1/files/{id}/content` proxies the result/error JSONL bytes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use httpmock::prelude::*;
use serde_json::{json, Value};
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, InMemoryProviderCredentialStore, KeyStore, ProviderCredentialStore,
};
use tt_core::{build_router, AppState, InMemoryBatchStore, ProviderRegistry};
use tt_provider_openai::{ClientConfig, OpenAiProvider};
use tt_shared::context::{ProviderCredentials, SecretString};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

// ── Harness ──────────────────────────────────────────────────────────────────

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System)
        .await
        .unwrap()
        .plaintext
}

struct Harness {
    app: axum::Router,
    key: String,
    /// A second org's key — used to prove cross-org 404 isolation.
    other_key: String,
}

/// Build the gateway app wired for the Batch Lane, with the upstream OpenAI
/// base_url pointed at `mock_base_url` for `org`'s `openai` credential.
async fn harness(mock_base_url: &str) -> Harness {
    let mut registry = ProviderRegistry::new();
    // Concrete OpenAI adapter (allow_local → bypass the SSRF guard for the local
    // mock). `register_openai` registers the dispatch clone AND the concrete
    // handle the Batch Lane needs.
    registry.register_openai(Arc::new(OpenAiProvider::new_allow_local(
        ClientConfig::default(),
    )));

    let key_raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let other_org = Uuid::now_v7();
    let key = issue_key(&key_raw, org).await;
    let other_key = issue_key(&key_raw, other_org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(key_raw);

    // Per-org credential store: both orgs have an OpenAI key whose base_url is
    // the mock, so credential resolution + upstream targeting both work.
    let creds = InMemoryProviderCredentialStore::new();
    for o in [org, other_org] {
        creds.insert(
            o,
            "openai",
            ProviderCredentials {
                api_key: SecretString::new("sk-test"),
                base_url: Some(mock_base_url.to_string()),
                extra_headers: vec![],
            },
        );
    }
    let cred_store: Arc<dyn ProviderCredentialStore> = Arc::new(creds);

    let batch_store = Arc::new(InMemoryBatchStore::new());

    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(cred_store)
            .with_batch_store(batch_store),
    );
    Harness {
        app,
        key,
        other_key,
    }
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

fn batch_body(id: &str, status: &str, with_output: bool) -> Value {
    let mut obj = json!({
        "id": id,
        "object": "batch",
        "endpoint": "/v1/chat/completions",
        "input_file_id": "file-input123",
        "completion_window": "24h",
        "status": status,
        "request_counts": { "total": 100, "completed": 0, "failed": 0 }
    });
    if with_output {
        obj["output_file_id"] = json!("file-output456");
        obj["error_file_id"] = json!("file-error789");
    }
    obj
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn unauthenticated_create_batch_is_401() {
    let server = MockServer::start();
    let h = harness(&server.base_url()).await;
    // No Authorization header at all → 401 (require_org rejects anonymous).
    let r = h
        .app
        .oneshot(req(
            "POST",
            "/v1/batches",
            None,
            Some(json!({
                "input_file_id": "file-input123",
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_batch_persists_org_scoped_row_and_returns_batch() {
    let server = MockServer::start();
    let create_mock = server.mock(|when, then| {
        when.method(POST).path("/batches").json_body(json!({
            "input_file_id": "file-input123",
            "endpoint": "/v1/chat/completions",
            "completion_window": "24h"
        }));
        then.status(200)
            .header("content-type", "application/json")
            .json_body(batch_body("batch_abc123", "validating", false));
    });
    let h = harness(&server.base_url()).await;

    let r = h
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/v1/batches",
            Some(&h.key),
            Some(json!({
                "input_file_id": "file-input123",
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    create_mock.assert();
    let v = body_json(r).await;
    assert_eq!(v["id"], "batch_abc123");
    assert_eq!(v["object"], "batch");
    assert_eq!(v["status"], "validating");
    assert_eq!(v["endpoint"], "/v1/chat/completions");

    // GET returns the persisted row. The batch is non-terminal, so the handler
    // does a best-effort upstream refresh — answer that here too.
    let get_mock = server.mock(|when, then| {
        when.method(GET).path("/batches/batch_abc123");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(batch_body("batch_abc123", "in_progress", false));
    });
    let r = h
        .app
        .clone()
        .oneshot(req("GET", "/v1/batches/batch_abc123", Some(&h.key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    get_mock.assert();
    let v = body_json(r).await;
    assert_eq!(v["id"], "batch_abc123");
    // On-demand refresh adopted the fresher upstream status.
    assert_eq!(v["status"], "in_progress");
}

#[tokio::test]
async fn another_orgs_batch_id_is_404() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/batches");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(batch_body("batch_org_a", "validating", false));
    });
    let h = harness(&server.base_url()).await;

    // org A creates a batch.
    let r = h
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/v1/batches",
            Some(&h.key),
            Some(json!({
                "input_file_id": "file-input123",
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    // org B GETs org A's batch id → 404 (org-scoped read finds no row).
    let r = h
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/v1/batches/batch_org_a",
            Some(&h.other_key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // org A still sees it.
    let r = h
        .app
        .oneshot(req("GET", "/v1/batches/batch_org_a", Some(&h.key), None))
        .await
        .unwrap();
    // org A's GET refresh has no mock for batch_org_a's GET — but a refresh
    // failure is non-fatal, so the stored row is still returned 200.
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn list_batches_is_org_scoped_and_returns_list_envelope() {
    let server = MockServer::start();
    let h = harness(&server.base_url()).await;

    // Create two batches for org A and one for org B. Each create answers from
    // the same `/batches` mock keyed by the request's input_file_id-independent
    // id field; here we just return a distinct id per call via separate mocks.
    let create = |id: &'static str| {
        server.mock(move |when, then| {
            when.method(POST).path("/batches").json_body(json!({
                "input_file_id": id,
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(batch_body(id, "validating", false));
        })
    };
    let _m_a1 = create("batch_a1");
    let _m_a2 = create("batch_a2");
    let _m_b1 = create("batch_b1");

    let submit = |key: &str, id: &str| {
        req(
            "POST",
            "/v1/batches",
            Some(key),
            Some(json!({
                "input_file_id": id,
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            })),
        )
    };
    for id in ["batch_a1", "batch_a2"] {
        let r = h.app.clone().oneshot(submit(&h.key, id)).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
    let r = h
        .app
        .clone()
        .oneshot(submit(&h.other_key, "batch_b1"))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    // org A's list: exactly its two batches, in the OpenAI list envelope. Each
    // item matches the single-get shape (object:"batch").
    let r = h
        .app
        .clone()
        .oneshot(req("GET", "/v1/batches", Some(&h.key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["object"], "list");
    let data = v["data"].as_array().expect("data array");
    assert_eq!(data.len(), 2, "org A sees exactly its two batches");
    let ids: std::collections::HashSet<&str> =
        data.iter().map(|b| b["id"].as_str().unwrap()).collect();
    assert!(ids.contains("batch_a1") && ids.contains("batch_a2"));
    // Items carry the single-get shape (never org B's batch).
    assert!(data.iter().all(|b| b["object"] == "batch"));
    assert!(!ids.contains("batch_b1"), "must never leak org B's batch");

    // org B's list: only its one batch.
    let r = h
        .app
        .clone()
        .oneshot(req("GET", "/v1/batches", Some(&h.other_key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["id"], "batch_b1");
}

#[tokio::test]
async fn unauthenticated_list_batches_is_401() {
    let server = MockServer::start();
    let h = harness(&server.base_url()).await;
    let r = h
        .app
        .oneshot(req("GET", "/v1/batches", None, None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cancel_updates_stored_status() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/batches");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(batch_body("batch_cancel", "in_progress", false));
    });
    let cancel_mock = server.mock(|when, then| {
        when.method(POST).path("/batches/batch_cancel/cancel");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(batch_body("batch_cancel", "cancelling", false));
    });
    let h = harness(&server.base_url()).await;

    h.app
        .clone()
        .oneshot(req(
            "POST",
            "/v1/batches",
            Some(&h.key),
            Some(json!({
                "input_file_id": "file-input123",
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            })),
        ))
        .await
        .unwrap();

    let r = h
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/v1/batches/batch_cancel/cancel",
            Some(&h.key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    cancel_mock.assert();
    let v = body_json(r).await;
    assert_eq!(v["status"], "cancelling");

    // The persisted row now reflects the cancel. `cancelling` is non-terminal,
    // so GET attempts a refresh — answer it with the same status.
    let get_mock = server.mock(|when, then| {
        when.method(GET).path("/batches/batch_cancel");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(batch_body("batch_cancel", "cancelling", false));
    });
    let r = h
        .app
        .oneshot(req("GET", "/v1/batches/batch_cancel", Some(&h.key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    get_mock.assert();
    assert_eq!(body_json(r).await["status"], "cancelling");
}

#[tokio::test]
async fn cancel_unknown_id_is_404() {
    let server = MockServer::start();
    let h = harness(&server.base_url()).await;
    let r = h
        .app
        .oneshot(req(
            "POST",
            "/v1/batches/does-not-exist/cancel",
            Some(&h.key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_file_proxies_multipart_and_returns_file_id() {
    let server = MockServer::start();
    let upload_mock = server.mock(|when, then| {
        when.method(POST).path("/files");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({
                "id": "file-input123",
                "object": "file",
                "bytes": 42,
                "purpose": "batch"
            }));
    });
    let h = harness(&server.base_url()).await;

    // Build a minimal multipart body by hand (purpose + file parts).
    let b = "----ttboundary";
    let data = r#"{"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{}}"#;
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n\
         --{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"batch.jsonl\"\r\n\
         Content-Type: application/jsonl\r\n\r\n{data}\r\n--{b}--\r\n"
    );
    let boundary = b;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/files")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("authorization", format!("Bearer {}", h.key))
        .body(Body::from(body))
        .unwrap();

    let r = h.app.oneshot(request).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    upload_mock.assert();
    let v = body_json(r).await;
    // The gateway re-emits its OWN complete OpenAI FileObject (not the upstream
    // body): the id is the upstream file id, but bytes/created_at/filename/status
    // are filled by the gateway so the OpenAI SDK's `FileObject` parses.
    assert_eq!(v["id"], "file-input123");
    assert_eq!(v["object"], "file");
    assert_eq!(v["purpose"], "batch");
    assert_eq!(v["status"], "processed");
    // `bytes` is the byte length of the uploaded `file` part (the JSONL body).
    assert_eq!(v["bytes"], data.len());
    // The multipart filename round-trips into `filename`.
    assert_eq!(v["filename"], "batch.jsonl");
    // `created_at` is a unix timestamp (a positive integer).
    assert!(v["created_at"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn download_file_content_proxies_jsonl_bytes() {
    let server = MockServer::start_async().await;
    let create_mock = server.mock(|when, then| {
        when.method(POST).path("/batches");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(batch_body("batch_download", "completed", true));
    });
    let jsonl = "{\"custom_id\":\"req-1\",\"response\":{\"status_code\":200}}\n";
    let download_mock = server.mock(|when, then| {
        when.method(GET).path("/files/file-output456/content");
        then.status(200)
            .header("content-type", "application/jsonl")
            .body(jsonl);
    });
    let h = harness(&server.base_url()).await;

    let create = h
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/v1/batches",
            Some(&h.key),
            Some(json!({
                "input_file_id": "file-input123",
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::OK);
    create_mock.assert();

    // A second TokenTrimmer organization can share the same upstream account,
    // so the provider credential is not sufficient ownership. The local
    // batch-row predicate must reject this exact file before upstream I/O.
    let foreign = h
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/v1/files/file-output456/content",
            Some(&h.other_key),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    download_mock.assert_calls(0);

    let r = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        h.app.oneshot(req(
            "GET",
            "/v1/files/file-output456/content",
            Some(&h.key),
            None,
        )),
    )
    .await
    .expect("gateway response headers timed out")
    .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        axum::body::to_bytes(r.into_body(), usize::MAX),
    )
    .await
    .expect("gateway response body timed out")
    .unwrap();
    download_mock.assert();
    assert_eq!(std::str::from_utf8(&bytes).unwrap(), jsonl);
}
