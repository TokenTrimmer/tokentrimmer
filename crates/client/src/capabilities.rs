//! Bounded typed read for authenticated `GET /v1/capabilities` evidence.

use std::net::IpAddr;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use reqwest::Url;
use tt_shared::{
    CapabilityReason, GatewayCapabilitiesDocument, SchemaVersionEvidence, TierEvidence,
    UnknownEvidence, CAPABILITIES_SCHEMA_VERSION, CAPABILITIES_SCOPE, CAPABILITIES_SNAPSHOT_SCOPE,
};

use crate::{Client, CostInfo, Error, Result};

/// Capability evidence is a small control-metadata document.
pub const MAX_CAPABILITIES_RESPONSE_BYTES: usize = 64 * 1024;
/// One deadline covers response headers and the complete bounded body.
pub const CAPABILITIES_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_REASON_CODE_BYTES: usize = 96;
const MAX_REASON_MESSAGE_BYTES: usize = 600;

impl Client {
    fn capabilities_endpoint(&self) -> Result<Url> {
        let mut endpoint =
            Url::parse(&self.base).map_err(|_| Error::InvalidGatewayCapabilities("base_url"))?;
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || (endpoint.scheme() != "https"
                && !(endpoint.scheme() == "http" && is_literal_loopback(&endpoint)))
        {
            return Err(Error::InvalidGatewayCapabilities("base_url"));
        }
        let base_path = endpoint.path().trim_end_matches('/');
        endpoint.set_path(&format!("{base_path}/v1/capabilities"));
        Ok(endpoint)
    }

    fn capabilities_request(&self, endpoint: Url) -> Result<reqwest::RequestBuilder> {
        if !self.key.starts_with("tt_live_") || self.key.len() == "tt_live_".len() {
            return Err(Error::InvalidGatewayCapabilities("api_key"));
        }
        Ok(self
            .control_http
            .as_ref()
            .ok_or(Error::ControlMetadataClientUnavailable)?
            .get(endpoint)
            .header(ACCEPT, "application/json")
            .bearer_auth(&self.key)
            .timeout(CAPABILITIES_REQUEST_TIMEOUT))
    }

