//! Fusion panel — caller-facing opt-in surface.
//!
//! # Off-by-default
//! A request with no `X-TokenTrimmer-Panel` header is completely untouched —
//! `panel_from_header` returns `None`. An unknown strategy value is also silently
//! treated as `None` (never an error at the header layer).
//!
//! # Types produced (consumed by Tasks 3–7)
//! - [`ArbiterStrategyKind`] — the three dispatch strategies
//! - [`ModelRef`]            — a model + optional provider pin
//! - [`PanelConfig`]         — resolved, complete panel configuration
//! - [`PanelAdmission`]      — opaque proof of static pre-dispatch admission
//! - [`PanelDefaults`]       — gateway-level defaults sourced from env vars
//! - [`PanelExtras`]         — per-request overrides from `tt_extras.panel`
//! - [`LegRole`]             — whether a leg is a panel member or the arbiter
//! - [`LegStatus`]           — outcome of a single leg dispatch
//! - [`LegResult`]           — full result record for one dispatched leg
//! - [`ArbiterOutcome`]      — the synthesized / picked response from arbitration
//! - [`ArbiterStrategy`]     — trait implemented by each arbitration algorithm
//! - [`Synthesize`]          — the synthesize-a-new-answer arbitration strategy
//! - [`strategy_for`]        — factory: [`PanelConfig`] → [`Box<dyn ArbiterStrategy>`]

use std::time::Duration;

use async_trait::async_trait;
use axum::http::HeaderMap;
use chrono::Utc;
use serde_json::json;
use tt_telemetry::{panel_legs::PanelLegRow, request_logs::RequestLogRow};
use uuid::Uuid;

use futures::stream::BoxStream;
use tt_shared::{
    messages::{ChatCompletionRequest, Message, MessageContent, PanelExtras},
    ChatCompletionChunk, ChatCompletionResponse, ProviderError, RequestContext, Usage,
};

use crate::routes::chat::{
    spawn_request_log, CompletionHeaders, CompletionOutcome, CostBreakdown, Prepared,
};
use crate::{ApiError, ApiResult, AppState};

// ---------------------------------------------------------------------------
// Strategy kind
// ---------------------------------------------------------------------------

/// Which arbitration algorithm the panel should run after collecting all legs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArbiterStrategyKind {
    /// Synthesize a new answer from all legs using an arbiter model.
    Synthesize,
    /// Pick the single best leg as judged by the arbiter model.
    BestOfN,
    /// Return the majority-vote answer (simple token-overlap majority).
    Majority,
}

impl ArbiterStrategyKind {
    /// Wire-format string (matches the header value and config serialization).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synthesize => "synthesize",
            Self::BestOfN => "best-of-n",
            Self::Majority => "majority",
        }
    }

    /// Parse a strategy from its wire-format string (case-insensitive).
    /// Returns `None` for unknown values — callers should treat that as "no panel".
    ///
    /// `pub(crate)` so the gateway-wiring in `chat.rs` can authoritatively parse
    /// a route's `then.panel.strategy` string at request time (header-wins
    /// fallback) and the drift-guard test can assert every
    /// `tt_routing::PANEL_STRATEGY_VALUES` value parses.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "synthesize" => Some(Self::Synthesize),
            "best-of-n" | "best_of_n" => Some(Self::BestOfN),
            "majority" => Some(Self::Majority),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ModelRef — a model + optional provider pin
// ---------------------------------------------------------------------------

/// A model reference with an optional explicit provider.
///
/// `"model-id"` without a provider → the gateway resolves the provider via
/// its standard registry. `"model-id"` with `provider = Some("openai")` →
/// dispatch is pinned to that provider.
#[derive(Clone, Debug, Default)]
pub struct ModelRef {
    pub model: String,
    pub provider: Option<String>,
}

// ---------------------------------------------------------------------------
// PanelConfig — resolved, complete panel configuration
// ---------------------------------------------------------------------------

/// Fully-resolved panel configuration for one request.
///
/// Constructed by [`PanelConfig::resolve`] from the header strategy + optional
/// per-request [`PanelExtras`] + gateway [`PanelDefaults`].
#[derive(Clone, Debug)]
pub struct PanelConfig {
    /// Which arbitration algorithm to run.
    pub strategy: ArbiterStrategyKind,
    /// Panel member models (at least one guaranteed after `resolve`).
    pub members: Vec<ModelRef>,
    /// The arbiter model used for Synthesize / BestOfN.
    pub arbiter_model: ModelRef,
    /// Minimum legs that must succeed for the panel to return a result.
    /// `None` uses the strategy's safe default: a strict majority for
    /// [`ArbiterStrategyKind::Majority`] and one successful member for the
    /// arbiter-backed strategies.
    pub quorum: Option<usize>,
    /// Preflight admission budget in USD across all legs + arbitration.
    /// `None` requires a separate request-level cost limit. This estimate gate
    /// is not a post-dispatch spending guarantee.
    pub max_cost_usd: Option<f64>,
}

// ---------------------------------------------------------------------------
// Member-count cap
// ---------------------------------------------------------------------------

