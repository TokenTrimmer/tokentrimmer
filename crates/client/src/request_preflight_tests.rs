use httpmock::prelude::*;
use httpmock::Then;

use super::*;

fn reason(code: &str) -> CapabilityReason {
    CapabilityReason {
        code: code.into(),
        message: "Bounded responder explanation.".into(),
    }
}

fn request() -> RequestPreflightRequest {
    RequestPreflightRequest {
        schema_version: REQUEST_PREFLIGHT_SCHEMA_VERSION,
        model: "gpt-4o-mini".into(),
        provider: None,
        required_capabilities: vec![Capability::Text, Capability::Tools],
        declared_input_tokens: Some(1_024),
        requested_max_output_tokens: Some(4_096),
    }
}

fn document(request: RequestPreflightRequest) -> RequestPreflightResponse {
    RequestPreflightResponse {
        schema_version: REQUEST_PREFLIGHT_SCHEMA_VERSION,
        scope: REQUEST_PREFLIGHT_SCOPE.into(),
        snapshot_scope: CAPABILITIES_SNAPSHOT_SCOPE.into(),
        generated_at: "2026-07-26T12:34:56.000Z".into(),
        request,
        provider_resolution: PreflightProviderResolution {
            state: "exact_catalog_match".into(),
            provider: Some("openai".into()),
            source: "registered_provider_catalog".into(),
            reason: reason("preflight_exact_model_match"),
        },
        credential: PreflightCredentialEvidence {
            state: "configured".into(),
            source: "organization_credential_store".into(),
            reason: reason("preflight_credential_record_configured"),
        },
        model_support: PreflightModelSupportEvidence {
            state: "supported_by_catalog".into(),
            source: "registered_provider_catalog".into(),
            missing_capabilities: Vec::new(),
            reason: reason("preflight_required_capabilities_catalog_match"),
        },
        catalog_limits: PreflightLimitEvidence {
            state: "within_catalog_metadata".into(),
            source: "registered_provider_catalog".into(),
            catalog_max_input_tokens: Some(128_000),
            catalog_max_output_tokens: Some(16_384),
            reason: reason("preflight_declared_tokens_within_catalog"),
        },
        catalog_cost: PreflightCostEvidence {
            state: "catalog_projection".into(),
            source: "registered_provider_pricing_catalog".into(),
            standard_input_rate_usd_per_million: Some(0.15),
            standard_output_rate_usd_per_million: Some(0.60),
            input_tokens_low: Some(1_024),
            input_tokens_high: Some(1_024),
            output_tokens_low: Some(0),
            output_tokens_high: Some(4_096),
            standard_cost_usd_low: Some(0.000_153_6),
            standard_cost_usd_high: Some(0.002_611_2),
            reason: reason("preflight_standard_cost_catalog_projection"),
        },
        provider_health: UnknownEvidence {
            state: "unknown".into(),
            source: "not_negotiated".into(),
            reason: reason("provider_health_not_probed"),
        },
        request_acceptance: UnknownEvidence {
            state: "unknown".into(),
            source: "not_negotiated".into(),
            reason: reason("request_acceptance_not_attempted"),
        },
        actions: vec![PreflightAction {
            code: "execute_request_and_handle_result".into(),
            required_before_request: false,
            reason: reason("preflight_action_real_request_authoritative"),
        }],
    }
}

fn headers(then: Then) -> Then {
    then.header("content-type", "application/json")
        .header("cache-control", "private, no-store")
        .header("x-content-type-options", "nosniff")
}

#[tokio::test]
async fn preflight_posts_one_exact_authenticated_declaration() {
    let server = MockServer::start_async().await;
    let request = request();
    let response = document(request.clone());
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/capabilities/preflight")
            .header("accept", "application/json")
            .header("authorization", "Bearer tt_live_test")
            .json_body_obj(&request);
        let then = headers(then);
        then.status(200).json_body_obj(&response);
    });

    let received = Client::new(server.base_url(), "tt_live_test")
        .preflight(&request)
        .await
        .expect("valid request preflight");
    assert_eq!(received.credential.state, "configured");
    assert_eq!(received.provider_health.state, "unknown");
    mock.assert();
}