    /// Read and validate this key's capability evidence from one gateway
    /// process.
    ///
    /// An available Fusion gate proves only that this responder's kill switch
    /// and tier gate passed. The document does not prove fleet convergence,
    /// credentials, provider health, model/modality support, request
    /// acceptance, a reservation, or later execution.
    ///
    /// # Errors
    /// Fails before I/O for a non-live key or a non-HTTPS/non-loopback base.
    /// Also fails on transport/timeout, redirects, non-success status, a body
    /// above 64 KiB, malformed cache/content headers, invalid JSON, future or
    /// contradictory v1 evidence, unsafe reason text, or unstable reason-code
    /// mappings. Remote error text is never surfaced.
    pub async fn capabilities(&self) -> Result<GatewayCapabilitiesDocument> {
        let endpoint = self.capabilities_endpoint()?;
        let request = self.capabilities_request(endpoint.clone())?;
        let response = request.send().await.map_err(Error::Request)?;

        if response.url() != &endpoint {
            let _ = read_bounded(response).await?;
            return Err(Error::UnexpectedGatewayCapabilitiesRedirect);
        }
        let status = response.status();
        if status.is_redirection() {
            let _ = read_bounded(response).await?;
            return Err(Error::UnexpectedGatewayCapabilitiesRedirect);
        }
        if !status.is_success() {
            let _ = read_bounded(response).await?;
            return Err(Error::Status {
                status: status.as_u16(),
                body: "gateway capabilities request failed".into(),
                cost: Box::<CostInfo>::default(),
            });
        }

        let header_result = validate_response_headers(&response);
        let body = read_bounded(response).await?;
        header_result?;
        let document = serde_json::from_slice::<GatewayCapabilitiesDocument>(&body)
            .map_err(Error::InvalidResponse)?;
        validate_document(&document)?;
        Ok(document)
    }
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
        return Err(Error::InvalidGatewayCapabilities("content_type"));
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
        return Err(Error::InvalidGatewayCapabilities("cache_control"));
    }
    if response
        .headers()
        .get(X_CONTENT_TYPE_OPTIONS)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| !value.eq_ignore_ascii_case("nosniff"))
    {
        return Err(Error::InvalidGatewayCapabilities("content_type_options"));
    }
    Ok(())
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CAPABILITIES_RESPONSE_BYTES as u64)
    {
        return Err(Error::ResponseTooLarge {
            limit: MAX_CAPABILITIES_RESPONSE_BYTES,
        });
    }

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_CAPABILITIES_RESPONSE_BYTES as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await.map_err(Error::Request)? {
        if chunk.len() > MAX_CAPABILITIES_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(Error::ResponseTooLarge {
                limit: MAX_CAPABILITIES_RESPONSE_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_document(document: &GatewayCapabilitiesDocument) -> Result<()> {
    if document.schema_version != CAPABILITIES_SCHEMA_VERSION
        || document.scope != CAPABILITIES_SCOPE
        || document.snapshot_scope != CAPABILITIES_SNAPSHOT_SCOPE
    {
        return invalid("metadata");
    }
    validate_timestamp(&document.generated_at)?;
    validate_schema_version(
        &document.schema_versions.capabilities_document,
        "known",
        Some(CAPABILITIES_SCHEMA_VERSION),
        "capabilities_document_version",
    )?;
    validate_schema_version(
        &document.schema_versions.fusion_request,
        "unversioned",
        None,
        "fusion_request_schema_not_versioned",
    )?;

    let fusion = &document.features.fusion;
    let switch_enabled = match fusion.enabled.state.as_str() {
        "enabled" => {
            validate_reason(&fusion.enabled.reason, "fusion_kill_switch_enabled")?;
            true
        }
        "disabled" => {
            validate_reason(&fusion.enabled.reason, "fusion_kill_switch_disabled")?;
            false
        }
        _ => return invalid("fusion_enabled"),
    };
    if fusion.enabled.source != CAPABILITIES_SCOPE {
        return invalid("fusion_enabled");
    }

    let current_rank = validate_current_tier(&fusion.current_tier)?;
    let minimum_rank = validate_minimum_tier(&fusion.minimum_tier)?;
    let (expected_access, expected_reason) = if !switch_enabled {
        ("unavailable", "fusion_disabled")
    } else if current_rank < minimum_rank {
        ("unavailable", "fusion_tier_below_minimum")
    } else {
        ("available", "fusion_gateway_gate_passed")
    };
    if fusion.access.state != expected_access {
        return invalid("fusion_access");
    }
    validate_reason(&fusion.access.reason, expected_reason)?;

    let member_limit = &fusion.limits.member_models_max;
    if member_limit.value == 0 || member_limit.enforcement != CAPABILITIES_SCOPE {
        return invalid("member_models_max");
    }
    validate_reason(&member_limit.reason, "fusion_member_cap")?;

    validate_unknown(
        &document.provider_credentials,
        "provider_credentials_not_inspected",
    )?;
    validate_unknown(&document.provider_health, "provider_health_not_probed")?;
    validate_unknown(&document.model_support, "model_support_not_negotiated")?;
    validate_unknown(
        &document.modality_support,
        "modality_support_not_negotiated",
    )?;
    Ok(())
}

fn validate_timestamp(value: &str) -> Result<()> {
    if !is_bounded_text(value, 64) {
        return invalid("generated_at");
    }
    let timestamp = DateTime::parse_from_rfc3339(value)
        .map_err(|_| Error::InvalidGatewayCapabilities("generated_at"))?;
    if timestamp
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
        != value
    {
        return invalid("generated_at");
    }
    Ok(())
}

fn validate_schema_version(
    evidence: &SchemaVersionEvidence,
    state: &str,
    version: Option<u32>,
    reason_code: &'static str,
) -> Result<()> {
    if evidence.state != state
        || evidence.version != version
        || evidence.source != CAPABILITIES_SCOPE
    {
        return invalid("schema_versions");
    }
    validate_reason(&evidence.reason, reason_code)
}

fn validate_current_tier(evidence: &TierEvidence) -> Result<u8> {
    if evidence.state != "known" {
        return invalid("current_tier");
    }
    let rank =
        tier_rank(&evidence.value).ok_or(Error::InvalidGatewayCapabilities("current_tier"))?;
    let reason_code = match evidence.source.as_str() {
        "authenticated_api_key" => "effective_tier_from_authenticated_key",
        "gateway_free_default" if evidence.value == "free" => "effective_tier_defaulted_to_free",
        _ => return invalid("current_tier"),
    };
    validate_reason(&evidence.reason, reason_code)?;
    Ok(rank)
}

fn validate_minimum_tier(evidence: &TierEvidence) -> Result<u8> {
    if evidence.state != "known" || evidence.source != CAPABILITIES_SCOPE {
        return invalid("minimum_tier");
    }
    let rank =
        tier_rank(&evidence.value).ok_or(Error::InvalidGatewayCapabilities("minimum_tier"))?;
    validate_reason(&evidence.reason, "fusion_minimum_tier_configured")?;
    Ok(rank)
}

fn tier_rank(value: &str) -> Option<u8> {
    match value {
        "free" => Some(0),
        "pro" => Some(1),
        "team" => Some(2),
        "scale" => Some(3),
        _ => None,
    }
}

fn validate_unknown(evidence: &UnknownEvidence, reason_code: &'static str) -> Result<()> {
    if evidence.state != "unknown" || evidence.source != "not_negotiated" {
        return invalid("unknown_evidence");
    }
    validate_reason(&evidence.reason, reason_code)
}

fn validate_reason(reason: &CapabilityReason, expected_code: &'static str) -> Result<()> {
    if reason.code != expected_code
        || !is_bounded_text(&reason.code, MAX_REASON_CODE_BYTES)
        || !reason.code.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b':')
        })
        || !is_bounded_text(&reason.message, MAX_REASON_MESSAGE_BYTES)
        || reason.message.chars().any(char::is_control)
    {
        return invalid("reason");
    }
    Ok(())
}