/// Hard cap on the number of panel members (the arbiter is not counted).
///
/// Override with the `TT_PANEL_MAX_MEMBERS` environment variable (must be ≥ 1;
/// invalid or zero values are silently ignored and the default is used).
///
/// This is deliberately the one resolver used both by request validation and
/// the authenticated runtime-capabilities endpoint. A capability response is
/// a snapshot rather than a reservation, but it must never advertise a member
/// cap different from the cap the responding process will enforce.
pub fn panel_max_members() -> usize {
    std::env::var("TT_PANEL_MAX_MEMBERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(8)
}

impl PanelConfig {
    /// Validate a resolved panel before it can enter the dispatch engine.
    ///
    /// [`PanelConfig::resolve`] calls this after merging header/default inputs,
    /// but the fields are public for intentional direct-engine construction.
    /// Keep the same validation at the shared admission boundary so a direct
    /// caller cannot mint or revalidate a [`PanelAdmission`] for blank,
    /// duplicated, over-cap, invalid-quorum, or invalid-budget work.
    pub fn validate_for_dispatch(&self) -> ApiResult<()> {
        if self.members.is_empty() {
            return Err(ApiError::InvalidRequest(
                "panel requires at least one member model".to_string(),
            ));
        }

        let max = panel_max_members();
        if self.members.len() > max {
            return Err(ApiError::InvalidRequest(format!(
                "panel: {} members exceeds the maximum of {}",
                self.members.len(),
                max
            )));
        }

        let mut member_identities = std::collections::HashSet::with_capacity(self.members.len());
        for member in &self.members {
            let model = member.model.trim();
            if model.is_empty() {
                return Err(ApiError::InvalidRequest(
                    "panel member model must not be blank".to_string(),
                ));
            }
            let provider = member.provider.as_deref().map(str::trim);
            if provider.is_some_and(str::is_empty) {
                return Err(ApiError::InvalidRequest(
                    "panel member provider must not be blank when specified".to_string(),
                ));
            }
            if !member_identities.insert((model, provider.unwrap_or_default())) {
                return Err(ApiError::InvalidRequest(format!(
                    "panel member {model:?} is configured more than once"
                )));
            }
        }

        if self.arbiter_model.model.trim().is_empty() {
            return Err(ApiError::InvalidRequest(
                "panel arbiter model must not be blank".to_string(),
            ));
        }
        if self
            .arbiter_model
            .provider
            .as_deref()
            .is_some_and(|provider| provider.trim().is_empty())
        {
            return Err(ApiError::InvalidRequest(
                "panel arbiter provider must not be blank when specified".to_string(),
            ));
        }

        if let Some(quorum) = self.quorum {
            if quorum == 0 {
                return Err(ApiError::InvalidRequest(
                    "panel quorum must be at least one".to_string(),
                ));
            }
            if quorum > self.members.len() {
                return Err(ApiError::InvalidRequest(format!(
                    "panel quorum {quorum} exceeds the {} configured members",
                    self.members.len()
                )));
            }
        }

        if self
            .max_cost_usd
            .is_some_and(|budget| !budget.is_finite() || budget <= 0.0)
        {
            return Err(ApiError::InvalidRequest(
                "panel max_cost_usd must be a finite number greater than zero".to_string(),
            ));
        }

        Ok(())
    }

    /// Resolve a complete [`PanelConfig`] from its three input sources.
    ///
    /// Precedence (highest → lowest):
    /// 1. `extras` — per-request `tt_extras.panel` overrides
    /// 2. `defaults` — gateway-level defaults from env vars
    ///
    /// Returns [`ApiError::InvalidRequest`] when the merged member list is
    /// empty, invalid, duplicated, exceeds the cap set by
    /// `TT_PANEL_MAX_MEMBERS` (default 8), or when the optional quorum/budget
    /// cannot be honored safely.
    pub fn resolve(
        strategy: ArbiterStrategyKind,
        extras: Option<&PanelExtras>,
        defaults: &PanelDefaults,
    ) -> ApiResult<PanelConfig> {
        // Members: extras override defaults entirely when non-empty.
        let members: Vec<ModelRef> = if let Some(e) = extras {
            if !e.members.is_empty() {
                e.members
                    .iter()
                    .map(|m| ModelRef {
                        model: m.clone(),
                        provider: None,
                    })
                    .collect()
            } else {
                defaults.members.clone()
            }
        } else {
            defaults.members.clone()
        };

        // Arbiter: extras override defaults.
        let arbiter_model = if let Some(e) = extras {
            if let Some(ref am) = e.arbiter_model {
                ModelRef {
                    model: am.clone(),
                    provider: None,
                }
            } else {
                defaults.arbiter_model.clone()
            }
        } else {
            defaults.arbiter_model.clone()
        };

        let quorum = extras.and_then(|e| e.quorum);
        let max_cost_usd = extras.and_then(|e| e.max_cost_usd);
        let config = PanelConfig {
            strategy,
            members,
            arbiter_model,
            quorum,
            max_cost_usd,
        };
        // Fail closed before any provider resolution or dispatch. Dashboard
        // validation is only a convenience layer: clients can send the API
        // payload directly, and defaults may be configured outside the UI.
        config.validate_for_dispatch()?;
        Ok(config)
    }
}

/// Return the effective member quorum after [`PanelConfig::validate_for_dispatch`]
/// has established that the configuration is structurally valid.
pub(crate) fn required_panel_quorum(cfg: &PanelConfig) -> usize {
    cfg.quorum.unwrap_or(match cfg.strategy {
        ArbiterStrategyKind::Majority => (cfg.members.len() / 2) + 1,
        ArbiterStrategyKind::Synthesize | ArbiterStrategyKind::BestOfN => 1,
    })
}

/// Reject a Fusion configuration that cannot start with the credentials already
/// resolved for this request.
///
/// This checks only the request-local provider-id → credential map: enough
/// configured member *legs* must be eligible to meet quorum, and the LLM
/// arbiter used by Synthesize/Best-of-N must have an explicitly mapped
/// credential. It does not contact providers or validate credentials, reserve
/// spend, establish runtime readiness, or guarantee a successful execution.
///
/// Missing credentials for additional members remain representable as
/// [`LegStatus::SkippedNoCred`] once this fence proves that the remaining
/// credentialed legs can meet quorum. Provider IDs are intentionally resolved
/// by the same registry lookup used by fan-out; optional `ModelRef::provider`
/// pins are not interpreted differently at this seam.
pub(crate) fn validate_panel_credential_preflight(
    state: &AppState,
    cfg: &PanelConfig,
    creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
) -> ApiResult<()> {
    cfg.validate_for_dispatch()?;

    let mut credentialed_members = 0;
    for member in &cfg.members {
        let provider =
            state
                .registry
                .resolve(&member.model)
                .ok_or_else(|| ApiError::ModelNotFound {
                    model: member.model.clone(),
                })?;
        if creds.contains_key(provider.id()) {
            credentialed_members += 1;
        }
    }

    let missing_arbiter = match cfg.strategy {
        ArbiterStrategyKind::Synthesize | ArbiterStrategyKind::BestOfN => {
            let provider = state
                .registry
                .resolve(&cfg.arbiter_model.model)
                .ok_or_else(|| ApiError::ModelNotFound {
                    model: cfg.arbiter_model.model.clone(),
                })?;
            !creds.contains_key(provider.id())
        }
        // Majority uses the embedding path rather than the configured LLM
        // arbiter, so this map cannot make a meaningful arbiter claim here.
        ArbiterStrategyKind::Majority => false,
    };

    let required = required_panel_quorum(cfg);
    if credentialed_members < required || missing_arbiter {
        return Err(ApiError::PanelCredentialPreflight {
            required,
            credentialed: credentialed_members,
            missing_arbiter,
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// PanelDefaults — gateway-level defaults from env vars
// ---------------------------------------------------------------------------

/// Gateway-level panel defaults, sourced from environment variables:
/// - `TT_PANEL_DEFAULT_MEMBERS` — comma-separated model ids
/// - `TT_PANEL_DEFAULT_ARBITER` — a single model id
///
/// Construct with [`PanelDefaults::from_env`] at server startup.
#[derive(Clone, Debug, Default)]
pub struct PanelDefaults {
    /// Default panel members when the request does not specify `tt_extras.panel.members`.
    pub members: Vec<ModelRef>,
    /// Default arbiter model when the request does not specify
    /// `tt_extras.panel.arbiter_model`.
    pub arbiter_model: ModelRef,
}

impl PanelDefaults {
    /// Build [`PanelDefaults`] from environment variables.
    ///
    /// - `TT_PANEL_DEFAULT_MEMBERS`: comma-separated model ids
    ///   (e.g. `"gpt-4o,claude-3-5-sonnet"`). Absent → empty list.
    /// - `TT_PANEL_DEFAULT_ARBITER`: a single model id used as the arbiter.
    ///   Absent → `""` (will cause `resolve` to fail unless extras provide one).
    pub fn from_env() -> Self {
        let members: Vec<ModelRef> = std::env::var("TT_PANEL_DEFAULT_MEMBERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|model| ModelRef {
                model,
                provider: None,
            })
            .collect();

        let arbiter_model = ModelRef {
            model: std::env::var("TT_PANEL_DEFAULT_ARBITER").unwrap_or_default(),
            provider: None,
        };

        PanelDefaults {
            members,
            arbiter_model,
        }
    }
}

// ---------------------------------------------------------------------------
// Entitlement rank
// ---------------------------------------------------------------------------

/// Entitlement rank for the panel min-tier gate (Free < Pro < Team < Scale).
/// Panel-local (not a global CallerTier Ord — Pro/Team share a TTL band).
pub(crate) fn panel_tier_rank(t: tt_shared::CallerTier) -> u8 {
    use tt_shared::CallerTier::*;
    match t {
        Free => 0,
        Pro => 1,
        Team => 2,
        Scale => 3,
    }
}

// ---------------------------------------------------------------------------
// Header parser
// ---------------------------------------------------------------------------

/// Parse `X-TokenTrimmer-Panel` into an [`ArbiterStrategyKind`].
///
/// Returns `None` when the header is absent **or** when the value is not a
/// recognized strategy — treat both as "no panel requested". The caller should
/// never return an error for an unknown strategy value (off-by-default contract).
pub fn panel_from_header(headers: &HeaderMap) -> Option<ArbiterStrategyKind> {
    headers
        .get("x-tokentrimmer-panel")
        .and_then(|v| v.to_str().ok())
        .and_then(ArbiterStrategyKind::parse)
}

// ---------------------------------------------------------------------------
// LegRole — panel member vs. arbiter call
// ---------------------------------------------------------------------------

/// Whether a dispatched leg is a panel member or the arbiter synthesis call.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // consumed by Tasks 4–7 (not yet wired)
pub enum LegRole {
    /// A regular panel member leg (fan-out dispatch).
    Leg,
    /// The arbiter model call (synthesis / best-of-N / majority).
    Arbiter,
}

// ---------------------------------------------------------------------------
// LegStatus — outcome of a single leg dispatch
// ---------------------------------------------------------------------------

/// Terminal status of a single dispatched leg.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // consumed by Tasks 4–7 (not yet wired)
pub enum LegStatus {
    /// Leg completed successfully.
    Ok,
    /// Leg returned an upstream error.
    Error,
    /// Leg exceeded its deadline.
    Timeout,
    /// Leg was skipped because no provider credential was available.
    SkippedNoCred,
}

impl LegStatus {
    /// Wire-format string for metrics / log attributes.
    #[allow(dead_code)] // used by Tasks 4–7 (not yet wired)
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::SkippedNoCred => "skipped_no_cred",
        }
    }
}

// ---------------------------------------------------------------------------
// LegResult — full result record for one dispatched leg
// ---------------------------------------------------------------------------

/// Full result record for one dispatched leg (member or arbiter).
#[derive(Debug)]
#[allow(dead_code)] // fields consumed by Tasks 4–7 (not yet wired)
pub struct LegResult {
    /// Internal zero-based index into [`PanelConfig::members`]; `usize::MAX`
    /// is used only internally for the arbiter (which has no member index).
    /// [`panel_body_json`] remaps every outgoing leg to a bounded wire index.
    pub leg_index: usize,
    /// Whether this is a regular member leg or the arbiter call.
    pub role: LegRole,
    /// The model id that was dispatched.
    pub model: String,
    /// The provider name used for dispatch.
    pub provider: String,
    /// Terminal outcome of this leg.
    pub status: LegStatus,
    /// The upstream response; `None` when `status != Ok`.
    ///
    /// For the [`LegRole::Arbiter`] leg this is always `None` — the arbiter's
    /// answer is returned in [`PanelResult::response`] instead.
    pub response: Option<ChatCompletionResponse>,
    /// Cost in USD for this leg; `None` when the model has no catalog pricing.
    pub cost_usd: Option<f64>,
    /// Token usage reported by the provider; `None` when `status != Ok`.
    pub usage: Option<Usage>,
    /// Wall-clock latency of the leg dispatch in milliseconds.
    pub latency_ms: u64,
}

// ---------------------------------------------------------------------------
// ArbiterDetail — per-strategy metadata surfaced in the response body
// ---------------------------------------------------------------------------

/// Strategy-specific metadata produced during arbitration.
///
/// All fields are `None`/`false` for the [`Synthesize`] strategy.
/// [`BestOfN`] and [`Majority`] (Tasks 2/3) fill in the relevant fields.
#[derive(Default, Clone, Debug)]
pub struct ArbiterDetail {
    /// `best-of-n`: internal member `leg_index` of the chosen answer.
    /// [`panel_body_json`] remaps it to the emitted wire-index space.
    pub chosen_leg: Option<usize>,
    /// `best-of-n`: judge's one-line reason for the choice.
    pub reason: Option<String>,
    /// `best-of-n`: `true` when the judge response was unparseable and we fell
    /// back to the first surviving leg.
    pub fell_back: bool,
    /// `majority`: number of legs in the winning cluster.
    pub winning_cluster_size: Option<usize>,
    /// `majority`: total number of distinct clusters found.
    pub total_clusters: Option<usize>,
    /// `majority`: `true` when every answer was distinct (no majority found).
    pub no_majority: bool,
    /// `majority`: `true` when embedding failed and we fell back to first leg.
    pub degraded: bool,
}

// ---------------------------------------------------------------------------
// ArbiterOutcome — the response produced by the arbiter
// ---------------------------------------------------------------------------

/// The final response produced by an [`ArbiterStrategy`].
pub struct ArbiterOutcome {
    /// The synthesized / chosen upstream response.
    pub response: ChatCompletionResponse,
    /// Cost of the arbiter call itself; `None` when unpriced (see
    /// [`crate::measurement::MeasuredDispatch::cost_usd`]).
    pub cost_usd: Option<f64>,
    /// Strategy-specific metadata (all `None`/`false` for [`Synthesize`]).
    pub detail: ArbiterDetail,
}

// ---------------------------------------------------------------------------
// ArbiterCostPlan — no-double-count guard for streaming arbiter cost
// ---------------------------------------------------------------------------

/// How the aggregate billing path obtains the arbiter's cost at stream-end.
///
/// Streaming panels defer the single `request_logs` row to the `DropGuard`,
/// which sees the *streamed* answer's accumulated usage. For `Synthesize`
/// the streamed tokens are fresh arbiter work and must be priced (`Live`).
/// For `BestOfN`/`Majority` the streamed tokens are a **replay of a member
/// leg's answer already counted in `Σ legs`** — repricing them would
/// double-count, so the cost is fixed up front and the streamed figure is
/// discarded (`Known`). See spec §5.4 (invariant 3).
#[derive(Clone, Debug)]
pub enum ArbiterCostPlan {
    /// Price the streamed answer's accumulated usage (Synthesize live arbiter).
    Live,
    /// Use this pre-computed cost; ignore the streamed usage (replay strategies).
    Known(Option<f64>),
}

impl ArbiterCostPlan {
    /// Resolve the arbiter's contribution to the aggregate.
    ///
    /// `streamed_arbiter_cost_usd` is the cost the `DropGuard` computed from
    /// the streamed answer's accumulated usage. `Known` discards it.
    pub fn finalize(&self, streamed_arbiter_cost_usd: Option<f64>) -> Option<f64> {
        match self {
            ArbiterCostPlan::Live => streamed_arbiter_cost_usd,
            ArbiterCostPlan::Known(c) => *c,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Extract the text answers from all successful member legs, in slice order.
///
/// Returns `(position_in_legs_slice, answer_text)` pairs so callers can trace
/// back to the original [`LegResult`] via `legs[pos]`.  Only legs with
/// `status == Ok` and `role == Leg` that carry a [`MessageContent::Text`]
/// assistant message are included.
pub fn surviving_answers(legs: &[LegResult]) -> Vec<(usize, String)> {
    legs.iter()
        .enumerate()
        .filter(|(_, l)| l.status == LegStatus::Ok && l.role == LegRole::Leg)
        .filter_map(|(pos, l)| {
            let resp = l.response.as_ref()?;
            let text = resp.choices.first().and_then(|c| match &c.message {
                Message::Assistant {
                    content: Some(MessageContent::Text(t)),
                    ..
                } => Some(t.clone()),
                _ => None,
            })?;
            Some((pos, text))
        })
        .collect()
}

/// Cosine similarity between two vectors.
///
/// Returns `0.0` when either vector has zero norm (avoids division by zero).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

// ---------------------------------------------------------------------------
// ArbiterStrategy — trait
// ---------------------------------------------------------------------------

/// Arbitration algorithm executed after all panel legs have been dispatched.
#[async_trait]
pub trait ArbiterStrategy {
    /// Run arbitration over the completed `legs` and return the final answer.
    ///
    /// `request` is the original caller request (used as the user prompt
    /// context). `state` and `ctx` give access to the provider registry and
    /// the request's credential / deadline context. `creds` is the same
    /// provider-id → credential map passed to `run_panel`; an LLM arbiter
    /// requires its own explicit map entry and never inherits
    /// `ctx.credentials` as a fallback.
    async fn arbitrate(
        &self,
        request: &ChatCompletionRequest,
        legs: &[LegResult],
        state: &AppState,
        ctx: &RequestContext,
        creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) -> Result<ArbiterOutcome, ApiError>;

    /// Streaming variant. Default impl runs the buffered `arbitrate` then replays
    /// the chosen response as a chunk stream (`Known` cost — the replayed tokens
    /// are already a member leg's, counted in Σ legs; see ArbiterCostPlan). Live
    /// strategies (Synthesize) override this. (spec §5.3)
    async fn arbitrate_streaming(
        &self,
        request: &ChatCompletionRequest,
        legs: &[LegResult],
        state: &AppState,
        ctx: &RequestContext,
        creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) -> Result<
        (
            BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
            ArbiterCostPlan,
            ArbiterDetail,
        ),
        ApiError,
    > {
        let outcome = self.arbitrate(request, legs, state, ctx, creds).await?;
        Ok((
            crate::routes::sse::fake_stream_from_response(outcome.response),
            ArbiterCostPlan::Known(outcome.cost_usd),
            outcome.detail,
        ))
    }
}

/// Build an arbiter request context from the credential explicitly resolved
/// for that provider. Arbiter dispatch must never inherit the source provider's
/// credential when the map is incomplete.
fn panel_context_for_provider(
    ctx: &RequestContext,
    provider_id: &str,
    creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
) -> Result<RequestContext, ApiError> {
    let credentials =
        creds
            .get(provider_id)
            .cloned()
            .ok_or_else(|| ApiError::MissingProviderCredential {
                provider: provider_id.to_string(),
            })?;
    Ok(RequestContext {
        credentials,
        ..ctx.clone()
    })
}

// ---------------------------------------------------------------------------
// Synthesize — the only currently-implemented strategy
// ---------------------------------------------------------------------------

/// The **Synthesize** strategy: collect all successful leg answers, build a
/// single prompt asking the arbiter model to synthesize one best answer, then
/// dispatch exactly one [`crate::measurement::measured_single_dispatch`] call.
pub struct Synthesize {
    /// The arbiter model (and optional provider pin) to use for synthesis.
    pub arbiter_model: ModelRef,
}

#[async_trait]
impl ArbiterStrategy for Synthesize {
    async fn arbitrate(
        &self,
        request: &ChatCompletionRequest,
        legs: &[LegResult],
        state: &AppState,
        ctx: &RequestContext,
        creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) -> Result<ArbiterOutcome, ApiError> {
        // Collect the text content of all successful legs.
        let ok_answers = surviving_answers(legs);

        // Defensive guard: Task 5 enforces quorum upstream, but arbitrate must
        // not dispatch with an empty candidate set.
        if ok_answers.is_empty() {
            return Err(ApiError::InvalidRequest(
                "panel: no successful legs to synthesize".into(),
            ));
        }

        let n = ok_answers.len();

        // Build the arbiter synthesis instruction.
        let synthesis_instruction = format!(
            "You are an expert synthesis engine. You have received {n} candidate \
             answers from different AI models responding to the same user request. \
             Your task is to synthesize them into one single best answer. \
             Combine the strongest insights, resolve any contradictions by preferring \
             the most accurate information, and produce a clear, complete, and \
             well-structured response. Output only the synthesized answer — no \
             preamble, no meta-commentary about the synthesis process."
        );

        // Preserve the caller's original system message(s) — they may carry
        // safety instructions or persona context the arbiter must respect.
        // Append the synthesis instruction as an additional system turn after them.
        let mut messages = request.messages.clone();
        messages.push(Message::System {
            content: MessageContent::Text(synthesis_instruction),
        });
        for (i, (_, answer)) in ok_answers.iter().enumerate() {
            messages.push(Message::User {
                content: MessageContent::Text(format!(
                    "Candidate answer {} of {}:\n\n{}",
                    i + 1,
                    n,
                    answer
                )),
                name: None,
            });
        }

        let arbiter_req = ChatCompletionRequest {
            model: self.arbiter_model.model.clone(),
            messages,
            // Arbitration is always non-streaming: we need the full synthesized
            // answer before we can return a response to the caller.
            stream: false,
            max_tokens: Some(4096),
            ..Default::default()
        };

        // Resolve the arbiter provider.
        let provider = state
            .registry
            .resolve(&self.arbiter_model.model)
            .ok_or_else(|| ApiError::ModelNotFound {
                model: self.arbiter_model.model.clone(),
            })?;

        let arb_ctx = panel_context_for_provider(ctx, provider.id(), creds)?;

        // Derive the arbiter deadline from the caller's remaining budget when
        // available; otherwise use a bounded default. The outer route
        // TimeoutLayer (60 s) caps all requests regardless.
        let deadline = arb_ctx.deadline.unwrap_or(Duration::from_secs(120));
        let measured = crate::measurement::measured_single_dispatch(
            &provider,
            arbiter_req,
            &arb_ctx,
            deadline,
        )
        .await
        .map_err(|e| ApiError::ServiceUnavailable(format!("arbiter dispatch failed: {e}")))?;

        Ok(ArbiterOutcome {
            response: measured.response,
            cost_usd: measured.cost_usd,
            detail: ArbiterDetail::default(),
        })
    }

    async fn arbitrate_streaming(
        &self,
        request: &ChatCompletionRequest,
        legs: &[LegResult],
        state: &AppState,
        ctx: &RequestContext,
        creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) -> Result<
        (
            BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
            ArbiterCostPlan,
            ArbiterDetail,
        ),
        ApiError,
    > {
        // Collect the text content of all successful legs.
        let ok_answers = surviving_answers(legs);

        // Defensive guard: no successful legs → error.
        if ok_answers.is_empty() {
            return Err(ApiError::InvalidRequest(
                "panel: no successful legs to synthesize".into(),
            ));
        }

        let n = ok_answers.len();

        // Build the arbiter synthesis instruction (verbatim from `arbitrate`).
        let synthesis_instruction = format!(
            "You are an expert synthesis engine. You have received {n} candidate \
             answers from different AI models responding to the same user request. \
             Your task is to synthesize them into one single best answer. \
             Combine the strongest insights, resolve any contradictions by preferring \
             the most accurate information, and produce a clear, complete, and \
             well-structured response. Output only the synthesized answer — no \
             preamble, no meta-commentary about the synthesis process."
        );

        // Preserve caller system messages; append synthesis instruction + candidates.
        let mut messages = request.messages.clone();
        messages.push(Message::System {
            content: MessageContent::Text(synthesis_instruction),
        });
        for (i, (_, answer)) in ok_answers.iter().enumerate() {
            messages.push(Message::User {
                content: MessageContent::Text(format!(
                    "Candidate answer {} of {}:\n\n{}",
                    i + 1,
                    n,
                    answer
                )),
                name: None,
            });
        }

        let arbiter_req = ChatCompletionRequest {
            model: self.arbiter_model.model.clone(),
            messages,
            stream: true,
            max_tokens: Some(4096),
            ..Default::default()
        };

        // Resolve the arbiter provider.
        let provider = state
            .registry
            .resolve(&self.arbiter_model.model)
            .ok_or_else(|| ApiError::ModelNotFound {
                model: self.arbiter_model.model.clone(),
            })?;

        let arb_ctx = panel_context_for_provider(ctx, provider.id(), creds)?;

        let deadline = arb_ctx.deadline.unwrap_or(Duration::from_secs(120));
        let stream = tokio::time::timeout(
            deadline,
            provider.chat_completion_stream(arbiter_req, &arb_ctx),
        )
        .await
        .map_err(|_| ApiError::ServiceUnavailable("arbiter stream establishment timed out".into()))?
        .map_err(|e| ApiError::ServiceUnavailable(format!("arbiter stream failed: {e}")))?;

        Ok((stream, ArbiterCostPlan::Live, ArbiterDetail::default()))
    }
}

// ---------------------------------------------------------------------------
// BestOfN — single-pass LLM judge, returns chosen leg verbatim
// ---------------------------------------------------------------------------

/// The **BestOfN** strategy: ask the arbiter model to pick the single best
/// candidate answer by number, then return that leg's original response
/// verbatim — no paraphrasing or synthesis.
pub struct BestOfN {
    /// The arbiter model (and optional provider pin) used as the judge.
    pub arbiter_model: ModelRef,
}

#[async_trait]
impl ArbiterStrategy for BestOfN {
    async fn arbitrate(
        &self,
        request: &ChatCompletionRequest,
        legs: &[LegResult],
        state: &AppState,
        ctx: &RequestContext,
        creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) -> Result<ArbiterOutcome, ApiError> {
        // Collect the text content of all successful legs.
        let answers = surviving_answers(legs);

        // Defensive guard: no successful legs → error.
        if answers.is_empty() {
            return Err(ApiError::InvalidRequest("panel: no successful legs".into()));
        }

        // Single survivor — no judge call needed; return it directly.
        if answers.len() == 1 {
            let pos = answers[0].0;
            return Ok(ArbiterOutcome {
                response: legs[pos].response.clone().expect("Ok leg has response"),
                cost_usd: None,
                detail: ArbiterDetail {
                    chosen_leg: Some(legs[pos].leg_index),
                    ..Default::default()
                },
            });
        }

        let n = answers.len();

        // Build the judge prompt.
        // Preserve caller system messages, then append the judge instruction,
        // then push numbered candidate messages (mirrors Synthesize's loop).
        let judge_instruction = format!(
            "You are selecting the single best of the candidate answers below. \
             On the FIRST line reply with ONLY the candidate number (1 to {n}). \
             On the next line, give one sentence explaining why."
        );

        let mut messages = request.messages.clone();
        messages.push(Message::System {
            content: MessageContent::Text(judge_instruction),
        });
        for (i, (_, answer)) in answers.iter().enumerate() {
            messages.push(Message::User {
                content: MessageContent::Text(format!(
                    "Candidate {} of {}:\n\n{}",
                    i + 1,
                    n,
                    answer
                )),
                name: None,
            });
        }

        let arbiter_req = ChatCompletionRequest {
            model: self.arbiter_model.model.clone(),
            messages,
            stream: false,
            max_tokens: Some(512),
            ..Default::default()
        };

        // Resolve the arbiter provider.
        let provider = state
            .registry
            .resolve(&self.arbiter_model.model)
            .ok_or_else(|| ApiError::ModelNotFound {
                model: self.arbiter_model.model.clone(),
            })?;

        let arb_ctx = panel_context_for_provider(ctx, provider.id(), creds)?;

        // Derive the arbiter deadline from the caller's remaining budget when
        // available; otherwise use a bounded default. The outer route
        // TimeoutLayer (60 s) caps all requests regardless.
        let deadline = arb_ctx.deadline.unwrap_or(Duration::from_secs(120));
        let measured = crate::measurement::measured_single_dispatch(
            &provider,
            arbiter_req,
            &arb_ctx,
            deadline,
        )
        .await
        .map_err(|e| ApiError::ServiceUnavailable(format!("arbiter dispatch failed: {e}")))?;

        // Extract the judge's assistant text.
        let judge_text = measured
            .response
            .choices
            .first()
            .and_then(|c| match &c.message {
                tt_shared::messages::Message::Assistant {
                    content: Some(tt_shared::messages::MessageContent::Text(t)),
                    ..
                } => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_default();

        // Parse: first integer token on the first line → candidate number.
        let first_line = judge_text.lines().next().unwrap_or("").trim();
        let parsed: Option<usize> = first_line
            .split_whitespace()
            .find_map(|tok| tok.parse::<usize>().ok());

        let (chosen, fell_back) = match parsed {
            Some(p) if p >= 1 && p <= answers.len() => (answers[p - 1].0, false),
            _ => (answers[0].0, true),
        };

        // reason = trimmed text after the first line, None if empty.
        let reason = {
            let after_first = judge_text
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            if after_first.is_empty() {
                None
            } else {
                Some(after_first)
            }
        };

        Ok(ArbiterOutcome {
            response: legs[chosen].response.clone().expect("Ok leg has response"),
            cost_usd: measured.cost_usd,
            detail: ArbiterDetail {
                chosen_leg: Some(legs[chosen].leg_index),
                reason,
                fell_back,
                ..Default::default()
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Majority — embedding clustering, returns medoid leg verbatim
// ---------------------------------------------------------------------------

/// The **Majority** strategy: embed all surviving leg answers, cluster them by
/// cosine similarity, pick the largest cluster (tie-break: earliest), and return
/// the cluster's medoid response verbatim — no paraphrasing or synthesis.
///
/// When embedding fails, falls back to the first surviving leg and sets
/// `detail.degraded = true`. When no cluster has more than one member
/// (all answers are distinct), the global medoid (highest mean cosine to all
/// others) is returned and `detail.no_majority = true`.
///
/// The clustering threshold defaults to `0.83` and can be overridden via the
/// `TT_PANEL_MAJORITY_THRESHOLD` environment variable (float in `(0.0, 1.0]`).
pub struct Majority;

#[async_trait]
impl ArbiterStrategy for Majority {
    async fn arbitrate(
        &self,
        _request: &ChatCompletionRequest,
        legs: &[LegResult],
        state: &AppState,
        _ctx: &RequestContext,
        _creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) -> Result<ArbiterOutcome, ApiError> {
        let answers = surviving_answers(legs);

        // Empty case — no surviving legs.
        if answers.is_empty() {
            return Err(ApiError::InvalidRequest("panel: no successful legs".into()));
        }

        // Single answer — no clustering needed.
        if answers.len() == 1 {
            return Ok(ArbiterOutcome {
                response: legs[answers[0].0]
                    .response
                    .clone()
                    .expect("surviving_answers guarantees response"),
                cost_usd: None,
                detail: ArbiterDetail {
                    winning_cluster_size: Some(1),
                    total_clusters: Some(1),
                    ..Default::default()
                },
            });
        }

        // Resolve the embedder from the L2 config.
        let embedder = match state.l2.as_ref().map(|l| &l.embedder) {
            Some(e) => e,
            None => {
                // No embedder wired → degrade to first leg.
                return Ok(ArbiterOutcome {
                    response: legs[answers[0].0]
                        .response
                        .clone()
                        .expect("surviving_answers guarantees response"),
                    cost_usd: None,
                    detail: ArbiterDetail {
                        degraded: true,
                        ..Default::default()
                    },
                });
            }
        };

        // Embed all answers; return on first error (degrade to first leg).
        let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(answers.len());
        for (_, text) in &answers {
            match embedder.embed(text).await {
                Ok(v) => vecs.push(v),
                Err(_) => {
                    return Ok(ArbiterOutcome {
                        response: legs[answers[0].0]
                            .response
                            .clone()
                            .expect("surviving_answers guarantees response"),
                        cost_usd: None,
                        detail: ArbiterDetail {
                            degraded: true,
                            ..Default::default()
                        },
                    });
                }
            }
        }

        // Threshold (env-overridable, default 0.83).
        let t = std::env::var("TT_PANEL_MAJORITY_THRESHOLD")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|x| *x > 0.0 && *x <= 1.0)
            .unwrap_or(0.83);

        // Greedy clustering: assign each answer to the first cluster whose
        // representative (first member) has cosine ≥ t with this answer.
        let mut clusters: Vec<Vec<usize>> = Vec::new();
        for k in 0..answers.len() {
            let mut assigned = false;
            for cluster in &mut clusters {
                let rep = cluster[0];
                if cosine(&vecs[k], &vecs[rep]) >= t {
                    cluster.push(k);
                    assigned = true;
                    break;
                }
            }
            if !assigned {
                clusters.push(vec![k]);
            }
        }

        // Winner = largest cluster; tie-break by earliest first-element index
        // (smallest cluster-creation order = stable, deterministic).
        let winner_idx = clusters
            .iter()
            .enumerate()
            .max_by(|(ai, a), (bi, b)| {
                // Larger len wins; on tie prefer smaller cluster index (earlier).
                a.len().cmp(&b.len()).then_with(|| bi.cmp(ai))
            })
            .map(|(i, _)| i)
            .expect("clusters is non-empty (answers.len() >= 2)");

        let winner = &clusters[winner_idx];
        let no_majority = winner.len() == 1;

        // Medoid selection:
        // - no_majority → global medoid (highest mean cosine to ALL others)
        // - majority    → winner medoid (highest mean cosine to other WINNER members)
        let medoid = if no_majority {
            // Global medoid: answer with highest mean cosine to all others.
            (0..answers.len())
                .max_by(|&a, &b| {
                    let mean_cos = |idx: usize| -> f32 {
                        let others: Vec<f32> = (0..answers.len())
                            .filter(|&j| j != idx)
                            .map(|j| cosine(&vecs[idx], &vecs[j]))
                            .collect();
                        if others.is_empty() {
                            0.0
                        } else {
                            others.iter().sum::<f32>() / others.len() as f32
                        }
                    };
                    mean_cos(a)
                        .partial_cmp(&mean_cos(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(0)
        } else if winner.len() == 1 {
            winner[0]
        } else {
            // Winner medoid: highest mean cosine to other winner members.
            *winner
                .iter()
                .max_by(|&&a, &&b| {
                    let mean_cos = |idx: usize| -> f32 {
                        let others: Vec<f32> = winner
                            .iter()
                            .filter(|&&j| j != idx)
                            .map(|&j| cosine(&vecs[idx], &vecs[j]))
                            .collect();
                        if others.is_empty() {
                            0.0
                        } else {
                            others.iter().sum::<f32>() / others.len() as f32
                        }
                    };
                    mean_cos(a)
                        .partial_cmp(&mean_cos(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(&winner[0])
        };

        Ok(ArbiterOutcome {
            response: legs[answers[medoid].0]
                .response
                .clone()
                .expect("surviving_answers guarantees response"),
            cost_usd: None,
            detail: ArbiterDetail {
                winning_cluster_size: Some(winner.len()),
                total_clusters: Some(clusters.len()),
                no_majority,
                ..Default::default()
            },
        })
    }
}

// ---------------------------------------------------------------------------
// strategy_for — factory
// ---------------------------------------------------------------------------

/// Instantiate the correct [`ArbiterStrategy`] for the given [`PanelConfig`].
pub fn strategy_for(cfg: &PanelConfig) -> Result<Box<dyn ArbiterStrategy + Send + Sync>, ApiError> {
    match cfg.strategy {
        ArbiterStrategyKind::Synthesize => Ok(Box::new(Synthesize {
            arbiter_model: cfg.arbiter_model.clone(),
        })),
        ArbiterStrategyKind::BestOfN => Ok(Box::new(BestOfN {
            arbiter_model: cfg.arbiter_model.clone(),
        })),
        ArbiterStrategyKind::Majority => Ok(Box::new(Majority)),
    }
}

// ---------------------------------------------------------------------------
// Budget estimation + gate
// ---------------------------------------------------------------------------

/// Static inputs to Fusion's pre-dispatch admission estimate.
///
/// This deliberately carries both OpenAI-shaped output caps: provider adapters
/// give `max_completion_tokens` precedence when both are supplied, so a budget
/// estimate that looked only at legacy `max_tokens` could accept work whose
/// known member output allowance is materially larger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelAdmissionEstimate {
    /// Whole original-prompt input estimate, before member fan-out.
    pub input_tokens: u32,
    /// Legacy output cap, if the caller supplied one.
    pub max_tokens: Option<u32>,
    /// Newer output cap, authoritative when both caps are supplied.
    pub max_completion_tokens: Option<u32>,
    /// Number of completions requested per member. `None` means one.
    pub n: Option<u32>,
}

/// Opaque proof that a panel request passed Fusion's static pre-dispatch
/// admission gate.
///
/// Obtain one with [`admit_panel_request`] and pass it to [`run_panel`]. Its
/// fields are private deliberately: a direct Rust caller cannot construct a
/// proof and skip the same fail-closed budget estimate used by the HTTP route.
/// `run_panel` revalidates the proof against the current request and config
/// immediately before fan-out, so a proof cannot be reused to bypass a later
/// request/config change. This remains admission only, not a reservation or a
/// runtime spending ceiling.
#[must_use = "pass the admission proof to run_panel before dispatching a panel"]
pub struct PanelAdmission {
    /// Header-style request budget, retained so revalidation preserves header
    /// precedence over `PanelConfig::max_cost_usd`.
    request_budget: Option<f64>,
    /// The source-provider tokenizer selected at ingress. This is private so
    /// direct callers cannot choose a cheaper tokenizer estimate after the
    /// proof has been minted (and lets a pinned HTTP provider stay exact).
    tokenizer_provider_id: String,
}

impl PanelAdmissionEstimate {
    /// Return the cap that provider adapters will use for member dispatches.
    ///
    /// A static budget gate cannot prove a bound when the provider is allowed
    /// to choose its own output limit, so callers must set one of these
    /// fields. `max_completion_tokens` remains authoritative when both are
    /// present, matching the provider adapters.
    fn member_output_tokens(self) -> Option<u32> {
        self.max_completion_tokens.or(self.max_tokens)
    }

    fn member_choice_count(self) -> Option<u32> {
        let count = self.n.unwrap_or(1);
        (count > 0).then_some(count)
    }
}

/// Synthesize appends this instruction plus one labelled user message per
/// member answer. Keep a fixed slack allowance for those tokens in the static
/// plan rather than pretending the arbiter receives only the original prompt.
const ARBITER_PROMPT_OVERHEAD_TOKENS: u32 = 512;
/// The concrete Synthesize request always asks for this output allowance.
const SYNTHESIZE_ARBITER_OUTPUT_TOKENS: u32 = 4_096;
/// The concrete BestOfN judge request always asks for this output allowance.
const BEST_OF_N_ARBITER_OUTPUT_TOKENS: u32 = 512;

fn estimate_model_dispatch(
    state: &AppState,
    model: &ModelRef,
    input_tokens: u32,
    output_tokens_per_choice: u32,
    choice_count: u32,
) -> Option<f64> {
    let provider = state.registry.resolve(&model.model)?; // unknown model → fail-closed
    let pricing = provider.pricing(&model.model)?; // unpriceable → fail-closed
    let total_output_tokens = output_tokens_per_choice.checked_mul(choice_count)?;
    // A malformed dynamic catalog entry or provider fee must never turn the
    // `est > ceiling` comparison into false through NaN, infinity, or a
    // negative figure. Normal callers receive validated catalog data, but this
    // is an admission boundary and must fail closed for every Provider impl.
    let base =
        crate::routes::chat::estimate_cost_usd(&pricing, input_tokens, Some(total_output_tokens));
    let fee_multiplier = provider.fee_multiplier();
    let cost = base * fee_multiplier;
    (base.is_finite()
        && base >= 0.0
        && fee_multiplier.is_finite()
        && fee_multiplier >= 0.0
        && cost.is_finite()
        && cost >= 0.0)
        .then_some(cost)
}

fn arbiter_input_tokens(
    original_input_tokens: u32,
    member_count: usize,
    member_output_tokens: u32,
) -> Option<u32> {
    let member_count = u32::try_from(member_count).ok()?;
    let candidate_tokens = member_output_tokens.checked_mul(member_count)?;
    original_input_tokens
        .checked_add(candidate_tokens)?
        .checked_add(ARBITER_PROMPT_OVERHEAD_TOKENS)
}

/// Per-leg view of Fusion's known static dispatch-cost plan.
///
/// This is intentionally crate-private: HTTP preview can expose the same
/// plan that admission uses without turning the dry-run into an admission
/// proof. A `None` component means the corresponding known work cannot be
/// priced safely; `total_cost_usd` is then also `None`.
#[derive(Debug)]
pub(crate) struct PanelCostBreakdown {
    pub member_costs: Vec<Option<f64>>,
    pub arbiter_cost: Option<f64>,
    pub total_cost_usd: Option<f64>,
}

/// Price each known component of Fusion's static dispatch plan.
///
/// This is the single implementation behind both [`estimate_panel_cost`] and
/// the side-effect-free preview endpoint. It prices every requested member
/// choice, then the worst-case known arbiter fan-in/output shape. It does not
/// perform config admission, credential lookup, provider-health checks,
/// reservations, or dispatch.
pub(crate) fn estimate_panel_cost_breakdown(
    state: &AppState,
    cfg: &PanelConfig,
    estimate: PanelAdmissionEstimate,
) -> PanelCostBreakdown {
    let Some(member_output_tokens) = estimate.member_output_tokens() else {
        return PanelCostBreakdown {
            member_costs: vec![None; cfg.members.len()],
            arbiter_cost: None,
            total_cost_usd: None,
        };
    };
    let Some(member_choice_count) = estimate.member_choice_count() else {
        return PanelCostBreakdown {
            member_costs: vec![None; cfg.members.len()],
            arbiter_cost: None,
            total_cost_usd: None,
        };
    };

    let member_costs: Vec<Option<f64>> = cfg
        .members
        .iter()
        .map(|member| {
            estimate_model_dispatch(
                state,
                member,
                estimate.input_tokens,
                member_output_tokens,
                member_choice_count,
            )
        })
        .collect();

    let arbiter_cost = match cfg.strategy {
        ArbiterStrategyKind::Synthesize => arbiter_input_tokens(
            estimate.input_tokens,
            cfg.members.len(),
            member_output_tokens,
        )
        .and_then(|input_tokens| {
            estimate_model_dispatch(
                state,
                &cfg.arbiter_model,
                input_tokens,
                SYNTHESIZE_ARBITER_OUTPUT_TOKENS,
                1,
            )
        }),
        ArbiterStrategyKind::BestOfN => arbiter_input_tokens(
            estimate.input_tokens,
            cfg.members.len(),
            member_output_tokens,
        )
        .and_then(|input_tokens| {
            estimate_model_dispatch(
                state,
                &cfg.arbiter_model,
                input_tokens,
                BEST_OF_N_ARBITER_OUTPUT_TOKENS,
                1,
            )
        }),
        // Majority performs an embedding pass whose provider/model and token
        // pricing are not represented by PanelConfig. Do not use the unused
        // LLM arbiter field as a proxy: it could undercount the real work.
        ArbiterStrategyKind::Majority => None,
    };

    let total_cost_usd = member_costs
        .iter()
        .copied()
        .chain(std::iter::once(arbiter_cost))
        .try_fold(0.0_f64, |total, cost| {
            let total = total + cost?;
            (total.is_finite() && total >= 0.0).then_some(total)
        });

    PanelCostBreakdown {
        member_costs,
        arbiter_cost,
        total_cost_usd,
    }
}

/// Estimated total cost for the known Fusion dispatch plan, in USD.
///
/// Every configured member is included because credentials are resolved after
/// admission and any of them can dispatch. Member output is priced at the
/// effective caller cap (`max_completion_tokens` first) for each requested
/// completion. Arbiter-backed strategies also include the worst-case prompt
/// fan-in of one capped candidate answer per member and their fixed arbiter
/// output caps. This remains a static admission estimate, not a reservation or
/// runtime spending ceiling.
///
/// Returns `None` (fail-closed) if a configured model is unknown/unpriceable,
/// the request has no explicit effective output cap, `n` is zero, arithmetic
/// overflows, or the strategy requires work with no pricing contract (currently
/// Majority's embedding pass).
pub fn estimate_panel_cost(
    state: &AppState,
    cfg: &PanelConfig,
    estimate: PanelAdmissionEstimate,
) -> Option<f64> {
    estimate_panel_cost_breakdown(state, cfg, estimate).total_cost_usd
}

/// Gate: reject a panel request when its static plan exceeds the configured
/// pre-dispatch admission budget. This is not a reservation or runtime-spend
/// check.
///
/// `ceiling` takes precedence over `cfg.max_cost_usd`. If neither is set, the
/// request is **always** rejected — a panel requires an explicit budget.
///
/// Returns `Err(ApiError::CostLimitExceeded)` when:
/// - neither `ceiling` nor `cfg.max_cost_usd` is set (no budget)
/// - the estimate is `None` (unpriceable / unknown model — fail-closed)
/// - the estimate exceeds the effective admission budget
pub fn panel_budget_gate(
    state: &AppState,
    cfg: &PanelConfig,
    estimate: PanelAdmissionEstimate,
    ceiling: Option<f64>,
) -> Result<(), ApiError> {
    let ceiling = match ceiling.or(cfg.max_cost_usd) {
        Some(value) if value.is_finite() && value > 0.0 => value,
        // Header/config parsers normally reject these values before this seam,
        // but callers can construct PanelConfig directly. Do not let NaN or an
        // infinite ceiling bypass the comparison below.
        _ => {
            return Err(ApiError::CostLimitExceeded {
                estimated_usd: f64::INFINITY,
                ceiling_usd: 0.0,
            });
        }
    };
    let est = estimate_panel_cost(state, cfg, estimate).ok_or(ApiError::CostLimitExceeded {
        estimated_usd: f64::INFINITY,
        ceiling_usd: ceiling,
    })?;
    if est > ceiling {
        return Err(ApiError::CostLimitExceeded {
            estimated_usd: est,
            ceiling_usd: ceiling,
        });
    }
    Ok(())
}

fn panel_admission_estimate(
    req: &ChatCompletionRequest,
    tokenizer_provider_id: &str,
) -> PanelAdmissionEstimate {
    let combined = tt_shared::message_text_for_estimation(req);
    PanelAdmissionEstimate {
        input_tokens: tt_tokenize::estimate_tokens(tokenizer_provider_id, &combined),
        max_tokens: req.max_tokens,
        max_completion_tokens: req.max_completion_tokens,
        n: req.n,
    }
}

fn validate_panel_admission(
    state: &AppState,
    cfg: &PanelConfig,
    req: &ChatCompletionRequest,
    tokenizer_provider_id: &str,
    request_budget: Option<f64>,
) -> Result<(), ApiError> {
    cfg.validate_for_dispatch()?;
    panel_budget_gate(
        state,
        cfg,
        panel_admission_estimate(req, tokenizer_provider_id),
        request_budget,
    )
}

impl PanelAdmission {
    /// Re-run the static gate against the work that is about to dispatch.
    ///
    /// This is intentionally private to the panel engine: callers receive the
    /// proof only to satisfy [`run_panel`]'s type-level admission requirement.
    fn revalidate(
        &self,
        state: &AppState,
        cfg: &PanelConfig,
        req: &ChatCompletionRequest,
    ) -> Result<(), ApiError> {
        validate_panel_admission(
            state,
            cfg,
            req,
            &self.tokenizer_provider_id,
            self.request_budget,
        )
    }
}

/// Admit a direct Fusion engine request through the same static gate used by
/// the HTTP route.
///
/// The request model must be registered so the engine can select its tokenizer
/// without letting a caller choose a less conservative estimate. Direct callers
/// should retain the returned [`PanelAdmission`] and provide it to
/// [`run_panel`].
pub fn admit_panel_request(
    state: &AppState,
    cfg: &PanelConfig,
    req: &ChatCompletionRequest,
    request_budget: Option<f64>,
) -> Result<PanelAdmission, ApiError> {
    let provider = state
        .registry
        .resolve(&req.model)
        .ok_or_else(|| ApiError::ModelNotFound {
            model: req.model.clone(),
        })?;
    admit_panel_request_with_tokenizer_provider(state, cfg, req, provider.id(), request_budget)
}

/// Internal ingress variant for a request whose selected source provider is
/// already known (including an explicit provider pin). Keeping this crate-only
/// prevents direct callers from choosing an arbitrary tokenizer for admission.
pub(crate) fn admit_panel_request_with_tokenizer_provider(
    state: &AppState,
    cfg: &PanelConfig,
    req: &ChatCompletionRequest,
    tokenizer_provider_id: &str,
    request_budget: Option<f64>,
) -> Result<PanelAdmission, ApiError> {
    validate_panel_admission(state, cfg, req, tokenizer_provider_id, request_budget)?;
    Ok(PanelAdmission {
        request_budget,
        tokenizer_provider_id: tokenizer_provider_id.to_string(),
    })
}

// ---------------------------------------------------------------------------
// PanelResult — the outcome of a completed run_panel call
// ---------------------------------------------------------------------------

/// The outcome of a completed [`run_panel`] call.
#[derive(Debug)]
pub struct PanelResult {
    /// The final synthesized / chosen response to return to the caller.
    pub response: ChatCompletionResponse,
    /// Full leg records for all dispatched members plus the arbiter.
    pub legs: Vec<LegResult>,
    /// None-aware sum of all leg costs (member legs + arbiter leg).
    /// `None` only when every leg returned `None` for cost.
    pub total_cost_usd: Option<f64>,
    /// Minimum number of member legs that had to succeed.
    pub quorum_required: usize,
    /// Number of member legs that actually succeeded.
    pub quorum_met: usize,
    /// Strategy-specific metadata from the arbiter (surfaced in the response body).
    pub arbiter_detail: ArbiterDetail,
}

// ---------------------------------------------------------------------------
// run_panel — concurrent fan-out, quorum, cost aggregation
// ---------------------------------------------------------------------------

/// Fold an iterator of `Option<f64>` values: accumulate all `Some` values;
/// return `None` only when every item was `None`.
pub(crate) fn sum_metered_iter(it: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut total: Option<f64> = None;
    for x in it.flatten() {
        total = Some(total.unwrap_or(0.0) + x);
    }
    total
}

/// Phases 1–2 of a panel run: fan out member legs concurrently, join them, and
/// enforce quorum. Returns the completed member legs plus the None-aware
/// **leg-only** cost sum. Shared by `run_panel` (non-streaming) and
/// `complete_panel_streaming` (Phase 5). The arbiter step lives in the callers.
async fn run_panel_legs_and_quorum(
    state: &crate::AppState,
    ctx: &tt_shared::RequestContext,
    base_req: &ChatCompletionRequest,
    creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    cfg: &PanelConfig,
    deadline: Duration,
) -> Result<(Vec<LegResult>, Option<f64>), crate::ApiError> {
    use std::time::Instant;
    use tokio::task::JoinSet;

    // 1. Resolve legs; skip members with no credential.
    let mut set: JoinSet<LegResult> = JoinSet::new();
    let mut legs_out: Vec<LegResult> = Vec::new();

    for (i, m) in cfg.members.iter().enumerate() {
        let provider =
            state
                .registry
                .resolve(&m.model)
                .ok_or_else(|| crate::ApiError::ModelNotFound {
                    model: m.model.clone(),
                })?;
        let pid = provider.id().to_string();

        if !creds.contains_key(&pid) {
            crate::metrics::record_panel_leg("leg", LegStatus::SkippedNoCred.as_str());
            legs_out.push(LegResult {
                leg_index: i,
                role: LegRole::Leg,
                model: m.model.clone(),
                provider: pid,
                status: LegStatus::SkippedNoCred,
                response: None,
                cost_usd: None,
                usage: None,
                latency_ms: 0,
            });
            continue;
        }

        // Build a per-leg request with the correct model.
        let mut req = base_req.clone();
        req.model = m.model.clone();

        // Substitute the provider-specific credential into the context.
        // pid is known-present in creds (checked above); clone defensively.
        let mut leg_ctx = ctx.clone();
        if let Some(c) = creds.get(&pid) {
            leg_ctx.credentials = c.clone();
        }

        let model_id = m.model.clone();
        set.spawn(async move {
            let started = Instant::now();
            match crate::measurement::measured_single_dispatch(&provider, req, &leg_ctx, deadline)
                .await
            {
                Ok(md) => LegResult {
                    leg_index: i,
                    role: LegRole::Leg,
                    model: md.response.model.clone(),
                    provider: pid,
                    status: LegStatus::Ok,
                    cost_usd: md.cost_usd,
                    usage: Some(md.response.usage.clone()),
                    latency_ms: started.elapsed().as_millis() as u64,
                    response: Some(md.response),
                },
                Err(_) => LegResult {
                    leg_index: i,
                    role: LegRole::Leg,
                    model: model_id,
                    provider: pid,
                    status: LegStatus::Error,
                    response: None,
                    cost_usd: None,
                    usage: None,
                    latency_ms: started.elapsed().as_millis() as u64,
                },
            }
        });
    }

    // Collect all completed member legs.
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(leg) => {
                crate::metrics::record_panel_leg("leg", leg.status.as_str());
                legs_out.push(leg);
            }
            Err(e) => {
                // A spawned task panicked (or was cancelled). Log, record a
                // metric, and push a synthetic Error leg so quorum counting
                // stays accurate — a panicked leg must not silently disappear.
                tracing::error!(error = %e, "panel leg task panicked");
                crate::metrics::record_panel_leg("leg", LegStatus::Error.as_str());
                legs_out.push(LegResult {
                    leg_index: usize::MAX,
                    role: LegRole::Leg,
                    model: String::new(),
                    provider: String::new(),
                    status: LegStatus::Error,
                    response: None,
                    cost_usd: None,
                    usage: None,
                    latency_ms: 0,
                });
            }
        }
    }

    // 2. Quorum check.
    let required = required_panel_quorum(cfg);
    let met = legs_out
        .iter()
        .filter(|l| matches!(l.status, LegStatus::Ok))
        .count();
    if met < required {
        return Err(crate::ApiError::PanelQuorumUnmet { required, met });
    }

    let leg_cost_total = sum_metered_iter(legs_out.iter().map(|l| l.cost_usd));
    Ok((legs_out, leg_cost_total))
}

/// Fan-out all panel member legs concurrently, enforce quorum, then arbitrate.
///
/// `creds` maps **provider id** → credentials for that provider. Before any
/// member can dispatch, the engine verifies that credentialed member legs can
/// meet quorum and that an LLM arbiter has an explicit mapped credential.
/// Additional members whose provider id is absent are then recorded as
/// [`LegStatus::SkippedNoCred`] and do not count toward quorum. `admission`
/// must come from [`admit_panel_request`]; it is revalidated before any member
/// leg can be dispatched.
pub async fn run_panel(
    state: &crate::AppState,
    ctx: &tt_shared::RequestContext,
    base_req: &ChatCompletionRequest,
    creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    cfg: &PanelConfig,
    admission: &PanelAdmission,
    deadline: Duration,
) -> Result<PanelResult, crate::ApiError> {
    // Direct Rust callers cannot reach the fan-out primitive without first
    // obtaining this opaque proof. Re-check the exact work now: request
    // shaping/config mutation between ingress and dispatch must not turn a
    // cheap admission into an expensive fan-out.
    admission.revalidate(state, cfg, base_req)?;
    validate_panel_credential_preflight(state, cfg, creds)?;
    let (mut legs_out, leg_cost_total) =
        run_panel_legs_and_quorum(state, ctx, base_req, creds, cfg, deadline).await?;
    let required = required_panel_quorum(cfg);
    let met = legs_out
        .iter()
        .filter(|l| matches!(l.status, LegStatus::Ok))
        .count();
    // (legs_out is already quorum-checked inside the helper; `required`/`met`
    //  are recomputed here only to populate PanelResult.)

    // 3. Arbitrate.
    let strategy = strategy_for(cfg)?;
    let arb_start = std::time::Instant::now();
    let arb = strategy
        .arbitrate(base_req, &legs_out, state, ctx, creds)
        .await?;
    let arb_latency_ms = arb_start.elapsed().as_millis() as u64;

    let arbiter_provider_id = state
        .registry
        .resolve(&cfg.arbiter_model.model)
        .map(|p| p.id().to_string())
        .unwrap_or_default();

    let arbiter_leg = LegResult {
        leg_index: usize::MAX,
        role: LegRole::Arbiter,
        model: cfg.arbiter_model.model.clone(),
        provider: arbiter_provider_id,
        status: LegStatus::Ok,
        cost_usd: arb.cost_usd,
        usage: Some(arb.response.usage.clone()),
        latency_ms: arb_latency_ms,
        response: None,
    };
    crate::metrics::record_panel_leg("arbiter", LegStatus::Ok.as_str());

    // 4. None-aware cost aggregation: leg_cost_total (already None-aware summed)
    //    plus the arbiter cost.
    let total_cost_usd =
        sum_metered_iter(std::iter::once(leg_cost_total).chain(std::iter::once(arb.cost_usd)));

    legs_out.push(arbiter_leg);

    Ok(PanelResult {
        response: arb.response,
        legs: legs_out,
        total_cost_usd,
        quorum_required: required,
        quorum_met: met,
        arbiter_detail: arb.detail,
    })
}

// ---------------------------------------------------------------------------
// complete_panel — dispatch + aggregate one-row billing
// ---------------------------------------------------------------------------

/// The dashboard's independently validated SSE contract permits at most 2,000
/// UTF-16 code units for an arbiter reason. Keep that boundary here, where the
/// gateway owns the schema, rather than turning a verbose judge explanation
/// into a rejected otherwise-valid terminal panel receipt downstream.
const MAX_PANEL_ARBITER_REASON_UTF16_UNITS: usize = 2_000;

fn bounded_panel_arbiter_reason(value: &str) -> String {
    let mut used = 0usize;
    let mut end = 0usize;
    for (start, ch) in value.char_indices() {
        let units = ch.len_utf16();
        if used + units > MAX_PANEL_ARBITER_REASON_UTF16_UNITS {
            break;
        }
        used += units;
        end = start + ch.len_utf8();
    }
    value[..end].to_owned()
}

/// Build the `tokentrimmer.panel` attribution object.
///
/// Single source of truth shared by the non-streaming response body
/// (`build_panel_body`) and the streaming terminal SSE event
/// (`TrackedEventStream::panel_event` in `sse.rs`).
///
/// Parameters:
/// - `strategy`         — arbitration strategy kind (for top-level + arbiter sub-object).
/// - `legs`             — all leg records (member + arbiter); rendered in order.
/// - `arbiter_detail`   — per-strategy metadata (chosen_leg, reason, majority fields…).
/// - `quorum_required`  — quorum threshold.
/// - `quorum_met`       — how many legs satisfied quorum.
/// - `total_cost_usd`   — aggregate cost (Σ legs + arbiter). `None` ⇒ JSON null.
/// - `arbiter_cost_usd` — the arbiter leg's individual cost for the `arbiter.cost_usd`
///   field. For non-streaming callers this is extracted from the legs slice; streaming
///   callers pass in the finalized figure from `ArbiterCostPlan::finalize`.
pub(crate) fn panel_body_json(
    strategy: ArbiterStrategyKind,
    legs: &[LegResult],
    arbiter_detail: &ArbiterDetail,
    quorum_required: usize,
    quorum_met: usize,
    total_cost_usd: Option<f64>,
    arbiter_cost_usd: Option<f64>,
) -> serde_json::Value {
    // `LegResult::leg_index` is an internal member index. The arbiter has no
    // member slot and therefore historically carried `usize::MAX` internally.
    // Never serialize that sentinel across the JSON/JavaScript boundary: emit
    // a compact, bounded index for each leg in the order this response exposes
    // it. This keeps the arbiter addressable without a precision-losing 64-bit
    // integer and gives clients one canonical index space for `chosen_leg`.
    let wire_chosen_leg = arbiter_detail.chosen_leg.and_then(|chosen_member_index| {
        legs.iter()
            .position(|leg| leg.role == LegRole::Leg && leg.leg_index == chosen_member_index)
    });
    let legs_json: Vec<serde_json::Value> = legs
        .iter()
        .enumerate()
        .map(|(wire_leg_index, l)| {
            let role = match l.role {
                LegRole::Leg => "leg",
                LegRole::Arbiter => "arbiter",
            };
            // Token attribution from the leg's recorded usage, when present.
            let tokens = l.usage.as_ref().map(|u| {
                json!({
                    "input_tokens": u.prompt_tokens,
                    "output_tokens": u.completion_tokens,
                    "cached_tokens": u.cached_tokens,
                })
            });
            json!({
                "leg_index": wire_leg_index,
                "role": role,
                "model": l.model,
                "provider": l.provider,
                "cost_usd": l.cost_usd,
                "status": l.status.as_str(),
                "tokens": tokens,
            })
        })
        .collect();

    // `cost_incomplete`: any surviving *member* leg with no priced cost ⇒ the
    // recorded aggregate is a lower bound (spec §6.4 step 8).
    // The arbiter leg is explicitly excluded: its cost arrives deferred (Live
    // plan) or is always measured (Known plan / non-streaming), so including it
    // would over-set the flag on a normal Synthesize Live stream.
    let cost_incomplete = legs
        .iter()
        .any(|l| l.role != LegRole::Arbiter && l.status == LegStatus::Ok && l.cost_usd.is_none());

    // Build the `arbiter` sub-object: base fields + non-default ArbiterDetail fields.
    let d = arbiter_detail;
    let mut arbiter = serde_json::Map::new();
    arbiter.insert("strategy".into(), json!(strategy.as_str()));
    arbiter.insert("cost_usd".into(), json!(arbiter_cost_usd));
    // best-of-n detail.
    if let Some(cl) = wire_chosen_leg {
        arbiter.insert("chosen_leg".into(), json!(cl));
    }
    if let Some(ref r) = d.reason {
        let reason = bounded_panel_arbiter_reason(r);
        if !reason.is_empty() {
            arbiter.insert("reason".into(), json!(reason));
        }
    }
    if d.fell_back {
        arbiter.insert("fell_back".into(), json!(true));
    }
    // majority detail.
    if let Some(wcs) = d.winning_cluster_size {
        arbiter.insert("winning_cluster_size".into(), json!(wcs));
    }
    if let Some(tc) = d.total_clusters {
        arbiter.insert("total_clusters".into(), json!(tc));
    }
    if d.no_majority {
        arbiter.insert("no_majority".into(), json!(true));
    }
    if d.degraded {
        arbiter.insert("degraded".into(), json!(true));
    }

    json!({
        "strategy": strategy.as_str(),
        "legs": legs_json,
        "total_cost_usd": total_cost_usd,
        "quorum": {
            "required": quorum_required,
            "met": quorum_met,
        },
        "cost_incomplete": cost_incomplete,
        "arbiter": serde_json::Value::Object(arbiter),
    })
}

/// Build the `tokentrimmer.panel` attribution object for the response body.
///
/// Mirrors spec §6.4 step 9: per-leg breakdown + quorum + a `cost_incomplete`
/// flag set when any **surviving** (status == Ok) leg reported `None` cost (an
/// unpriceable model makes the recorded aggregate an honest lower bound).
fn build_panel_body(cfg: &PanelConfig, result: &PanelResult) -> serde_json::Value {
    // Arbiter leg cost from the legs list (the last leg with role == Arbiter).
    let arbiter_cost_usd = result
        .legs
        .iter()
        .find(|l| l.role == LegRole::Arbiter)
        .and_then(|l| l.cost_usd);
    panel_body_json(
        cfg.strategy,
        &result.legs,
        &result.arbiter_detail,
        result.quorum_required,
        result.quorum_met,
        result.total_cost_usd,
        arbiter_cost_usd,
    )
}

/// Build the aggregate [`CostBreakdown`] for a panel: the summed leg + arbiter
/// cost is the single billable `cost_usd`. The baseline is set EQUAL to the
/// realized cost — a panel is the service the caller explicitly opted into, so
/// no routing/cache saving is claimed (`tt_saved_usd()` == 0), and every other
/// savings/penalty field is zero. This keeps the row's TT headline honest and
/// invoice-reconcilable: `cost_usd` is the sum, nothing more, nothing less.
fn aggregate_cost_breakdown(total_cost_usd: f64) -> CostBreakdown {
    CostBreakdown {
        cost_usd: total_cost_usd,
        baseline_cost_usd: total_cost_usd,
        provider_cache_saved_usd: 0.0,
        flex_saved_usd: 0.0,
        compression_saved_usd: 0.0,
        doc_compaction_saved_usd: 0.0,
        cache_bust_penalty_usd: 0.0,
        summarizer_tax_usd: 0.0,
        batch_forgone_usd: 0.0,
        minify_saved_est_usd: 0.0,
        diff_saved_usd: 0.0,
        format_switch_saved_est_usd: 0.0,
        diff_failed_cost_usd: 0.0,
        // A panel claims no routing/cache saving; the vision-avoided saving is
        // likewise 0 (the Document Lane seam does not run on a panel path).
        doc_vision_saved_est_usd: 0.0,
        // Content_compress does not run on a panel path → 0.
        content_compress_saved_est_usd: 0.0,
    }
}

/// Complete a Fusion panel request: fan out via [`run_panel`], aggregate
/// leg + arbiter cost into ONE billable figure, record it with the EXACT
/// dispatched-path discipline (spend `record` + `settle(false)` once; ONE
/// `cached=false` `request_logs` row), inject the `tokentrimmer.panel`
/// attribution object into the response body, and return the same
/// [`CompletionOutcome::Dispatched`] shape the single-model path returns.
///
/// # Billing discipline (invariants §2.1.3/4/5 — replicates `complete_once`'s
/// dispatched tail VERBATIM, substituting the aggregate cost + `'panel'` stamp)
/// - **`record_request_served` exactly once:** NOT called here. The chat
///   `handler` (and the agent-loop consumer) bumps `record_request_served` once
///   per `CompletionOutcome::Dispatched` in-band — returning `Dispatched`
///   therefore yields exactly one served increment, identical to the
///   single-model path (which also leaves the served bump to the consumer).
/// - **Realized spend recorded exactly once:** one
///   `spend_sink().record(cost_usd)` followed by one `settle(false)`, mirroring
///   `complete_once` lines (record then settle-false). `cost_usd` is the
///   aggregate `total_cost_usd` — NOT recomputed from the arbiter response (that
///   would drop the legs) and NOT a per-leg double-count (`run_panel` already
///   summed legs + arbiter via the None-aware fold).
/// - **One `request_logs` row, `cached = false`:** a single `spawn_request_log`
///   with `provider = "panel"` (decision-A sentinel), `model =
///   cfg.arbiter_model.model`, and `cached: false` — so the cloud overage meter
///   (`COUNT(*) WHERE NOT cached`) and the gateway month-request accumulator
///   both count the panel as exactly one billable request.
///
/// Panels bypass L1/L2 + single-flight entirely (the caller branches here BEFORE
/// the cache checks in `complete_once`), so there is no cache insert and no
/// negative-cache interaction to replicate.
pub(crate) async fn complete_panel(
    state: &AppState,
    ctx: &RequestContext,
    prep: Prepared,
    cfg: PanelConfig,
    admission: PanelAdmission,
) -> Result<CompletionOutcome, ApiError> {
    // Per-leg / arbiter deadline: derive from the caller's per-request upstream
    // timeout when present, else a bounded default. The outer route
    // `TimeoutLayer` (60 s) caps the whole request regardless.
    let deadline = prep
        .request_timeout
        .unwrap_or_else(|| Duration::from_secs(120));

    // Fan out + arbitrate. A quorum-unmet / strategy-unsupported / unresolved
    // error propagates via `?` BEFORE any billing side effect — a failed panel
    // writes NO billable row and advances NO counter, exactly like a failed
    // single-model dispatch (which returns before the spend/settle/log block).
    // Panel member credentials were pre-resolved per-provider in `prepare`
    // (`panel_creds`, spec §6.4 step 4) — NOT `failover_creds`, which is only
    // populated on a failover route. `run_panel` fences the map before any
    // dispatch; only extra members beyond a credentialed quorum can be recorded
    // as `skipped_no_cred` (never dispatched/billed).
    let result = match run_panel(
        state,
        ctx,
        &prep.req,
        &prep.panel_creds,
        &cfg,
        &admission,
        deadline,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            // Bounded outcome label for the request-level panel counter.
            let outcome = match &e {
                ApiError::PanelQuorumUnmet { .. } => "quorum_unmet",
                ApiError::PanelCredentialPreflight { .. } => "credential_preflight",
                ApiError::PanelStrategyUnsupported { .. } => "strategy_unsupported",
                _ => "error",
            };
            crate::metrics::record_panel_request(cfg.strategy.as_str(), outcome);
            return Err(e);
        }
    };
    crate::metrics::record_panel_request(cfg.strategy.as_str(), "success");

    // Aggregate cost: the single billable figure (legs + arbiter, already summed
    // None-aware by `run_panel`). `None` (every leg unpriced) books $0 honestly.
    let total_cost_usd = result.total_cost_usd.unwrap_or(0.0);
    let cost_breakdown = aggregate_cost_breakdown(total_cost_usd);

    // ── Record realized spend EXACTLY ONCE (mirrors complete_once's dispatched
    //    tail: `spend_sink().record(...)` then `settle(..., cached=false, ...)`).
    state
        .spend_sink()
        .record(ctx.org_id, ctx.api_key_id, total_cost_usd, Utc::now());
    state
        .spend_sink()
        .settle(ctx.org_id, ctx.api_key_id, false, Utc::now());

    // ── Write EXACTLY ONE `request_logs` row, `cached = false`. Provider is the
    //    `'panel'` sentinel (decision A: a multi-provider panel is excluded from
    //    per-provider drift checks rather than misattributed); model is the
    //    arbiter model. Token counts come from the served (arbiter) response.
    let served = &result.response;
    let trace_id = ctx.trace_id;
    let arbiter_model = cfg.arbiter_model.model.clone();
    // Hoist the parent id so the per-leg rows can reference it.
    let parent_id = Uuid::now_v7();
    spawn_request_log(
        state.telemetry_tracker.as_ref(),
        state.request_log_writer.as_ref(),
        RequestLogRow {
            id: parent_id,
            org_id: ctx.org_id,
            api_key_id: ctx.api_key_id,
            ts: Utc::now(),
            // Decision-A sentinel: one stamped provider for the aggregate row.
            provider: "panel".to_string(),
            model: arbiter_model.clone(),
            input_tokens: served.usage.prompt_tokens as i32,
            output_tokens: served.usage.completion_tokens as i32,
            cached_tokens: served.usage.cached_tokens as i32,
            cost_usd: total_cost_usd,
            baseline_cost_usd: total_cost_usd,
            provider_cache_saved_usd: 0.0,
            cache_bust_penalty_usd: 0.0,
            flex_saved_usd: 0.0,
            doc_compaction_saved_usd: 0.0,
            summarizer_tax_usd: 0.0,
            // INVARIANT §2.1.5: every panel row is `cached = false` so the cloud
            // overage meter + month accumulator both count it.
            cached: false,
            cache_layer: None,
            route_id: prep.matched_route_id,
            route_version_id: prep.matched_route_version_id,
            latency_ms: prep
                .request_started
                .elapsed()
                .as_millis()
                .min(i32::MAX as u128) as i32,
            upstream_latency_ms: None,
            status: 200,
            tag: ctx.tag.clone(),
            error_class: None,
            trace_id: Some(trace_id.to_string()),
            truncated: false,
            // Panels never run the canary shadow / traffic-split / batch / diff /
            // minify / format-switch levers — all zero/absent.
            shadow_model: None,
            shadow_cost_usd: None,
            traffic_split_arm: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            batch_eligible: false,
            batch_forgone_usd: 0.0,
            route_paused: prep.route_paused,
            minify_saved_est_usd: 0.0,
            format_switched: None,
            format_switch_saved_est_usd: 0.0,
            diff_applied: false,
            diff_saved_usd: 0.0,
            diff_failed: false,
            diff_failed_cost_usd: 0.0,
            retrieval_tokens_saved: prep.retrieval_telemetry.tokens_saved,
            // Panels never run the doc-compaction pass (multi-member fan-out).
            doc_compaction_tokens_removed: 0,
            // Panels never run the compress pass → 0 (TR-2).
            compression_saved_usd: 0.0,
            compression_tokens_removed: 0,
            // Document Lane D4: panels never run the seam → 0.
            doc_vision_saved_est_usd: 0.0,
            // Panels never run agentic loops; run_id/node_id stay None.
            run_id: None,
            node_id: None,
            // Panels never run the content_compress pass → 0 / None.
            content_compress_saved_est_usd: 0.0,
            content_compress_kind: None,
            // No L2 provenance on a panel-synthesized row.
            l2_matched_entry_id: None,
            l2_similarity: None,
            l2_verdict: None,
        },
    );

    // ── Write per-leg `panel_legs` rows (fire-and-forget, MUST NOT block or
    //    fail the response). One row per enumeration position (not LegResult
    //    .leg_index) so the sentinel usize::MAX on the arbiter never appears.
    if let Some(writer) = state.panel_leg_writer.clone() {
        let leg_rows: Vec<PanelLegRow> = result
            .legs
            .iter()
            .enumerate()
            .map(|(i, leg)| PanelLegRow {
                request_log_id: parent_id,
                leg_index: i as i32,
                role: match leg.role {
                    LegRole::Leg => "leg".to_string(),
                    LegRole::Arbiter => "arbiter".to_string(),
                },
                provider: leg.provider.clone(),
                model: leg.model.clone(),
                input_tokens: leg.usage.as_ref().map(|u| u.prompt_tokens as i64),
                output_tokens: leg.usage.as_ref().map(|u| u.completion_tokens as i64),
                cached_tokens: leg.usage.as_ref().map(|u| u.cached_tokens as i64),
                cost_usd: leg.cost_usd,
                latency_ms: Some(leg.latency_ms as i64),
                status: leg.status.as_str().to_string(),
                error_class: None,
            })
            .collect();
        let fut = async move {
            let _ = writer.write_legs(leg_rows).await;
        };
        match state.telemetry_tracker.as_ref() {
            Some(t) => {
                t.spawn(fut);
            }
            None => {
                tokio::spawn(fut);
            }
        }
    }

    // ── Record OTel GenAI semconv + panel span attributes on the current
    //    `http_request` span.  Uses the same `set_attribute` mechanism as the
    //    single-model path (`record_request_span_attributes` in chat.rs) so no
    //    tracing-field pre-declaration is needed.  Panel attrs are ADDITIVE —
    //    non-panel spans carry none of them (off-by-default invariant).
    {
        let served = &result.response;
        tt_telemetry::gen_ai::record_request_attributes(
            &tracing::Span::current(),
            &tt_telemetry::gen_ai::RequestSpanAttributes {
                // Provider sentinel "panel" mirrors the request_logs row.
                provider_id: "panel",
                // The caller's originally-requested model is the arbiter model
                // for a panel (no pre-panel routing rewrites the top-level model).
                request_model: &arbiter_model,
                response_model: &arbiter_model,
                operation: "chat",
                cost: tt_telemetry::gen_ai::RequestSpanCost {
                    input_tokens: served.usage.prompt_tokens,
                    output_tokens: served.usage.completion_tokens,
                    cost_usd: cost_breakdown.cost_usd,
                    baseline_cost_usd: cost_breakdown.baseline_cost_usd,
                    saved_usd: cost_breakdown.tt_saved_usd(),
                    provider_cache_saved_usd: cost_breakdown.provider_cache_saved_usd,
                },
                cache_outcome: Some("none"),
                route: prep.route_matched_name.as_deref(),
                traffic_split_pct: None,
                shadow_model: None,
                shadow_cost_usd: None,
                // Panel-specific additive attributes.
                panel_strategy: Some(cfg.strategy.as_str()),
                panel_leg_count: Some(result.legs.len() as i64),
                panel_quorum_required: Some(result.quorum_required as i64),
                panel_quorum_met: Some(result.quorum_met as i64),
            },
        );
    }

    // Inject the `tokentrimmer.panel` attribution object into the body (merged
    // at the serialization boundary in the handler tail; see `panel_body`).
    let panel_body = build_panel_body(&cfg, &result);

    Ok(CompletionOutcome::Dispatched {
        response: result.response,
        headers: Box::new(CompletionHeaders {
            trace_id,
            provider_id: "panel".to_string(),
            model_used: arbiter_model,
            cost_breakdown,
            cache_state: "none",
            route_matched_name: prep.route_matched_name,
            body_captured: false,
            req: prep.req,
            provider: prep.provider,
            warnings: prep.warnings,
            panel_body: Some(panel_body),
        }),
    })
}

/// Complete a **streaming** Fusion panel request: fan out the member
/// legs + enforce quorum ([`run_panel_legs_and_quorum`]), establish the arbiter
/// as a chunk stream ([`ArbiterStrategy::arbitrate_streaming`]), and hand the
/// arbiter stream to [`crate::routes::sse::stream_response`] with a panel-aware
/// [`StreamLogContext`](crate::routes::sse::StreamLogContext). Mirrors
/// [`complete_panel`] but **defers** the single aggregate `request_logs` row,
/// the realized spend, and the per-leg `panel_legs` rows to the SSE
/// [`DropGuard`](crate::routes::sse) — those side effects fire only once the
/// arbiter stream drains (the streamed arbiter answer is not known up front).
///
/// # Fail-closed before 200 (invariants §2.1.3/4/5)
/// A quorum-unmet / strategy-unsupported / unresolved error from
/// `run_panel_legs_and_quorum`, AND an arbiter-establishment error from
/// `arbitrate_streaming`, both propagate as `Err(ApiError)` **before**
/// `stream_response` is ever called — so a failed streaming panel opens NO
/// stream, writes NO billable row, advances NO counter, exactly like a failed
/// single-model stream (`handle_streaming` returns before building the SSE body).
///
/// # `record_request_served` discipline
/// NOT called here. `stream_response` bumps `record_request_served("sse",
/// "dispatch")` exactly once, in-band, before handing back the SSE response —
/// identical to the single-model streaming path. A fail-closed return reaches
/// neither, so a rejected panel records nothing (the served counter only
/// advances on a 200 that actually opened a stream).
pub(crate) async fn complete_panel_streaming(
    state: &AppState,
    ctx: &RequestContext,
    prep: Prepared,
    cfg: PanelConfig,
    admission: PanelAdmission,
) -> Result<axum::response::Response, ApiError> {
    use std::time::Instant;

    // `run_panel_legs_and_quorum` is private so this is the only streaming
    // route to fan-out. Revalidate before it can make an upstream call.
    admission.revalidate(state, &cfg, &prep.req)?;
    if let Err(error) = validate_panel_credential_preflight(state, &cfg, &prep.panel_creds) {
        let outcome = match &error {
            ApiError::PanelCredentialPreflight { .. } => "credential_preflight",
            _ => "error",
        };
        crate::metrics::record_panel_request(cfg.strategy.as_str(), outcome);
        return Err(error);
    }

    // Per-leg / arbiter deadline: mirror `complete_panel`'s derivation.
    let deadline = prep
        .request_timeout
        .unwrap_or_else(|| Duration::from_secs(120));

    // ── Phases 1–2: fan out member legs + enforce quorum. A quorum-unmet (or any
    //    other) error propagates BEFORE any stream is opened — fail-closed, zero
    //    rows, mirroring `complete_panel`'s `run_panel` error arm + outcome label.
    let (mut legs, leg_cost_total) =
        match run_panel_legs_and_quorum(state, ctx, &prep.req, &prep.panel_creds, &cfg, deadline)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                let outcome = match &e {
                    ApiError::PanelQuorumUnmet { .. } => "quorum_unmet",
                    ApiError::PanelCredentialPreflight { .. } => "credential_preflight",
                    ApiError::PanelStrategyUnsupported { .. } => "strategy_unsupported",
                    _ => "error",
                };
                crate::metrics::record_panel_request(cfg.strategy.as_str(), outcome);
                return Err(e);
            }
        };

    // Recompute quorum required/met for the PanelStreamLog span attributes
    // (legs is already quorum-checked inside the helper).
    let required = required_panel_quorum(&cfg);
    let met = legs
        .iter()
        .filter(|l| matches!(l.status, LegStatus::Ok))
        .count();

    // ── Phase 3: establish the arbiter as a chunk stream. An establishment
    //    error (e.g. no successful legs to synthesize, arbiter dispatch failed)
    //    returns Err here — a proper non-200, BEFORE `stream_response`.
    let strategy = strategy_for(&cfg).inspect_err(|_| {
        crate::metrics::record_panel_request(cfg.strategy.as_str(), "error");
    })?;
    let arb_start = Instant::now();
    let (arbiter_stream, arbiter_cost_plan, arbiter_detail) = strategy
        .arbitrate_streaming(&prep.req, &legs, state, ctx, &prep.panel_creds)
        .await
        .inspect_err(|_| {
            crate::metrics::record_panel_request(cfg.strategy.as_str(), "error");
        })?;
    let arb_latency_ms = arb_start.elapsed().as_millis() as u64;
    crate::metrics::record_panel_request(cfg.strategy.as_str(), "success");

    // Resolve the arbiter provider — its pricing drives the streamed
    // (`Live`) arbiter cost finalize in the DropGuard, and its id stamps the
    // arbiter leg's `panel_legs` row + the span provider attribute.
    let arbiter_provider = state
        .registry
        .resolve(&cfg.arbiter_model.model)
        .ok_or_else(|| ApiError::ModelNotFound {
            model: cfg.arbiter_model.model.clone(),
        })?;
    let arbiter_provider_id = arbiter_provider.id().to_string();
    let arbiter_model = cfg.arbiter_model.model.clone();

    // ── Build the arbiter-leg record so the deferred `panel_legs` persistence
    //    includes the arbiter leg, mirroring `run_panel`'s `arbiter_leg`.
    //    `cost_usd` comes from the plan's `Known` value (replay strategies) or
    //    `None` for `Live` (the Synthesize arbiter cost is finalized in the
    //    DropGuard from the streamed usage). `usage = None` is acceptable on the
    //    deferred path (mirrors `PanelStreamLog`'s deferred-cost contract).
    let arbiter_leg = LegResult {
        leg_index: usize::MAX,
        role: LegRole::Arbiter,
        model: arbiter_model.clone(),
        provider: arbiter_provider_id.clone(),
        status: LegStatus::Ok,
        cost_usd: match &arbiter_cost_plan {
            ArbiterCostPlan::Known(c) => *c,
            ArbiterCostPlan::Live => None,
        },
        usage: None,
        latency_ms: arb_latency_ms,
        response: None,
    };
    crate::metrics::record_panel_leg("arbiter", LegStatus::Ok.as_str());
    legs.push(arbiter_leg);

    // ── PanelStreamLog (Task 4): the panel-specific billing context the
    //    DropGuard reads to write the ONE aggregate `provider='panel'` row +
    //    the per-leg `panel_legs` rows at stream end.
    let panel = std::sync::Arc::new(crate::routes::sse::PanelStreamLog {
        leg_records: legs,
        leg_cost_total,
        strategy: cfg.strategy,
        quorum_required: required,
        quorum_met: met,
        arbiter_detail,
        arbiter_cost_plan,
        arbiter_model: arbiter_model.clone(),
        panel_leg_writer: state.panel_leg_writer.clone(),
    });

    // Input-token estimate for the streamed row (best-effort; the DropGuard
    // overwrites the cost with the panel aggregate regardless). Estimated
    // against the arbiter provider so the tokenizer choice matches the streamed
    // provider.
    let estimated_input_tokens = tt_tokenize::estimate_tokens(
        arbiter_provider.id(),
        &tt_shared::message_text_for_estimation(&prep.req),
    ) as i32;

    // Honor `stream_options.include_usage` end-to-end (helper is private to
    // chat.rs; the probe is inlined here).
    let include_usage = prep
        .req
        .stream_options
        .as_ref()
        .and_then(|o| o.get("include_usage"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // Arbiter-model pricing drives the `Live` finalize + the terminal usage
    // event; baseline == pricing (a panel claims no routing/cache saving).
    let arbiter_pricing = arbiter_provider.pricing(&arbiter_model);

    // OTel span context, provider stamped "panel" to mirror `complete_panel`'s
    // span (the DropGuard fills the additive panel.* attributes from
    // PanelStreamLog regardless). Panels never cache, so `cache_outcome = none`.
    let span_ctx = Some(crate::routes::sse::StreamSpanContext {
        span: tracing::Span::current(),
        provider_id: "panel".to_string(),
        request_model: arbiter_model.clone(),
        response_model: arbiter_model.clone(),
        cache_outcome: "none".to_string(),
        route: prep.route_matched_name.clone(),
        traffic_split_pct: None,
    });

    let log_ctx = crate::routes::sse::StreamLogContext {
        writer: state.request_log_writer.as_ref().map(|w| w.clone()),
        tracker: state.telemetry_tracker.clone(),
        org_id: ctx.org_id,
        api_key_id: ctx.api_key_id,
        trace_id: ctx.trace_id,
        // Provider sentinel "panel" — the DropGuard forces the row provider to
        // "panel" for a panel context anyway; this also picks the tokenizer.
        provider_id: "panel".to_string(),
        model: arbiter_model.clone(),
        input_tokens: estimated_input_tokens,
        // Panels never cache.
        cached_tokens: 0,
        pricing: arbiter_pricing.clone(),
        baseline_pricing: arbiter_pricing,
        route_id: prep.matched_route_id,
        route_version_id: prep.matched_route_version_id,
        tag: ctx.tag.clone(),
        request_started: prep.request_started,
        spend_sink: state.spend_sink(),
        // Panels run no flex / compression / shaping levers — defaults.
        fee_multiplier: arbiter_provider.fee_multiplier(),
        flex_applied: false,
        pass_effects: crate::passes::PassEffects::default(),
        retrieval_tokens_saved: prep.retrieval_telemetry.tokens_saved,
        // Panels never cache.
        cache_insert: None,
        include_usage,
        span_ctx,
        // Panels never run the canary traffic-split lever.
        traffic_split_arm: None,
        route_paused: prep.route_paused,
        // The panel aggregate-billing context (off-by-default elsewhere).
        panel: Some(panel),
    };

    Ok(crate::routes::sse::stream_response(
        arbiter_stream,
        &arbiter_provider,
        ctx.trace_id,
        Some(log_ctx),
    ))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, time::Duration};

    use super::*;
    use tt_shared::{
        context::{ProviderCredentials, SecretString},
        messages::{ChatCompletionRequest, Message, MessageContent, PanelExtras},
    };
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // empty-legs guard (inline, no AppState/RequestContext needed)
    // -----------------------------------------------------------------------

    /// Verify that the ok-answer filter produces an empty set when all legs
    /// have a non-Ok status. The guard in `Synthesize::arbitrate` fires
    /// before any provider resolve/dispatch, so this test does not require a
    /// working AppState or RequestContext.
    ///
    /// NOTE: Because `ArbiterStrategy::arbitrate` is async and needs a real
    /// AppState to reach the dispatch path, we test the guard's *precondition*
    /// (the filter that produces `ok_answers`) inline here rather than calling
    /// `arbitrate` end-to-end. The guard's `if ok_answers.is_empty()` branch
    /// is the only path exercised; no mock provider is constructed.
    #[test]
    fn empty_ok_answers_set_when_all_legs_error() {
        let legs: Vec<LegResult> = vec![
            LegResult {
                leg_index: 0,
                role: LegRole::Leg,
                model: "m1".to_string(),
                provider: "p1".to_string(),
                status: LegStatus::Error,
                response: None,
                cost_usd: None,
                usage: None,
                latency_ms: 0,
            },
            LegResult {
                leg_index: 1,
                role: LegRole::Leg,
                model: "m2".to_string(),
                provider: "p2".to_string(),
                status: LegStatus::Timeout,
                response: None,
                cost_usd: None,
                usage: None,
                latency_ms: 0,
            },
        ];

        // Mirror the filter from `Synthesize::arbitrate` to confirm the guard
        // precondition: no leg is Ok+Leg, so ok_answers would be empty.
        let ok_answers: Vec<&LegResult> = legs
            .iter()
            .filter(|l| l.status == LegStatus::Ok && l.role == LegRole::Leg)
            .collect();

        assert!(
            ok_answers.is_empty(),
            "expected empty ok-answer set when all legs are Error/Timeout"
        );
        // The guard `if ok_answers.is_empty()` returns
        // Err(ApiError::InvalidRequest("panel: no successful legs to synthesize"))
        // before any dispatch — confirmed by code inspection.
    }

    #[test]
    fn as_str_round_trips() {
        assert_eq!(ArbiterStrategyKind::Synthesize.as_str(), "synthesize");
        assert_eq!(ArbiterStrategyKind::BestOfN.as_str(), "best-of-n");
        assert_eq!(ArbiterStrategyKind::Majority.as_str(), "majority");
    }

    #[test]
    fn parse_case_insensitive() {
        assert!(matches!(
            ArbiterStrategyKind::parse("SYNTHESIZE"),
            Some(ArbiterStrategyKind::Synthesize)
        ));
        assert!(ArbiterStrategyKind::parse("bogus").is_none());
    }

    /// Drift guard (Task 2): every wire value `tt_routing::validate_panel`
    /// accepts at route creation MUST be parseable by `ArbiterStrategyKind::parse`
    /// at request time. If the routing crate adds a strategy alias without a
    /// matching parse arm here, a route would validate at creation then silently
    /// fall through (defensive skip) at dispatch — this test fails the build first.
    #[test]
    fn every_validated_strategy_parses() {
        for s in tt_routing::PANEL_STRATEGY_VALUES {
            assert!(
                ArbiterStrategyKind::parse(s).is_some(),
                "PANEL_STRATEGY_VALUES contains {s:?} which ArbiterStrategyKind::parse rejects \
                 — validate_panel and parse have drifted"
            );
        }
    }

    #[test]
    fn panel_tier_rank_ordering() {
        use tt_shared::CallerTier;
        // Free < Pro < Team < Scale
        assert!(
            panel_tier_rank(CallerTier::Free) < panel_tier_rank(CallerTier::Pro),
            "Free must rank lower than Pro"
        );
        assert!(
            panel_tier_rank(CallerTier::Pro) < panel_tier_rank(CallerTier::Team),
            "Pro must rank lower than Team"
        );
        assert!(
            panel_tier_rank(CallerTier::Team) < panel_tier_rank(CallerTier::Scale),
            "Team must rank lower than Scale"
        );
        // Gate no-op: Free >= Free
        assert_eq!(
            panel_tier_rank(CallerTier::Free),
            panel_tier_rank(CallerTier::Free),
            "Free rank == Free rank (allow-all no-op)"
        );
    }

    #[test]
    fn resolve_extras_override_defaults() {
        let extras = PanelExtras {
            members: vec!["m1".to_string()],
            arbiter_model: Some("arbiter-x".to_string()),
            quorum: Some(1),
            max_cost_usd: Some(0.10),
        };
        let defaults = PanelDefaults {
            members: vec![ModelRef {
                model: "fallback".to_string(),
                provider: None,
            }],
            arbiter_model: ModelRef {
                model: "default-arbiter".to_string(),
                provider: None,
            },
        };
        let cfg =
            PanelConfig::resolve(ArbiterStrategyKind::BestOfN, Some(&extras), &defaults).unwrap();
        assert_eq!(cfg.members.len(), 1);
        assert_eq!(cfg.members[0].model, "m1");
        assert_eq!(cfg.arbiter_model.model, "arbiter-x");
        assert_eq!(cfg.quorum, Some(1));
        assert_eq!(cfg.max_cost_usd, Some(0.10));
    }

    fn panel_defaults() -> PanelDefaults {
        PanelDefaults {
            members: vec![ModelRef {
                model: "fallback".to_string(),
                provider: None,
            }],
            arbiter_model: ModelRef {
                model: "default-arbiter".to_string(),
                provider: None,
            },
        }
    }

    #[test]
    fn resolve_rejects_blank_or_duplicate_members() {
        let defaults = panel_defaults();
        for (members, expected) in [
            (vec![" ".to_string()], "member model must not be blank"),
            (
                vec!["member-a".to_string(), "member-a".to_string()],
                "configured more than once",
            ),
        ] {
            let extras = PanelExtras {
                members,
                ..Default::default()
            };
            let error =
                PanelConfig::resolve(ArbiterStrategyKind::Synthesize, Some(&extras), &defaults)
                    .expect_err("unsafe panel members must be rejected");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn resolve_rejects_blank_arbiter_invalid_quorum_and_invalid_budget() {
        let defaults = panel_defaults();
        let cases = [
            (
                PanelExtras {
                    arbiter_model: Some("  ".to_string()),
                    ..Default::default()
                },
                "arbiter model must not be blank",
            ),
            (
                PanelExtras {
                    quorum: Some(0),
                    ..Default::default()
                },
                "quorum must be at least one",
            ),
            (
                PanelExtras {
                    quorum: Some(2),
                    ..Default::default()
                },
                "quorum 2 exceeds",
            ),
            (
                PanelExtras {
                    max_cost_usd: Some(0.0),
                    ..Default::default()
                },
                "max_cost_usd must be a finite number greater than zero",
            ),
            (
                PanelExtras {
                    max_cost_usd: Some(f64::NAN),
                    ..Default::default()
                },
                "max_cost_usd must be a finite number greater than zero",
            ),
        ];

        for (extras, expected) in cases {
            let error =
                PanelConfig::resolve(ArbiterStrategyKind::Synthesize, Some(&extras), &defaults)
                    .expect_err("unsafe panel configuration must be rejected");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    fn direct_engine_admission_config(max_cost_usd: Option<f64>) -> PanelConfig {
        PanelConfig {
            strategy: ArbiterStrategyKind::Synthesize,
            members: vec![ModelRef {
                model: "gpt-4o".to_string(),
                provider: None,
            }],
            arbiter_model: ModelRef {
                model: "gpt-4o".to_string(),
                provider: None,
            },
            quorum: Some(1),
            max_cost_usd,
        }
    }

    fn direct_engine_admission_request(max_tokens: u32) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".to_string(),
            messages: vec![Message::User {
                content: MessageContent::Text("hello".to_string()),
                name: None,
            }],
            max_tokens: Some(max_tokens),
            ..Default::default()
        }
    }

    fn direct_engine_admission_context() -> RequestContext {
        RequestContext {
            trace_id: Uuid::nil(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: ProviderCredentials {
                api_key: SecretString::new("test-key"),
                base_url: None,
                extra_headers: Vec::new(),
            },
            tag: None,
            deadline: None,
            run_id: None,
            node_id: None,
        }
    }

    #[test]
    fn direct_engine_admission_rejects_invalid_public_config_literals() {
        let state = AppState::with_default_providers();
        let req = direct_engine_admission_request(1);

        let mut blank_member = direct_engine_admission_config(Some(1_000.0));
        blank_member.members[0].model = "   ".to_string();

        let mut duplicate_member = direct_engine_admission_config(Some(1_000.0));
        let duplicate = duplicate_member.members[0].clone();
        duplicate_member.members.push(duplicate);

        let mut impossible_quorum = direct_engine_admission_config(Some(1_000.0));
        impossible_quorum.quorum = Some(2);

        let invalid_config_budget = direct_engine_admission_config(Some(0.0));

        let mut over_cap = direct_engine_admission_config(Some(1_000.0));
        over_cap.members = (0..=panel_max_members())
            .map(|index| ModelRef {
                model: format!("direct-member-{index}"),
                provider: None,
            })
            .collect();

        for (cfg, expected) in [
            (blank_member, "member model must not be blank"),
            (duplicate_member, "configured more than once"),
            (impossible_quorum, "quorum 2 exceeds"),
            (
                invalid_config_budget,
                "max_cost_usd must be a finite number greater than zero",
            ),
            (over_cap, "exceeds the maximum"),
        ] {
            let error = match admit_panel_request(&state, &cfg, &req, Some(1_000.0)) {
                Ok(_) => {
                    panic!("direct literals must pass static config validation before admission")
                }
                Err(error) => error,
            };
            assert!(
                matches!(&error, ApiError::InvalidRequest(message) if message.contains(expected)),
                "expected InvalidRequest containing {expected:?}, got {error:?}"
            );
        }
    }

    /// The public engine cannot be called with a forged proof, and a real proof
    /// is checked again against the request/config that will actually fan out.
    /// This stays entirely static: no mock provider or integration harness is
    /// required to establish the pre-dispatch boundary.
    #[tokio::test]
    async fn direct_engine_admission_revalidates_request_and_config() {
        let state = AppState::with_default_providers();
        let mut cfg = direct_engine_admission_config(Some(0.10));
        let cheap = direct_engine_admission_request(1);
        let admission = admit_panel_request(&state, &cfg, &cheap, None)
            .expect("cheap, explicitly budgeted panel should be admitted");
        let ctx = direct_engine_admission_context();
        let creds: HashMap<String, ProviderCredentials> = HashMap::new();

        let expensive = direct_engine_admission_request(1_000_000);
        assert!(matches!(
            run_panel(
                &state,
                &ctx,
                &expensive,
                &creds,
                &cfg,
                &admission,
                Duration::from_secs(1),
            )
            .await,
            Err(ApiError::CostLimitExceeded { .. })
        ));

        cfg.max_cost_usd = Some(0.001);
        assert!(matches!(
            run_panel(
                &state,
                &ctx,
                &cheap,
                &creds,
                &cfg,
                &admission,
                Duration::from_secs(1),
            )
            .await,
            Err(ApiError::CostLimitExceeded { .. })
        ));

        cfg.max_cost_usd = Some(0.10);
        let duplicate = cfg.members[0].clone();
        cfg.members.push(duplicate);
        let error = run_panel(
            &state,
            &ctx,
            &cheap,
            &creds,
            &cfg,
            &admission,
            Duration::from_secs(1),
        )
        .await
        .expect_err("post-admission invalid config must stop before fan-out");
        assert!(
            matches!(&error, ApiError::InvalidRequest(message) if message.contains("configured more than once")),
            "expected duplicate-member admission error, got {error:?}"
        );
    }

    #[test]
    fn direct_engine_admission_requires_an_explicit_budget() {
        let state = AppState::with_default_providers();
        let cfg = direct_engine_admission_config(None);
        let req = direct_engine_admission_request(1);

        assert!(matches!(
            admit_panel_request(&state, &cfg, &req, None),
            Err(ApiError::CostLimitExceeded { .. })
        ));
    }

    #[test]
    fn panel_body_uses_bounded_wire_indexes_for_internal_sentinels() {
        // Member legs can arrive in completion order rather than configured
        // order. The arbiter has no member index and uses usize::MAX only
        // internally, so the wire contract must neither leak that value nor
        // leave `chosen_leg` in a different index space than `legs`.
        let legs = vec![
            LegResult {
                leg_index: 7,
                role: LegRole::Leg,
                model: "member-late".to_string(),
                provider: "mock".to_string(),
                status: LegStatus::Ok,
                response: None,
                cost_usd: Some(0.001),
                usage: None,
                latency_ms: 1,
            },
            LegResult {
                leg_index: 2,
                role: LegRole::Leg,
                model: "member-chosen".to_string(),
                provider: "mock".to_string(),
                status: LegStatus::Ok,
                response: None,
                cost_usd: Some(0.001),
                usage: None,
                latency_ms: 1,
            },
            LegResult {
                leg_index: usize::MAX,
                role: LegRole::Arbiter,
                model: "arbiter".to_string(),
                provider: "mock".to_string(),
                status: LegStatus::Ok,
                response: None,
                cost_usd: Some(0.002),
                usage: None,
                latency_ms: 1,
            },
        ];
        let body = panel_body_json(
            ArbiterStrategyKind::BestOfN,
            &legs,
            &ArbiterDetail {
                chosen_leg: Some(2),
                ..Default::default()
            },
            1,
            2,
            Some(0.004),
            Some(0.002),
        );

        let wire_legs = body["legs"].as_array().expect("legs array");
        let indexes: Vec<u64> = wire_legs
            .iter()
            .map(|leg| leg["leg_index"].as_u64().expect("bounded wire index"))
            .collect();
        assert_eq!(indexes, vec![0, 1, 2]);
        assert_eq!(wire_legs[2]["role"], "arbiter");
        assert_eq!(
            body["arbiter"]["chosen_leg"].as_u64(),
            Some(1),
            "chosen_leg must address the emitted member index, not the internal member slot"
        );
        assert!(
            !body.to_string().contains(&usize::MAX.to_string()),
            "internal sentinel must never reach dashboard JSON"
        );
    }

    #[test]
    fn panel_arbiter_reason_is_bounded_in_dashboard_utf16_units() {
        let reason = bounded_panel_arbiter_reason(&"🙂".repeat(2_001));
        assert_eq!(
            reason.encode_utf16().count(),
            MAX_PANEL_ARBITER_REASON_UTF16_UNITS
        );
    }
}

#[cfg(test)]
mod arbiter_cost_plan_tests {
    use super::ArbiterCostPlan;

    #[test]
    fn known_ignores_streamed_cost() {
        // BestOfN/Majority: the streamed usage is the replayed leg, already in Σ legs.
        let plan = ArbiterCostPlan::Known(Some(0.0021));
        assert_eq!(plan.finalize(Some(999.0)), Some(0.0021)); // streamed cost discarded
        let none = ArbiterCostPlan::Known(None); // Majority: embeddings unmetered
        assert_eq!(none.finalize(Some(999.0)), None);
    }

    #[test]
    fn live_uses_streamed_cost() {
        // Synthesize: fresh arbiter tokens — price what was streamed.
        let plan = ArbiterCostPlan::Live;
        assert_eq!(plan.finalize(Some(0.0042)), Some(0.0042));
        assert_eq!(plan.finalize(None), None); // unpriceable arbiter model
    }
}
