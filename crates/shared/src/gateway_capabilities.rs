//! Exact wire types for authenticated `GET /v1/capabilities` evidence.
//!
//! These types describe one responding gateway process. They intentionally do
//! not represent fleet convergence, provider readiness, request acceptance, or
//! a reservation for a later request.

use serde::{Deserialize, Serialize};

/// Wire version for `GET /v1/capabilities`.
///
/// New optional fields are additive. A breaking change must use a new version
/// instead of asking clients to infer changed meaning from a familiar field.
pub const CAPABILITIES_SCHEMA_VERSION: u32 = 1;
pub const CAPABILITIES_SCOPE: &str = "gateway_runtime";
pub const CAPABILITIES_SNAPSHOT_SCOPE: &str = "responding_process";

/// One authenticated caller's capability evidence from one gateway process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct GatewayCapabilitiesDocument {
    pub schema_version: u32,
    pub scope: String,
    pub snapshot_scope: String,
    pub generated_at: String,
    pub features: GatewayFeatures,
    pub provider_credentials: UnknownEvidence,
    pub provider_health: UnknownEvidence,
    pub model_support: UnknownEvidence,
    pub modality_support: UnknownEvidence,
    pub schema_versions: SchemaVersions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct GatewayFeatures {
    pub fusion: FusionCapability,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct FusionCapability {
    pub enabled: EnabledEvidence,
    /// Result of this responder's Fusion kill-switch + tier gate only.
    pub access: AccessEvidence,
    pub current_tier: TierEvidence,
    pub minimum_tier: TierEvidence,
    pub limits: FusionLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct EnabledEvidence {
    pub state: String,
    pub source: String,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct AccessEvidence {
    pub state: String,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct TierEvidence {
    pub state: String,
    pub value: String,
    pub source: String,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct FusionLimits {
    pub member_models_max: NumericLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct NumericLimit {
    pub value: usize,
    pub enforcement: String,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct UnknownEvidence {
    pub state: String,
    pub source: String,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct SchemaVersions {
    pub capabilities_document: SchemaVersionEvidence,
    pub fusion_request: SchemaVersionEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct SchemaVersionEvidence {
    pub state: String,
    pub version: Option<u32>,
    pub source: String,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct CapabilityReason {
    pub code: String,
    pub message: String,
}
