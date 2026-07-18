//! `tt capabilities` — bounded, read-only gateway-runtime evidence.
//!
//! This command deliberately reads only one authenticated
//! `GET /v1/capabilities` response. It reports facts from that responding
//! gateway process, not fleet health, provider readiness, a future request, or
//! authorization to create/activate a route.

use std::time::Duration;

use anyhow::{bail, Context as _};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{header, redirect::Policy, Client, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::ResolvedContext;

const CAPABILITIES_SCHEMA_VERSION: u32 = 1;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REASON_CODE_BYTES: usize = 96;
const MAX_REASON_MESSAGE_BYTES: usize = 600;

/// A normalized, bounded view of one responding process's capability document.
/// It intentionally keeps only reason codes; server-provided prose is checked
/// for shape but is not printed to a terminal by the default human view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilitySnapshot {
    pub schema_version: u32,
    pub scope: &'static str,
    pub snapshot_scope: &'static str,
    pub generated_at: String,
    pub fusion: FusionSnapshot,
    pub provider_credentials: UnknownFact,
    pub provider_health: UnknownFact,
    pub model_support: UnknownFact,
    pub modality_support: UnknownFact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FusionSnapshot {
    pub kill_switch: KillSwitchState,
    pub kill_switch_reason_code: String,
    pub access: FusionAccess,
    pub access_reason_code: String,
    pub current_tier: TierFact,
    pub minimum_tier: TierFact,
    pub member_models_max: u64,
    pub member_models_max_reason_code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KillSwitchState {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionAccess {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Free,
    Pro,
    Team,
    Scale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TierFact {
    pub value: Tier,
    pub source: TierSource,
    pub reason_code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TierSource {
    AuthenticatedApiKey,
    GatewayFreeDefault,
    GatewayRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnknownFact {
    pub state: &'static str,
    pub source: &'static str,
    pub reason_code: String,
}

#[derive(Debug, Deserialize)]
struct RawDocument {
    schema_version: u32,
    scope: String,
    snapshot_scope: String,
    generated_at: String,
    features: RawFeatures,
    provider_credentials: RawUnknownFact,
    provider_health: RawUnknownFact,
    model_support: RawUnknownFact,
    modality_support: RawUnknownFact,
    schema_versions: RawSchemaVersions,
}

#[derive(Debug, Deserialize)]
struct RawFeatures {
    fusion: RawFusion,
}

#[derive(Debug, Deserialize)]
struct RawFusion {
    enabled: RawEnabledFact,
    access: RawAccessFact,
    current_tier: RawTierFact,
    minimum_tier: RawTierFact,
    limits: RawFusionLimits,
}

#[derive(Debug, Deserialize)]
struct RawEnabledFact {
    state: String,
    source: String,
    reason: RawReason,
}

#[derive(Debug, Deserialize)]
struct RawAccessFact {
    state: String,
    reason: RawReason,
}

#[derive(Debug, Deserialize)]
struct RawTierFact {
    state: String,
    value: String,
    source: String,
    reason: RawReason,
}

#[derive(Debug, Deserialize)]
struct RawFusionLimits {
    member_models_max: RawMemberModelsMax,
}

#[derive(Debug, Deserialize)]
struct RawMemberModelsMax {
    value: u64,
    enforcement: String,
    reason: RawReason,
}

#[derive(Debug, Deserialize)]
struct RawUnknownFact {
    state: String,
    source: String,
    reason: RawReason,
}

#[derive(Debug, Deserialize)]
struct RawSchemaVersions {
    capabilities_document: RawSchemaVersionFact,
    fusion_request: RawSchemaVersionFact,
}

#[derive(Debug, Deserialize)]
struct RawSchemaVersionFact {
    state: String,
    version: Value,
    source: String,
    reason: RawReason,
}

#[derive(Debug, Deserialize)]
struct RawReason {
    code: String,
    message: String,
}

/// Run `tt capabilities`.
pub async fn run(
    flag_key: Option<String>,
    flag_base: Option<String>,
    json_output: bool,
) -> anyhow::Result<()> {
    let context = ResolvedContext::load(flag_key, flag_base)?;
    let key = context
        .api_key_string()
        .context("no API key — `tt capabilities` requires a configured tt_live_* key")?;
    let snapshot = fetch_capabilities(context.base_url.trim_end_matches('/'), &key).await?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&snapshot)
                .context("serialize normalized capabilities snapshot")?
        );
    } else {
        println!("{}", format_snapshot(&snapshot));
    }
    Ok(())
}

/// Fetch and validate exactly one responding-process capability snapshot.
pub async fn fetch_capabilities(base: &str, key: &str) -> anyhow::Result<CapabilitySnapshot> {
    validate_live_key(key)?;
    let client = Client::builder()
        .redirect(Policy::none())
        .build()
        .context("build capabilities HTTP client")?;
    fetch_with_client(&client, base, key).await
}

async fn fetch_with_client(
    client: &Client,
    base: &str,
    key: &str,
) -> anyhow::Result<CapabilitySnapshot> {
    fetch_with_client_timeout(client, base, key, OPERATION_TIMEOUT).await
}

async fn fetch_with_client_timeout(
    client: &Client,
    base: &str,
    key: &str,
    operation_timeout: Duration,
) -> anyhow::Result<CapabilitySnapshot> {
    validate_live_key(key)?;
    let endpoint = capabilities_endpoint(base)?;
    let operation = async {
        let response = client
            .get(endpoint)
            .header(header::ACCEPT, "application/json")
            .bearer_auth(key)
            .send()
            .await
            .context("request gateway capabilities")?;

        if !response.status().is_success() {
            return status_error(response.status());
        }

        let body = read_bounded_body(response).await?;
        parse_snapshot(&body)
    };

    tokio::time::timeout(operation_timeout, operation)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "gateway capabilities request timed out after {}",
                describe_timeout(operation_timeout)
            )
        })?
}

