//! Bounded typed client for authenticated request-specific local preflight.

use std::net::IpAddr;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use reqwest::Url;
use tt_shared::{
    Capability, CapabilityReason, PreflightAction, PreflightCostEvidence,
    PreflightCredentialEvidence, PreflightLimitEvidence, PreflightModelSupportEvidence,
    PreflightProviderResolution, RequestPreflightBatchRequest, RequestPreflightBatchResponse,
    RequestPreflightRequest, RequestPreflightResponse, UnknownEvidence,
    CAPABILITIES_SNAPSHOT_SCOPE, REQUEST_PREFLIGHT_BATCH_MAX_REQUESTS,
    REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION, REQUEST_PREFLIGHT_BATCH_SCOPE,
    REQUEST_PREFLIGHT_SCHEMA_VERSION, REQUEST_PREFLIGHT_SCOPE, REQUEST_PREFLIGHT_TOKEN_VALUE_MAX,
};

use crate::{Client, CostInfo, Error, Result};

/// Request-preflight evidence is a small control-metadata document.
pub const MAX_REQUEST_PREFLIGHT_RESPONSE_BYTES: usize = 64 * 1024;
/// One deadline covers response headers and the complete bounded body.
pub const REQUEST_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_MODEL_BYTES: usize = 256;
const MAX_PROVIDER_BYTES: usize = 64;
const MAX_CAPABILITIES: usize = 8;
const MAX_REASON_CODE_BYTES: usize = 96;
const MAX_REASON_MESSAGE_BYTES: usize = 600;

impl Client {
    fn request_preflight_endpoint(&self) -> Result<Url> {
        let mut endpoint =
            Url::parse(&self.base).map_err(|_| Error::InvalidRequestPreflight("base_url"))?;
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || (endpoint.scheme() != "https"
                && !(endpoint.scheme() == "http" && is_literal_loopback(&endpoint)))
        {
            return invalid("base_url");
        }
        let base_path = endpoint.path().trim_end_matches('/');
        endpoint.set_path(&format!("{base_path}/v1/capabilities/preflight"));
        Ok(endpoint)
    }

    fn request_preflight_batch_endpoint(&self) -> Result<Url> {
        let mut endpoint = self.request_preflight_endpoint()?;
        let path = endpoint.path().trim_end_matches('/');
        endpoint.set_path(&format!("{path}/batch"));
        Ok(endpoint)
    }

    /// Evaluate one request against facts available to one responding gateway
    /// without provider I/O.
    ///
    /// The result can identify local catalog, credential-record, and
    /// caller-declared-token blockers plus a standard-rate catalog cost
    /// interval when the responder can price the exact row. It does not
    /// validate a credential, tokenize a prompt, probe a provider, establish a
    /// provider-accepted hard limit, quote live pricing, reserve
    /// capacity/spend, or prove request acceptance or execution.
    ///
    /// # Errors
    /// Fails before I/O for an invalid request, non-live key, or
    /// non-HTTPS/non-loopback base. Also fails on transport/timeout, redirects,
    /// non-success status, a body above 64 KiB, invalid private JSON response
    /// headers, malformed JSON, or contradictory v1 evidence. Remote error
    /// prose is never surfaced.
    pub async fn preflight(
        &self,
        request: &RequestPreflightRequest,
    ) -> Result<RequestPreflightResponse> {
        validate_request(request)?;
        if !self.key.starts_with("tt_live_") || self.key.len() == "tt_live_".len() {
            return invalid("api_key");
        }
        let endpoint = self.request_preflight_endpoint()?;
        let response = self
            .control_http
            .as_ref()
            .ok_or(Error::ControlMetadataClientUnavailable)?
            .post(endpoint.clone())
            .header(ACCEPT, "application/json")
            .bearer_auth(&self.key)
            .json(request)
            .timeout(REQUEST_PREFLIGHT_TIMEOUT)
            .send()
            .await
            .map_err(Error::Request)?;

        if response.url() != &endpoint {
            let _ = read_bounded(response).await?;
            return Err(Error::UnexpectedRequestPreflightRedirect);
        }
        let status = response.status();
        if status.is_redirection() {
            let _ = read_bounded(response).await?;
            return Err(Error::UnexpectedRequestPreflightRedirect);
        }
        if !status.is_success() {
            let _ = read_bounded(response).await?;
            return Err(Error::Status {
                status: status.as_u16(),
                body: "request preflight failed".into(),
                cost: Box::<CostInfo>::default(),
            });
        }

        let header_result = validate_response_headers(&response);
        let body = read_bounded(response).await?;
        header_result?;
        let document = serde_json::from_slice::<RequestPreflightResponse>(&body)
            .map_err(Error::InvalidResponse)?;
        validate_document(&document, request)?;
        Ok(document)
    }

