//! RequestContext carries authenticated identity, trace IDs, and credentials
//! through the request lifecycle. Adapters are stateless — every call gets a
//! fresh context.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use uuid::Uuid;

/// Subscription tier for the calling organisation, as surfaced by the cloud
/// tier-resolution layer.
///
/// The tier drives cache TTL selection per spec §8.4 (24h / 7d / 30d bands).
/// The tier is carried as `Option<CallerTier>` on `tt_auth::ApiKeyContext`
/// (NOT on [`crate::context::RequestContext`], which has no `tier` field): when
/// `None`, the gateway falls back to the conservative 24h default. The cloud
/// will inject the real tier once `rv-tier-limits-enforcement` is wired; until
/// then all requests run as if Free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerTier {
    /// Free tier — 24h cache TTL (spec §8.4).
    Free,
    /// Pro tier — 7d cache TTL (spec §8.4).
    Pro,
    /// Team tier — 7d cache TTL (spec §8.4, same band as Pro).
    Team,
    /// Scale tier — 30d cache TTL (spec §8.4).
    Scale,
}

impl CallerTier {
    /// Cache TTL in seconds for this tier, per spec §8.4.
    ///
    /// | Tier        | TTL    |
    /// | ----------- | ------ |
    /// | Free        | 24h    |
    /// | Pro / Team  | 7d     |
    /// | Scale       | 30d    |
    #[must_use]
    pub fn ttl_secs(self) -> u64 {
        match self {
            CallerTier::Free => 24 * 60 * 60,
            CallerTier::Pro | CallerTier::Team => 7 * 24 * 60 * 60,
            CallerTier::Scale => 30 * 24 * 60 * 60,
        }
    }
}

const MICRO_USD_PER_USD: f64 = 1_000_000.0;

/// Why a run-scoped provider reservation settled at its final amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunBudgetSettlementBasis {
    /// Provider usage was available and priced.
    ProviderUsage,
    /// Usage or pricing was unavailable, so the admitted upper bound was kept.
    ConservativeEstimate,
}

/// One provider attempt settled against a run's caller-supplied cash ceiling.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunBudgetComponent {
    pub dispatch_key: [u8; 32],
    pub provider: String,
    pub operation: String,
    pub model: String,
    pub estimated_micros: u64,
    pub settled_micros: u64,
    pub basis: RunBudgetSettlementBasis,
}

/// Immutable view of one run's reservation ledger.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunBudgetSnapshot {
    pub cap_micros: u64,
    pub settled_micros: u64,
    pub reserved_micros: u64,
    pub components: Vec<RunBudgetComponent>,
}

