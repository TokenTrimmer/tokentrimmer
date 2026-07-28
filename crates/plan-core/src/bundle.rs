//! Versioned wire types for portable, deterministic savings replay bundles.
//!
//! The CLI owns file I/O and verification presentation. Keeping the envelope
//! beside [`crate::PlanInput`] and [`crate::PlanResult`] gives schema and
//! TypeScript generation one lean authority without pulling in the CLI binary.

use serde::{Deserialize, Serialize};
use tt_telemetry::audit::AuditEntry;

use crate::types::{PlanInput, PlanResult};

/// Current bundle schema version. Bumped only on a breaking shape change.
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// A self-contained, offline-reproducible savings bundle.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavingsBundle {
    /// Schema version — see [`BUNDLE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The `tt` version that produced the bundle (informational only).
    pub tool_version: String,
    /// RFC 3339 production timestamp (informational only).
    pub created_at: String,
    /// Complete deterministic replay input, including pricing and RNG seed.
    pub plan_input: PlanInput,
    /// Expected replay output, compared with a fresh replay by the verifier.
    pub expected_result: PlanResult,
    /// Optional signed audit-chain reference checked by the bundle verifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<BundleAttestation>,
}

/// Signed audit-chain reference carried inside a savings bundle.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleAttestation {
    /// Hex-encoded Ed25519 verifying key for [`Self::entries`].
    pub verifying_key: String,
    /// Signed, hash-chained audit entries.
    pub entries: Vec<AuditEntry>,
}
