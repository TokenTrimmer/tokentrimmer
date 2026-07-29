//! Authenticated, request-specific capability preflight without provider I/O.
//!
//! This endpoint resolves only responder-owned facts. A catalog match, stored
//! credential record, and declared-token comparison are useful before a
//! request, but none validates a secret, probes a provider, reserves capacity
//! or spend, tokenizes a prompt, or proves upstream acceptance.

use std::collections::HashSet;

use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    Extension, Json,
};
use chrono::{DateTime, SecondsFormat, Utc};
use tt_auth::ApiKeyContext;
use tt_shared::{
    CapabilityReason, ModelInfo, ModelPricing, PreflightAction, PreflightCostEvidence,
    PreflightCredentialEvidence, PreflightLimitEvidence, PreflightModelSupportEvidence,
    PreflightProviderResolution, RequestPreflightBatchRequest, RequestPreflightBatchResponse,
    RequestPreflightRequest, RequestPreflightResponse, UnknownEvidence,
    CAPABILITIES_SNAPSHOT_SCOPE, REQUEST_PREFLIGHT_BATCH_MAX_REQUESTS,
    REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION, REQUEST_PREFLIGHT_BATCH_SCOPE,
    REQUEST_PREFLIGHT_SCHEMA_VERSION, REQUEST_PREFLIGHT_SCOPE, REQUEST_PREFLIGHT_TOKEN_VALUE_MAX,
};

use crate::{ApiError, ApiResult, AppState};

const MAX_MODEL_BYTES: usize = 256;
const MAX_PROVIDER_BYTES: usize = 64;
const MAX_REQUIRED_CAPABILITIES: usize = 8;

