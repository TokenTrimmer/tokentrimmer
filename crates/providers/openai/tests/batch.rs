//! Integration tests for the OpenAI Batch API client using [`httpmock`].
//!
//! Each test spins up a local mock HTTP server, points the adapter at it via
//! `base_url`, and verifies the Batch client methods: file upload, batch
//! create/get/cancel, and result-file download — including error mapping that
//! reuses the existing provider error type.

use httpmock::prelude::*;
use tt_provider_openai::{Batch, BatchStatus, ClientConfig, OpenAiProvider};
use tt_shared::{
    context::{ProviderCredentials, RequestContext, SecretString},
    ProviderError,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_ctx(base_url: &str) -> RequestContext {
    RequestContext {
        trace_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("test-key"),
            base_url: Some(base_url.to_string()),
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
        run_id: None,
        node_id: None,
    }
}

fn provider() -> OpenAiProvider {
    // Tests use a local httpmock server — allow_local bypasses the SSRF guard.
    OpenAiProvider::new_allow_local(ClientConfig::default())
}

fn batch_body(status: &str, with_output: bool) -> String {
    let mut obj = serde_json::json!({
        "id": "batch_abc123",
        "object": "batch",
        "endpoint": "/v1/chat/completions",
        "input_file_id": "file-input123",
        "completion_window": "24h",
        "status": status,
        "request_counts": {
            "total": 100,
            "completed": 95,
            "failed": 5
        }
    });
    if with_output {
        obj["output_file_id"] = serde_json::json!("file-output456");
        obj["error_file_id"] = serde_json::json!("file-error789");
    }
    obj.to_string()
}

// ---------------------------------------------------------------------------
// 1. upload_batch_input — multipart upload returns the file id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_batch_input_returns_file_id() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/files");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(
                serde_json::json!({
                    "id": "file-input123",
                    "object": "file",
                    "bytes": 42,
                    "created_at": 1716681600_i64,
                    "filename": "batch.jsonl",
                    "purpose": "batch"
                })
                .to_string(),
            );
    });

    let ctx = make_ctx(&server.base_url());
    let jsonl = br#"{"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{}}"#;

    let file_id = provider()
        .upload_batch_input(jsonl.to_vec(), &ctx)
        .await
        .expect("upload should succeed");

    assert_eq!(file_id, "file-input123");
}

// ---------------------------------------------------------------------------
// 2. create_batch — request body shape + returns the Batch object
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_batch_sends_correct_body_and_returns_batch() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/batches")
            .json_body(serde_json::json!({
                "input_file_id": "file-input123",
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            }));
        then.status(200)
            .header("Content-Type", "application/json")
            .body(batch_body("validating", false));
    });

    let ctx = make_ctx(&server.base_url());

    let batch: Batch = provider()
        .create_batch("file-input123", "/v1/chat/completions", "24h", &ctx)
        .await
        .expect("create should succeed");

    assert_eq!(batch.id, "batch_abc123");
    assert_eq!(batch.status, BatchStatus::Validating);
    assert_eq!(batch.input_file_id.as_deref(), Some("file-input123"));
    assert_eq!(batch.endpoint.as_deref(), Some("/v1/chat/completions"));
    assert_eq!(batch.request_counts.total, 100);
    assert_eq!(batch.request_counts.completed, 95);
    assert_eq!(batch.request_counts.failed, 5);
}

// ---------------------------------------------------------------------------
// 3. get_batch — in_progress status maps correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_batch_in_progress() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(GET).path("/batches/batch_abc123");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(batch_body("in_progress", false));
    });

    let ctx = make_ctx(&server.base_url());

    let batch = provider()
        .get_batch("batch_abc123", &ctx)
        .await
        .expect("get should succeed");

    assert_eq!(batch.status, BatchStatus::InProgress);
    assert!(batch.output_file_id.is_none());
}

// ---------------------------------------------------------------------------
// 4. get_batch — completed with output_file_id + error_file_id present
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_batch_completed_with_output_file() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(GET).path("/batches/batch_abc123");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(batch_body("completed", true));
    });

    let ctx = make_ctx(&server.base_url());

    let batch = provider()
        .get_batch("batch_abc123", &ctx)
        .await
        .expect("get should succeed");

    assert_eq!(batch.status, BatchStatus::Completed);
    assert_eq!(batch.output_file_id.as_deref(), Some("file-output456"));
    assert_eq!(batch.error_file_id.as_deref(), Some("file-error789"));
}

