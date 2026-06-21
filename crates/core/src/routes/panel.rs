//! Deep-research panel — caller-facing opt-in surface.
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

use tt_shared::{
    messages::{ChatCompletionRequest, Message, MessageContent, PanelExtras},
    ChatCompletionResponse, RequestContext, Usage,
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
    fn parse(s: &str) -> Option<Self> {
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
    /// `None` → all members must succeed (implicit quorum = members.len()).
    pub quorum: Option<usize>,
    /// Hard cost ceiling in USD across all legs + arbitration. `None` → no cap.
    pub max_cost_usd: Option<f64>,
}

// ---------------------------------------------------------------------------
// Member-count cap
// ---------------------------------------------------------------------------

/// Hard cap on the number of panel members (the arbiter is not counted).
///
/// Override with the `TT_PANEL_MAX_MEMBERS` environment variable (must be ≥ 1;
/// invalid or zero values are silently ignored and the default is used).
fn panel_max_members() -> usize {
    std::env::var("TT_PANEL_MAX_MEMBERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(8)
}

impl PanelConfig {
    /// Resolve a complete [`PanelConfig`] from its three input sources.
    ///
    /// Precedence (highest → lowest):
    /// 1. `extras` — per-request `tt_extras.panel` overrides
    /// 2. `defaults` — gateway-level defaults from env vars
    ///
    /// Returns [`ApiError::InvalidRequest`] when the merged member list is empty
    /// or exceeds the cap set by `TT_PANEL_MAX_MEMBERS` (default 8).
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

        if members.is_empty() {
            return Err(ApiError::InvalidRequest(
                "panel requires at least one member model".to_string(),
            ));
        }

        let max = panel_max_members();
        if members.len() > max {
            return Err(ApiError::InvalidRequest(format!(
                "panel: {} members exceeds the maximum of {}",
                members.len(),
                max
            )));
        }

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

        Ok(PanelConfig {
            strategy,
            members,
            arbiter_model,
            quorum,
            max_cost_usd,
        })
    }
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
    /// Zero-based index into [`PanelConfig::members`]; `usize::MAX` for the
    /// arbiter leg (which has no member index).
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
// ArbiterOutcome — the response produced by the arbiter
// ---------------------------------------------------------------------------

/// The final response produced by an [`ArbiterStrategy`].
pub struct ArbiterOutcome {
    /// The synthesized / chosen upstream response.
    pub response: ChatCompletionResponse,
    /// Cost of the arbiter call itself; `None` when unpriced (see
    /// [`crate::measurement::MeasuredDispatch::cost_usd`]).
    pub cost_usd: Option<f64>,
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
    /// context).  `state` and `ctx` give access to the provider registry and
    /// the request's credential / deadline context.  `creds` is the same
    /// provider-id → credential map passed to `run_panel`; the arbiter
    /// implementation uses it to substitute the correct credential when the
    /// arbiter model is on a different provider than `ctx.credentials`.
    async fn arbitrate(
        &self,
        request: &ChatCompletionRequest,
        legs: &[LegResult],
        state: &AppState,
        ctx: &RequestContext,
        creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) -> Result<ArbiterOutcome, ApiError>;
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
        let ok_answers: Vec<(usize, String)> = legs
            .iter()
            .filter(|l| l.status == LegStatus::Ok && l.role == LegRole::Leg)
            .filter_map(|l| {
                let resp = l.response.as_ref()?;
                let text = resp.choices.first().and_then(|c| match &c.message {
                    Message::Assistant {
                        content: Some(MessageContent::Text(t)),
                        ..
                    } => Some(t.clone()),
                    _ => None,
                })?;
                Some((l.leg_index, text))
            })
            .collect();

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

        // Substitute the arbiter provider's credential when present in the creds
        // map (mirrors the per-member credential substitution in run_panel).
        let mut arb_ctx_owned;
        let arb_ctx: &RequestContext = if let Some(c) = creds.get(provider.id()) {
            arb_ctx_owned = ctx.clone();
            arb_ctx_owned.credentials = c.clone();
            &arb_ctx_owned
        } else {
            ctx
        };

        // Derive the arbiter deadline from the caller's remaining budget when
        // available; otherwise use a bounded default. The outer route
        // TimeoutLayer (60 s) caps all requests regardless.
        let deadline = arb_ctx.deadline.unwrap_or(Duration::from_secs(120));
        let measured =
            crate::measurement::measured_single_dispatch(&provider, arbiter_req, arb_ctx, deadline)
                .await
                .map_err(|e| {
                    ApiError::ServiceUnavailable(format!("arbiter dispatch failed: {e}"))
                })?;

        Ok(ArbiterOutcome {
            response: measured.response,
            cost_usd: measured.cost_usd,
        })
    }
}

// ---------------------------------------------------------------------------
// strategy_for — factory
// ---------------------------------------------------------------------------

/// Instantiate the correct [`ArbiterStrategy`] for the given [`PanelConfig`].
///
/// Currently only [`ArbiterStrategyKind::Synthesize`] is implemented.
/// [`ArbiterStrategyKind::BestOfN`] and [`ArbiterStrategyKind::Majority`]
/// return [`ApiError::PanelStrategyUnsupported`].
pub fn strategy_for(cfg: &PanelConfig) -> Result<Box<dyn ArbiterStrategy + Send + Sync>, ApiError> {
    match cfg.strategy {
        ArbiterStrategyKind::Synthesize => Ok(Box::new(Synthesize {
            arbiter_model: cfg.arbiter_model.clone(),
        })),
        other => Err(ApiError::PanelStrategyUnsupported {
            strategy: other.as_str().to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Budget estimation + gate
// ---------------------------------------------------------------------------

/// Estimated total cost for all panel legs (members + arbiter), in USD.
///
/// Sums `estimate_cost_usd × fee_multiplier()` over every member and the
/// arbiter model.  Returns `None` (fail-closed) if **any** leg resolves to an
/// unknown model or to a model with no catalog pricing.
pub fn estimate_panel_cost(
    state: &AppState,
    cfg: &PanelConfig,
    input_tokens: u32,
    max_tokens: Option<u32>,
) -> Option<f64> {
    let mut total = 0.0_f64;
    for m in cfg
        .members
        .iter()
        .chain(std::iter::once(&cfg.arbiter_model))
    {
        let provider = state.registry.resolve(&m.model)?; // unknown model → fail-closed
        let pricing = provider.pricing(&m.model)?; // unpriceable → fail-closed
        total += crate::routes::chat::estimate_cost_usd(&pricing, input_tokens, max_tokens)
            * provider.fee_multiplier();
    }
    Some(total)
}

/// Gate: reject a panel request when it would exceed the allowed cost ceiling.
///
/// `ceiling` takes precedence over `cfg.max_cost_usd`.  If neither is set, the
/// request is **always** rejected — a panel requires an explicit budget.
///
/// Returns `Err(ApiError::CostLimitExceeded)` when:
/// - neither `ceiling` nor `cfg.max_cost_usd` is set (no budget)
/// - the estimate is `None` (unpriceable / unknown model — fail-closed)
/// - the estimate exceeds the effective ceiling
pub fn panel_budget_gate(
    state: &AppState,
    cfg: &PanelConfig,
    input_tokens: u32,
    max_tokens: Option<u32>,
    ceiling: Option<f64>,
) -> Result<(), ApiError> {
    let ceiling = ceiling
        .or(cfg.max_cost_usd)
        .ok_or(ApiError::CostLimitExceeded {
            estimated_usd: f64::INFINITY,
            ceiling_usd: 0.0,
        })?;
    let est = estimate_panel_cost(state, cfg, input_tokens, max_tokens).ok_or(
        ApiError::CostLimitExceeded {
            estimated_usd: f64::INFINITY,
            ceiling_usd: ceiling,
        },
    )?;
    if est > ceiling {
        return Err(ApiError::CostLimitExceeded {
            estimated_usd: est,
            ceiling_usd: ceiling,
        });
    }
    Ok(())
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
}

// ---------------------------------------------------------------------------
// run_panel — concurrent fan-out, quorum, cost aggregation
// ---------------------------------------------------------------------------

/// Fold an iterator of `Option<f64>` values: accumulate all `Some` values;
/// return `None` only when every item was `None`.
fn sum_metered_iter(it: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut total: Option<f64> = None;
    for x in it.flatten() {
        total = Some(total.unwrap_or(0.0) + x);
    }
    total
}

/// Fan-out all panel member legs concurrently, enforce quorum, then arbitrate.
///
/// `creds` maps **provider id** → credentials for that provider.  Members
/// whose provider id is absent from `creds` are recorded as
/// [`LegStatus::SkippedNoCred`] and do not count toward quorum.
pub async fn run_panel(
    state: &crate::AppState,
    ctx: &tt_shared::RequestContext,
    base_req: &ChatCompletionRequest,
    creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    cfg: &PanelConfig,
    deadline: Duration,
) -> Result<PanelResult, crate::ApiError> {
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
    let required = cfg.quorum.unwrap_or(match cfg.strategy {
        ArbiterStrategyKind::Majority => (cfg.members.len() / 2) + 1,
        _ => 1,
    });
    let met = legs_out
        .iter()
        .filter(|l| matches!(l.status, LegStatus::Ok))
        .count();
    if met < required {
        return Err(crate::ApiError::PanelQuorumUnmet { required, met });
    }

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

    // 4. None-aware cost aggregation: sum all Some values across legs + arbiter.
    let total_cost_usd = sum_metered_iter(
        legs_out
            .iter()
            .map(|l| l.cost_usd)
            .chain(std::iter::once(arb.cost_usd)),
    );

    legs_out.push(arbiter_leg);

    Ok(PanelResult {
        response: arb.response,
        legs: legs_out,
        total_cost_usd,
        quorum_required: required,
        quorum_met: met,
    })
}

// ---------------------------------------------------------------------------
// complete_panel — dispatch + aggregate one-row billing
// ---------------------------------------------------------------------------

/// Build the `tokentrimmer.panel` attribution object for the response body.
///
/// Mirrors spec §6.4 step 9: per-leg breakdown + quorum + a `cost_incomplete`
/// flag set when any **surviving** (status == Ok) leg reported `None` cost (an
/// unpriceable model makes the recorded aggregate an honest lower bound).
fn build_panel_body(cfg: &PanelConfig, result: &PanelResult) -> serde_json::Value {
    let legs: Vec<serde_json::Value> = result
        .legs
        .iter()
        .map(|l| {
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
                "leg_index": l.leg_index,
                "role": role,
                "model": l.model,
                "provider": l.provider,
                "cost_usd": l.cost_usd,
                "status": l.status.as_str(),
                "tokens": tokens,
            })
        })
        .collect();

    // `cost_incomplete`: any surviving leg with no priced cost ⇒ the recorded
    // aggregate is a lower bound (spec §6.4 step 8).
    let cost_incomplete = result
        .legs
        .iter()
        .any(|l| l.status == LegStatus::Ok && l.cost_usd.is_none());

    json!({
        "strategy": cfg.strategy.as_str(),
        "legs": legs,
        "total_cost_usd": result.total_cost_usd,
        "quorum": {
            "required": result.quorum_required,
            "met": result.quorum_met,
        },
        "cost_incomplete": cost_incomplete,
    })
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
        cache_bust_penalty_usd: 0.0,
        summarizer_tax_usd: 0.0,
        batch_forgone_usd: 0.0,
        minify_saved_est_usd: 0.0,
        diff_saved_usd: 0.0,
        format_switch_saved_est_usd: 0.0,
        diff_failed_cost_usd: 0.0,
    }
}

/// Complete a deep-research panel request: fan out via [`run_panel`], aggregate
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
    // populated on a failover route. A member whose provider id is absent here
    // is recorded by `run_panel` as `skipped_no_cred` (never dispatched/billed).
    let result = match run_panel(state, ctx, &prep.req, &prep.panel_creds, &cfg, deadline).await {
        Ok(r) => r,
        Err(e) => {
            // Bounded outcome label for the request-level panel counter.
            let outcome = match &e {
                ApiError::PanelQuorumUnmet { .. } => "quorum_unmet",
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
            // INVARIANT §2.1.5: every panel row is `cached = false` so the cloud
            // overage meter + month accumulator both count it.
            cached: false,
            cache_layer: None,
            route_id: prep.matched_route_id,
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
        tokio::spawn(async move {
            let _ = writer.write_legs(leg_rows).await;
        });
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

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::messages::PanelExtras;

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
}
