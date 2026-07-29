//! Authenticated gateway-runtime capability evidence.
//!
//! This endpoint is deliberately narrower than a product-wide entitlement
//! system. It can report facts owned by the responding gateway process (the
//! Fusion kill switch, effective caller tier, minimum tier, and member cap),
//! but it does not decrypt credentials, probe providers, or pretend a catalog
//! row proves that a request will be accepted. Those unproven facts are
//! explicit `unknown` values in the wire document.

use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    Extension, Json,
};
use chrono::{DateTime, SecondsFormat, Utc};
use tt_auth::ApiKeyContext;
use tt_shared::CallerTier;
pub use tt_shared::{
    AccessEvidence, CapabilityReason, EnabledEvidence, FusionCapability, FusionLimits,
    GatewayCapabilitiesDocument, GatewayFeatures, NumericLimit, SchemaVersionEvidence,
    SchemaVersions, TierEvidence, UnknownEvidence, CAPABILITIES_SCHEMA_VERSION, CAPABILITIES_SCOPE,
    CAPABILITIES_SNAPSHOT_SCOPE,
};

use crate::{
    routes::panel::{panel_max_members, panel_tier_rank},
    ApiError, ApiResult, AppState, DOGFOOD_ORG_ID,
};

// These are stable v1 wire codes, not provider observations. Each identifies
// why the responding process deliberately leaves the corresponding fact
// unknown rather than inferring readiness from local configuration or catalog
// metadata.
const PROVIDER_CREDENTIALS_NOT_INSPECTED_CODE: &str = "provider_credentials_not_inspected";
const PROVIDER_HEALTH_NOT_PROBED_CODE: &str = "provider_health_not_probed";
const MODEL_SUPPORT_NOT_NEGOTIATED_CODE: &str = "model_support_not_negotiated";
const MODALITY_SUPPORT_NOT_NEGOTIATED_CODE: &str = "modality_support_not_negotiated";