    /// Evaluate an ordered, bounded declaration set on one responding gateway
    /// process under one generated-at marker.
    ///
    /// This avoids cross-process drift between Fusion roles, but does not make
    /// credential/configuration reads transactional and performs no secret
    /// validation, provider I/O, tokenization, admission, or reservation.
    ///
    /// # Errors
    /// Applies the same validation, transport, redirect, header, body-bound,
    /// and evidence checks as [`Client::preflight`] to the batch envelope and
    /// every nested document.
    pub async fn preflight_batch(
        &self,
        request: &RequestPreflightBatchRequest,
    ) -> Result<RequestPreflightBatchResponse> {
        validate_batch_request(request)?;
        if !self.key.starts_with("tt_live_") || self.key.len() == "tt_live_".len() {
            return invalid("api_key");
        }
        let endpoint = self.request_preflight_batch_endpoint()?;
        let response = self
            .control_http
            .as_ref()
            .ok_or(Error::ControlMetadataClientUnavailable)?
            .post(endpoint.clone())
            .header(ACCEPT, "application/json")
            .bearer_auth(&self.key)
            .json(request)
            .timeout(REQUEST_PREFLIGHT_TIMEOUT)
            .send()
            .await
            .map_err(Error::Request)?;

        if response.url() != &endpoint {
            let _ = read_bounded(response).await?;
            return Err(Error::UnexpectedRequestPreflightRedirect);
        }
        let status = response.status();
        if status.is_redirection() {
            let _ = read_bounded(response).await?;
            return Err(Error::UnexpectedRequestPreflightRedirect);
        }
        if !status.is_success() {
            let _ = read_bounded(response).await?;
            return Err(Error::Status {
                status: status.as_u16(),
                body: "request preflight batch failed".into(),
                cost: Box::<CostInfo>::default(),
            });
        }

        let header_result = validate_response_headers(&response);
        let body = read_bounded(response).await?;
        header_result?;
        let document = serde_json::from_slice::<RequestPreflightBatchResponse>(&body)
            .map_err(Error::InvalidResponse)?;
        validate_batch_document(&document, request)?;
        Ok(document)
    }
}

fn validate_batch_request(request: &RequestPreflightBatchRequest) -> Result<()> {
    if request.schema_version != REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION
        || request.requests.is_empty()
        || request.requests.len() > REQUEST_PREFLIGHT_BATCH_MAX_REQUESTS
    {
        return invalid("batch_request");
    }
    for declaration in &request.requests {
        validate_request(declaration)?;
    }
    Ok(())
}

fn validate_batch_document(
    document: &RequestPreflightBatchResponse,
    request: &RequestPreflightBatchRequest,
) -> Result<()> {
    if document.schema_version != REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION
        || document.scope != REQUEST_PREFLIGHT_BATCH_SCOPE
        || document.snapshot_scope != CAPABILITIES_SNAPSHOT_SCOPE
        || document.request != *request
        || document.documents.len() != request.requests.len()
        || document.limitations.len() != 2
    {
        return invalid("batch_metadata");
    }
    validate_timestamp(&document.generated_at)?;
    for (nested, declaration) in document.documents.iter().zip(&request.requests) {
        validate_document(nested, declaration)?;
        if nested.generated_at != document.generated_at {
            return invalid("batch_generated_at");
        }
    }
    validate_reason(
        &document.limitations[0],
        "preflight_batch_single_responder_not_atomic",
    )?;
    validate_reason(
        &document.limitations[1],
        "preflight_batch_provider_execution_not_observed",
    )?;
    Ok(())
}