// ---------------------------------------------------------------------------
// 5. All terminal/transient statuses map to the right enum variant
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batch_status_mapping_all_variants() {
    let cases = [
        ("validating", BatchStatus::Validating),
        ("in_progress", BatchStatus::InProgress),
        ("finalizing", BatchStatus::Finalizing),
        ("completed", BatchStatus::Completed),
        ("failed", BatchStatus::Failed),
        ("expired", BatchStatus::Expired),
        ("cancelling", BatchStatus::Cancelling),
        ("cancelled", BatchStatus::Cancelled),
    ];

    for (wire, expected) in cases {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/batches/batch_abc123");
            then.status(200)
                .header("Content-Type", "application/json")
                .body(batch_body(wire, false));
        });

        let ctx = make_ctx(&server.base_url());
        let batch = provider()
            .get_batch("batch_abc123", &ctx)
            .await
            .expect("get should succeed");

        assert_eq!(batch.status, expected, "wire status {wire} mismatched");
    }
}

// ---------------------------------------------------------------------------
// 6. cancel_batch — POST .../cancel returns the (cancelling) Batch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_batch_returns_batch() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/batches/batch_abc123/cancel");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(batch_body("cancelling", false));
    });

    let ctx = make_ctx(&server.base_url());

    let batch = provider()
        .cancel_batch("batch_abc123", &ctx)
        .await
        .expect("cancel should succeed");

    assert_eq!(batch.id, "batch_abc123");
    assert_eq!(batch.status, BatchStatus::Cancelling);
}

// ---------------------------------------------------------------------------
// 7. download_file_content — returns the raw result JSONL bytes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_file_content_returns_bytes() {
    let server = MockServer::start();

    let jsonl = "{\"custom_id\":\"req-1\",\"response\":{\"status_code\":200}}\n";

    let _mock = server.mock(|when, then| {
        when.method(GET).path("/files/file-output456/content");
        then.status(200)
            .header("Content-Type", "application/octet-stream")
            .body(jsonl);
    });

    let ctx = make_ctx(&server.base_url());

    let bytes = provider()
        .download_file_content("file-output456", &ctx)
        .await
        .expect("download should succeed");

    assert_eq!(bytes.as_ref(), jsonl.as_bytes());
}

// ---------------------------------------------------------------------------
// 8. Error mapping — 401 on upload → Unauthorized (reuses provider error type)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_401_unauthorized() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/files");
        then.status(401).header("Content-Type", "application/json").body(
            r#"{"error":{"message":"Invalid API key","type":"invalid_api_key","code":"invalid_api_key","param":null}}"#,
        );
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .upload_batch_input(b"{}".to_vec(), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::Unauthorized(_)),
        "expected Unauthorized, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 9. Error mapping — 400 invalid_request_error on create → InvalidRequest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_batch_400_invalid_request() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/batches");
        then.status(400)
            .header("Content-Type", "application/json")
            .body(
                r#"{"error":{"message":"Invalid input_file_id","type":"invalid_request_error","code":null,"param":"input_file_id"}}"#,
            );
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .create_batch("file-bad", "/v1/chat/completions", "24h", &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 10. Error mapping — 429 with Retry-After on get → RateLimited{5000}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_batch_429_rate_limited() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(GET).path("/batches/batch_abc123");
        then.status(429)
            .header("Content-Type", "application/json")
            .header("Retry-After", "5")
            .body(
                r#"{"error":{"message":"Rate limit exceeded","type":"requests","code":null,"param":null}}"#,
            );
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .get_batch("batch_abc123", &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(
            err,
            ProviderError::RateLimited {
                retry_after_ms: 5000
            }
        ),
        "expected RateLimited{{5000}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 11. Auth header is sent (Bearer test-key) on a batch call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batch_call_sends_bearer_auth() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(GET)
            .path("/batches/batch_abc123")
            .header("Authorization", "Bearer test-key");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(batch_body("completed", true));
    });

    let ctx = make_ctx(&server.base_url());
    let batch = provider()
        .get_batch("batch_abc123", &ctx)
        .await
        .expect("get should succeed with auth header");

    assert_eq!(batch.status, BatchStatus::Completed);
}

// ---------------------------------------------------------------------------
// 12. download_file_content — 401 maps to Unauthorized
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_file_content_401_unauthorized() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(GET).path("/files/file-x/content");
        then.status(401).header("Content-Type", "application/json").body(
            r#"{"error":{"message":"Invalid API key","type":"invalid_api_key","code":"invalid_api_key","param":null}}"#,
        );
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .download_file_content("file-x", &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::Unauthorized(_)),
        "expected Unauthorized, got {err:?}"
    );
}