fn capabilities_endpoint(base: &str) -> anyhow::Result<Url> {
    let mut endpoint = Url::parse(base.trim_end_matches('/'))
        .context("gateway capabilities base must be an absolute URL")?;
    if endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        bail!("gateway capabilities base must not contain userinfo, query, or fragment")
    }
    if endpoint.scheme() != "https" && !(endpoint.scheme() == "http" && is_loopback_http(&endpoint))
    {
        bail!(
            "gateway capabilities requires an HTTPS base URL (or literal loopback HTTP for local development)"
        )
    }

    let base_path = endpoint.path().trim_end_matches('/');
    endpoint.set_path(&format!("{base_path}/v1/capabilities"));
    Ok(endpoint)
}

fn is_loopback_http(url: &Url) -> bool {
    url.host_str()
        .map(|host| host.trim_matches(&['[', ']'][..]))
        .and_then(|host| host.parse::<std::net::IpAddr>().ok())
        .is_some_and(|address| address.is_loopback())
}

fn describe_timeout(timeout: Duration) -> String {
    if timeout == Duration::from_secs(1) {
        "1 second".to_string()
    } else if timeout.as_millis() % 1_000 == 0 {
        format!("{} seconds", timeout.as_secs())
    } else {
        format!("{} ms", timeout.as_millis())
    }
}

fn validate_live_key(key: &str) -> anyhow::Result<()> {
    if !key.starts_with("tt_live_") || key.len() <= "tt_live_".len() {
        bail!("`tt capabilities` requires a configured tt_live_* key")
    }
    Ok(())
}

fn status_error(status: StatusCode) -> anyhow::Result<CapabilitySnapshot> {
    match status {
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            bail!("gateway capabilities endpoint is unavailable on this gateway revision")
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            bail!("gateway rejected the configured capabilities key")
        }
        _ => bail!(
            "gateway capabilities request failed with HTTP {}",
            status.as_u16()
        ),
    }
}