fn validate_request(request: &RequestPreflightRequest) -> Result<()> {
    if request.schema_version != REQUEST_PREFLIGHT_SCHEMA_VERSION {
        return invalid("request_schema_version");
    }
    if !bounded_text(&request.model, MAX_MODEL_BYTES) || request.model.chars().any(char::is_control)
    {
        return invalid("request_model");
    }
    if let Some(provider) = request.provider.as_deref() {
        if !bounded_text(provider, MAX_PROVIDER_BYTES)
            || !provider.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
        {
            return invalid("request_provider");
        }
    }
    if request.required_capabilities.len() > MAX_CAPABILITIES
        || has_duplicates(&request.required_capabilities)
    {
        return invalid("request_capabilities");
    }
    if request
        .declared_input_tokens
        .is_some_and(|value| value > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX)
        || request
            .requested_max_output_tokens
            .is_some_and(|value| value == 0 || value > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX)
    {
        return invalid("request_tokens");
    }
    Ok(())
}

fn validate_document(
    document: &RequestPreflightResponse,
    request: &RequestPreflightRequest,
) -> Result<()> {
    if document.schema_version != REQUEST_PREFLIGHT_SCHEMA_VERSION
        || document.scope != REQUEST_PREFLIGHT_SCOPE
        || document.snapshot_scope != CAPABILITIES_SNAPSHOT_SCOPE
        || document.request != *request
    {
        return invalid("metadata");
    }
    validate_timestamp(&document.generated_at)?;
    validate_resolution(&document.provider_resolution, request)?;
    validate_credential(&document.credential, &document.provider_resolution)?;
    validate_model_support(
        &document.model_support,
        &document.provider_resolution,
        request,
    )?;
    validate_limits(
        &document.catalog_limits,
        &document.provider_resolution,
        request,
    )?;
    validate_cost(
        &document.catalog_cost,
        &document.provider_resolution,
        &document.catalog_limits,
        request,
    )?;
    validate_unknown(&document.provider_health, "provider_health_not_probed")?;
    validate_unknown(
        &document.request_acceptance,
        "request_acceptance_not_attempted",
    )?;
    validate_actions(document)?;
    Ok(())
}

fn validate_cost(
    cost: &PreflightCostEvidence,
    resolution: &PreflightProviderResolution,
    limits: &PreflightLimitEvidence,
    request: &RequestPreflightRequest,
) -> Result<()> {
    if cost.state == "unknown" {
        if cost.source != "not_negotiated"
            || [
                cost.standard_input_rate_usd_per_million,
                cost.standard_output_rate_usd_per_million,
                cost.standard_cost_usd_low,
                cost.standard_cost_usd_high,
            ]
            .into_iter()
            .any(|value| value.is_some())
            || [
                cost.input_tokens_low,
                cost.input_tokens_high,
                cost.output_tokens_low,
                cost.output_tokens_high,
            ]
            .into_iter()
            .any(|value| value.is_some())
        {
            return invalid("catalog_cost");
        }
        return validate_reason(&cost.reason, "preflight_standard_cost_unavailable");
    }

    if cost.state != "catalog_projection"
        || cost.source != "registered_provider_pricing_catalog"
        || resolution.state != "exact_catalog_match"
    {
        return invalid("catalog_cost");
    }
    let (
        Some(input_rate),
        Some(output_rate),
        Some(input_low),
        Some(input_high),
        Some(output_low),
        Some(output_high),
        Some(cost_low),
        Some(cost_high),
        Some(catalog_input_max),
        Some(catalog_output_max),
    ) = (
        cost.standard_input_rate_usd_per_million,
        cost.standard_output_rate_usd_per_million,
        cost.input_tokens_low,
        cost.input_tokens_high,
        cost.output_tokens_low,
        cost.output_tokens_high,
        cost.standard_cost_usd_low,
        cost.standard_cost_usd_high,
        limits.catalog_max_input_tokens,
        limits.catalog_max_output_tokens,
    )
    else {
        return invalid("catalog_cost");
    };
    if !input_rate.is_finite()
        || input_rate < 0.0
        || !output_rate.is_finite()
        || output_rate < 0.0
        || !cost_low.is_finite()
        || cost_low < 0.0
        || !cost_high.is_finite()
        || cost_high < cost_low
        || output_low != 0
    {
        return invalid("catalog_cost");
    }
    let expected_input = request
        .declared_input_tokens
        .map_or((0, catalog_input_max), |tokens| (tokens, tokens));
    let expected_output_high = request
        .requested_max_output_tokens
        .unwrap_or(catalog_output_max);
    if (input_low, input_high) != expected_input || output_high != expected_output_high {
        return invalid("catalog_cost");
    }
    let expected_low = projected_standard_cost(input_low, output_low, input_rate, output_rate);
    let expected_high = projected_standard_cost(input_high, output_high, input_rate, output_rate);
    if !approximately_equal(cost_low, expected_low)
        || !approximately_equal(cost_high, expected_high)
    {
        return invalid("catalog_cost");
    }
    validate_reason(&cost.reason, "preflight_standard_cost_catalog_projection")
}