#[tokio::test]
async fn preflight_batch_posts_once_and_validates_every_ordered_document() {
    let server = MockServer::start_async().await;
    let first = request();
    let mut second = request();
    second.model = "gpt-4o".into();
    let request = RequestPreflightBatchRequest {
        schema_version: REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION,
        requests: vec![first.clone(), second.clone()],
    };
    let response = RequestPreflightBatchResponse {
        schema_version: REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION,
        scope: REQUEST_PREFLIGHT_BATCH_SCOPE.into(),
        snapshot_scope: CAPABILITIES_SNAPSHOT_SCOPE.into(),
        generated_at: "2026-07-26T12:34:56.000Z".into(),
        request: request.clone(),
        documents: vec![document(first), document(second)],
        limitations: vec![
            reason("preflight_batch_single_responder_not_atomic"),
            reason("preflight_batch_provider_execution_not_observed"),
        ],
    };
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/capabilities/preflight/batch")
            .header("authorization", "Bearer tt_live_test")
            .json_body_obj(&request);
        let then = headers(then);
        then.status(200).json_body_obj(&response);
    });

    let received = Client::new(server.base_url(), "tt_live_test")
        .preflight_batch(&request)
        .await
        .expect("valid request preflight batch");
    assert_eq!(received.documents.len(), 2);
    assert_eq!(received.documents[1].request.model, "gpt-4o");
    mock.assert();
}

#[tokio::test]
async fn preflight_rejects_contradiction_and_redacts_remote_error() {
    let server = MockServer::start_async().await;
    let request = request();
    let mut response = document(request.clone());
    response.request_acceptance.state = "available".into();
    server.mock(|when, then| {
        when.method(POST).path("/v1/capabilities/preflight");
        let then = headers(then);
        then.status(200).json_body_obj(&response);
    });
    assert!(matches!(
        Client::new(server.base_url(), "tt_live_test")
            .preflight(&request)
            .await,
        Err(Error::InvalidRequestPreflight("unknown_evidence"))
    ));

    let cost_server = MockServer::start_async().await;
    let mut contradictory_cost = document(request.clone());
    contradictory_cost.catalog_cost.standard_cost_usd_high = Some(999.0);
    cost_server.mock(|when, then| {
        when.method(POST).path("/v1/capabilities/preflight");
        let then = headers(then);
        then.status(200).json_body_obj(&contradictory_cost);
    });
    assert!(matches!(
        Client::new(cost_server.base_url(), "tt_live_test")
            .preflight(&request)
            .await,
        Err(Error::InvalidRequestPreflight("catalog_cost"))
    ));

    let failed = MockServer::start_async().await;
    failed.mock(|when, then| {
        when.method(POST).path("/v1/capabilities/preflight");
        then.status(503).body("private credential diagnostic");
    });
    let error = Client::new(failed.base_url(), "tt_live_test")
        .preflight(&request)
        .await
        .expect_err("503 must fail");
    match error {
        Error::Status { body, .. } => {
            assert_eq!(body, "request preflight failed");
            assert!(!body.contains("credential"));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn preflight_rejects_unsafe_request_and_declared_oversize_before_io() {
    let no_request = MockServer::start_async().await;
    let mock = no_request.mock(|when, then| {
        when.method(POST).path("/v1/capabilities/preflight");
        then.status(200);
    });
    let mut invalid_request = request();
    invalid_request.requested_max_output_tokens = Some(0);
    assert!(matches!(
        Client::new(no_request.base_url(), "tt_live_test")
            .preflight(&invalid_request)
            .await,
        Err(Error::InvalidRequestPreflight("request_tokens"))
    ));
    assert_eq!(mock.calls(), 0);

    let oversized = MockServer::start_async().await;
    oversized.mock(|when, then| {
        when.method(POST).path("/v1/capabilities/preflight");
        let then = headers(then);
        then.status(200)
            .body("x".repeat(MAX_REQUEST_PREFLIGHT_RESPONSE_BYTES + 1));
    });
    assert!(matches!(
        Client::new(oversized.base_url(), "tt_live_test")
            .preflight(&request())
            .await,
        Err(Error::ResponseTooLarge {
            limit: MAX_REQUEST_PREFLIGHT_RESPONSE_BYTES
        })
    ));
}

#[tokio::test]
async fn preflight_refuses_redirect_without_forwarding_bearer() {
    let server = MockServer::start_async().await;
    server.mock(|when, then| {
        when.method(POST)
            .path("/v1/capabilities/preflight")
            .header("authorization", "Bearer tt_live_test");
        then.status(307).header("location", "/elsewhere");
    });
    let target = server.mock(|when, then| {
        when.method(POST).path("/elsewhere");
        then.status(200).body("{}");
    });
    let caller_http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("build caller client");

    assert!(matches!(
        Client::with_http_client(caller_http, server.base_url(), "tt_live_test")
            .preflight(&request())
            .await,
        Err(Error::UnexpectedRequestPreflightRedirect)
    ));
    assert_eq!(target.calls(), 0, "the bearer target must not be contacted");
}