async fn read_bounded_body(mut response: Response) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("gateway capabilities response exceeds the 64 KiB limit")
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read gateway capabilities response")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("gateway capabilities response exceeds the 64 KiB limit")
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Strictly parse the known v1 evidence while allowing additive JSON members.
/// Unknown enums, scopes, versions, readiness claims, and contradictions fail
/// closed rather than being interpreted as a favorable capability state.
pub fn parse_snapshot(body: &[u8]) -> anyhow::Result<CapabilitySnapshot> {
    let document: RawDocument =
        serde_json::from_slice(body).context("parse gateway capabilities document")?;
    if document.schema_version != CAPABILITIES_SCHEMA_VERSION
        || document.scope != "gateway_runtime"
        || document.snapshot_scope != "responding_process"
    {
        bail!("unsupported gateway capabilities document scope or version")
    }

    let generated_at = parse_canonical_timestamp(&document.generated_at)?;
    validate_schema_versions(document.schema_versions)?;

    let (kill_switch, kill_switch_reason_code) =
        parse_kill_switch(document.features.fusion.enabled)?;
    let (access, access_reason_code) = parse_access(document.features.fusion.access)?;
    let current_tier = parse_tier_fact(
        document.features.fusion.current_tier,
        &[
            TierSource::AuthenticatedApiKey,
            TierSource::GatewayFreeDefault,
        ],
    )?;
    let minimum_tier = parse_tier_fact(
        document.features.fusion.minimum_tier,
        &[TierSource::GatewayRuntime],
    )?;
    let (member_models_max, member_models_max_reason_code) =
        parse_member_models_max(document.features.fusion.limits.member_models_max)?;
    let provider_credentials = parse_unknown_fact(document.provider_credentials)?;
    let provider_health = parse_unknown_fact(document.provider_health)?;
    let model_support = parse_unknown_fact(document.model_support)?;
    let modality_support = parse_unknown_fact(document.modality_support)?;

    if (kill_switch == KillSwitchState::Disabled && access != FusionAccess::Unavailable)
        || (access == FusionAccess::Available
            && (kill_switch != KillSwitchState::Enabled || current_tier.value < minimum_tier.value))
    {
        bail!("gateway capabilities document has contradictory Fusion gate evidence")
    }

    Ok(CapabilitySnapshot {
        schema_version: CAPABILITIES_SCHEMA_VERSION,
        scope: "gateway_runtime",
        snapshot_scope: "responding_process",
        generated_at,
        fusion: FusionSnapshot {
            kill_switch,
            kill_switch_reason_code,
            access,
            access_reason_code,
            current_tier,
            minimum_tier,
            member_models_max,
            member_models_max_reason_code,
        },
        provider_credentials,
        provider_health,
        model_support,
        modality_support,
    })
}

fn parse_canonical_timestamp(value: &str) -> anyhow::Result<String> {
    if !is_bounded_text(value, 64) {
        bail!("gateway capabilities document has an invalid generated_at timestamp")
    }
    let timestamp = DateTime::parse_from_rfc3339(value)
        .context("parse gateway capabilities generated_at timestamp")?
        .with_timezone(&Utc);
    if timestamp.to_rfc3339_opts(SecondsFormat::Millis, true) != value {
        bail!("gateway capabilities generated_at must be canonical UTC milliseconds")
    }
    Ok(value.to_string())
}

fn validate_schema_versions(raw: RawSchemaVersions) -> anyhow::Result<()> {
    if raw.capabilities_document.state != "known"
        || raw.capabilities_document.version.as_u64() != Some(CAPABILITIES_SCHEMA_VERSION as u64)
        || raw.capabilities_document.source != "gateway_runtime"
        || raw.fusion_request.state != "unversioned"
        || raw.fusion_request.version != Value::Null
        || raw.fusion_request.source != "gateway_runtime"
    {
        bail!("gateway capabilities document has incompatible schema-version evidence")
    }
    validate_reason(raw.capabilities_document.reason)?;
    validate_reason(raw.fusion_request.reason)?;
    Ok(())
}

fn parse_kill_switch(raw: RawEnabledFact) -> anyhow::Result<(KillSwitchState, String)> {
    if raw.source != "gateway_runtime" {
        bail!("gateway capabilities document has an invalid Fusion switch source")
    }
    let reason_code = validate_reason(raw.reason)?;
    match raw.state.as_str() {
        "enabled" => Ok((KillSwitchState::Enabled, reason_code)),
        "disabled" => Ok((KillSwitchState::Disabled, reason_code)),
        _ => bail!("gateway capabilities document has an invalid Fusion switch state"),
    }
}

fn parse_access(raw: RawAccessFact) -> anyhow::Result<(FusionAccess, String)> {
    let reason_code = validate_reason(raw.reason)?;
    match raw.state.as_str() {
        "available" => Ok((FusionAccess::Available, reason_code)),
        "unavailable" => Ok((FusionAccess::Unavailable, reason_code)),
        _ => bail!("gateway capabilities document has an invalid Fusion access state"),
    }
}

