//! Exact wire types for authenticated request-specific capability preflight.
//!
//! This contract reports only facts the responding gateway can establish
//! without provider I/O: dispatch-provider resolution, catalog metadata,
//! organization credential-record presence, comparisons against
//! caller-declared token values, and a standard-rate catalog cost projection.
//! Provider health and request acceptance remain explicitly unknown until a
//! real request is attempted.

use serde::{Deserialize, Serialize};

use crate::{Capability, CapabilityReason, UnknownEvidence};

pub const REQUEST_PREFLIGHT_SCHEMA_VERSION: u32 = 1;
pub const REQUEST_PREFLIGHT_SCOPE: &str = "request_preflight";
pub const REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION: u32 = 1;
pub const REQUEST_PREFLIGHT_BATCH_SCOPE: &str = "request_preflight_batch";
pub const REQUEST_PREFLIGHT_BATCH_MAX_REQUESTS: usize = 9;
/// Largest caller-declared token value accepted by the v1 cross-language wire.
pub const REQUEST_PREFLIGHT_TOKEN_VALUE_MAX: u64 = u32::MAX as u64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct RequestPreflightRequest {
    pub schema_version: u32,
    pub model: String,
    pub provider: Option<String>,
    pub required_capabilities: Vec<Capability>,
    /// Caller-declared value. The preflight does not tokenize a prompt.
    pub declared_input_tokens: Option<u64>,
    pub requested_max_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct RequestPreflightResponse {
    pub schema_version: u32,
    pub scope: String,
    pub snapshot_scope: String,
    pub generated_at: String,
    pub request: RequestPreflightRequest,
    pub provider_resolution: PreflightProviderResolution,
    pub credential: PreflightCredentialEvidence,
    pub model_support: PreflightModelSupportEvidence,
    pub catalog_limits: PreflightLimitEvidence,
    pub catalog_cost: PreflightCostEvidence,
    pub provider_health: UnknownEvidence,
    pub request_acceptance: UnknownEvidence,
    pub actions: Vec<PreflightAction>,
}

/// One responder-local evaluation of several request declarations.
///
/// This removes cross-process drift between roles, but it is not a database
/// transaction or provider-observed admission. Each nested document retains
/// the exact single-request limitations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct RequestPreflightBatchRequest {
    pub schema_version: u32,
    pub requests: Vec<RequestPreflightRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct RequestPreflightBatchResponse {
    pub schema_version: u32,
    pub scope: String,
    pub snapshot_scope: String,
    pub generated_at: String,
    pub request: RequestPreflightBatchRequest,
    pub documents: Vec<RequestPreflightResponse>,
    pub limitations: Vec<CapabilityReason>,
}

/// Cost interval under one responder's standard fresh-input/output catalog
/// rates. This is a projection, not provider-observed pricing, a quote, a
/// reservation, settlement, or an enforced spending limit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct PreflightCostEvidence {
    pub state: String,
    pub source: String,
    pub standard_input_rate_usd_per_million: Option<f64>,
    pub standard_output_rate_usd_per_million: Option<f64>,
    /// Low/high token assumptions used for the interval. When input is not
    /// declared, the range spans zero through the responder's catalog limit.
    pub input_tokens_low: Option<u64>,
    pub input_tokens_high: Option<u64>,
    /// The low output assumption is zero; the high value is the caller's
    /// requested maximum or, when omitted, the responder's catalog limit.
    pub output_tokens_low: Option<u64>,
    pub output_tokens_high: Option<u64>,
    pub standard_cost_usd_low: Option<f64>,
    pub standard_cost_usd_high: Option<f64>,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PreflightProviderResolution {
    pub state: String,
    pub provider: Option<String>,
    pub source: String,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PreflightCredentialEvidence {
    pub state: String,
    pub source: String,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PreflightModelSupportEvidence {
    pub state: String,
    pub source: String,
    pub missing_capabilities: Vec<Capability>,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PreflightLimitEvidence {
    pub state: String,
    pub source: String,
    pub catalog_max_input_tokens: Option<u64>,
    pub catalog_max_output_tokens: Option<u64>,
    pub reason: CapabilityReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PreflightAction {
    pub code: String,
    pub required_before_request: bool,
    pub reason: CapabilityReason,
}