fn projected_standard_cost(
    input_tokens: u64,
    output_tokens: u64,
    input_rate: f64,
    output_rate: f64,
) -> f64 {
    (input_tokens as f64 * input_rate + output_tokens as f64 * output_rate) / 1_000_000.0
}

fn approximately_equal(left: f64, right: f64) -> bool {
    let scale = left.abs().max(right.abs()).max(1.0);
    (left - right).abs() <= f64::EPSILON * 16.0 * scale
}

fn validate_resolution(
    resolution: &PreflightProviderResolution,
    request: &RequestPreflightRequest,
) -> Result<()> {
    match resolution.state.as_str() {
        "exact_catalog_match" => match request.provider.as_deref() {
            Some(provider)
                if resolution.provider.as_deref() == Some(provider)
                    && resolution.source == "gateway_runtime" =>
            {
                validate_reason(&resolution.reason, "preflight_exact_provider_model_match")
            }
            None if valid_provider(resolution.provider.as_deref())
                && resolution.source == "registered_provider_catalog" =>
            {
                validate_reason(&resolution.reason, "preflight_exact_model_match")
            }
            _ => invalid("provider_resolution"),
        },
        "provider_registered_catalog_miss"
            if request.provider.as_deref() == resolution.provider.as_deref()
                && valid_provider(resolution.provider.as_deref())
                && resolution.source == "gateway_runtime" =>
        {
            validate_reason(
                &resolution.reason,
                "preflight_provider_registered_model_unlisted",
            )
        }
        "provider_unregistered"
            if request.provider.is_some()
                && resolution.provider.is_none()
                && resolution.source == "gateway_runtime" =>
        {
            validate_reason(&resolution.reason, "preflight_provider_unregistered")
        }
        "dispatch_resolved_catalog_unknown"
            if request.provider.is_none()
                && valid_provider(resolution.provider.as_deref())
                && resolution.source == "gateway_dispatch_resolution" =>
        {
            validate_reason(&resolution.reason, "preflight_dispatch_provider_inferred")
        }
        "unresolved" if resolution.provider.is_none() && resolution.source == "gateway_runtime" => {
            validate_reason(&resolution.reason, "preflight_provider_unresolved")
        }
        _ => invalid("provider_resolution"),
    }
}

fn validate_credential(
    credential: &PreflightCredentialEvidence,
    resolution: &PreflightProviderResolution,
) -> Result<()> {
    if resolution.provider.is_none() {
        if credential.state != "unknown" || credential.source != "not_inspected" {
            return invalid("credential");
        }
        return validate_reason(
            &credential.reason,
            "preflight_credential_provider_unresolved",
        );
    }
    match credential.state.as_str() {
        "configured" if credential.source == "organization_credential_store" => {
            validate_reason(&credential.reason, "preflight_credential_record_configured")
        }
        "missing" if credential.source == "organization_credential_store" => {
            validate_reason(&credential.reason, "preflight_credential_record_missing")
        }
        "unavailable" if credential.source == "organization_credential_store" => {
            validate_reason(&credential.reason, "preflight_credential_store_unavailable")
        }
        "unknown" if credential.source == "not_inspected" => validate_reason(
            &credential.reason,
            "preflight_credential_store_not_configured",
        ),
        _ => invalid("credential"),
    }
}