fn parse_tier_fact(raw: RawTierFact, allowed_sources: &[TierSource]) -> anyhow::Result<TierFact> {
    if raw.state != "known" {
        bail!("gateway capabilities document has an unknown tier state")
    }
    let value = parse_tier(&raw.value)?;
    let source = parse_tier_source(&raw.source)?;
    if !allowed_sources.contains(&source) {
        bail!("gateway capabilities document has an invalid tier evidence source")
    }
    let reason_code = validate_reason(raw.reason)?;
    Ok(TierFact {
        value,
        source,
        reason_code,
    })
}

fn parse_tier(value: &str) -> anyhow::Result<Tier> {
    match value {
        "free" => Ok(Tier::Free),
        "pro" => Ok(Tier::Pro),
        "team" => Ok(Tier::Team),
        "scale" => Ok(Tier::Scale),
        _ => bail!("gateway capabilities document has an invalid tier value"),
    }
}

fn parse_tier_source(value: &str) -> anyhow::Result<TierSource> {
    match value {
        "authenticated_api_key" => Ok(TierSource::AuthenticatedApiKey),
        "gateway_free_default" => Ok(TierSource::GatewayFreeDefault),
        "gateway_runtime" => Ok(TierSource::GatewayRuntime),
        _ => bail!("gateway capabilities document has an invalid tier source"),
    }
}

fn parse_member_models_max(raw: RawMemberModelsMax) -> anyhow::Result<(u64, String)> {
    if raw.enforcement != "gateway_runtime" || raw.value == 0 {
        bail!("gateway capabilities document has an unsupported Fusion member-model cap")
    }
    Ok((raw.value, validate_reason(raw.reason)?))
}

fn parse_unknown_fact(raw: RawUnknownFact) -> anyhow::Result<UnknownFact> {
    if raw.state != "unknown" || raw.source != "not_negotiated" {
        bail!("gateway capabilities document made an unsupported provider or model readiness claim")
    }
    Ok(UnknownFact {
        state: "unknown",
        source: "not_negotiated",
        reason_code: validate_reason(raw.reason)?,
    })
}

fn validate_reason(raw: RawReason) -> anyhow::Result<String> {
    if !is_bounded_text(&raw.code, MAX_REASON_CODE_BYTES)
        || !is_bounded_text(&raw.message, MAX_REASON_MESSAGE_BYTES)
        || !raw.code.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b':')
        })
        || raw.message.chars().any(char::is_control)
    {
        bail!("gateway capabilities document has an invalid reason")
    }
    Ok(raw.code)
}

fn is_bounded_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && value.trim() == value
}

/// Human-readable output deliberately omits remote reason prose and avoids a
/// readiness claim. The four unknown facts are rendered explicitly instead of
/// being inferred from the catalog or a stored credential.
#[must_use]
pub fn format_snapshot(snapshot: &CapabilitySnapshot) -> String {
    let switch = match snapshot.fusion.kill_switch {
        KillSwitchState::Enabled => "on",
        KillSwitchState::Disabled => "off",
    };
    let access = match snapshot.fusion.access {
        FusionAccess::Available => "passed this responder's kill-switch + tier gate",
        FusionAccess::Unavailable => "did not pass this responder's kill-switch + tier gate",
    };
    format!(
        "Gateway runtime capabilities — one responding process\n\
         Snapshot: {}\n\
         Fusion kill switch: {switch}\n\
         Fusion gate: {access}\n\
         Current tier: {} ({})\n\
         Fusion minimum tier: {}\n\
         Fusion member-model cap: {}\n\
         Provider credentials: unknown (not negotiated)\n\
         Provider health: unknown (not negotiated)\n\
         Model support: unknown (not negotiated)\n\
         Modality support: unknown (not negotiated)\n\n\
         This snapshot does not prove fleet/deployment consistency, a later request's success, credential validity, provider health, model or modality support, request acceptance, route activation, or route execution.",
        snapshot.generated_at,
        tier_name(snapshot.fusion.current_tier.value),
        tier_source_name(snapshot.fusion.current_tier.source),
        tier_name(snapshot.fusion.minimum_tier.value),
        snapshot.fusion.member_models_max,
    )
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Free => "free",
        Tier::Pro => "pro",
        Tier::Team => "team",
        Tier::Scale => "scale",
    }
}