pub async fn handler(
    State(state): State<AppState>,
    context: Option<Extension<ApiKeyContext>>,
    Json(request): Json<RequestPreflightRequest>,
) -> ApiResult<Response> {
    let context = super::capabilities::require_real_key(context)?;
    let document = build_document(&state, context.org_id, request, Utc::now()).await?;
    let mut response = Json(document).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

pub async fn batch_handler(
    State(state): State<AppState>,
    context: Option<Extension<ApiKeyContext>>,
    Json(request): Json<RequestPreflightBatchRequest>,
) -> ApiResult<Response> {
    let context = super::capabilities::require_real_key(context)?;
    let document = build_batch_document(&state, context.org_id, request, Utc::now()).await?;
    let mut response = Json(document).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

pub async fn build_batch_document(
    state: &AppState,
    org_id: uuid::Uuid,
    request: RequestPreflightBatchRequest,
    generated_at: DateTime<Utc>,
) -> ApiResult<RequestPreflightBatchResponse> {
    if request.schema_version != REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION {
        return Err(ApiError::InvalidRequest(
            "unsupported request preflight batch schema_version".into(),
        ));
    }
    if request.requests.is_empty() || request.requests.len() > REQUEST_PREFLIGHT_BATCH_MAX_REQUESTS
    {
        return Err(ApiError::InvalidRequest(format!(
            "request preflight batch must contain 1 to {REQUEST_PREFLIGHT_BATCH_MAX_REQUESTS} requests"
        )));
    }

    for declaration in &request.requests {
        validate_request(declaration)?;
    }
    let resolved_providers = request
        .requests
        .iter()
        .map(|declaration| resolve_provider(state, declaration).0.provider)
        .collect::<Vec<_>>();
    let mut provider_ids = resolved_providers
        .iter()
        .filter_map(Clone::clone)
        .collect::<Vec<_>>();
    provider_ids.sort();
    provider_ids.dedup();
    let credential_snapshot =
        capture_credential_snapshot(state, org_id, provider_ids.as_slice()).await;

    let mut documents = Vec::with_capacity(request.requests.len());
    for (declaration, provider) in request
        .requests
        .iter()
        .cloned()
        .zip(resolved_providers.iter())
    {
        documents.push(build_document_with_credential(
            state,
            declaration,
            generated_at,
            credential_from_snapshot(provider.as_deref(), &credential_snapshot),
        ));
    }

    Ok(RequestPreflightBatchResponse {
        schema_version: REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION,
        scope: REQUEST_PREFLIGHT_BATCH_SCOPE.into(),
        snapshot_scope: CAPABILITIES_SNAPSHOT_SCOPE.into(),
        generated_at: generated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        request,
        documents,
        limitations: vec![
            reason(
                "preflight_batch_single_responder_not_atomic",
                "All declarations use this responder's runtime catalog and one request-local credential-presence snapshot under one generated-at marker. Postgres captures credential presence in one MVCC statement and the in-memory store under one lock; composite/default stores may combine sequential reads, so this is not a cross-store or configuration transaction.",
            ),
            reason(
                "preflight_batch_provider_execution_not_observed",
                "This batch performs no secret validation, provider I/O, tokenization, reservation, admission, settlement, or execution.",
            ),
        ],
    })
}

pub async fn build_document(
    state: &AppState,
    org_id: uuid::Uuid,
    request: RequestPreflightRequest,
    generated_at: DateTime<Utc>,
) -> ApiResult<RequestPreflightResponse> {
    validate_request(&request)?;
    let (provider_resolution, model_info) = resolve_provider(state, &request);
    let provider_ids = provider_resolution
        .provider
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let snapshot = capture_credential_snapshot(state, org_id, &provider_ids).await;
    let credential = credential_from_snapshot(provider_resolution.provider.as_deref(), &snapshot);
    Ok(build_document_from_parts(
        state,
        request,
        generated_at,
        provider_resolution,
        model_info,
        credential,
    ))
}

fn build_document_with_credential(
    state: &AppState,
    request: RequestPreflightRequest,
    generated_at: DateTime<Utc>,
    credential: PreflightCredentialEvidence,
) -> RequestPreflightResponse {
    let (provider_resolution, model_info) = resolve_provider(state, &request);
    build_document_from_parts(
        state,
        request,
        generated_at,
        provider_resolution,
        model_info,
        credential,
    )
}

fn build_document_from_parts(
    state: &AppState,
    request: RequestPreflightRequest,
    generated_at: DateTime<Utc>,
    provider_resolution: PreflightProviderResolution,
    model_info: Option<ModelInfo>,
    credential: PreflightCredentialEvidence,
) -> RequestPreflightResponse {
    let model_support = model_support(&request, model_info.as_ref());
    let catalog_limits = catalog_limits(&request, model_info.as_ref());
    let pricing = provider_resolution
        .provider
        .as_deref()
        .and_then(|provider| state.registry.by_id(provider))
        .and_then(|provider| provider.pricing(&request.model));
    let catalog_cost = catalog_cost(&request, model_info.as_ref(), pricing.as_ref());
    let actions = actions(
        &provider_resolution,
        &credential,
        &model_support,
        &catalog_limits,
    );

    RequestPreflightResponse {
        schema_version: REQUEST_PREFLIGHT_SCHEMA_VERSION,
        scope: REQUEST_PREFLIGHT_SCOPE.into(),
        snapshot_scope: CAPABILITIES_SNAPSHOT_SCOPE.into(),
        generated_at: generated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        request,
        provider_resolution,
        credential,
        model_support,
        catalog_limits,
        catalog_cost,
        provider_health: unknown(
            "provider_health_not_probed",
            "This preflight performs no provider request or health probe.",
        ),
        request_acceptance: unknown(
            "request_acceptance_not_attempted",
            "Only the actual provider-bound request can establish acceptance or execution.",
        ),
        actions,
    }
}

fn catalog_cost(
    request: &RequestPreflightRequest,
    model_info: Option<&ModelInfo>,
    pricing: Option<&ModelPricing>,
) -> PreflightCostEvidence {
    let Some(model_info) = model_info else {
        return unknown_cost();
    };
    let Some(pricing) = pricing.filter(|pricing| {
        pricing.input_per_million.is_finite()
            && pricing.input_per_million >= 0.0
            && pricing.output_per_million.is_finite()
            && pricing.output_per_million >= 0.0
    }) else {
        return unknown_cost();
    };
    if model_info.max_input_tokens > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX
        || model_info.max_output_tokens > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX
    {
        return unknown_cost();
    }

    let (input_tokens_low, input_tokens_high) = request
        .declared_input_tokens
        .map_or((0, model_info.max_input_tokens), |tokens| (tokens, tokens));
    let output_tokens_low = 0;
    let output_tokens_high = request
        .requested_max_output_tokens
        .unwrap_or(model_info.max_output_tokens);
    let standard_cost_usd_low =
        projected_standard_cost(input_tokens_low, output_tokens_low, pricing);
    let standard_cost_usd_high =
        projected_standard_cost(input_tokens_high, output_tokens_high, pricing);
    if !standard_cost_usd_low.is_finite() || !standard_cost_usd_high.is_finite() {
        return unknown_cost();
    }

    PreflightCostEvidence {
        state: "catalog_projection".into(),
        source: "registered_provider_pricing_catalog".into(),
        standard_input_rate_usd_per_million: Some(pricing.input_per_million),
        standard_output_rate_usd_per_million: Some(pricing.output_per_million),
        input_tokens_low: Some(input_tokens_low),
        input_tokens_high: Some(input_tokens_high),
        output_tokens_low: Some(output_tokens_low),
        output_tokens_high: Some(output_tokens_high),
        standard_cost_usd_low: Some(standard_cost_usd_low),
        standard_cost_usd_high: Some(standard_cost_usd_high),
        reason: reason(
            "preflight_standard_cost_catalog_projection",
            "This interval applies the responder's standard fresh-input/output catalog rates to caller-declared or catalog-limit token bounds; it is not live pricing, a quote, reservation, settlement, invoice, or enforced spending limit.",
        ),
    }
}

fn projected_standard_cost(input_tokens: u64, output_tokens: u64, pricing: &ModelPricing) -> f64 {
    (input_tokens as f64 * pricing.input_per_million
        + output_tokens as f64 * pricing.output_per_million)
        / 1_000_000.0
}

fn unknown_cost() -> PreflightCostEvidence {
    PreflightCostEvidence {
        state: "unknown".into(),
        source: "not_negotiated".into(),
        standard_input_rate_usd_per_million: None,
        standard_output_rate_usd_per_million: None,
        input_tokens_low: None,
        input_tokens_high: None,
        output_tokens_low: None,
        output_tokens_high: None,
        standard_cost_usd_low: None,
        standard_cost_usd_high: None,
        reason: reason(
            "preflight_standard_cost_unavailable",
            "This responder cannot form a standard-rate catalog cost interval for the declaration.",
        ),
    }
}

fn validate_request(request: &RequestPreflightRequest) -> ApiResult<()> {
    if request.schema_version != REQUEST_PREFLIGHT_SCHEMA_VERSION {
        return Err(ApiError::InvalidRequest(
            "unsupported request preflight schema_version".into(),
        ));
    }
    if !bounded_text(&request.model, MAX_MODEL_BYTES) {
        return Err(ApiError::InvalidRequest(
            "request preflight model must be nonempty bounded text".into(),
        ));
    }
    if let Some(provider) = request.provider.as_deref() {
        if !bounded_text(provider, MAX_PROVIDER_BYTES)
            || !provider.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
        {
            return Err(ApiError::InvalidRequest(
                "request preflight provider must be a lowercase provider id".into(),
            ));
        }
    }
    if request.required_capabilities.len() > MAX_REQUIRED_CAPABILITIES
        || request
            .required_capabilities
            .iter()
            .enumerate()
            .any(|(index, capability)| request.required_capabilities[..index].contains(capability))
    {
        return Err(ApiError::InvalidRequest(
            "request preflight capabilities must be unique and bounded".into(),
        ));
    }
    if request.requested_max_output_tokens == Some(0) {
        return Err(ApiError::InvalidRequest(
            "requested_max_output_tokens must be positive when supplied".into(),
        ));
    }
    if request
        .declared_input_tokens
        .is_some_and(|value| value > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX)
        || request
            .requested_max_output_tokens
            .is_some_and(|value| value > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX)
    {
        return Err(ApiError::InvalidRequest(
            "request preflight token values exceed the v1 wire limit".into(),
        ));
    }
    Ok(())
}

fn bounded_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn resolve_provider(
    state: &AppState,
    request: &RequestPreflightRequest,
) -> (PreflightProviderResolution, Option<ModelInfo>) {
    if let Some(requested_provider) = request.provider.as_deref() {
        let Some(provider) = state.registry.by_id(requested_provider) else {
            return (
                provider_resolution(
                    "provider_unregistered",
                    None,
                    "gateway_runtime",
                    "preflight_provider_unregistered",
                    "The requested provider is not registered on this responding gateway process.",
                ),
                None,
            );
        };
        let info = provider
            .models()
            .into_iter()
            .find(|candidate| candidate.id == request.model);
        let (state_name, code, message) = if info.is_some() {
            (
                "exact_catalog_match",
                "preflight_exact_provider_model_match",
                "The requested provider and model exactly match this responder's registered catalog.",
            )
        } else {
            (
                "provider_registered_catalog_miss",
                "preflight_provider_registered_model_unlisted",
                "The provider is registered for dispatch, but this exact model is absent from its responder catalog.",
            )
        };
        return (
            provider_resolution(
                state_name,
                Some(provider.id()),
                "gateway_runtime",
                code,
                message,
            ),
            info,
        );
    }

    if let Some(provider) = state.registry.by_model(&request.model) {
        let info = provider
            .models()
            .into_iter()
            .find(|candidate| candidate.id == request.model);
        return (
            provider_resolution(
                "exact_catalog_match",
                Some(provider.id()),
                "registered_provider_catalog",
                "preflight_exact_model_match",
                "The model exactly matches this responder's registered provider catalog.",
            ),
            info,
        );
    }
    if let Some(provider) = state.registry.resolve(&request.model) {
        return (
            provider_resolution(
                "dispatch_resolved_catalog_unknown",
                Some(provider.id()),
                "gateway_dispatch_resolution",
                "preflight_dispatch_provider_inferred",
                "The gateway can resolve a dispatch provider, but the model has no exact responder catalog row.",
            ),
            None,
        );
    }
    (
        provider_resolution(
            "unresolved",
            None,
            "gateway_runtime",
            "preflight_provider_unresolved",
            "This responding gateway cannot resolve a dispatch provider for the requested model.",
        ),
        None,
    )
}

fn provider_resolution(
    state: &str,
    provider: Option<&str>,
    source: &str,
    code: &'static str,
    message: &'static str,
) -> PreflightProviderResolution {
    PreflightProviderResolution {
        state: state.into(),
        provider: provider.map(str::to_string),
        source: source.into(),
        reason: reason(code, message),
    }
}

enum CredentialPresenceSnapshot {
    StoreNotConfigured,
    Available(HashSet<String>),
    Unavailable,
}

async fn capture_credential_snapshot(
    state: &AppState,
    org_id: uuid::Uuid,
    provider_ids: &[String],
) -> CredentialPresenceSnapshot {
    let Some(store) = state.credential_store.as_ref() else {
        return CredentialPresenceSnapshot::StoreNotConfigured;
    };
    match store.configured_snapshot(org_id, provider_ids).await {
        Ok(configured) => CredentialPresenceSnapshot::Available(configured),
        Err(_) => {
            tracing::warn!("credential store unavailable during request capability preflight");
            CredentialPresenceSnapshot::Unavailable
        }
    }
}

fn credential_from_snapshot(
    provider: Option<&str>,
    snapshot: &CredentialPresenceSnapshot,
) -> PreflightCredentialEvidence {
    let Some(provider) = provider else {
        return credential(
            "unknown",
            "not_inspected",
            "preflight_credential_provider_unresolved",
            "Credential-record presence cannot be inspected until a provider resolves.",
        );
    };
    match snapshot {
        CredentialPresenceSnapshot::StoreNotConfigured => credential(
            "unknown",
            "not_inspected",
            "preflight_credential_store_not_configured",
            "This gateway has no organization credential store to inspect; no credential-validity claim is made.",
        ),
        CredentialPresenceSnapshot::Available(configured) if configured.contains(provider) => credential(
            "configured",
            "organization_credential_store",
            "preflight_credential_record_configured",
            "An organization credential record exists for this provider; its secret was not decrypted, probed, or validated.",
        ),
        CredentialPresenceSnapshot::Available(_) => credential(
            "missing",
            "organization_credential_store",
            "preflight_credential_record_missing",
            "No organization credential record exists for this provider.",
        ),
        CredentialPresenceSnapshot::Unavailable => credential(
            "unavailable",
            "organization_credential_store",
            "preflight_credential_store_unavailable",
            "Credential-record presence could not be read; retry or use the actual request result.",
        ),
    }
}

fn credential(
    state: &str,
    source: &str,
    code: &'static str,
    message: &'static str,
) -> PreflightCredentialEvidence {
    PreflightCredentialEvidence {
        state: state.into(),
        source: source.into(),
        reason: reason(code, message),
    }
}

fn model_support(
    request: &RequestPreflightRequest,
    model_info: Option<&ModelInfo>,
) -> PreflightModelSupportEvidence {
    let Some(model_info) = model_info else {
        return PreflightModelSupportEvidence {
            state: "unknown".into(),
            source: "not_negotiated".into(),
            missing_capabilities: Vec::new(),
            reason: reason(
                "preflight_model_support_catalog_unknown",
                "Without an exact catalog row, this preflight cannot establish model capability support.",
            ),
        };
    };
    let missing_capabilities = request
        .required_capabilities
        .iter()
        .filter(|capability| !model_info.capabilities.contains(capability))
        .cloned()
        .collect::<Vec<_>>();
    let (state, code, message) = if missing_capabilities.is_empty() {
        (
            "supported_by_catalog",
            "preflight_required_capabilities_catalog_match",
            "Every requested capability appears in this responder's exact catalog row.",
        )
    } else {
        (
            "unsupported_by_catalog",
            "preflight_required_capabilities_catalog_miss",
            "One or more requested capabilities are absent from this responder's exact catalog row.",
        )
    };
    PreflightModelSupportEvidence {
        state: state.into(),
        source: "registered_provider_catalog".into(),
        missing_capabilities,
        reason: reason(code, message),
    }
}

fn catalog_limits(
    request: &RequestPreflightRequest,
    model_info: Option<&ModelInfo>,
) -> PreflightLimitEvidence {
    let Some(model_info) = model_info else {
        return limit(
            "unknown",
            "not_negotiated",
            None,
            None,
            "preflight_catalog_limits_unknown",
            "No exact catalog row is available for a declared-token comparison.",
        );
    };
    if model_info.max_input_tokens > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX
        || model_info.max_output_tokens > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX
    {
        return limit(
            "unknown",
            "not_negotiated",
            None,
            None,
            "preflight_catalog_limits_outside_v1_wire",
            "The exact catalog row has token metadata outside this preflight wire's representable range.",
        );
    }
    if request.declared_input_tokens.is_none() && request.requested_max_output_tokens.is_none() {
        return limit(
            "not_evaluated",
            "caller_not_supplied",
            Some(model_info.max_input_tokens),
            Some(model_info.max_output_tokens),
            "preflight_declared_tokens_not_supplied",
            "Catalog limits are shown, but the caller supplied no token values to compare.",
        );
    }
    let exceeds = request
        .declared_input_tokens
        .is_some_and(|value| value > model_info.max_input_tokens)
        || request
            .requested_max_output_tokens
            .is_some_and(|value| value > model_info.max_output_tokens);
    if exceeds {
        limit(
            "exceeds_catalog_metadata",
            "registered_provider_catalog",
            Some(model_info.max_input_tokens),
            Some(model_info.max_output_tokens),
            "preflight_declared_tokens_exceed_catalog",
            "At least one caller-declared token value exceeds this responder's catalog metadata.",
        )
    } else {
        limit(
            "within_catalog_metadata",
            "registered_provider_catalog",
            Some(model_info.max_input_tokens),
            Some(model_info.max_output_tokens),
            "preflight_declared_tokens_within_catalog",
            "Every supplied token value is within this responder's catalog metadata.",
        )
    }
}

fn limit(
    state: &str,
    source: &str,
    max_input: Option<u64>,
    max_output: Option<u64>,
    code: &'static str,
    message: &'static str,
) -> PreflightLimitEvidence {
    PreflightLimitEvidence {
        state: state.into(),
        source: source.into(),
        catalog_max_input_tokens: max_input,
        catalog_max_output_tokens: max_output,
        reason: reason(code, message),
    }
}

fn actions(
    resolution: &PreflightProviderResolution,
    credential: &PreflightCredentialEvidence,
    support: &PreflightModelSupportEvidence,
    limits: &PreflightLimitEvidence,
) -> Vec<PreflightAction> {
    let mut actions = Vec::new();
    if resolution.provider.is_none() {
        actions.push(action(
            "choose_registered_provider_or_model",
            true,
            "preflight_action_provider_required",
            "Choose a model/provider that this gateway can resolve before sending the request.",
        ));
    }
    match credential.state.as_str() {
        "missing" => actions.push(action(
            "configure_provider_credential",
            true,
            "preflight_action_configure_credential",
            "Configure an organization credential for the resolved provider before sending the request.",
        )),
        "unavailable" => actions.push(action(
            "retry_preflight_or_contact_operator",
            true,
            "preflight_action_retry_credential_check",
            "Retry the credential check or contact the gateway operator before relying on it.",
        )),
        _ => {}
    }
    if support.state == "unsupported_by_catalog" {
        actions.push(action(
            "change_model_or_required_capabilities",
            true,
            "preflight_action_change_capability_request",
            "Choose a catalog row with the required capabilities or change the requested capability set.",
        ));
    }
    if limits.state == "exceeds_catalog_metadata" {
        actions.push(action(
            "reduce_declared_tokens_or_choose_model",
            true,
            "preflight_action_reduce_declared_tokens",
            "Reduce the declared token values or choose a catalog model with larger metadata limits.",
        ));
    }
    actions.push(action(
        "execute_request_and_handle_result",
        false,
        "preflight_action_real_request_authoritative",
        "The actual request result remains authoritative for credential validity, provider health, limits, acceptance, and execution.",
    ));
    actions
}

fn action(
    code: &'static str,
    required_before_request: bool,
    reason_code: &'static str,
    message: &'static str,
) -> PreflightAction {
    PreflightAction {
        code: code.into(),
        required_before_request,
        reason: reason(reason_code, message),
    }
}

fn unknown(code: &'static str, message: &'static str) -> UnknownEvidence {
    UnknownEvidence {
        state: "unknown".into(),
        source: "not_negotiated".into(),
        reason: reason(code, message),
    }
}

fn reason(code: &'static str, message: &'static str) -> CapabilityReason {
    CapabilityReason {
        code: code.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
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
            document.catalog_cost.standard_cost_usd_high
                > document.catalog_cost.standard_cost_usd_low
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
}