fn validate_model_support(
    support: &PreflightModelSupportEvidence,
    resolution: &PreflightProviderResolution,
    request: &RequestPreflightRequest,
) -> Result<()> {
    if resolution.state != "exact_catalog_match" {
        if support.state != "unknown"
            || support.source != "not_negotiated"
            || !support.missing_capabilities.is_empty()
        {
            return invalid("model_support");
        }
        return validate_reason(&support.reason, "preflight_model_support_catalog_unknown");
    }
    if support.source != "registered_provider_catalog"
        || support.missing_capabilities.len() > MAX_CAPABILITIES
        || has_duplicates(&support.missing_capabilities)
        || support
            .missing_capabilities
            .iter()
            .any(|capability| !request.required_capabilities.contains(capability))
    {
        return invalid("model_support");
    }
    match support.state.as_str() {
        "supported_by_catalog" if support.missing_capabilities.is_empty() => validate_reason(
            &support.reason,
            "preflight_required_capabilities_catalog_match",
        ),
        "unsupported_by_catalog" if !support.missing_capabilities.is_empty() => validate_reason(
            &support.reason,
            "preflight_required_capabilities_catalog_miss",
        ),
        _ => invalid("model_support"),
    }
}

fn validate_limits(
    limits: &PreflightLimitEvidence,
    resolution: &PreflightProviderResolution,
    request: &RequestPreflightRequest,
) -> Result<()> {
    if resolution.state != "exact_catalog_match" {
        return validate_unknown_limits(limits, "preflight_catalog_limits_unknown");
    }
    if limits.state == "unknown" {
        return validate_unknown_limits(limits, "preflight_catalog_limits_outside_v1_wire");
    }
    let (Some(max_input), Some(max_output)) = (
        limits.catalog_max_input_tokens,
        limits.catalog_max_output_tokens,
    ) else {
        return invalid("catalog_limits");
    };
    if max_input == 0
        || max_input > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX
        || max_output > REQUEST_PREFLIGHT_TOKEN_VALUE_MAX
    {
        return invalid("catalog_limits");
    }
    if request.declared_input_tokens.is_none() && request.requested_max_output_tokens.is_none() {
        if limits.state != "not_evaluated" || limits.source != "caller_not_supplied" {
            return invalid("catalog_limits");
        }
        return validate_reason(&limits.reason, "preflight_declared_tokens_not_supplied");
    }
    let exceeds = request
        .declared_input_tokens
        .is_some_and(|value| value > max_input)
        || request
            .requested_max_output_tokens
            .is_some_and(|value| value > max_output);
    let (expected_state, expected_reason) = if exceeds {
        (
            "exceeds_catalog_metadata",
            "preflight_declared_tokens_exceed_catalog",
        )
    } else {
        (
            "within_catalog_metadata",
            "preflight_declared_tokens_within_catalog",
        )
    };
    if limits.state != expected_state || limits.source != "registered_provider_catalog" {
        return invalid("catalog_limits");
    }
    validate_reason(&limits.reason, expected_reason)
}

fn validate_unknown_limits(
    limits: &PreflightLimitEvidence,
    reason_code: &'static str,
) -> Result<()> {
    if limits.state != "unknown"
        || limits.source != "not_negotiated"
        || limits.catalog_max_input_tokens.is_some()
        || limits.catalog_max_output_tokens.is_some()
    {
        return invalid("catalog_limits");
    }
    validate_reason(&limits.reason, reason_code)
}

