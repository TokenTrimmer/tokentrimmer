//! Versioned, multi-basis cost evidence for local price-governed agent runs.
//!
//! The contract keeps API cash, subscription economics, customer-owned
//! inference TCO, counterfactuals, and unknown evidence separate. It is a pure
//! wire/validation boundary: it does not perform billing, infer plan value, or
//! authorize a provider dispatch.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable schema identifier for [`AgentRunCostEvidence`].
pub const AGENT_COST_SCHEMA_ID: &str = "tokentrimmer.agent-cost-evidence.v1";
/// Current wire-schema version for [`AgentRunCostEvidence`].
pub const AGENT_COST_SCHEMA_VERSION: u32 = 1;
/// Defensive upper bound for cost components retained for one run.
pub const AGENT_COST_COMPONENTS_MAX: usize = 512;
/// Defensive upper bound for distinct unmeasured reasons on one component.
pub const AGENT_COST_REASONS_MAX: usize = 16;

/// Complete cost evidence emitted for one agent run.
///
/// Every model, summarizer, judge, retry, shadow, and other chargeable call is
/// represented by one component. Missing evidence is a first-class component;
/// absence and numeric zero are never interchangeable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRunCostEvidence {
    pub schema_version: u32,
    #[schemars(length(min = 1, max = 256))]
    pub run_id: String,
    #[schemars(length(min = 1, max = 512))]
    pub components: Vec<AgentCostComponent>,
}