/// `GET /v1/capabilities` — a no-store snapshot of the responding gateway
/// process's known runtime facts for one authenticated `tt_live_*` caller.
///
/// It intentionally rejects anonymous, sandbox, and dogfood traffic. The
/// document is scoped to the authenticated caller's effective tier but never
/// returns an org ID, key ID, credential, provider configuration, or secret.
pub async fn handler(
    State(state): State<AppState>,
    context: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Response> {
    let context = require_real_key(context)?;
    let document = build_document(&state, context.tier, Utc::now());
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

pub(crate) fn require_real_key(
    context: Option<Extension<ApiKeyContext>>,
) -> Result<ApiKeyContext, ApiError> {
    match context {
        Some(Extension(context)) if context.org_id != DOGFOOD_ORG_ID => Ok(context),
        // `tt_test_*` requests intentionally bypass live-key verification and
        // dogfood traffic carries a synthetic identity. Neither is evidence for
        // an organization capability snapshot.
        _ => Err(ApiError::Unauthorized),
    }
}

/// Build the serializable document from already-authenticated, process-local
/// facts. Kept pure so the distinction between a known gate and an unknown
/// request/provider outcome stays easy to regression-test.
pub fn build_document(
    state: &AppState,
    authenticated_tier: Option<CallerTier>,
    generated_at: DateTime<Utc>,
) -> GatewayCapabilitiesDocument {
    let current_tier = authenticated_tier.unwrap_or(CallerTier::Free);
    let current_tier_source = if authenticated_tier.is_some() {
        "authenticated_api_key"
    } else {
        "gateway_free_default"
    };
    let current_tier_reason = if authenticated_tier.is_some() {
        reason(
            "effective_tier_from_authenticated_key",
            "The gateway resolved this authenticated API key's effective organization tier before handling the request.",
        )
    } else {
        reason(
            "effective_tier_defaulted_to_free",
            "No resolved tier was attached to this authenticated request, so the gateway applies its actual Free-tier fallback.",
        )
    };

    let fusion_enabled = if state.panel_enabled {
        EnabledEvidence {
            state: "enabled".into(),
            source: CAPABILITIES_SCOPE.into(),
            reason: reason(
                "fusion_kill_switch_enabled",
                "Fusion is enabled on the responding gateway process.",
            ),
        }
    } else {
        EnabledEvidence {
            state: "disabled".into(),
            source: CAPABILITIES_SCOPE.into(),
            reason: reason(
                "fusion_kill_switch_disabled",
                "Fusion is disabled on the responding gateway process.",
            ),
        }
    };

    let fusion_access = if !state.panel_enabled {
        AccessEvidence {
            state: "unavailable".into(),
            reason: reason(
                "fusion_disabled",
                "The Fusion kill switch rejects panel requests before provider dispatch or billing.",
            ),
        }
    } else if panel_tier_rank(current_tier) < panel_tier_rank(state.panel_min_tier) {
        AccessEvidence {
            state: "unavailable".into(),
            reason: reason(
                "fusion_tier_below_minimum",
                "This authenticated request's effective tier is below the responding gateway process's configured Fusion minimum tier.",
            ),
        }
    } else {
        AccessEvidence {
            state: "available".into(),
            reason: reason(
                "fusion_gateway_gate_passed",
                "The responding gateway process allows this caller through Fusion's kill-switch and tier gate. Credential, model, budget, and provider checks still happen for each request.",
            ),
        }
    };

    GatewayCapabilitiesDocument {
        schema_version: CAPABILITIES_SCHEMA_VERSION,
        scope: CAPABILITIES_SCOPE.into(),
        // A load-balanced deployment can briefly contain mixed binary/config
        // versions. This field prevents consumers from mistaking one response
        // for a fleet-wide reservation or a later dispatch guarantee.
        snapshot_scope: CAPABILITIES_SNAPSHOT_SCOPE.into(),
        generated_at: generated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        features: GatewayFeatures {
            fusion: FusionCapability {
                enabled: fusion_enabled,
                access: fusion_access,
                current_tier: TierEvidence {
                    state: "known".into(),
                    value: tier_name(current_tier).into(),
                    source: current_tier_source.into(),
                    reason: current_tier_reason,
                },
                minimum_tier: TierEvidence {
                    state: "known".into(),
                    value: tier_name(state.panel_min_tier).into(),
                    source: CAPABILITIES_SCOPE.into(),
                    reason: reason(
                        "fusion_minimum_tier_configured",
                        "This is the Fusion minimum tier configured on the responding gateway process.",
                    ),
                },
                limits: FusionLimits {
                    member_models_max: NumericLimit {
                        value: panel_max_members(),
                        enforcement: CAPABILITIES_SCOPE.into(),
                        reason: reason(
                            "fusion_member_cap",
                            "The gateway rejects Fusion configurations with more member models than this cap.",
                        ),
                    },
                },
            },
        },
        provider_credentials: unknown_fact(
            PROVIDER_CREDENTIALS_NOT_INSPECTED_CODE,
            "This endpoint does not decrypt, count, or probe provider credentials. A configured record is not treated as provider readiness.",
        ),
        provider_health: unknown_fact(
            PROVIDER_HEALTH_NOT_PROBED_CODE,
            "This endpoint does not make provider health probes or spend-producing test requests.",
        ),
        model_support: unknown_fact(
            MODEL_SUPPORT_NOT_NEGOTIATED_CODE,
            "A registered catalog model or provider inference path does not prove this request's credentials, modality, budget, or upstream acceptance. Use it only as metadata, not readiness evidence.",
        ),
        modality_support: unknown_fact(
            MODALITY_SUPPORT_NOT_NEGOTIATED_CODE,
            "Modality support is request-specific and is not negotiated by this endpoint.",
        ),
        schema_versions: SchemaVersions {
            capabilities_document: SchemaVersionEvidence {
                state: "known".into(),
                version: Some(CAPABILITIES_SCHEMA_VERSION),
                source: CAPABILITIES_SCOPE.into(),
                reason: reason(
                    "capabilities_document_version",
                    "This response follows the gateway runtime capabilities document schema.",
                ),
            },
            fusion_request: SchemaVersionEvidence {
                state: "unversioned".into(),
                version: None,
                source: CAPABILITIES_SCOPE.into(),
                reason: reason(
                    "fusion_request_schema_not_versioned",
                    "Fusion currently uses the OpenAI-compatible request envelope and tt_extras without an independently negotiated request schema version.",
                ),
            },
        },
    }
}

fn tier_name(tier: CallerTier) -> &'static str {
    match tier {
        CallerTier::Free => "free",
        CallerTier::Pro => "pro",
        CallerTier::Team => "team",
        CallerTier::Scale => "scale",
    }
}

fn unknown_fact(code: &'static str, message: &'static str) -> UnknownEvidence {
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
    use chrono::TimeZone;

    use super::*;
    use crate::ProviderRegistry;

    #[test]
    fn document_keeps_provider_and_model_readiness_unknown() {
        let state = AppState::new(ProviderRegistry::new())
            .with_panel_enabled(true)
            .with_panel_min_tier(CallerTier::Pro);
        let document = build_document(
            &state,
            Some(CallerTier::Pro),
            Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0).unwrap(),
        );

        assert_eq!(document.scope, "gateway_runtime");
        assert_eq!(document.snapshot_scope, "responding_process");
        assert_eq!(document.features.fusion.access.state, "available");
        assert_eq!(
            document.features.fusion.limits.member_models_max.value,
            panel_max_members(),
            "the capabilities document must use the same resolver as request validation"
        );
        assert_eq!(document.provider_credentials.state, "unknown");
        assert_eq!(document.provider_health.state, "unknown");
        assert_eq!(document.model_support.state, "unknown");
        assert_eq!(document.modality_support.state, "unknown");
    }

    #[test]
    fn unavailable_gate_distinguishes_disabled_from_tier_blocked() {
        let disabled = AppState::new(ProviderRegistry::new())
            .with_panel_enabled(false)
            .with_panel_min_tier(CallerTier::Pro);
        let disabled_document = build_document(&disabled, Some(CallerTier::Free), Utc::now());
        assert_eq!(
            disabled_document.features.fusion.access.state,
            "unavailable"
        );
        assert_eq!(
            disabled_document.features.fusion.access.reason.code,
            "fusion_disabled"
        );

        let tier_blocked = AppState::new(ProviderRegistry::new())
            .with_panel_enabled(true)
            .with_panel_min_tier(CallerTier::Pro);
        let tier_blocked_document =
            build_document(&tier_blocked, Some(CallerTier::Free), Utc::now());
        assert_eq!(
            tier_blocked_document.features.fusion.access.state,
            "unavailable"
        );
        assert_eq!(
            tier_blocked_document.features.fusion.access.reason.code,
            "fusion_tier_below_minimum"
        );
    }
}
