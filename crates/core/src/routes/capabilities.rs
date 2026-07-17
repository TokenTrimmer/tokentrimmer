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
use serde::Serialize;
use tt_auth::ApiKeyContext;
use tt_shared::CallerTier;

use crate::{
    routes::panel::{panel_max_members, panel_tier_rank},
    ApiError, ApiResult, AppState, DOGFOOD_ORG_ID,
};

/// Wire version for `GET /v1/capabilities`.
///
/// New optional fields are additive. A breaking change must use a new version
/// instead of asking clients to infer a changed meaning from a familiar field.
pub const CAPABILITIES_SCHEMA_VERSION: u32 = 1;

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

fn require_real_key(context: Option<Extension<ApiKeyContext>>) -> Result<ApiKeyContext, ApiError> {
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
        CapabilityReason {
            code: "effective_tier_from_authenticated_key",
            message:
                "The gateway resolved this authenticated API key's effective organization tier before handling the request.",
        }
    } else {
        CapabilityReason {
            code: "effective_tier_defaulted_to_free",
            message:
                "No resolved tier was attached to this authenticated request, so the gateway applies its actual Free-tier fallback.",
        }
    };

    let fusion_enabled = if state.panel_enabled {
        EnabledEvidence {
            state: "enabled",
            source: "gateway_runtime",
            reason: CapabilityReason {
                code: "fusion_kill_switch_enabled",
                message: "Fusion is enabled on the responding gateway process.",
            },
        }
    } else {
        EnabledEvidence {
            state: "disabled",
            source: "gateway_runtime",
            reason: CapabilityReason {
                code: "fusion_kill_switch_disabled",
                message: "Fusion is disabled on the responding gateway process.",
            },
        }
    };

    let fusion_access = if !state.panel_enabled {
        AccessEvidence {
            state: "unavailable",
            reason: CapabilityReason {
                code: "fusion_disabled",
                message:
                    "The Fusion kill switch rejects panel requests before provider dispatch or billing.",
            },
        }
    } else if panel_tier_rank(current_tier) < panel_tier_rank(state.panel_min_tier) {
        AccessEvidence {
            state: "unavailable",
            reason: CapabilityReason {
                code: "fusion_tier_below_minimum",
                message:
                    "This authenticated request's effective tier is below the responding gateway process's configured Fusion minimum tier.",
            },
        }
    } else {
        AccessEvidence {
            state: "available",
            reason: CapabilityReason {
                code: "fusion_gateway_gate_passed",
                message:
                    "The responding gateway process allows this caller through Fusion's kill-switch and tier gate. Credential, model, budget, and provider checks still happen for each request.",
            },
        }
    };

    GatewayCapabilitiesDocument {
        schema_version: CAPABILITIES_SCHEMA_VERSION,
        scope: "gateway_runtime",
        // A load-balanced deployment can briefly contain mixed binary/config
        // versions. This field prevents consumers from mistaking one response
        // for a fleet-wide reservation or a later dispatch guarantee.
        snapshot_scope: "responding_process",
        generated_at: generated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        features: GatewayFeatures {
            fusion: FusionCapability {
                enabled: fusion_enabled,
                access: fusion_access,
                current_tier: TierEvidence {
                    state: "known",
                    value: tier_name(current_tier),
                    source: current_tier_source,
                    reason: current_tier_reason,
                },
                minimum_tier: TierEvidence {
                    state: "known",
                    value: tier_name(state.panel_min_tier),
                    source: "gateway_runtime",
                    reason: CapabilityReason {
                        code: "fusion_minimum_tier_configured",
                        message:
                            "This is the Fusion minimum tier configured on the responding gateway process.",
                    },
                },
                limits: FusionLimits {
                    member_models_max: NumericLimit {
                        value: panel_max_members(),
                        enforcement: "gateway_runtime",
                        reason: CapabilityReason {
                            code: "fusion_member_cap",
                            message:
                                "The gateway rejects Fusion configurations with more member models than this cap.",
                        },
                    },
                },
            },
        },
        provider_credentials: unknown_fact(
            "provider_credentials_not_inspected",
            "This endpoint does not decrypt, count, or probe provider credentials. A configured record is not treated as provider readiness.",
        ),
        provider_health: unknown_fact(
            "provider_health_not_probed",
            "This endpoint does not make provider health probes or spend-producing test requests.",
        ),
        model_support: unknown_fact(
            "model_support_not_negotiated",
            "A registered catalog model or provider inference path does not prove this request's credentials, modality, budget, or upstream acceptance. Use it only as metadata, not readiness evidence.",
        ),
        modality_support: unknown_fact(
            "modality_support_not_negotiated",
            "Modality support is request-specific and is not negotiated by this endpoint.",
        ),
        schema_versions: SchemaVersions {
            capabilities_document: SchemaVersionEvidence {
                state: "known",
                version: Some(CAPABILITIES_SCHEMA_VERSION),
                source: "gateway_runtime",
                reason: CapabilityReason {
                    code: "capabilities_document_version",
                    message: "This response follows the gateway runtime capabilities document schema.",
                },
            },
            fusion_request: SchemaVersionEvidence {
                state: "unversioned",
                version: None,
                source: "gateway_runtime",
                reason: CapabilityReason {
                    code: "fusion_request_schema_not_versioned",
                    message:
                        "Fusion currently uses the OpenAI-compatible request envelope and tt_extras without an independently negotiated request schema version.",
                },
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
        state: "unknown",
        source: "not_negotiated",
        reason: CapabilityReason { code, message },
    }
}

#[derive(Debug, Serialize)]
pub struct GatewayCapabilitiesDocument {
    pub schema_version: u32,
    pub scope: &'static str,
    pub snapshot_scope: &'static str,
    pub generated_at: String,
    pub features: GatewayFeatures,
    pub provider_credentials: UnknownEvidence,
    pub provider_health: UnknownEvidence,
    pub model_support: UnknownEvidence,
    pub modality_support: UnknownEvidence,
    pub schema_versions: SchemaVersions,
}

#[derive(Debug, Serialize)]
pub struct GatewayFeatures {
    pub fusion: FusionCapability,
}

#[derive(Debug, Serialize)]
pub struct FusionCapability {
    pub enabled: EnabledEvidence,
    /// Result of this responder's Fusion kill-switch + tier gate only.
    pub access: AccessEvidence,
    pub current_tier: TierEvidence,
    pub minimum_tier: TierEvidence,
    pub limits: FusionLimits,
}

#[derive(Debug, Serialize)]
pub struct EnabledEvidence {
    pub state: &'static str,
    pub source: &'static str,
    pub reason: CapabilityReason,
}

#[derive(Debug, Serialize)]
pub struct AccessEvidence {
    pub state: &'static str,
    pub reason: CapabilityReason,
}

#[derive(Debug, Serialize)]
pub struct TierEvidence {
    pub state: &'static str,
    pub value: &'static str,
    pub source: &'static str,
    pub reason: CapabilityReason,
}

#[derive(Debug, Serialize)]
pub struct FusionLimits {
    pub member_models_max: NumericLimit,
}

#[derive(Debug, Serialize)]
pub struct NumericLimit {
    pub value: usize,
    pub enforcement: &'static str,
    pub reason: CapabilityReason,
}

#[derive(Debug, Serialize)]
pub struct UnknownEvidence {
    pub state: &'static str,
    pub source: &'static str,
    pub reason: CapabilityReason,
}

#[derive(Debug, Serialize)]
pub struct SchemaVersions {
    pub capabilities_document: SchemaVersionEvidence,
    pub fusion_request: SchemaVersionEvidence,
}

#[derive(Debug, Serialize)]
pub struct SchemaVersionEvidence {
    pub state: &'static str,
    pub version: Option<u32>,
    pub source: &'static str,
    pub reason: CapabilityReason,
}

#[derive(Debug, Serialize)]
pub struct CapabilityReason {
    pub code: &'static str,
    pub message: &'static str,
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