fn validate_actions(document: &RequestPreflightResponse) -> Result<()> {
    let mut expected = Vec::new();
    if document.provider_resolution.provider.is_none() {
        expected.push((
            "choose_registered_provider_or_model",
            true,
            "preflight_action_provider_required",
        ));
    }
    match document.credential.state.as_str() {
        "missing" => expected.push((
            "configure_provider_credential",
            true,
            "preflight_action_configure_credential",
        )),
        "unavailable" => expected.push((
            "retry_preflight_or_contact_operator",
            true,
            "preflight_action_retry_credential_check",
        )),
        _ => {}
    }
    if document.model_support.state == "unsupported_by_catalog" {
        expected.push((
            "change_model_or_required_capabilities",
            true,
            "preflight_action_change_capability_request",
        ));
    }
    if document.catalog_limits.state == "exceeds_catalog_metadata" {
        expected.push((
            "reduce_declared_tokens_or_choose_model",
            true,
            "preflight_action_reduce_declared_tokens",
        ));
    }
    expected.push((
        "execute_request_and_handle_result",
        false,
        "preflight_action_real_request_authoritative",
    ));

    if document.actions.len() != expected.len() {
        return invalid("actions");
    }
    for (action, (code, required, reason_code)) in document.actions.iter().zip(expected) {
        if action.code != code || action.required_before_request != required {
            return invalid("actions");
        }
        validate_action(action, reason_code)?;
    }
    Ok(())
}

fn validate_action(action: &PreflightAction, reason_code: &'static str) -> Result<()> {
    if !bounded_code(&action.code) {
        return invalid("actions");
    }
    validate_reason(&action.reason, reason_code)
}

fn validate_unknown(evidence: &UnknownEvidence, reason_code: &'static str) -> Result<()> {
    if evidence.state != "unknown" || evidence.source != "not_negotiated" {
        return invalid("unknown_evidence");
    }
    validate_reason(&evidence.reason, reason_code)
}

fn validate_reason(reason: &CapabilityReason, expected_code: &'static str) -> Result<()> {
    if reason.code != expected_code
        || !bounded_code(&reason.code)
        || !bounded_text(&reason.message, MAX_REASON_MESSAGE_BYTES)
        || reason.message.chars().any(char::is_control)
    {
        return invalid("reason");
    }
    Ok(())
}

fn bounded_code(value: &str) -> bool {
    bounded_text(value, MAX_REASON_CODE_BYTES)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b':')
        })
}

fn validate_timestamp(value: &str) -> Result<()> {
    if !bounded_text(value, 64) {
        return invalid("generated_at");
    }
    let timestamp = DateTime::parse_from_rfc3339(value)
        .map_err(|_| Error::InvalidRequestPreflight("generated_at"))?;
    if timestamp
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
        != value
    {
        return invalid("generated_at");
    }
    Ok(())
}

fn valid_provider(value: Option<&str>) -> bool {
    value.is_some_and(|provider| {
        bounded_text(provider, MAX_PROVIDER_BYTES)
            && provider.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
    })
}

fn has_duplicates(capabilities: &[Capability]) -> bool {
    capabilities
        .iter()
        .enumerate()
        .any(|(index, capability)| capabilities[..index].contains(capability))
}

fn bounded_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && value.trim() == value
}

fn is_literal_loopback(endpoint: &Url) -> bool {
    endpoint
        .host_str()
        .map(|host| host.trim_matches(&['[', ']'][..]))
        .and_then(|host| host.parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_loopback())
}

fn validate_response_headers(response: &reqwest::Response) -> Result<()> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return invalid("content_type");
    }
    let cache_control = response
        .headers()
        .get(CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !cache_control
        .split(',')
        .any(|directive| directive.trim().eq_ignore_ascii_case("no-store"))
    {
        return invalid("cache_control");
    }
    if response
        .headers()
        .get(X_CONTENT_TYPE_OPTIONS)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| !value.eq_ignore_ascii_case("nosniff"))
    {
        return invalid("content_type_options");
    }
    Ok(())
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_REQUEST_PREFLIGHT_RESPONSE_BYTES as u64)
    {
        return Err(Error::ResponseTooLarge {
            limit: MAX_REQUEST_PREFLIGHT_RESPONSE_BYTES,
        });
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_REQUEST_PREFLIGHT_RESPONSE_BYTES as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await.map_err(Error::Request)? {
        if chunk.len() > MAX_REQUEST_PREFLIGHT_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(Error::ResponseTooLarge {
                limit: MAX_REQUEST_PREFLIGHT_RESPONSE_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn invalid<T>(code: &'static str) -> Result<T> {
    Err(Error::InvalidRequestPreflight(code))
}

#[cfg(test)]
#[path = "request_preflight_tests.rs"]
mod tests;