impl AgentRunCostEvidence {
    /// Validate cross-field invariants that JSON Schema cannot express.
    pub fn validate(&self) -> Result<(), AgentCostValidationError> {
        if self.schema_version != AGENT_COST_SCHEMA_VERSION {
            return Err(AgentCostValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        validate_label("run_id", &self.run_id)?;
        if self.components.is_empty() {
            return Err(AgentCostValidationError::EmptyComponents);
        }
        if self.components.len() > AGENT_COST_COMPONENTS_MAX {
            return Err(AgentCostValidationError::TooManyComponents(
                self.components.len(),
            ));
        }

        let mut component_ids = HashSet::with_capacity(self.components.len());
        for component in &self.components {
            validate_label("component_id", &component.component_id)?;
            if !component_ids.insert(component.component_id.as_str()) {
                return Err(AgentCostValidationError::DuplicateComponentId(
                    component.component_id.clone(),
                ));
            }
            if component.attempt == 0 {
                return Err(AgentCostValidationError::ZeroAttempt(
                    component.component_id.clone(),
                ));
            }
            component.cost.validate(&component.component_id)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn measured_components(&self) -> usize {
        self.components
            .iter()
            .filter(|component| !matches!(&component.cost, AgentCostBasis::Unmeasured { .. }))
            .count()
    }

    #[must_use]
    pub fn unmeasured_components(&self) -> usize {
        self.components.len() - self.measured_components()
    }
}

/// One independently attributable call or local execution cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostComponent {
    /// Stable run-local id used for deduplication and receipt references.
    #[schemars(length(min = 1, max = 256))]
    pub component_id: String,
    pub purpose: AgentCostPurpose,
    /// One-indexed attempt number. Retries are separate components.
    #[schemars(range(min = 1))]
    pub attempt: u32,
    pub cost: AgentCostBasis,
}

/// Why a cost-bearing operation ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentCostPurpose {
    PrimaryTurn,
    Summarizer,
    Judge,
    Retry,
    Shadow,
    Validation,
    Embedding,
    Routing,
    Other,
}

/// Mutually exclusive accounting bases.
///
/// All cash fields are integer micro-USD. Counterfactual and allocated amounts
/// are named explicitly and must never be added to realized cash totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "basis", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentCostBasis {
    /// A provider API call with catalog/provider/invoice evidence.
    ApiMetered {
        #[schemars(length(min = 1, max = 256))]
        provider: String,
        #[schemars(length(min = 1, max = 256))]
        model: String,
        #[schemars(range(min = 0))]
        amount_micros: i64,
        evidence: ApiMeteredEvidence,
    },
    /// A local vendor runtime using an already-paid subscription entitlement.
    Subscription {
        #[schemars(length(min = 1, max = 256))]
        vendor: String,
        #[schemars(length(min = 1, max = 256))]
        plan_reference: String,
        /// Incremental cash charged for this component. Zero is valid evidence.
        #[schemars(range(min = 0))]
        marginal_cash_micros: i64,
        /// Optional user-configured allocation; never presented as marginal cash.
        #[schemars(range(min = 0))]
        allocated_plan_micros: Option<i64>,
        /// Optional API-price counterfactual; never presented as realized cost.
        #[schemars(range(min = 0))]
        api_equivalent_micros: Option<i64>,
        quota: Option<SubscriptionQuotaEvidence>,
    },
    /// Customer-owned inference costed from an explicit versioned TCO profile.
    SelfHosted {
        #[schemars(length(min = 1, max = 256))]
        profile_id: String,
        #[schemars(length(min = 1, max = 256))]
        profile_revision: String,
        #[schemars(range(min = 0))]
        energy_micros: Option<i64>,
        #[schemars(range(min = 0))]
        hardware_amortization_micros: Option<i64>,
        #[schemars(range(min = 0))]
        hosting_micros: Option<i64>,
        #[schemars(range(min = 0))]
        operator_micros: Option<i64>,
    },
    /// Evidence was unavailable. Never coerce this state to numeric zero.
    Unmeasured {
        expected_basis: ExpectedAgentCostBasis,
        #[schemars(length(min = 1, max = 16))]
        reasons: Vec<UnmeasuredCostReason>,
    },
}

impl AgentCostBasis {
    fn validate(&self, component_id: &str) -> Result<(), AgentCostValidationError> {
        match self {
            Self::ApiMetered {
                provider,
                model,
                amount_micros,
                evidence,
            } => {
                validate_label("provider", provider)?;
                validate_label("model", model)?;
                validate_nonnegative(component_id, "amount_micros", *amount_micros)?;
                evidence.validate()?;
            }
            Self::Subscription {
                vendor,
                plan_reference,
                marginal_cash_micros,
                allocated_plan_micros,
                api_equivalent_micros,
                quota,
            } => {
                validate_label("vendor", vendor)?;
                validate_label("plan_reference", plan_reference)?;
                validate_nonnegative(component_id, "marginal_cash_micros", *marginal_cash_micros)?;
                validate_optional_nonnegative(
                    component_id,
                    "allocated_plan_micros",
                    *allocated_plan_micros,
                )?;
                validate_optional_nonnegative(
                    component_id,
                    "api_equivalent_micros",
                    *api_equivalent_micros,
                )?;
                if let Some(quota) = quota {
                    quota.validate()?;
                }
            }
            Self::SelfHosted {
                profile_id,
                profile_revision,
                energy_micros,
                hardware_amortization_micros,
                hosting_micros,
                operator_micros,
            } => {
                validate_label("profile_id", profile_id)?;
                validate_label("profile_revision", profile_revision)?;
                for (field, value) in [
                    ("energy_micros", *energy_micros),
                    (
                        "hardware_amortization_micros",
                        *hardware_amortization_micros,
                    ),
                    ("hosting_micros", *hosting_micros),
                    ("operator_micros", *operator_micros),
                ] {
                    validate_optional_nonnegative(component_id, field, value)?;
                }
                if energy_micros.is_none()
                    && hardware_amortization_micros.is_none()
                    && hosting_micros.is_none()
                    && operator_micros.is_none()
                {
                    return Err(AgentCostValidationError::EmptySelfHostedCost(
                        component_id.to_owned(),
                    ));
                }
            }
            Self::Unmeasured { reasons, .. } => {
                if reasons.is_empty() {
                    return Err(AgentCostValidationError::EmptyUnmeasuredReasons(
                        component_id.to_owned(),
                    ));
                }
                if reasons.len() > AGENT_COST_REASONS_MAX {
                    return Err(AgentCostValidationError::TooManyUnmeasuredReasons {
                        component_id: component_id.to_owned(),
                        count: reasons.len(),
                    });
                }
                for reason in reasons {
                    if let Some(detail) = &reason.detail {
                        validate_detail(detail)?;
                    }
                    if reason.code == UnmeasuredCostReasonCode::Other && reason.detail.is_none() {
                        return Err(AgentCostValidationError::OtherReasonMissingDetail(
                            component_id.to_owned(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Strength of evidence for a measured API amount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApiMeteredEvidence {
    /// Provider usage settled against a versioned price; not invoice proof.
    Billed {
        #[schemars(length(min = 1, max = 256))]
        pricing_revision: String,
        #[schemars(length(min = 1, max = 256))]
        provider_usage_reference: Option<String>,
    },
    /// Catalog estimate only; must remain visibly estimated.
    Estimated {
        #[schemars(length(min = 1, max = 256))]
        pricing_revision: String,
        price_observed_at: DateTime<Utc>,
    },
    /// Amount matched to a retained provider invoice artifact.
    InvoiceReconciled {
        #[schemars(length(min = 1, max = 256))]
        invoice_reference: String,
    },
}

impl ApiMeteredEvidence {
    fn validate(&self) -> Result<(), AgentCostValidationError> {
        match self {
            Self::Billed {
                pricing_revision,
                provider_usage_reference,
            } => {
                validate_label("pricing_revision", pricing_revision)?;
                if let Some(reference) = provider_usage_reference {
                    validate_label("provider_usage_reference", reference)?;
                }
            }
            Self::Estimated {
                pricing_revision, ..
            } => validate_label("pricing_revision", pricing_revision)?,
            Self::InvoiceReconciled { invoice_reference } => {
                validate_label("invoice_reference", invoice_reference)?;
            }
        }
        Ok(())
    }
}

/// Optional quota/window evidence exposed by a subscription runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionQuotaEvidence {
    pub unit: SubscriptionQuotaUnit,
    pub used: u64,
    #[schemars(range(min = 1))]
    pub limit: Option<u64>,
    pub window_ends_at: Option<DateTime<Utc>>,
    #[schemars(length(min = 1, max = 256))]
    pub source: String,
}

impl SubscriptionQuotaEvidence {
    fn validate(&self) -> Result<(), AgentCostValidationError> {
        validate_label("quota.source", &self.source)?;
        if let Some(limit) = self.limit {
            if limit == 0 || self.used > limit {
                return Err(AgentCostValidationError::InvalidQuota {
                    used: self.used,
                    limit,
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionQuotaUnit {
    Requests,
    Tokens,
    ToolCalls,
    VendorUnits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedAgentCostBasis {
    ApiMetered,
    Subscription,
    SelfHosted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnmeasuredCostReason {
    pub code: UnmeasuredCostReasonCode,
    #[schemars(length(min = 1, max = 512))]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnmeasuredCostReasonCode {
    PriceUnknown,
    ProviderUsageMissing,
    InvoiceUnavailable,
    SubscriptionQuotaUnavailable,
    SubscriptionAllocationUnconfigured,
    LocalTcoProfileMissing,
    LocalTelemetryMissing,
    VendorSignalUnavailable,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AgentCostValidationError {
    #[error("unsupported agent cost schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("agent cost evidence must contain at least one component")]
    EmptyComponents,
    #[error("agent cost evidence has {0} components; maximum is {AGENT_COST_COMPONENTS_MAX}")]
    TooManyComponents(usize),
    #[error("duplicate agent cost component id: {0}")]
    DuplicateComponentId(String),
    #[error("agent cost component {0} has attempt 0")]
    ZeroAttempt(String),
    #[error("{field} must be 1..=256 visible characters")]
    InvalidLabel { field: &'static str },
    #[error("unmeasured detail must be 1..=512 visible characters")]
    InvalidDetail,
    #[error("agent cost component {component_id} has negative {field}")]
    NegativeAmount {
        component_id: String,
        field: &'static str,
    },
    #[error("self-hosted component {0} has no measured TCO field")]
    EmptySelfHostedCost(String),
    #[error("unmeasured component {0} has no reason")]
    EmptyUnmeasuredReasons(String),
    #[error("unmeasured component {component_id} has {count} reasons; maximum is {AGENT_COST_REASONS_MAX}")]
    TooManyUnmeasuredReasons { component_id: String, count: usize },
    #[error("unmeasured component {0} uses other without detail")]
    OtherReasonMissingDetail(String),
    #[error("subscription quota must satisfy 0 <= used <= nonzero limit; got {used}/{limit}")]
    InvalidQuota { used: u64, limit: u64 },
}

fn validate_nonnegative(
    component_id: &str,
    field: &'static str,
    value: i64,
) -> Result<(), AgentCostValidationError> {
    if value < 0 {
        return Err(AgentCostValidationError::NegativeAmount {
            component_id: component_id.to_owned(),
            field,
        });
    }
    Ok(())
}

fn validate_optional_nonnegative(
    component_id: &str,
    field: &'static str,
    value: Option<i64>,
) -> Result<(), AgentCostValidationError> {
    if let Some(value) = value {
        validate_nonnegative(component_id, field, value)?;
    }
    Ok(())
}

fn validate_label(field: &'static str, value: &str) -> Result<(), AgentCostValidationError> {
    let chars = value.chars().count();
    if chars == 0 || chars > 256 || value.chars().any(char::is_control) {
        return Err(AgentCostValidationError::InvalidLabel { field });
    }
    Ok(())
}

fn validate_detail(value: &str) -> Result<(), AgentCostValidationError> {
    let chars = value.chars().count();
    if chars == 0 || chars > 512 || value.chars().any(char::is_control) {
        return Err(AgentCostValidationError::InvalidDetail);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(cost: AgentCostBasis) -> AgentCostComponent {
        AgentCostComponent {
            component_id: "turn-1".into(),
            purpose: AgentCostPurpose::PrimaryTurn,
            attempt: 1,
            cost,
        }
    }

    fn evidence(cost: AgentCostBasis) -> AgentRunCostEvidence {
        AgentRunCostEvidence {
            schema_version: AGENT_COST_SCHEMA_VERSION,
            run_id: "run-123".into(),
            components: vec![component(cost)],
        }
    }

    #[test]
    fn preserves_zero_subscription_marginal_cost_without_inventing_allocation() {
        let value = evidence(AgentCostBasis::Subscription {
            vendor: "openai".into(),
            plan_reference: "local-codex-session".into(),
            marginal_cash_micros: 0,
            allocated_plan_micros: None,
            api_equivalent_micros: Some(12_500),
            quota: None,
        });

        value.validate().unwrap();
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["components"][0]["cost"]["marginal_cash_micros"], 0);
        assert!(json["components"][0]["cost"]["allocated_plan_micros"].is_null());
        assert_eq!(value.measured_components(), 1);
        assert_eq!(value.unmeasured_components(), 0);
    }

    #[test]
    fn keeps_unmeasured_separate_from_numeric_zero() {
        let value = evidence(AgentCostBasis::Unmeasured {
            expected_basis: ExpectedAgentCostBasis::ApiMetered,
            reasons: vec![UnmeasuredCostReason {
                code: UnmeasuredCostReasonCode::PriceUnknown,
                detail: Some("model missing from the versioned catalog".into()),
            }],
        });

        value.validate().unwrap();
        let json = serde_json::to_value(&value).unwrap();
        assert!(json["components"][0]["cost"].get("amount_micros").is_none());
        assert_eq!(value.measured_components(), 0);
        assert_eq!(value.unmeasured_components(), 1);
    }

    #[test]
    fn rejects_duplicate_components_negative_money_and_empty_local_tco() {
        let mut duplicate = evidence(AgentCostBasis::ApiMetered {
            provider: "openai".into(),
            model: "gpt-example".into(),
            amount_micros: 42,
            evidence: ApiMeteredEvidence::Billed {
                pricing_revision: "catalog-2026-08-15".into(),
                provider_usage_reference: None,
            },
        });
        duplicate.components.push(duplicate.components[0].clone());
        assert!(matches!(
            duplicate.validate(),
            Err(AgentCostValidationError::DuplicateComponentId(_))
        ));

        let negative = evidence(AgentCostBasis::Subscription {
            vendor: "anthropic".into(),
            plan_reference: "local-session".into(),
            marginal_cash_micros: -1,
            allocated_plan_micros: None,
            api_equivalent_micros: None,
            quota: None,
        });
        assert!(matches!(
            negative.validate(),
            Err(AgentCostValidationError::NegativeAmount { .. })
        ));

        let empty_local = evidence(AgentCostBasis::SelfHosted {
            profile_id: "mac-studio".into(),
            profile_revision: "1".into(),
            energy_micros: None,
            hardware_amortization_micros: None,
            hosting_micros: None,
            operator_micros: None,
        });
        assert!(matches!(
            empty_local.validate(),
            Err(AgentCostValidationError::EmptySelfHostedCost(_))
        ));
    }

    #[test]
    fn rejects_unmeasured_other_without_explanation_and_invalid_quota() {
        let other = evidence(AgentCostBasis::Unmeasured {
            expected_basis: ExpectedAgentCostBasis::Subscription,
            reasons: vec![UnmeasuredCostReason {
                code: UnmeasuredCostReasonCode::Other,
                detail: None,
            }],
        });
        assert!(matches!(
            other.validate(),
            Err(AgentCostValidationError::OtherReasonMissingDetail(_))
        ));

        let quota = evidence(AgentCostBasis::Subscription {
            vendor: "openai".into(),
            plan_reference: "local-session".into(),
            marginal_cash_micros: 0,
            allocated_plan_micros: None,
            api_equivalent_micros: None,
            quota: Some(SubscriptionQuotaEvidence {
                unit: SubscriptionQuotaUnit::VendorUnits,
                used: 11,
                limit: Some(10),
                window_ends_at: None,
                source: "vendor-runtime".into(),
            }),
        });
        assert!(matches!(
            quota.validate(),
            Err(AgentCostValidationError::InvalidQuota { .. })
        ));
    }
}