fn is_bounded_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && value.trim() == value
}

fn invalid<T>(code: &'static str) -> Result<T> {
    Err(Error::InvalidGatewayCapabilities(code))
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use httpmock::Then;
    use tt_shared::{
        AccessEvidence, EnabledEvidence, FusionCapability, FusionLimits, GatewayFeatures,
        NumericLimit, SchemaVersions,
    };

    use super::*;

    fn reason(code: &str) -> CapabilityReason {
        CapabilityReason {
            code: code.into(),
            message: "Bounded responder explanation.".into(),
        }
    }

    fn unknown(code: &str) -> UnknownEvidence {
        UnknownEvidence {
            state: "unknown".into(),
            source: "not_negotiated".into(),
            reason: reason(code),
        }
    }

    fn document() -> GatewayCapabilitiesDocument {
        GatewayCapabilitiesDocument {
            schema_version: CAPABILITIES_SCHEMA_VERSION,
            scope: CAPABILITIES_SCOPE.into(),
            snapshot_scope: CAPABILITIES_SNAPSHOT_SCOPE.into(),
            generated_at: "2026-07-26T12:34:56.000Z".into(),
            features: GatewayFeatures {
                fusion: FusionCapability {
                    enabled: EnabledEvidence {
                        state: "enabled".into(),
                        source: CAPABILITIES_SCOPE.into(),
                        reason: reason("fusion_kill_switch_enabled"),
                    },
                    access: AccessEvidence {
                        state: "available".into(),
                        reason: reason("fusion_gateway_gate_passed"),
                    },
                    current_tier: TierEvidence {
                        state: "known".into(),
                        value: "pro".into(),
                        source: "authenticated_api_key".into(),
                        reason: reason("effective_tier_from_authenticated_key"),
                    },
                    minimum_tier: TierEvidence {
                        state: "known".into(),
                        value: "pro".into(),
                        source: CAPABILITIES_SCOPE.into(),
                        reason: reason("fusion_minimum_tier_configured"),
                    },
                    limits: FusionLimits {
                        member_models_max: NumericLimit {
                            value: 8,
                            enforcement: CAPABILITIES_SCOPE.into(),
                            reason: reason("fusion_member_cap"),
                        },
                    },
                },
            },
            provider_credentials: unknown("provider_credentials_not_inspected"),
            provider_health: unknown("provider_health_not_probed"),
            model_support: unknown("model_support_not_negotiated"),
            modality_support: unknown("modality_support_not_negotiated"),
            schema_versions: SchemaVersions {
                capabilities_document: SchemaVersionEvidence {
                    state: "known".into(),
                    version: Some(CAPABILITIES_SCHEMA_VERSION),
                    source: CAPABILITIES_SCOPE.into(),
                    reason: reason("capabilities_document_version"),
                },
                fusion_request: SchemaVersionEvidence {
                    state: "unversioned".into(),
                    version: None,
                    source: CAPABILITIES_SCOPE.into(),
                    reason: reason("fusion_request_schema_not_versioned"),
                },
            },
        }
    }

    fn capability_headers(then: Then) -> Then {
        then.header("content-type", "application/json")
            .header("cache-control", "private, no-store")
            .header("x-content-type-options", "nosniff")
    }

    #[tokio::test]
    async fn capabilities_reads_one_exact_authenticated_responder_document() {
        let server = MockServer::start_async().await;
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/capabilities")
                .header("accept", "application/json")
                .header("authorization", "Bearer tt_live_test");
            let then = capability_headers(then);
            then.status(200).json_body_obj(&document());
        });

        let response = Client::new(server.base_url(), "tt_live_test")
            .capabilities()
            .await
            .expect("valid capability document");
        assert_eq!(response.features.fusion.limits.member_models_max.value, 8);
        mock.assert();
    }

    #[tokio::test]
    async fn capabilities_rejects_contradiction_and_redacts_remote_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/capabilities");
            let mut value = document();
            value.features.fusion.access.state = "unavailable".into();
            value.features.fusion.access.reason = reason("fusion_disabled");
            let then = capability_headers(then);
            then.status(200).json_body_obj(&value);
        });
        assert!(matches!(
            Client::new(server.base_url(), "tt_live_test")
                .capabilities()
                .await,
            Err(Error::InvalidGatewayCapabilities("fusion_access"))
        ));

        let failed = MockServer::start_async().await;
        failed.mock(|when, then| {
            when.method(GET).path("/v1/capabilities");
            then.status(503).body("private provider diagnostic");
        });
        let error = Client::new(failed.base_url(), "tt_live_test")
            .capabilities()
            .await
            .expect_err("503 must fail");
        match error {
            Error::Status { body, .. } => {
                assert_eq!(body, "gateway capabilities request failed");
                assert!(!body.contains("provider"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn capabilities_rejects_invalid_key_and_declared_oversize() {
        let no_request = MockServer::start_async().await;
        let mock = no_request.mock(|when, then| {
            when.method(GET).path("/v1/capabilities");
            then.status(200);
        });
        assert!(matches!(
            Client::new(no_request.base_url(), "tt_test_not_live")
                .capabilities()
                .await,
            Err(Error::InvalidGatewayCapabilities("api_key"))
        ));
        assert_eq!(mock.calls(), 0);

        let oversized = MockServer::start_async().await;
        oversized.mock(|when, then| {
            when.method(GET).path("/v1/capabilities");
            let then = capability_headers(then);
            then.status(200)
                .body("x".repeat(MAX_CAPABILITIES_RESPONSE_BYTES + 1));
        });
        assert!(matches!(
            Client::new(oversized.base_url(), "tt_live_test")
                .capabilities()
                .await,
            Err(Error::ResponseTooLarge {
                limit: MAX_CAPABILITIES_RESPONSE_BYTES
            })
        ));
    }

    #[tokio::test]
    async fn capabilities_rejects_redirect_without_forwarding_bearer() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/capabilities")
                .header("authorization", "Bearer tt_live_test");
            then.status(302).header("location", "/elsewhere");
        });
        let target = server.mock(|when, then| {
            when.method(GET).path("/elsewhere");
            then.status(200).body("{}");
        });
        let caller_http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("build caller client");

        assert!(matches!(
            Client::with_http_client(caller_http, server.base_url(), "tt_live_test")
                .capabilities()
                .await,
            Err(Error::UnexpectedGatewayCapabilitiesRedirect)
        ));
        assert_eq!(target.calls(), 0, "the bearer target must not be contacted");
    }
}
