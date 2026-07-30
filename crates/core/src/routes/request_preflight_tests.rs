use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use chrono::TimeZone;
use tt_auth::{CredentialError, InMemoryProviderCredentialStore, ProviderCredentialStore};
use tt_shared::{context::ProviderCredentials, Capability};

use super::*;
use crate::{
    registry::{register_providers, ProvidersConfig},
    ProviderRegistry,
};

struct ExistenceOnlyCredentialStore {
    snapshot_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ProviderCredentialStore for ExistenceOnlyCredentialStore {
    async fn is_configured(
        &self,
        _org_id: uuid::Uuid,
        _provider_id: &str,
    ) -> Result<bool, CredentialError> {
        panic!("request preflight must use the secret-free batch snapshot seam")
    }

    async fn configured_snapshot(
        &self,
        _org_id: uuid::Uuid,
        provider_ids: &[String],
    ) -> Result<HashSet<String>, CredentialError> {
        self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
        Ok(provider_ids.iter().cloned().collect())
    }

    async fn get(
        &self,
        _org_id: uuid::Uuid,
        _provider_id: &str,
    ) -> Result<Option<ProviderCredentials>, CredentialError> {
        panic!("request preflight must not fetch or decrypt credential material")
    }
}

fn existence_only_store() -> (Arc<dyn ProviderCredentialStore>, Arc<AtomicUsize>) {
    let snapshot_calls = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(ExistenceOnlyCredentialStore {
            snapshot_calls: snapshot_calls.clone(),
        }),
        snapshot_calls,
    )
}

