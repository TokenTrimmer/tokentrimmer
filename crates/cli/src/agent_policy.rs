//! Fail-closed authorization contract for the local coding-agent supervisor.
//!
//! The repository policy is strict TOML. An optional organization policy is an
//! Ed25519-signed, repository-scoped JSON payload. Resolution is monotonic:
//! organization > repository > task > model request. Lower layers may only
//! remove authority or add exclusions/checks.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::io::Read;
use std::path::{Component, Path};

use base64::Engine as _;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const AGENT_POLICY_SCHEMA_VERSION: u32 = 1;
pub const ORGANIZATION_POLICY_ENVELOPE_VERSION: u32 = 1;
pub const MAX_POLICY_BYTES: usize = 1_048_576;
const ORGANIZATION_POLICY_DOMAIN: &[u8] = b"tokentrimmer-agent-org-policy:v1\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPolicy {
    pub schema_version: u32,
    pub filesystem: FilesystemPolicy,
    pub process: ProcessPolicy,
    pub network: NetworkPolicy,
    pub inference: InferencePolicy,
    pub limits: RunLimits,
    pub budgets: CostBudgets,
    pub approvals: ApprovalPolicy,
    pub validation: ValidationPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemPolicy {
    pub readable_roots: Vec<String>,
    pub writable_roots: Vec<String>,
    pub max_files: u64,
    pub max_file_bytes: u64,
    pub max_total_read_bytes: u64,
    pub max_total_write_bytes: u64,
    pub allow_symlinks: bool,
    /// Additive deny patterns. Lower layers cannot remove earlier exclusions.
    pub excluded_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessPolicy {
    pub allowed_commands: Vec<CommandRule>,
    /// Maximum broker-started external command processes in one run.
    /// Descendant cardinality is not separately expressible in schema v1.
    pub max_subprocesses: u32,
    pub max_duration_seconds: u64,
    pub max_output_bytes: u64,
    pub allow_shell: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRule {
    /// An absolute path or a separator-free executable name. The broker must
    /// resolve a name once and retain the resolved path/digest as evidence.
    pub executable: String,
    /// Allowed argument prefixes. `[[]]` permits any argv; `[]` is invalid.
    pub argv_prefixes: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    pub default: NetworkDefault,
    pub allowed_destinations: Vec<NetworkDestination>,
    pub allow_redirects: bool,
    pub inherit_proxy_env: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkDefault {
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDestination {
    pub scheme: NetworkScheme,
    /// Exact lowercase DNS name or IP literal. Wildcards are prohibited.
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkScheme {
    Http,
    Https,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferencePolicy {
    pub allowed_runners: Vec<AgentRunner>,
    pub allowed_providers: Vec<String>,
    /// Exact provider-qualified model identifiers; no wildcards.
    pub allowed_models: Vec<String>,
    pub allowed_cost_bases: Vec<PolicyCostBasis>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunner {
    TokenTrimmerApi,
    CodexSdk,
    ClaudeAgentSdk,
    CliSubprocess,
    SelfHosted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyCostBasis {
    ApiMetered,
    Subscription,
    SelfHosted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunLimits {
    pub max_api_calls: u32,
    pub max_model_turns: u32,
    pub max_retries: u32,
    pub max_wall_time_seconds: u64,
    pub max_diff_bytes: u64,
    pub max_changed_files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostBudgets {
    pub max_api_cash_micros: u64,
    pub max_subscription_marginal_cash_micros: u64,
    pub max_subscription_allocated_micros: u64,
    pub max_self_hosted_tco_micros: u64,
    pub subscription_quota_caps: Vec<SubscriptionQuotaCap>,
    pub allow_unmeasured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionQuotaCap {
    pub unit: SubscriptionQuotaUnit,
    pub max_units: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionQuotaUnit {
    Requests,
    Tokens,
    ToolCalls,
    VendorUnits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalPolicy {
    pub destructive_operations: ApprovalGate,
    pub rollback: ApprovalGate,
}

/// The only v1 destructive-operation states. Unattended authorization is not a
/// representable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalGate {
    Deny,
    OneUseApproval,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationPolicy {
    /// Additive exact commands. Lower layers cannot remove earlier checks.
    pub required_commands: Vec<ExactCommand>,
    pub stop_on_regression: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactCommand {
    pub executable: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedOrganizationPolicyEnvelope {
    pub envelope_version: u32,
    /// Base64 of the exact UTF-8 JSON payload bytes covered by the signature.
    pub payload_base64: String,
    pub signature_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPolicyPayload {
    pub schema_version: u32,
    pub issuer: String,
    pub key_id: String,
    pub organization_id: String,
    pub repository_id: String,
    pub revision: u64,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub policy: AgentPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedOrganizationPolicy<'a> {
    pub issuer: &'a str,
    pub key_id: &'a str,
    pub organization_id: &'a str,
    pub repository_id: &'a str,
    /// Highest revision already accepted from durable state outside the repo.
    pub minimum_revision: u64,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedOrganizationPolicy {
    pub payload: OrganizationPolicyPayload,
    pub payload_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRepositoryPolicy {
    pub policy: AgentPolicy,
    /// Hash of the exact TOML bytes, not a reserialization.
    pub source_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyLayerKind {
    Organization,
    Repository,
    Task,
    ModelRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyLayerEvidence {
    pub layer: PolicyLayerKind,
    pub source_sha256: String,
    pub issuer: Option<String>,
    pub revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedAgentPolicy {
    pub policy: AgentPolicy,
    pub effective_sha256: String,
    pub layers: Vec<PolicyLayerEvidence>,
}

#[derive(Debug, Clone, Copy)]
pub enum OrganizationPolicyMode<'a> {
    NotConfigured,
    Required(Option<&'a VerifiedOrganizationPolicy>),
}

#[derive(Debug, Error)]
pub enum AgentPolicyError {
    #[error("policy source exceeds {MAX_POLICY_BYTES} bytes")]
    TooLarge,
    #[error("policy source is not UTF-8")]
    InvalidUtf8,
    #[error("refusing symlinked policy file: {0}")]
    SymlinkedPolicyFile(String),
    #[error("policy path is not a regular file: {0}")]
    NonRegularPolicyFile(String),
    #[error("failed to read policy file {path}: {detail}")]
    ReadPolicyFile { path: String, detail: String },
    #[error("malformed repository agent policy: {0}")]
    MalformedRepository(String),
    #[error("malformed organization policy envelope: {0}")]
    MalformedEnvelope(String),
    #[error("organization policy envelope version {0} is unsupported")]
    UnsupportedEnvelopeVersion(u32),
    #[error("organization policy payload encoding is invalid: {0}")]
    InvalidPayloadEncoding(String),
    #[error("organization policy signature must be 64-byte hex")]
    InvalidSignatureEncoding,
    #[error("pinned organization verifying key must be 32-byte hex")]
    InvalidVerifyingKey,
    #[error("organization policy signature verification failed")]
    InvalidSignature,
    #[error("malformed organization policy payload: {0}")]
    MalformedPayload(String),
    #[error("agent policy schema version {0} is unsupported")]
    UnsupportedSchemaVersion(u32),
    #[error("invalid policy field {field}: {detail}")]
    InvalidField { field: &'static str, detail: String },
    #[error("organization policy issuer mismatch: expected {expected:?}, got {actual:?}")]
    IssuerMismatch { expected: String, actual: String },
    #[error("organization policy key id mismatch: expected {expected:?}, got {actual:?}")]
    KeyIdMismatch { expected: String, actual: String },
    #[error(
        "organization policy scope mismatch for {field}: expected {expected:?}, got {actual:?}"
    )]
    ScopeMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("organization policy was issued in the future")]
    NotYetValid,
    #[error("organization policy is expired")]
    Expired,
    #[error("organization policy revision rollback: minimum {minimum}, got {actual}")]
    RevisionRollback { minimum: u64, actual: u64 },
    #[error("required organization policy is missing")]
    MissingOrganizationPolicy,
    #[error("{layer:?} policy widens {field}: {detail}")]
    PolicyWidening {
        layer: PolicyLayerKind,
        field: &'static str,
        detail: String,
    },
    #[error("failed to serialize policy evidence: {0}")]
    EvidenceSerialization(String),
}

impl AgentPolicy {
    pub fn validate(&self) -> Result<(), AgentPolicyError> {
        if self.schema_version != AGENT_POLICY_SCHEMA_VERSION {
            return Err(AgentPolicyError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }

        validate_paths(
            "filesystem.readable_roots",
            &self.filesystem.readable_roots,
            false,
        )?;
        validate_paths(
            "filesystem.writable_roots",
            &self.filesystem.writable_roots,
            false,
        )?;
        validate_paths(
            "filesystem.excluded_paths",
            &self.filesystem.excluded_paths,
            true,
        )?;

        let mut command_rules = BTreeSet::new();
        for rule in &self.process.allowed_commands {
            validate_executable("process.allowed_commands.executable", &rule.executable)?;
            if rule.argv_prefixes.is_empty() {
                return invalid(
                    "process.allowed_commands.argv_prefixes",
                    "must contain at least one prefix; use [[]] to permit any argv",
                );
            }
            for prefix in &rule.argv_prefixes {
                validate_args("process.allowed_commands.argv_prefixes", prefix)?;
                if !command_rules.insert((rule.executable.as_str(), prefix)) {
                    return invalid(
                        "process.allowed_commands",
                        "contains a duplicate executable/argv prefix",
                    );
                }
            }
        }

        let mut destinations = BTreeSet::new();
        for destination in &self.network.allowed_destinations {
            validate_host(&destination.host)?;
            if destination.port == 0 {
                return invalid("network.allowed_destinations.port", "must be nonzero");
            }
            if !destinations.insert(destination) {
                return invalid("network.allowed_destinations", "contains a duplicate");
            }
        }

        validate_unique_labels(
            "inference.allowed_providers",
            &self.inference.allowed_providers,
        )?;
        validate_unique_labels("inference.allowed_models", &self.inference.allowed_models)?;
        validate_unique("inference.allowed_runners", &self.inference.allowed_runners)?;
        validate_unique(
            "inference.allowed_cost_bases",
            &self.inference.allowed_cost_bases,
        )?;

        let mut quota_units = BTreeSet::new();
        for cap in &self.budgets.subscription_quota_caps {
            if !quota_units.insert(cap.unit) {
                return invalid(
                    "budgets.subscription_quota_caps",
                    "contains a duplicate quota unit",
                );
            }
        }

        let mut required = BTreeSet::new();
        for command in &self.validation.required_commands {
            validate_executable(
                "validation.required_commands.executable",
                &command.executable,
            )?;
            validate_args("validation.required_commands.args", &command.args)?;
            if !required.insert(command) {
                return invalid("validation.required_commands", "contains a duplicate");
            }
            if !self
                .process
                .allowed_commands
                .iter()
                .any(|rule| exact_command_is_covered(rule, command))
            {
                return invalid(
                    "validation.required_commands",
                    format!(
                        "required command {:?} is not covered by process.allowed_commands",
                        command.executable
                    ),
                );
            }
        }
        Ok(())
    }
}

pub fn parse_repository_policy(text: &str) -> Result<ParsedRepositoryPolicy, AgentPolicyError> {
    if text.len() > MAX_POLICY_BYTES {
        return Err(AgentPolicyError::TooLarge);
    }
    let policy: AgentPolicy = toml::from_str(text)
        .map_err(|error| AgentPolicyError::MalformedRepository(error.to_string()))?;
    policy.validate()?;
    Ok(ParsedRepositoryPolicy {
        policy,
        source_sha256: sha256_hex(text.as_bytes()),
    })
}

pub fn load_repository_policy(
    repo_root: &Path,
) -> Result<ParsedRepositoryPolicy, AgentPolicyError> {
    let path = repo_root.join(".tokentrimmer").join("agent.toml");
    let bytes = read_bounded_regular_file(&path)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| AgentPolicyError::InvalidUtf8)?;
    parse_repository_policy(text)
}

pub fn verify_organization_policy(
    envelope_json: &str,
    pinned_verifying_key_hex: &str,
    expected: &ExpectedOrganizationPolicy<'_>,
) -> Result<VerifiedOrganizationPolicy, AgentPolicyError> {
    if envelope_json.len() > MAX_POLICY_BYTES {
        return Err(AgentPolicyError::TooLarge);
    }
    let envelope: SignedOrganizationPolicyEnvelope = serde_json::from_str(envelope_json)
        .map_err(|error| AgentPolicyError::MalformedEnvelope(error.to_string()))?;
    if envelope.envelope_version != ORGANIZATION_POLICY_ENVELOPE_VERSION {
        return Err(AgentPolicyError::UnsupportedEnvelopeVersion(
            envelope.envelope_version,
        ));
    }

    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(envelope.payload_base64.as_bytes())
        .map_err(|error| AgentPolicyError::InvalidPayloadEncoding(error.to_string()))?;
    if payload_bytes.len() > MAX_POLICY_BYTES {
        return Err(AgentPolicyError::TooLarge);
    }
    let signature_bytes: [u8; 64] = hex::decode(envelope.signature_hex.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(AgentPolicyError::InvalidSignatureEncoding)?;
    let verifying_key_bytes: [u8; 32] = hex::decode(pinned_verifying_key_hex.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(AgentPolicyError::InvalidVerifyingKey)?;
    let verifying_key = VerifyingKey::from_bytes(&verifying_key_bytes)
        .map_err(|_| AgentPolicyError::InvalidVerifyingKey)?;
    let signature = Signature::from_bytes(&signature_bytes);
    verifying_key
        .verify_strict(&organization_policy_message(&payload_bytes), &signature)
        .map_err(|_| AgentPolicyError::InvalidSignature)?;

    let payload: OrganizationPolicyPayload = serde_json::from_slice(&payload_bytes)
        .map_err(|error| AgentPolicyError::MalformedPayload(error.to_string()))?;
    validate_label_value("organization.issuer", &payload.issuer)?;
    validate_label_value("organization.key_id", &payload.key_id)?;
    validate_label_value("organization.organization_id", &payload.organization_id)?;
    validate_label_value("organization.repository_id", &payload.repository_id)?;
    if payload.schema_version != AGENT_POLICY_SCHEMA_VERSION {
        return Err(AgentPolicyError::UnsupportedSchemaVersion(
            payload.schema_version,
        ));
    }
    if payload.issuer != expected.issuer {
        return Err(AgentPolicyError::IssuerMismatch {
            expected: expected.issuer.to_owned(),
            actual: payload.issuer,
        });
    }
    if payload.key_id != expected.key_id {
        return Err(AgentPolicyError::KeyIdMismatch {
            expected: expected.key_id.to_owned(),
            actual: payload.key_id,
        });
    }
    ensure_scope(
        "organization_id",
        expected.organization_id,
        &payload.organization_id,
    )?;
    ensure_scope(
        "repository_id",
        expected.repository_id,
        &payload.repository_id,
    )?;
    if payload.issued_at >= payload.expires_at || payload.issued_at > expected.now {
        return Err(AgentPolicyError::NotYetValid);
    }
    if payload.expires_at <= expected.now {
        return Err(AgentPolicyError::Expired);
    }
    if payload.revision == 0 {
        return invalid("organization.revision", "must be at least 1");
    }
    if payload.revision < expected.minimum_revision {
        return Err(AgentPolicyError::RevisionRollback {
            minimum: expected.minimum_revision,
            actual: payload.revision,
        });
    }
    payload.policy.validate()?;

    Ok(VerifiedOrganizationPolicy {
        payload_sha256: sha256_hex(&payload_bytes),
        payload,
    })
}

pub fn load_and_verify_organization_policy(
    path: &Path,
    pinned_verifying_key_hex: &str,
    expected: &ExpectedOrganizationPolicy<'_>,
) -> Result<VerifiedOrganizationPolicy, AgentPolicyError> {
    let bytes = read_bounded_regular_file(path)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| AgentPolicyError::InvalidUtf8)?;
    verify_organization_policy(text, pinned_verifying_key_hex, expected)
}

pub fn resolve_agent_policy(
    organization: OrganizationPolicyMode<'_>,
    repository: &ParsedRepositoryPolicy,
    task: Option<&AgentPolicy>,
    model_request: Option<&AgentPolicy>,
) -> Result<ResolvedAgentPolicy, AgentPolicyError> {
    repository.policy.validate()?;
    let mut layers = Vec::with_capacity(4);

    let mut effective = match organization {
        OrganizationPolicyMode::NotConfigured => repository.policy.clone(),
        OrganizationPolicyMode::Required(None) => {
            return Err(AgentPolicyError::MissingOrganizationPolicy)
        }
        OrganizationPolicyMode::Required(Some(organization)) => {
            layers.push(PolicyLayerEvidence {
                layer: PolicyLayerKind::Organization,
                source_sha256: organization.payload_sha256.clone(),
                issuer: Some(organization.payload.issuer.clone()),
                revision: Some(organization.payload.revision),
            });
            intersect_policy(
                &organization.payload.policy,
                &repository.policy,
                PolicyLayerKind::Repository,
            )?
        }
    };
    layers.push(PolicyLayerEvidence {
        layer: PolicyLayerKind::Repository,
        source_sha256: repository.source_sha256.clone(),
        issuer: None,
        revision: None,
    });

    for (kind, policy) in [
        (PolicyLayerKind::Task, task),
        (PolicyLayerKind::ModelRequest, model_request),
    ] {
        if let Some(policy) = policy {
            effective = intersect_policy(&effective, policy, kind)?;
            layers.push(PolicyLayerEvidence {
                layer: kind,
                source_sha256: semantic_policy_hash(policy)?,
                issuer: None,
                revision: None,
            });
        }
    }

    let effective_sha256 = semantic_policy_hash(&effective)?;
    Ok(ResolvedAgentPolicy {
        policy: effective,
        effective_sha256,
        layers,
    })
}

fn intersect_policy(
    higher: &AgentPolicy,
    lower: &AgentPolicy,
    layer: PolicyLayerKind,
) -> Result<AgentPolicy, AgentPolicyError> {
    higher.validate()?;
    lower.validate()?;

    ensure_roots_narrower(
        layer,
        "filesystem.readable_roots",
        &higher.filesystem.readable_roots,
        &lower.filesystem.readable_roots,
    )?;
    ensure_roots_narrower(
        layer,
        "filesystem.writable_roots",
        &higher.filesystem.writable_roots,
        &lower.filesystem.writable_roots,
    )?;
    ensure_maxima(layer, higher, lower)?;
    ensure_permission(
        layer,
        "filesystem.allow_symlinks",
        higher.filesystem.allow_symlinks,
        lower.filesystem.allow_symlinks,
    )?;
    ensure_commands_narrower(
        layer,
        &higher.process.allowed_commands,
        &lower.process.allowed_commands,
    )?;
    for command in &higher.validation.required_commands {
        if !lower
            .process
            .allowed_commands
            .iter()
            .any(|rule| exact_command_is_covered(rule, command))
        {
            return widening(
                layer,
                "process.allowed_commands",
                format!(
                    "removes authority needed by higher-layer validation {:?} {:?}",
                    command.executable, command.args
                ),
            );
        }
    }
    ensure_permission(
        layer,
        "process.allow_shell",
        higher.process.allow_shell,
        lower.process.allow_shell,
    )?;
    ensure_subset(
        layer,
        "network.allowed_destinations",
        &higher.network.allowed_destinations,
        &lower.network.allowed_destinations,
    )?;
    ensure_permission(
        layer,
        "network.allow_redirects",
        higher.network.allow_redirects,
        lower.network.allow_redirects,
    )?;
    ensure_permission(
        layer,
        "network.inherit_proxy_env",
        higher.network.inherit_proxy_env,
        lower.network.inherit_proxy_env,
    )?;
    ensure_subset(
        layer,
        "inference.allowed_runners",
        &higher.inference.allowed_runners,
        &lower.inference.allowed_runners,
    )?;
    ensure_subset(
        layer,
        "inference.allowed_providers",
        &higher.inference.allowed_providers,
        &lower.inference.allowed_providers,
    )?;
    ensure_subset(
        layer,
        "inference.allowed_models",
        &higher.inference.allowed_models,
        &lower.inference.allowed_models,
    )?;
    ensure_subset(
        layer,
        "inference.allowed_cost_bases",
        &higher.inference.allowed_cost_bases,
        &lower.inference.allowed_cost_bases,
    )?;
    ensure_quota_caps_narrower(
        layer,
        &higher.budgets.subscription_quota_caps,
        &lower.budgets.subscription_quota_caps,
    )?;
    ensure_permission(
        layer,
        "budgets.allow_unmeasured",
        higher.budgets.allow_unmeasured,
        lower.budgets.allow_unmeasured,
    )?;
    ensure_gate_narrower(
        layer,
        "approvals.destructive_operations",
        higher.approvals.destructive_operations,
        lower.approvals.destructive_operations,
    )?;
    ensure_gate_narrower(
        layer,
        "approvals.rollback",
        higher.approvals.rollback,
        lower.approvals.rollback,
    )?;
    if higher.validation.stop_on_regression && !lower.validation.stop_on_regression {
        return widening(
            layer,
            "validation.stop_on_regression",
            "cannot disable a higher-layer regression stop",
        );
    }

    let mut effective = lower.clone();
    effective.filesystem.excluded_paths = ordered_union(
        &higher.filesystem.excluded_paths,
        &lower.filesystem.excluded_paths,
    );
    effective.validation.required_commands = ordered_union(
        &higher.validation.required_commands,
        &lower.validation.required_commands,
    );
    effective.validate()?;
    Ok(effective)
}

fn ensure_maxima(
    layer: PolicyLayerKind,
    higher: &AgentPolicy,
    lower: &AgentPolicy,
) -> Result<(), AgentPolicyError> {
    for (field, higher, lower) in [
        (
            "filesystem.max_files",
            higher.filesystem.max_files,
            lower.filesystem.max_files,
        ),
        (
            "filesystem.max_file_bytes",
            higher.filesystem.max_file_bytes,
            lower.filesystem.max_file_bytes,
        ),
        (
            "filesystem.max_total_read_bytes",
            higher.filesystem.max_total_read_bytes,
            lower.filesystem.max_total_read_bytes,
        ),
        (
            "filesystem.max_total_write_bytes",
            higher.filesystem.max_total_write_bytes,
            lower.filesystem.max_total_write_bytes,
        ),
        (
            "process.max_duration_seconds",
            higher.process.max_duration_seconds,
            lower.process.max_duration_seconds,
        ),
        (
            "process.max_output_bytes",
            higher.process.max_output_bytes,
            lower.process.max_output_bytes,
        ),
        (
            "limits.max_wall_time_seconds",
            higher.limits.max_wall_time_seconds,
            lower.limits.max_wall_time_seconds,
        ),
        (
            "limits.max_diff_bytes",
            higher.limits.max_diff_bytes,
            lower.limits.max_diff_bytes,
        ),
        (
            "budgets.max_api_cash_micros",
            higher.budgets.max_api_cash_micros,
            lower.budgets.max_api_cash_micros,
        ),
        (
            "budgets.max_subscription_marginal_cash_micros",
            higher.budgets.max_subscription_marginal_cash_micros,
            lower.budgets.max_subscription_marginal_cash_micros,
        ),
        (
            "budgets.max_subscription_allocated_micros",
            higher.budgets.max_subscription_allocated_micros,
            lower.budgets.max_subscription_allocated_micros,
        ),
        (
            "budgets.max_self_hosted_tco_micros",
            higher.budgets.max_self_hosted_tco_micros,
            lower.budgets.max_self_hosted_tco_micros,
        ),
    ] {
        ensure_maximum(layer, field, higher, lower)?;
    }
    for (field, higher, lower) in [
        (
            "process.max_subprocesses",
            higher.process.max_subprocesses,
            lower.process.max_subprocesses,
        ),
        (
            "limits.max_api_calls",
            higher.limits.max_api_calls,
            lower.limits.max_api_calls,
        ),
        (
            "limits.max_model_turns",
            higher.limits.max_model_turns,
            lower.limits.max_model_turns,
        ),
        (
            "limits.max_retries",
            higher.limits.max_retries,
            lower.limits.max_retries,
        ),
        (
            "limits.max_changed_files",
            higher.limits.max_changed_files,
            lower.limits.max_changed_files,
        ),
    ] {
        ensure_maximum(layer, field, u64::from(higher), u64::from(lower))?;
    }
    Ok(())
}

fn ensure_maximum(
    layer: PolicyLayerKind,
    field: &'static str,
    higher: u64,
    lower: u64,
) -> Result<(), AgentPolicyError> {
    if lower > higher {
        return widening(
            layer,
            field,
            format!("maximum {lower} exceeds higher-layer maximum {higher}"),
        );
    }
    Ok(())
}

fn ensure_permission(
    layer: PolicyLayerKind,
    field: &'static str,
    higher: bool,
    lower: bool,
) -> Result<(), AgentPolicyError> {
    if lower && !higher {
        return widening(layer, field, "cannot enable a higher-layer denial");
    }
    Ok(())
}

fn ensure_gate_narrower(
    layer: PolicyLayerKind,
    field: &'static str,
    higher: ApprovalGate,
    lower: ApprovalGate,
) -> Result<(), AgentPolicyError> {
    if lower > higher {
        return widening(layer, field, "cannot weaken a higher-layer approval gate");
    }
    Ok(())
}

fn ensure_subset<T>(
    layer: PolicyLayerKind,
    field: &'static str,
    higher: &[T],
    lower: &[T],
) -> Result<(), AgentPolicyError>
where
    T: Ord + Debug,
{
    let allowed: BTreeSet<_> = higher.iter().collect();
    if let Some(value) = lower.iter().find(|value| !allowed.contains(value)) {
        return widening(
            layer,
            field,
            format!("value {value:?} is not authorized by the higher layer"),
        );
    }
    Ok(())
}

fn ensure_roots_narrower(
    layer: PolicyLayerKind,
    field: &'static str,
    higher: &[String],
    lower: &[String],
) -> Result<(), AgentPolicyError> {
    if let Some(root) = lower.iter().find(|root| {
        !higher
            .iter()
            .any(|parent| relative_root_contains(parent, root))
    }) {
        return widening(
            layer,
            field,
            format!("root {root:?} is outside every higher-layer root"),
        );
    }
    Ok(())
}

fn ensure_commands_narrower(
    layer: PolicyLayerKind,
    higher: &[CommandRule],
    lower: &[CommandRule],
) -> Result<(), AgentPolicyError> {
    for rule in lower {
        for prefix in &rule.argv_prefixes {
            let covered = higher.iter().any(|candidate| {
                candidate.executable == rule.executable
                    && candidate
                        .argv_prefixes
                        .iter()
                        .any(|allowed| argv_prefix_contains(allowed, prefix))
            });
            if !covered {
                return widening(
                    layer,
                    "process.allowed_commands",
                    format!(
                        "command {:?} with argv prefix {prefix:?} is not authorized by the higher layer",
                        rule.executable
                    ),
                );
            }
        }
    }
    Ok(())
}

fn ensure_quota_caps_narrower(
    layer: PolicyLayerKind,
    higher: &[SubscriptionQuotaCap],
    lower: &[SubscriptionQuotaCap],
) -> Result<(), AgentPolicyError> {
    let higher_by_unit: BTreeMap<_, _> =
        higher.iter().map(|cap| (cap.unit, cap.max_units)).collect();
    for cap in lower {
        let Some(higher_max) = higher_by_unit.get(&cap.unit) else {
            return widening(
                layer,
                "budgets.subscription_quota_caps",
                format!("quota unit {:?} is not authorized", cap.unit),
            );
        };
        if cap.max_units > *higher_max {
            return widening(
                layer,
                "budgets.subscription_quota_caps",
                format!(
                    "quota {:?} maximum {} exceeds higher-layer maximum {}",
                    cap.unit, cap.max_units, higher_max
                ),
            );
        }
    }
    Ok(())
}

fn ordered_union<T>(higher: &[T], lower: &[T]) -> Vec<T>
where
    T: Clone + Ord,
{
    let mut seen = BTreeSet::new();
    higher
        .iter()
        .chain(lower)
        .filter(|value| seen.insert((*value).clone()))
        .cloned()
        .collect()
}

fn validate_paths(
    field: &'static str,
    paths: &[String],
    allow_globs: bool,
) -> Result<(), AgentPolicyError> {
    let mut seen = BTreeSet::new();
    for path in paths {
        if path.is_empty() || path.len() > 512 || path.contains('\0') || path.contains('\\') {
            return invalid(field, format!("invalid repository-relative path {path:?}"));
        }
        if allow_globs {
            if path.starts_with('!')
                || path.contains(':')
                || (path != "."
                    && path
                        .split('/')
                        .any(|component| component.is_empty() || matches!(component, "." | "..")))
            {
                return invalid(field, format!("unsafe additive deny pattern {path:?}"));
            }
        } else if !is_canonical_relative_root(path) {
            return invalid(
                field,
                format!("root {path:?} is not canonical and repository-relative"),
            );
        }
        if !seen.insert(path) {
            return invalid(field, format!("duplicate path {path:?}"));
        }
    }
    Ok(())
}

fn is_canonical_relative_root(value: &str) -> bool {
    value == "."
        || (!value.contains(':')
            && value
                .split('/')
                .all(|component| !component.is_empty() && !matches!(component, "." | "..")))
}

fn relative_root_contains(parent: &str, child: &str) -> bool {
    parent == "."
        || parent == child
        || child
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn validate_executable(field: &'static str, executable: &str) -> Result<(), AgentPolicyError> {
    if executable.is_empty()
        || executable.len() > 512
        || executable.contains(['\0', '\n', '\r'])
        || executable.ends_with('/')
    {
        return invalid(field, "must be a nonempty executable path or name");
    }
    let path = Path::new(executable);
    if !path.is_absolute() && executable.contains(['/', '\\']) {
        return invalid(
            field,
            "relative executable names cannot contain path separators",
        );
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return invalid(field, "executable path cannot contain . or .. components");
    }
    Ok(())
}

fn validate_args(field: &'static str, args: &[String]) -> Result<(), AgentPolicyError> {
    if args.len() > 128 {
        return invalid(field, "contains more than 128 arguments");
    }
    if args
        .iter()
        .any(|arg| arg.len() > 4096 || arg.contains(['\0', '\n', '\r']))
    {
        return invalid(
            field,
            "argument contains a NUL/newline or exceeds 4096 bytes",
        );
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<(), AgentPolicyError> {
    if host.is_empty()
        || host.len() > 253
        || host != host.to_ascii_lowercase()
        || host.contains('*')
        || host.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | ':' | '[' | ']'))
        })
    {
        return invalid(
            "network.allowed_destinations.host",
            "must be an exact lowercase DNS name or IP literal without wildcards",
        );
    }
    Ok(())
}

fn validate_unique_labels(field: &'static str, values: &[String]) -> Result<(), AgentPolicyError> {
    for value in values {
        validate_label_value(field, value)?;
    }
    validate_unique(field, values)
}

fn validate_label_value(field: &'static str, value: &str) -> Result<(), AgentPolicyError> {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(|character| {
            !(character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '-' | ':' | '/' | '@'))
        })
    {
        return invalid(field, format!("invalid exact identifier {value:?}"));
    }
    Ok(())
}

fn validate_unique<T>(field: &'static str, values: &[T]) -> Result<(), AgentPolicyError>
where
    T: Ord + Debug,
{
    let mut seen = BTreeSet::new();
    if let Some(value) = values.iter().find(|value| !seen.insert(*value)) {
        return invalid(field, format!("duplicate value {value:?}"));
    }
    Ok(())
}

fn exact_command_is_covered(rule: &CommandRule, command: &ExactCommand) -> bool {
    rule.executable == command.executable
        && rule
            .argv_prefixes
            .iter()
            .any(|prefix| argv_prefix_contains(prefix, &command.args))
}

fn argv_prefix_contains(parent: &[String], child: &[String]) -> bool {
    child.starts_with(parent)
}

fn ensure_scope(field: &'static str, expected: &str, actual: &str) -> Result<(), AgentPolicyError> {
    if actual != expected {
        return Err(AgentPolicyError::ScopeMismatch {
            field,
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        });
    }
    Ok(())
}

fn organization_policy_message(payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(ORGANIZATION_POLICY_DOMAIN.len() + payload.len());
    message.extend_from_slice(ORGANIZATION_POLICY_DOMAIN);
    message.extend_from_slice(payload);
    message
}

fn semantic_policy_hash(policy: &AgentPolicy) -> Result<String, AgentPolicyError> {
    serde_json::to_vec(policy)
        .map(|bytes| sha256_hex(&bytes))
        .map_err(|error| AgentPolicyError::EvidenceSerialization(error.to_string()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn read_bounded_regular_file(path: &Path) -> Result<Vec<u8>, AgentPolicyError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| AgentPolicyError::ReadPolicyFile {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
    if metadata.file_type().is_symlink() {
        return Err(AgentPolicyError::SymlinkedPolicyFile(
            path.display().to_string(),
        ));
    }
    if !metadata.is_file() {
        return Err(AgentPolicyError::NonRegularPolicyFile(
            path.display().to_string(),
        ));
    }
    if metadata.len() > MAX_POLICY_BYTES as u64 {
        return Err(AgentPolicyError::TooLarge);
    }
    let file = std::fs::File::open(path).map_err(|error| AgentPolicyError::ReadPolicyFile {
        path: path.display().to_string(),
        detail: error.to_string(),
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_POLICY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| AgentPolicyError::ReadPolicyFile {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
    if bytes.len() > MAX_POLICY_BYTES {
        return Err(AgentPolicyError::TooLarge);
    }
    Ok(bytes)
}

fn invalid<T>(field: &'static str, detail: impl Into<String>) -> Result<T, AgentPolicyError> {
    Err(AgentPolicyError::InvalidField {
        field,
        detail: detail.into(),
    })
}

fn widening<T>(
    layer: PolicyLayerKind,
    field: &'static str,
    detail: impl Into<String>,
) -> Result<T, AgentPolicyError> {
    Err(AgentPolicyError::PolicyWidening {
        layer,
        field,
        detail: detail.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use ed25519_dalek::{Signer as _, SigningKey};

    fn policy() -> AgentPolicy {
        AgentPolicy {
            schema_version: AGENT_POLICY_SCHEMA_VERSION,
            filesystem: FilesystemPolicy {
                readable_roots: vec![".".into()],
                writable_roots: vec!["src".into(), "tests".into()],
                max_files: 1_000,
                max_file_bytes: 1_000_000,
                max_total_read_bytes: 10_000_000,
                max_total_write_bytes: 2_000_000,
                allow_symlinks: false,
                excluded_paths: vec![".env*".into(), ".git/**".into()],
            },
            process: ProcessPolicy {
                allowed_commands: vec![
                    CommandRule {
                        executable: "cargo".into(),
                        argv_prefixes: vec![vec!["check".into()], vec!["test".into()]],
                    },
                    CommandRule {
                        executable: "git".into(),
                        argv_prefixes: vec![vec!["diff".into()], vec!["status".into()]],
                    },
                ],
                max_subprocesses: 8,
                max_duration_seconds: 300,
                max_output_bytes: 1_000_000,
                allow_shell: false,
            },
            network: NetworkPolicy {
                default: NetworkDefault::Deny,
                allowed_destinations: vec![NetworkDestination {
                    scheme: NetworkScheme::Https,
                    host: "api.tokentrimmer.com".into(),
                    port: 443,
                }],
                allow_redirects: false,
                inherit_proxy_env: false,
            },
            inference: InferencePolicy {
                allowed_runners: vec![AgentRunner::TokenTrimmerApi, AgentRunner::CodexSdk],
                allowed_providers: vec!["openai".into(), "anthropic".into()],
                allowed_models: vec!["openai/gpt-5".into(), "anthropic/claude-sonnet-4".into()],
                allowed_cost_bases: vec![
                    PolicyCostBasis::ApiMetered,
                    PolicyCostBasis::Subscription,
                ],
            },
            limits: RunLimits {
                max_api_calls: 30,
                max_model_turns: 20,
                max_retries: 2,
                max_wall_time_seconds: 1_800,
                max_diff_bytes: 200_000,
                max_changed_files: 30,
            },
            budgets: CostBudgets {
                max_api_cash_micros: 2_000_000,
                max_subscription_marginal_cash_micros: 100_000,
                max_subscription_allocated_micros: 500_000,
                max_self_hosted_tco_micros: 0,
                subscription_quota_caps: vec![SubscriptionQuotaCap {
                    unit: SubscriptionQuotaUnit::Requests,
                    max_units: 50,
                }],
                allow_unmeasured: false,
            },
            approvals: ApprovalPolicy {
                destructive_operations: ApprovalGate::OneUseApproval,
                rollback: ApprovalGate::OneUseApproval,
            },
            validation: ValidationPolicy {
                required_commands: vec![ExactCommand {
                    executable: "cargo".into(),
                    args: vec!["check".into(), "--locked".into()],
                }],
                stop_on_regression: true,
            },
        }
    }

    fn parsed(policy: AgentPolicy) -> ParsedRepositoryPolicy {
        ParsedRepositoryPolicy {
            source_sha256: semantic_policy_hash(&policy).unwrap(),
            policy,
        }
    }

    fn signed_envelope(
        policy: AgentPolicy,
        key: &SigningKey,
        now: DateTime<Utc>,
    ) -> (String, String, ExpectedOrganizationPolicy<'static>) {
        let payload = OrganizationPolicyPayload {
            schema_version: AGENT_POLICY_SCHEMA_VERSION,
            issuer: "acme-security".into(),
            key_id: "agent-policy-2026-08".into(),
            organization_id: "org_acme".into(),
            repository_id: "repo_tokentrimmer".into(),
            revision: 7,
            issued_at: now - TimeDelta::minutes(5),
            expires_at: now + TimeDelta::hours(1),
            policy,
        };
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let envelope = SignedOrganizationPolicyEnvelope {
            envelope_version: ORGANIZATION_POLICY_ENVELOPE_VERSION,
            payload_base64: base64::engine::general_purpose::STANDARD.encode(&payload_bytes),
            signature_hex: hex::encode(
                key.sign(&organization_policy_message(&payload_bytes))
                    .to_bytes(),
            ),
        };
        let expected = ExpectedOrganizationPolicy {
            issuer: "acme-security",
            key_id: "agent-policy-2026-08",
            organization_id: "org_acme",
            repository_id: "repo_tokentrimmer",
            minimum_revision: 7,
            now,
        };
        (
            serde_json::to_string(&envelope).unwrap(),
            hex::encode(key.verifying_key().to_bytes()),
            expected,
        )
    }

    #[test]
    fn generated_repository_template_is_strict_and_fail_closed() {
        let text = include_str!("../templates/init/.tokentrimmer/agent.toml");
        assert_eq!(
            text,
            include_str!("../../../.tokentrimmer/agent.toml"),
            "dogfood policy and tt init template drifted"
        );
        let parsed = parse_repository_policy(text).expect("template policy");
        assert!(parsed.policy.process.allowed_commands.is_empty());
        assert!(parsed.policy.network.allowed_destinations.is_empty());
        assert!(parsed.policy.inference.allowed_runners.is_empty());
        assert_eq!(parsed.policy.limits.max_api_calls, 0);
        assert!(!parsed.policy.budgets.allow_unmeasured);
        assert_eq!(parsed.policy.approvals.rollback, ApprovalGate::Deny);
    }

    #[test]
    fn unknown_repository_field_fails_closed() {
        let text = include_str!("../templates/init/.tokentrimmer/agent.toml").replace(
            "schema_version = 1",
            "schema_version = 1\norganization_policy = 'repo-controlled'",
        );
        assert!(matches!(
            parse_repository_policy(&text),
            Err(AgentPolicyError::MalformedRepository(_))
        ));
    }

    #[test]
    fn unknown_organization_envelope_and_payload_fields_fail_closed() {
        let now = "2026-08-15T12:00:00Z".parse().unwrap();
        let key = SigningKey::from_bytes(&[6; 32]);
        let (envelope, key_hex, expected) = signed_envelope(policy(), &key, now);

        let mut envelope_value: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        envelope_value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), true.into());
        assert!(matches!(
            verify_organization_policy(
                &serde_json::to_string(&envelope_value).unwrap(),
                &key_hex,
                &expected
            ),
            Err(AgentPolicyError::MalformedEnvelope(_))
        ));

        let mut signed: SignedOrganizationPolicyEnvelope = serde_json::from_str(&envelope).unwrap();
        let payload_bytes = base64::engine::general_purpose::STANDARD
            .decode(&signed.payload_base64)
            .unwrap();
        let mut payload_value: serde_json::Value = serde_json::from_slice(&payload_bytes).unwrap();
        payload_value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), true.into());
        let payload_bytes = serde_json::to_vec(&payload_value).unwrap();
        signed.payload_base64 = base64::engine::general_purpose::STANDARD.encode(&payload_bytes);
        signed.signature_hex = hex::encode(
            key.sign(&organization_policy_message(&payload_bytes))
                .to_bytes(),
        );
        assert!(matches!(
            verify_organization_policy(
                &serde_json::to_string(&signed).unwrap(),
                &key_hex,
                &expected
            ),
            Err(AgentPolicyError::MalformedPayload(_))
        ));
    }

    #[test]
    fn verifies_exact_signed_bytes_and_scope() {
        let now = "2026-08-15T12:00:00Z".parse().unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        let (envelope, key_hex, expected) = signed_envelope(policy(), &key, now);
        let verified = verify_organization_policy(&envelope, &key_hex, &expected).unwrap();
        assert_eq!(verified.payload.revision, 7);
        assert_eq!(verified.payload.policy, policy());

        let mut tampered: SignedOrganizationPolicyEnvelope =
            serde_json::from_str(&envelope).unwrap();
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(&tampered.payload_base64)
            .unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        tampered.payload_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert!(matches!(
            verify_organization_policy(
                &serde_json::to_string(&tampered).unwrap(),
                &key_hex,
                &expected
            ),
            Err(AgentPolicyError::InvalidSignature)
        ));
    }

    #[test]
    fn organization_metadata_failures_are_typed() {
        let now = "2026-08-15T12:00:00Z".parse().unwrap();
        let key = SigningKey::from_bytes(&[9; 32]);
        let (envelope, key_hex, mut expected) = signed_envelope(policy(), &key, now);

        expected.issuer = "other";
        assert!(matches!(
            verify_organization_policy(&envelope, &key_hex, &expected),
            Err(AgentPolicyError::IssuerMismatch { .. })
        ));
        expected.issuer = "acme-security";
        expected.repository_id = "other-repo";
        assert!(matches!(
            verify_organization_policy(&envelope, &key_hex, &expected),
            Err(AgentPolicyError::ScopeMismatch {
                field: "repository_id",
                ..
            })
        ));
        expected.repository_id = "repo_tokentrimmer";
        expected.minimum_revision = 8;
        assert!(matches!(
            verify_organization_policy(&envelope, &key_hex, &expected),
            Err(AgentPolicyError::RevisionRollback {
                minimum: 8,
                actual: 7
            })
        ));
        expected.minimum_revision = 7;
        expected.now = now + TimeDelta::hours(2);
        assert!(matches!(
            verify_organization_policy(&envelope, &key_hex, &expected),
            Err(AgentPolicyError::Expired)
        ));
    }

    #[test]
    fn every_lower_layer_can_narrow_but_not_widen() {
        let organization_policy = policy();
        let now = "2026-08-15T12:00:00Z".parse().unwrap();
        let key = SigningKey::from_bytes(&[11; 32]);
        let (envelope, key_hex, expected) = signed_envelope(organization_policy.clone(), &key, now);
        let verified = verify_organization_policy(&envelope, &key_hex, &expected).unwrap();

        let mut repository_policy = organization_policy.clone();
        repository_policy.filesystem.readable_roots = vec!["src".into(), "tests".into()];
        repository_policy.process.allowed_commands[0].argv_prefixes = vec![vec!["check".into()]];
        repository_policy.inference.allowed_providers = vec!["openai".into()];
        repository_policy.limits.max_model_turns = 15;
        repository_policy
            .filesystem
            .excluded_paths
            .push("target/**".into());

        let mut task_policy = repository_policy.clone();
        task_policy.filesystem.readable_roots = vec!["src".into()];
        task_policy.limits.max_model_turns = 10;
        task_policy.approvals.destructive_operations = ApprovalGate::Deny;
        task_policy.validation.required_commands.push(ExactCommand {
            executable: "cargo".into(),
            args: vec!["check".into()],
        });

        let mut model_policy = task_policy.clone();
        model_policy.inference.allowed_models = vec!["openai/gpt-5".into()];
        model_policy.limits.max_model_turns = 8;

        let resolved = resolve_agent_policy(
            OrganizationPolicyMode::Required(Some(&verified)),
            &parsed(repository_policy.clone()),
            Some(&task_policy),
            Some(&model_policy),
        )
        .unwrap();
        assert_eq!(resolved.layers.len(), 4);
        assert_eq!(resolved.policy.limits.max_model_turns, 8);
        assert_eq!(
            resolved.policy.filesystem.excluded_paths,
            vec![".env*", ".git/**", "target/**"]
        );
        assert_eq!(resolved.policy.validation.required_commands.len(), 2);

        for layer in [
            PolicyLayerKind::Repository,
            PolicyLayerKind::Task,
            PolicyLayerKind::ModelRequest,
        ] {
            let mut wider = task_policy.clone();
            wider.limits.max_model_turns = 21;
            let result = match layer {
                PolicyLayerKind::Repository => resolve_agent_policy(
                    OrganizationPolicyMode::Required(Some(&verified)),
                    &parsed(wider),
                    None,
                    None,
                ),
                PolicyLayerKind::Task => resolve_agent_policy(
                    OrganizationPolicyMode::Required(Some(&verified)),
                    &parsed(repository_policy.clone()),
                    Some(&wider),
                    None,
                ),
                PolicyLayerKind::ModelRequest => resolve_agent_policy(
                    OrganizationPolicyMode::Required(Some(&verified)),
                    &parsed(repository_policy.clone()),
                    Some(&task_policy),
                    Some(&wider),
                ),
                PolicyLayerKind::Organization => unreachable!(),
            };
            assert!(matches!(
                result,
                Err(AgentPolicyError::PolicyWidening {
                    layer: actual,
                    field: "limits.max_model_turns",
                    ..
                }) if actual == layer
            ));
        }
    }

    #[test]
    fn all_authority_categories_reject_widening() {
        let higher = policy();
        let cases: Vec<(&'static str, AgentPolicy)> = vec![
            ("filesystem.writable_roots", {
                let mut value = higher.clone();
                value.filesystem.writable_roots = vec!["docs".into()];
                value
            }),
            ("filesystem.allow_symlinks", {
                let mut value = higher.clone();
                value.filesystem.allow_symlinks = true;
                value
            }),
            ("process.allowed_commands", {
                let mut value = higher.clone();
                value.process.allowed_commands.push(CommandRule {
                    executable: "rm".into(),
                    argv_prefixes: vec![vec![]],
                });
                value
            }),
            ("process.allow_shell", {
                let mut value = higher.clone();
                value.process.allow_shell = true;
                value
            }),
            ("network.allowed_destinations", {
                let mut value = higher.clone();
                value.network.allowed_destinations.push(NetworkDestination {
                    scheme: NetworkScheme::Https,
                    host: "example.com".into(),
                    port: 443,
                });
                value
            }),
            ("network.allow_redirects", {
                let mut value = higher.clone();
                value.network.allow_redirects = true;
                value
            }),
            ("inference.allowed_runners", {
                let mut value = higher.clone();
                value
                    .inference
                    .allowed_runners
                    .push(AgentRunner::SelfHosted);
                value
            }),
            ("inference.allowed_models", {
                let mut value = higher.clone();
                value
                    .inference
                    .allowed_models
                    .push("openai/unapproved".into());
                value
            }),
            ("budgets.subscription_quota_caps", {
                let mut value = higher.clone();
                value.budgets.subscription_quota_caps[0].max_units += 1;
                value
            }),
            ("budgets.allow_unmeasured", {
                let mut value = higher.clone();
                value.budgets.allow_unmeasured = true;
                value
            }),
            ("validation.stop_on_regression", {
                let mut value = higher.clone();
                value.validation.stop_on_regression = false;
                value
            }),
        ];

        for (field, lower) in cases {
            let error = intersect_policy(&higher, &lower, PolicyLayerKind::Task).expect_err(field);
            assert!(
                matches!(error, AgentPolicyError::PolicyWidening { field: actual, .. } if actual == field),
                "expected widening for {field}, got {error:?}"
            );
        }
    }

    #[test]
    fn every_numeric_maximum_rejects_widening() {
        let higher = policy();
        let mut cases: Vec<(&'static str, AgentPolicy)> = Vec::new();
        macro_rules! wider {
            ($field:literal, $path:expr) => {{
                let mut value = higher.clone();
                $path(&mut value);
                cases.push(($field, value));
            }};
        }
        wider!("filesystem.max_files", |p: &mut AgentPolicy| p
            .filesystem
            .max_files +=
            1);
        wider!("filesystem.max_file_bytes", |p: &mut AgentPolicy| p
            .filesystem
            .max_file_bytes +=
            1);
        wider!("filesystem.max_total_read_bytes", |p: &mut AgentPolicy| {
            p.filesystem.max_total_read_bytes += 1
        });
        wider!("filesystem.max_total_write_bytes", |p: &mut AgentPolicy| {
            p.filesystem.max_total_write_bytes += 1
        });
        wider!("process.max_subprocesses", |p: &mut AgentPolicy| p
            .process
            .max_subprocesses +=
            1);
        wider!("process.max_duration_seconds", |p: &mut AgentPolicy| p
            .process
            .max_duration_seconds +=
            1);
        wider!("process.max_output_bytes", |p: &mut AgentPolicy| p
            .process
            .max_output_bytes +=
            1);
        wider!("limits.max_api_calls", |p: &mut AgentPolicy| p
            .limits
            .max_api_calls +=
            1);
        wider!("limits.max_model_turns", |p: &mut AgentPolicy| p
            .limits
            .max_model_turns +=
            1);
        wider!("limits.max_retries", |p: &mut AgentPolicy| p
            .limits
            .max_retries +=
            1);
        wider!("limits.max_wall_time_seconds", |p: &mut AgentPolicy| p
            .limits
            .max_wall_time_seconds +=
            1);
        wider!("limits.max_diff_bytes", |p: &mut AgentPolicy| p
            .limits
            .max_diff_bytes +=
            1);
        wider!("limits.max_changed_files", |p: &mut AgentPolicy| p
            .limits
            .max_changed_files +=
            1);
        wider!("budgets.max_api_cash_micros", |p: &mut AgentPolicy| p
            .budgets
            .max_api_cash_micros +=
            1);
        wider!(
            "budgets.max_subscription_marginal_cash_micros",
            |p: &mut AgentPolicy| p.budgets.max_subscription_marginal_cash_micros += 1
        );
        wider!(
            "budgets.max_subscription_allocated_micros",
            |p: &mut AgentPolicy| p.budgets.max_subscription_allocated_micros += 1
        );
        wider!(
            "budgets.max_self_hosted_tco_micros",
            |p: &mut AgentPolicy| p.budgets.max_self_hosted_tco_micros += 1
        );

        for (field, lower) in cases {
            let error = intersect_policy(&higher, &lower, PolicyLayerKind::Task).expect_err(field);
            assert!(
                matches!(error, AgentPolicyError::PolicyWidening { field: actual, .. } if actual == field),
                "expected widening for {field}, got {error:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn repository_loader_refuses_symlinked_policy() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let policy_dir = temp.path().join(".tokentrimmer");
        std::fs::create_dir(&policy_dir).unwrap();
        let target = temp.path().join("untrusted-agent.toml");
        std::fs::write(
            &target,
            include_str!("../templates/init/.tokentrimmer/agent.toml"),
        )
        .unwrap();
        symlink(&target, policy_dir.join("agent.toml")).unwrap();

        assert!(matches!(
            load_repository_policy(temp.path()),
            Err(AgentPolicyError::SymlinkedPolicyFile(_))
        ));
    }

    #[test]
    fn organization_requirement_cannot_silently_fall_back_to_repo() {
        assert!(matches!(
            resolve_agent_policy(
                OrganizationPolicyMode::Required(None),
                &parsed(policy()),
                None,
                None
            ),
            Err(AgentPolicyError::MissingOrganizationPolicy)
        ));
    }
}