fn tier_source_name(source: TierSource) -> &'static str {
    match source {
        TierSource::AuthenticatedApiKey => "authenticated key tier",
        TierSource::GatewayFreeDefault => "gateway free-tier fallback",
        TierSource::GatewayRuntime => "gateway runtime configuration",
    }
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn valid_document() -> Value {
        json!({
            "schema_version": 1,
            "scope": "gateway_runtime",
            "snapshot_scope": "responding_process",
            "generated_at": "2026-07-17T12:00:00.000Z",
            "features": {
                "fusion": {
                    "enabled": {
                        "state": "enabled",
                        "source": "gateway_runtime",
                        "reason": { "code": "fusion_kill_switch_enabled", "message": "Fusion is enabled." }
                    },
                    "access": {
                        "state": "available",
                        "reason": { "code": "fusion_gateway_gate_passed", "message": "The gate passed." }
                    },
                    "current_tier": {
                        "state": "known",
                        "value": "pro",
                        "source": "authenticated_api_key",
                        "reason": { "code": "effective_tier_from_authenticated_key", "message": "The key tier is known." }
                    },
                    "minimum_tier": {
                        "state": "known",
                        "value": "pro",
                        "source": "gateway_runtime",
                        "reason": { "code": "fusion_minimum_tier_configured", "message": "The minimum tier is known." }
                    },
                    "limits": {
                        "member_models_max": {
                            "value": 8,
                            "enforcement": "gateway_runtime",
                            "reason": { "code": "fusion_member_cap", "message": "The member cap is enforced." }
                        }
                    }
                }
            },
            "provider_credentials": unknown_fact("provider_credentials_not_inspected"),
            "provider_health": unknown_fact("provider_health_not_probed"),
            "model_support": unknown_fact("model_support_not_negotiated"),
            "modality_support": unknown_fact("modality_support_not_negotiated"),
            "schema_versions": {
                "capabilities_document": {
                    "state": "known",
                    "version": 1,
                    "source": "gateway_runtime",
                    "reason": { "code": "capabilities_document_version", "message": "Capabilities v1." }
                },
                "fusion_request": {
                    "state": "unversioned",
                    "version": null,
                    "source": "gateway_runtime",
                    "reason": { "code": "fusion_request_schema_not_versioned", "message": "Fusion has no independent schema." }
                }
            }
        })
    }

    fn unknown_fact(code: &str) -> Value {
        json!({
            "state": "unknown",
            "source": "not_negotiated",
            "reason": { "code": code, "message": "This fact is not negotiated." }
        })
    }

    fn parse(value: Value) -> anyhow::Result<CapabilitySnapshot> {
        parse_snapshot(&serde_json::to_vec(&value).expect("fixture JSON"))
    }

    #[test]
    fn accepts_one_consistent_responding_process_snapshot_and_additive_fields() {
        let mut value = valid_document();
        value["future_optional_field"] = json!({ "additive": true });
        value["features"]["fusion"]["future_optional_field"] = json!(true);

        let snapshot = parse(value).expect("valid v1 evidence");
        assert_eq!(snapshot.scope, "gateway_runtime");
        assert_eq!(snapshot.snapshot_scope, "responding_process");
        assert_eq!(snapshot.fusion.kill_switch, KillSwitchState::Enabled);
        assert_eq!(
            snapshot.fusion.kill_switch_reason_code,
            "fusion_kill_switch_enabled"
        );
        assert_eq!(snapshot.fusion.access, FusionAccess::Available);
        assert_eq!(
            snapshot.fusion.access_reason_code,
            "fusion_gateway_gate_passed"
        );
        assert_eq!(snapshot.fusion.member_models_max, 8);
        assert_eq!(
            snapshot.fusion.member_models_max_reason_code,
            "fusion_member_cap"
        );
        assert_eq!(snapshot.provider_health.state, "unknown");
    }

    #[test]
    fn accepts_disabled_and_tier_blocked_snapshots_without_turning_them_into_errors() {
        let mut disabled = valid_document();
        disabled["features"]["fusion"]["enabled"]["state"] = json!("disabled");
        disabled["features"]["fusion"]["access"]["state"] = json!("unavailable");
        assert_eq!(
            parse(disabled).unwrap().fusion.access,
            FusionAccess::Unavailable
        );

        let mut tier_blocked = valid_document();
        tier_blocked["features"]["fusion"]["current_tier"]["value"] = json!("free");
        tier_blocked["features"]["fusion"]["access"]["state"] = json!("unavailable");
        assert_eq!(
            parse(tier_blocked).unwrap().fusion.access,
            FusionAccess::Unavailable
        );
    }

    #[test]
    fn rejects_versions_scopes_unknown_readiness_and_gate_contradictions() {
        let mut future = valid_document();
        future["schema_version"] = json!(2);
        assert!(parse(future).is_err());

        let mut fleet = valid_document();
        fleet["snapshot_scope"] = json!("fleet");
        assert!(parse(fleet).is_err());

        let mut provider_claim = valid_document();
        provider_claim["provider_health"]["state"] = json!("available");
        assert!(parse(provider_claim).is_err());

        let mut contradiction = valid_document();
        contradiction["features"]["fusion"]["enabled"]["state"] = json!("disabled");
        assert!(parse(contradiction).is_err());
    }

    #[test]
    fn accepts_positive_gateway_caps_and_rejects_invalid_evidence() {
        let mut timestamp = valid_document();
        timestamp["generated_at"] = json!("2026-07-17T12:00:00Z");
        assert!(parse(timestamp).is_err());

        let mut schema_versions = valid_document();
        schema_versions["schema_versions"]["fusion_request"]["version"] = json!(1);
        assert!(parse(schema_versions).is_err());

        let mut cap = valid_document();
        cap["features"]["fusion"]["limits"]["member_models_max"]["value"] = json!(65);
        assert_eq!(parse(cap).unwrap().fusion.member_models_max, 65);

        let mut zero_cap = valid_document();
        zero_cap["features"]["fusion"]["limits"]["member_models_max"]["value"] = json!(0);
        assert!(parse(zero_cap).is_err());

        let mut unsafe_reason = valid_document();
        unsafe_reason["provider_health"]["reason"]["code"] = json!("safe\u{202e}spoof");
        assert!(parse(unsafe_reason).is_err());
    }

    #[test]
    fn human_output_keeps_unknowns_and_rejects_readiness_language() {
        let output = format_snapshot(&parse(valid_document()).unwrap());
        assert!(output.contains("one responding process"));
        assert!(output.contains("unknown (not negotiated)"));
        assert!(output.contains("does not prove fleet/deployment consistency"));
        assert!(!output.contains("ready"));
        assert!(!output.contains("healthy"));
        assert!(!output.contains("route activation/execution"));
    }

    #[test]
    fn normalized_json_keeps_all_reason_codes_without_remote_prose() {
        let mut disabled = valid_document();
        disabled["features"]["fusion"]["enabled"]["state"] = json!("disabled");
        disabled["features"]["fusion"]["enabled"]["reason"]["code"] =
            json!("fusion_kill_switch_disabled");
        disabled["features"]["fusion"]["access"]["state"] = json!("unavailable");
        disabled["features"]["fusion"]["access"]["reason"]["code"] = json!("fusion_disabled");
        disabled["features"]["fusion"]["access"]["reason"]["message"] =
            json!("remote prose must not escape");

        let json = serde_json::to_value(parse(disabled).unwrap()).expect("normalize JSON");
        assert_eq!(
            json["fusion"]["kill_switch_reason_code"],
            "fusion_kill_switch_disabled"
        );
        assert_eq!(json["fusion"]["access_reason_code"], "fusion_disabled");
        assert_eq!(
            json["fusion"]["member_models_max_reason_code"],
            "fusion_member_cap"
        );
        let serialized = json.to_string();
        assert!(!serialized.contains("remote prose"));
        assert!(!serialized.contains("tt_live_"));
    }

    #[test]
    fn endpoint_requires_safe_base_and_preserves_a_path_prefix() {
        assert!(capabilities_endpoint("http://gateway.example").is_err());
        assert!(capabilities_endpoint("https://key@gateway.example").is_err());
        assert!(capabilities_endpoint("https://gateway.example?tenant=other").is_err());
        assert!(capabilities_endpoint("https://gateway.example#fragment").is_err());
        assert_eq!(
            capabilities_endpoint("https://gateway.example/gateway/")
                .unwrap()
                .as_str(),
            "https://gateway.example/gateway/v1/capabilities"
        );
        assert!(capabilities_endpoint("http://127.0.0.1:8787").is_ok());
        assert!(capabilities_endpoint("http://[::1]:8787").is_ok());
    }

    #[tokio::test]
    async fn fetch_requires_a_live_key_and_does_not_follow_redirects() {
        let server = MockServer::start_async().await;
        let no_request = server.mock(|when, then| {
            when.method(GET).path("/v1/capabilities");
            then.status(200).json_body(valid_document());
        });
        assert!(fetch_capabilities(&server.base_url(), "tt_test_not_live")
            .await
            .is_err());
        assert_eq!(no_request.calls_async().await, 0);

        let server = MockServer::start_async().await;
        let redirect = server.mock(|when, then| {
            when.method(GET).path("/v1/capabilities");
            then.status(302).header("location", "/elsewhere");
        });
        let error = fetch_capabilities(&server.base_url(), "tt_live_capabilities")
            .await
            .expect_err("redirects must not forward a bearer");
        assert!(error.to_string().contains("HTTP 302"));
        assert_eq!(redirect.calls_async().await, 1);
    }

    #[tokio::test]
    async fn fetch_makes_one_authenticated_get_and_normalizes_the_snapshot() {
        let server = MockServer::start_async().await;
        let probe = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/capabilities")
                .header("authorization", "Bearer tt_live_capabilities")
                .header("accept", "application/json");
            then.status(201).json_body(valid_document());
        });

        let snapshot = fetch_capabilities(&server.base_url(), "tt_live_capabilities")
            .await
            .expect("one valid capability response");
        assert_eq!(snapshot.fusion.access, FusionAccess::Available);
        assert_eq!(probe.calls_async().await, 1);
    }

    #[tokio::test]
    async fn fetch_bounds_success_bodies_and_redacts_classified_error_bodies() {
        let server = MockServer::start_async().await;
        let oversized = "x".repeat(MAX_RESPONSE_BYTES + 1);
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/capabilities")
                .header("authorization", "Bearer tt_live_capabilities");
            then.status(200).body(oversized);
        });
        let error = fetch_capabilities(&server.base_url(), "tt_live_capabilities")
            .await
            .expect_err("oversized bodies fail locally");
        assert!(error.to_string().contains("64 KiB"));

        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/capabilities");
            then.status(401).body("provider diagnostic must not escape");
        });
        let error = fetch_capabilities(&server.base_url(), "tt_live_capabilities")
            .await
            .expect_err("rejected auth is unavailable evidence");
        assert!(error.to_string().contains("rejected"));
        assert!(!error.to_string().contains("provider diagnostic"));

        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/capabilities");
            then.status(404)
                .body("old gateway diagnostic must not escape");
        });
        let error = fetch_capabilities(&server.base_url(), "tt_live_capabilities")
            .await
            .expect_err("older endpoint is classified without its body");
        assert!(error.to_string().contains("endpoint is unavailable"));
        assert!(!error.to_string().contains("old gateway diagnostic"));
    }

    async fn raw_server(
        headers: &'static [u8],
        delayed_body: Vec<u8>,
        delay: Duration,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw test server");
        let address = listener.local_addr().expect("raw test server address");
        let task = tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0_u8; 4_096];
            let _ = stream.read(&mut request).await;
            let _ = stream.write_all(headers).await;
            let _ = stream.flush().await;
            tokio::time::sleep(delay).await;
            let _ = stream.write_all(&delayed_body).await;
            let _ = stream.flush().await;
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn full_operation_deadline_includes_a_slow_success_body() {
        let (base, server) = raw_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n",
            b"{}".to_vec(),
            Duration::from_millis(250),
        )
        .await;
        let client = Client::builder().build().expect("test client");
        let error = fetch_with_client_timeout(
            &client,
            &base,
            "tt_live_capabilities",
            Duration::from_millis(50),
        )
        .await
        .expect_err("the full body read must share the deadline");
        assert!(error.to_string().contains("timed out after 50 ms"));
        server.await.expect("raw test server task");
    }

    #[tokio::test]
    async fn fetch_counts_chunked_success_bodies_without_content_length() {
        let payload = vec![b'x'; MAX_RESPONSE_BYTES + 1];
        let mut chunked_body = format!("{:X}\r\n", payload.len()).into_bytes();
        chunked_body.extend_from_slice(&payload);
        chunked_body.extend_from_slice(b"\r\n0\r\n\r\n");
        let (base, server) = raw_server(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            chunked_body,
            Duration::ZERO,
        )
        .await;
        let error = fetch_capabilities(&base, "tt_live_capabilities")
            .await
            .expect_err("chunked success body must respect the cap");
        assert!(error.to_string().contains("64 KiB"));
        server.await.expect("raw test server task");
    }
}