fn openai_state(credential_store: Option<Arc<dyn ProviderCredentialStore>>) -> AppState {
    let mut registry = ProviderRegistry::new();
    let mut providers = ProvidersConfig::none();
    providers.openai = true;
    register_providers(&mut registry, &providers);
    let state = AppState::new(registry);
    if let Some(store) = credential_store {
        state.with_credential_store(store)
    } else {
        state
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

#[tokio::test]
async fn exact_catalog_and_credential_record_stay_short_of_readiness() {
    let org_id = uuid::Uuid::now_v7();
    let (store, snapshot_calls) = existence_only_store();
    let document = build_document(
        &openai_state(Some(store)),
        org_id,
        request(),
        Utc.with_ymd_and_hms(2026, 7, 26, 12, 0, 0).unwrap(),
    )
    .await
    .expect("request preflight");

    assert_eq!(snapshot_calls.load(Ordering::SeqCst), 1);
    assert_eq!(document.provider_resolution.state, "exact_catalog_match");
    assert_eq!(document.credential.state, "configured");
    assert_eq!(document.model_support.state, "supported_by_catalog");
    assert_eq!(document.catalog_limits.state, "within_catalog_metadata");
    assert_eq!(document.catalog_cost.state, "catalog_projection");
    assert_eq!(
        document.catalog_cost.source,
        "registered_provider_pricing_catalog"
    );
    assert_eq!(document.catalog_cost.input_tokens_low, Some(1_024));
    assert_eq!(document.catalog_cost.input_tokens_high, Some(1_024));
    assert_eq!(document.catalog_cost.output_tokens_low, Some(0));
    assert_eq!(document.catalog_cost.output_tokens_high, Some(4_096));
    assert!(
        document.catalog_cost.standard_cost_usd_high > document.catalog_cost.standard_cost_usd_low
    );
    assert_eq!(document.provider_health.state, "unknown");
    assert_eq!(document.request_acceptance.state, "unknown");
    assert_eq!(
        document
            .actions
            .iter()
            .filter(|action| action.required_before_request)
            .count(),
        0
    );
}

#[tokio::test]
async fn batch_keeps_order_and_one_responder_timestamp_without_execution_claims() {
    let mut second = request();
    second.model = "gpt-4o".into();
    second.required_capabilities = vec![Capability::Text, Capability::Streaming];
    second.declared_input_tokens = None;
    let generated_at = Utc.with_ymd_and_hms(2026, 7, 27, 18, 0, 0).unwrap();
    let batch_request = RequestPreflightBatchRequest {
        schema_version: REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION,
        requests: vec![request(), second],
    };

    let (store, snapshot_calls) = existence_only_store();
    let document = build_batch_document(
        &openai_state(Some(store)),
        uuid::Uuid::now_v7(),
        batch_request.clone(),
        generated_at,
    )
    .await
    .expect("request preflight batch");

    assert_eq!(
        snapshot_calls.load(Ordering::SeqCst),
        1,
        "the complete role batch must capture credential presence once"
    );
    assert_eq!(document.request, batch_request);
    assert_eq!(document.scope, REQUEST_PREFLIGHT_BATCH_SCOPE);
    assert_eq!(document.documents.len(), 2);
    assert_eq!(document.documents[0].request.model, "gpt-4o-mini");
    assert_eq!(document.documents[1].request.model, "gpt-4o");
    assert!(document
        .documents
        .iter()
        .all(|nested| nested.generated_at == document.generated_at));
    assert_eq!(
        document.limitations[0].code,
        "preflight_batch_single_responder_not_atomic"
    );
    assert_eq!(
        document.limitations[1].code,
        "preflight_batch_provider_execution_not_observed"
    );
}

#[tokio::test]
async fn batch_rejects_empty_or_oversized_declaration_sets() {
    let state = openai_state(None);
    for requests in [
        Vec::new(),
        vec![request(); REQUEST_PREFLIGHT_BATCH_MAX_REQUESTS + 1],
    ] {
        let result = build_batch_document(
            &state,
            uuid::Uuid::now_v7(),
            RequestPreflightBatchRequest {
                schema_version: REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION,
                requests,
            },
            Utc::now(),
        )
        .await;
        assert!(matches!(result, Err(ApiError::InvalidRequest(_))));
    }
}

#[tokio::test]
async fn known_catalog_and_credential_blockers_get_stable_actions() {
    let mut request = request();
    request.required_capabilities.push(Capability::Audio);
    request.requested_max_output_tokens = Some(16_001);
    let document = build_document(
        &openai_state(Some(Arc::new(InMemoryProviderCredentialStore::new()))),
        uuid::Uuid::now_v7(),
        request,
        Utc::now(),
    )
    .await
    .expect("blocked preflight");

    assert_eq!(document.credential.state, "missing");
    assert_eq!(
        document.model_support.missing_capabilities,
        vec![Capability::Audio]
    );
    assert_eq!(document.catalog_limits.state, "exceeds_catalog_metadata");
    let codes = document
        .actions
        .iter()
        .filter(|action| action.required_before_request)
        .map(|action| action.code.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        codes,
        [
            "configure_provider_credential",
            "change_model_or_required_capabilities",
            "reduce_declared_tokens_or_choose_model",
        ]
    );
}

#[tokio::test]
async fn unlisted_model_keeps_support_limits_and_credential_unknown() {
    let mut unlisted_request = request();
    unlisted_request.model = "custom-model-without-provider".into();
    let document = build_document(
        &openai_state(None),
        uuid::Uuid::now_v7(),
        unlisted_request,
        Utc::now(),
    )
    .await
    .expect("unknown preflight");

    assert_eq!(document.provider_resolution.state, "unresolved");
    assert_eq!(document.credential.state, "unknown");
    assert_eq!(document.model_support.state, "unknown");
    assert_eq!(document.catalog_limits.state, "unknown");
    assert_eq!(document.catalog_cost.state, "unknown");
    assert!(document.catalog_cost.standard_cost_usd_high.is_none());
    assert_eq!(
        document.actions[0].code,
        "choose_registered_provider_or_model"
    );

    let mut oversized = request();
    oversized.declared_input_tokens = Some(REQUEST_PREFLIGHT_TOKEN_VALUE_MAX + 1);
    assert!(build_document(
        &openai_state(None),
        uuid::Uuid::now_v7(),
        oversized,
        Utc::now(),
    )
    .await
    .is_err());
}

#[tokio::test]
async fn undeclared_tokens_get_an_explicit_catalog_envelope_not_a_quote() {
    let mut declaration = request();
    declaration.declared_input_tokens = None;
    declaration.requested_max_output_tokens = None;
    let document = build_document(
        &openai_state(None),
        uuid::Uuid::now_v7(),
        declaration,
        Utc::now(),
    )
    .await
    .expect("catalog envelope");

    assert_eq!(document.catalog_cost.state, "catalog_projection");
    assert_eq!(document.catalog_cost.input_tokens_low, Some(0));
    assert_eq!(
        document.catalog_cost.input_tokens_high,
        document.catalog_limits.catalog_max_input_tokens
    );
    assert_eq!(document.catalog_cost.output_tokens_low, Some(0));
    assert_eq!(
        document.catalog_cost.output_tokens_high,
        document.catalog_limits.catalog_max_output_tokens
    );
    assert_eq!(document.catalog_cost.standard_cost_usd_low, Some(0.0));
    assert_eq!(
        document.catalog_cost.reason.code,
        "preflight_standard_cost_catalog_projection"
    );
}