impl RunBudgetSnapshot {
    #[must_use]
    pub fn remaining_micros(&self) -> u64 {
        self.cap_micros
            .saturating_sub(self.settled_micros.saturating_add(self.reserved_micros))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunBudgetAdmissionError {
    PriceUnknown {
        model: String,
    },
    Exceeded {
        estimated_micros: u64,
        remaining_micros: u64,
    },
    DuplicateDispatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunBudgetReservation {
    dispatch_key: [u8; 32],
    estimated_micros: u64,
}

#[derive(Debug)]
struct ActiveRunBudgetReservation {
    provider: String,
    operation: String,
    model: String,
    estimated_micros: u64,
}

#[derive(Debug)]
struct RunBudgetLedger {
    cap_micros: u64,
    settled_micros: u64,
    reserved_micros: u64,
    active: HashMap<[u8; 32], ActiveRunBudgetReservation>,
    completed: HashSet<[u8; 32]>,
    components: Vec<RunBudgetComponent>,
}

/// Run-local, clone-shared reservation ledger.
///
/// Provider wrappers reserve a conservative upper bound before every upstream
/// call and settle it from provider usage afterward. Integer micro-USD keeps
/// admission deterministic and prevents floating-point drift from widening the
/// caller's ceiling.
#[derive(Clone)]
pub struct RunBudgetState {
    inner: Arc<Mutex<RunBudgetLedger>>,
}

impl RunBudgetState {
    #[must_use]
    pub fn from_usd(cap_usd: f64, already_settled_usd: f64) -> Option<Self> {
        let cap_micros = usd_to_micros_floor(cap_usd)?;
        let settled_micros = usd_to_micros_ceil(already_settled_usd)?;
        Some(Self {
            inner: Arc::new(Mutex::new(RunBudgetLedger {
                cap_micros,
                settled_micros,
                reserved_micros: 0,
                active: HashMap::new(),
                completed: HashSet::new(),
                components: Vec::new(),
            })),
        })
    }

    #[must_use]
    pub fn from_persisted(
        cap_usd: f64,
        settled_micros: u64,
        components: Vec<RunBudgetComponent>,
    ) -> Option<Self> {
        let cap_micros = usd_to_micros_floor(cap_usd)?;
        let completed: HashSet<_> = components
            .iter()
            .map(|component| component.dispatch_key)
            .collect();
        if completed.len() != components.len()
            || components
                .iter()
                .try_fold(0_u64, |sum, component| {
                    sum.checked_add(component.settled_micros)
                })
                .is_none_or(|component_total| component_total > settled_micros)
        {
            return None;
        }
        Some(Self {
            inner: Arc::new(Mutex::new(RunBudgetLedger {
                cap_micros,
                settled_micros,
                reserved_micros: 0,
                active: HashMap::new(),
                completed,
                components,
            })),
        })
    }

    pub fn reserve(
        &self,
        dispatch_key: [u8; 32],
        provider: &str,
        operation: &str,
        model: &str,
        estimated_usd: Option<f64>,
    ) -> Result<RunBudgetReservation, RunBudgetAdmissionError> {
        let estimated_micros = estimated_usd.and_then(usd_to_micros_ceil).ok_or_else(|| {
            RunBudgetAdmissionError::PriceUnknown {
                model: model.to_string(),
            }
        })?;
        let mut ledger = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ledger.active.contains_key(&dispatch_key) || ledger.completed.contains(&dispatch_key) {
            return Err(RunBudgetAdmissionError::DuplicateDispatch);
        }
        let remaining_micros = ledger
            .cap_micros
            .saturating_sub(ledger.settled_micros.saturating_add(ledger.reserved_micros));
        if estimated_micros > remaining_micros {
            return Err(RunBudgetAdmissionError::Exceeded {
                estimated_micros,
                remaining_micros,
            });
        }
        ledger.reserved_micros = ledger.reserved_micros.saturating_add(estimated_micros);
        ledger.active.insert(
            dispatch_key,
            ActiveRunBudgetReservation {
                provider: provider.to_string(),
                operation: operation.to_string(),
                model: model.to_string(),
                estimated_micros,
            },
        );
        Ok(RunBudgetReservation {
            dispatch_key,
            estimated_micros,
        })
    }

    /// Release admission when a second, durable scope rejects before dispatch.
    pub fn release(&self, reservation: RunBudgetReservation) {
        let mut ledger = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ledger.active.remove(&reservation.dispatch_key).is_some() {
            ledger.reserved_micros = ledger
                .reserved_micros
                .saturating_sub(reservation.estimated_micros);
        }
    }

    pub fn settle(
        &self,
        reservation: RunBudgetReservation,
        actual_usd: Option<f64>,
        basis: RunBudgetSettlementBasis,
    ) {
        let mut ledger = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = ledger.active.remove(&reservation.dispatch_key) else {
            return;
        };
        ledger.reserved_micros = ledger
            .reserved_micros
            .saturating_sub(active.estimated_micros);
        let (settled_micros, basis) = match actual_usd.and_then(usd_to_micros_ceil) {
            Some(actual_micros) => (actual_micros, basis),
            None => (
                active.estimated_micros,
                RunBudgetSettlementBasis::ConservativeEstimate,
            ),
        };
        ledger.settled_micros = ledger.settled_micros.saturating_add(settled_micros);
        ledger.completed.insert(reservation.dispatch_key);
        ledger.components.push(RunBudgetComponent {
            dispatch_key: reservation.dispatch_key,
            provider: active.provider,
            operation: active.operation,
            model: active.model,
            estimated_micros: active.estimated_micros,
            settled_micros,
            basis,
        });
    }

    #[must_use]
    pub fn snapshot(&self) -> RunBudgetSnapshot {
        let ledger = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        RunBudgetSnapshot {
            cap_micros: ledger.cap_micros,
            settled_micros: ledger.settled_micros,
            reserved_micros: ledger.reserved_micros,
            components: ledger.components.clone(),
        }
    }
}

impl std::fmt::Debug for RunBudgetState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunBudgetState")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

fn usd_to_micros_floor(value: f64) -> Option<u64> {
    let scaled = value * MICRO_USD_PER_USD;
    (value.is_finite() && value >= 0.0 && scaled <= u64::MAX as f64).then(|| scaled.floor() as u64)
}

fn usd_to_micros_ceil(value: f64) -> Option<u64> {
    let scaled = value * MICRO_USD_PER_USD;
    (value.is_finite() && value >= 0.0 && scaled <= u64::MAX as f64).then(|| scaled.ceil() as u64)
}
/// Request-scoped privacy constraints applied to every primary, retry,
/// fallback, shadow, judge, and other provider dispatch sharing this state.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceDispatchPolicy {
    #[serde(default)]
    pub no_external_egress: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residency: Option<String>,
}

impl InferenceDispatchPolicy {
    pub fn validate(&self) -> Result<(), DispatchPolicyError> {
        if self.residency.as_deref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                || value.starts_with('-')
                || value.ends_with('-')
        }) {
            return Err(DispatchPolicyError::InvalidResidency);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderDispatchEvidence {
    pub schema: String,
    pub provider: String,
    pub model: String,
    pub evidence: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DispatchPolicyError {
    #[error("inference residency must be a lower-case region identifier")]
    InvalidResidency,
    #[error("inference dispatch policy was already fixed to a different value")]
    AlreadySet,
}

const MAX_PROVIDER_DISPATCH_EVIDENCE: usize = 64;

/// Request-scoped state for deterministic provider-dispatch admission.
///
/// The seed is random by default and can instead be derived one-way from a
/// caller idempotency key. Clones share per-fingerprint attempt counters, so
/// retries of the same provider request get distinct, deterministic ordinals
/// while a replay of the whole request starts from the same seed and ordinals.
#[derive(Clone)]
pub struct BudgetDispatchState {
    seed: [u8; 32],
    attempts: Arc<Mutex<HashMap<[u8; 32], u32>>>,
    inference_policy: Arc<OnceLock<InferenceDispatchPolicy>>,
    provider_evidence: Arc<Mutex<Vec<ProviderDispatchEvidence>>>,
    run_budget: Option<RunBudgetState>,
}

impl BudgetDispatchState {
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            seed,
            attempts: Arc::new(Mutex::new(HashMap::new())),
            run_budget: None,
            inference_policy: Arc::new(OnceLock::new()),
            provider_evidence: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[must_use]
    pub fn with_run_budget(mut self, run_budget: Option<RunBudgetState>) -> Self {
        self.run_budget = run_budget;
        self
    }

    #[must_use]
    pub fn run_budget(&self) -> Option<&RunBudgetState> {
        self.run_budget.as_ref()
    }

    pub fn set_inference_policy(
        &self,
        policy: InferenceDispatchPolicy,
    ) -> Result<(), DispatchPolicyError> {
        policy.validate()?;
        if self
            .inference_policy
            .get()
            .is_some_and(|current| current == &policy)
        {
            return Ok(());
        }
        self.inference_policy
            .set(policy)
            .map_err(|_| DispatchPolicyError::AlreadySet)
    }

    #[must_use]
    pub fn inference_policy(&self) -> InferenceDispatchPolicy {
        self.inference_policy.get().cloned().unwrap_or_default()
    }

    /// Retain bounded, secret-free provider evidence for the terminal trace or receipt.
    pub fn record_provider_evidence(&self, evidence: ProviderDispatchEvidence) -> bool {
        let mut values = self
            .provider_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if values.len() >= MAX_PROVIDER_DISPATCH_EVIDENCE {
            return false;
        }
        values.push(evidence);
        true
    }

    #[must_use]
    pub fn provider_evidence(&self) -> Vec<ProviderDispatchEvidence> {
        self.provider_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    /// Return the next zero-based attempt ordinal for one dispatch fingerprint.
    #[must_use]
    pub fn next_attempt(&self, fingerprint: [u8; 32]) -> u32 {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = attempts.entry(fingerprint).or_insert(0);
        let ordinal = *next;
        *next = next.saturating_add(1);
        ordinal
    }
}

impl Default for BudgetDispatchState {
    fn default() -> Self {
        let mut seed = [0_u8; 32];
        seed[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        seed[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        Self::from_seed(seed)
    }
}

impl std::fmt::Debug for BudgetDispatchState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BudgetDispatchState([redacted])")
    }
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub trace_id: Uuid,
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    /// One-way, request-stable state for durable provider-dispatch admission.
    /// HTTP entry points derive its seed from a caller idempotency key; all
    /// other callers use an unguessable request-local seed. Raw keys are never
    /// stored.
    pub budget_dispatch: BudgetDispatchState,
    pub credentials: ProviderCredentials,
    /// Free-form cost-attribution tag from `X-TokenTrimmer-Tag` header.
    pub tag: Option<String>,
    /// Deadline for the entire request. Adapters should respect this.
    pub deadline: Option<Duration>,
    /// Agent run ID — set when this request belongs to an agent run loop
    /// (W0b durable-run-grain). `None` for standalone (non-agent) requests.
    pub run_id: Option<Uuid>,
    /// Agent node/step ID within the run — set alongside `run_id`.
    /// `None` for standalone requests.
    pub node_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct ProviderCredentials {
    pub api_key: SecretString,
    /// Self-hosted endpoint override (used for Ollama, vLLM, LM Studio, OpenRouter, etc.).
    pub base_url: Option<String>,
    pub extra_headers: Vec<(String, String)>,
}

/// String wrapper whose `Debug` impl never prints the secret.
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretString(****)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_context_has_run_id_node_id_fields() {
        // construction itself is the gate; this asserts the fields exist + accept Option<Uuid>
        let ctx = RequestContext {
            budget_dispatch: BudgetDispatchState::default(),
            trace_id: Uuid::nil(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: ProviderCredentials {
                api_key: SecretString::new("test"),
                base_url: None,
                extra_headers: Vec::new(),
            },
            tag: None,
            deadline: None,
            run_id: None,
            node_id: None,
        };
        assert_eq!(ctx.run_id, None);
        assert_eq!(ctx.node_id, None);
    }

    #[test]
    fn run_budget_reserves_atomically_and_settles_integer_micro_usd() {
        let budget = RunBudgetState::from_usd(1.0, 0.25).unwrap();
        let first = budget
            .reserve([1; 32], "openai", "chat", "gpt-test", Some(0.5))
            .unwrap();
        let rejected = budget.reserve([2; 32], "openai", "chat", "gpt-test", Some(0.3));
        assert_eq!(
            rejected,
            Err(RunBudgetAdmissionError::Exceeded {
                estimated_micros: 300_000,
                remaining_micros: 250_000,
            })
        );

        budget.settle(first, Some(0.125), RunBudgetSettlementBasis::ProviderUsage);
        let second = budget
            .reserve([2; 32], "openai", "chat", "gpt-test", Some(0.6))
            .unwrap();
        budget.settle(second, None, RunBudgetSettlementBasis::ProviderUsage);

        let snapshot = budget.snapshot();
        assert_eq!(snapshot.cap_micros, 1_000_000);
        assert_eq!(snapshot.settled_micros, 975_000);
        assert_eq!(snapshot.reserved_micros, 0);
        assert_eq!(snapshot.remaining_micros(), 25_000);
        assert_eq!(snapshot.components.len(), 2);
        assert_eq!(
            snapshot.components[0].basis,
            RunBudgetSettlementBasis::ProviderUsage
        );
        assert_eq!(
            snapshot.components[1].basis,
            RunBudgetSettlementBasis::ConservativeEstimate
        );

        let restored = RunBudgetState::from_persisted(
            1.0,
            snapshot.settled_micros,
            snapshot.components.clone(),
        )
        .unwrap();
        assert_eq!(
            restored.reserve([1; 32], "openai", "chat", "gpt-test", Some(0.01)),
            Err(RunBudgetAdmissionError::DuplicateDispatch)
        );
        assert!(restored
            .reserve([3; 32], "openai", "chat", "gpt-test", Some(0.025))
            .is_ok());
    }

    #[test]
    fn run_budget_rejects_unknown_price_and_releases_predispatch_admission() {
        let budget = RunBudgetState::from_usd(0.5, 0.0).unwrap();
        assert_eq!(
            budget.reserve([1; 32], "custom", "chat", "unknown", None),
            Err(RunBudgetAdmissionError::PriceUnknown {
                model: "unknown".into(),
            })
        );
        let reservation = budget
            .reserve([2; 32], "openai", "chat", "gpt-test", Some(0.5))
            .unwrap();
        budget.release(reservation);
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.settled_micros, 0);
        assert_eq!(snapshot.reserved_micros, 0);
        assert!(snapshot.components.is_empty());
    }
}
