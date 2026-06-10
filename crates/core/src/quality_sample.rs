//! Sampled async quality judge on rerouted (downgraded) traffic.
//!
//! When the routing engine rewrites a request to a model *cheaper* than the one
//! the caller asked for, we want to confirm the optimization preserved quality.
//! This module samples ~2% of those rerouted-down requests and, **after the user
//! response has already been returned**, dispatches a cheap-model quality judge
//! that compares the served (cheaper-model) output against a one-shot original-
//! model reference. The verdict is recorded as a [`tt_plan_core::SampleScore`] +
//! aggregated [`tt_plan_core::RiskBand`] — the trust signal that gates aggressive
//! optimization.
//!
//! # Hard invariants
//!
//! 1. **Zero added user latency.** The judge runs in a detached
//!    [`tokio::spawn`] kicked off *after* the HTTP response is constructed. The
//!    response path never awaits the judge. This is enforced structurally: the
//!    only public entry point, [`spawn_quality_judge`], takes owned data and
//!    returns immediately.
//! 2. **Reroute-down only.** A request is eligible only when the routing engine
//!    rewrote `req.model` AND the served model is cheaper than the originally
//!    requested one (see [`is_downgrade`]). A non-rerouted request — or a reroute
//!    that did not lower cost — is never judged.
//! 3. **One task class (MVP).** Only [`JudgeTaskClass::ChatCompletions`] is
//!    sampled. The class filter is explicit + extensible so a follow-up can opt
//!    additional classes in.
//! 4. **Deterministic-but-uniform sampling.** [`should_sample`] hashes the trace
//!    id to a uniform `[0, 1)` fraction and compares it to the rate. Same trace +
//!    rate → same decision; across traces the keep-set is uniform. Rate `1.0`
//!    judges every eligible request, `0.0` judges none.
//! 5. **Record only.** The judge records a score + risk band. It never pauses a
//!    route — auto-pause is a deliberate follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use tt_plan_core::{JudgeProvider, JudgeVerdict, QualityError, RiskBand, SampleScore};
use tt_shared::{
    messages::Message, ChatCompletionRequest, ChatCompletionResponse, MessageContent, ModelPricing,
    Provider, RequestContext, Usage,
};
use uuid::Uuid;

/// Default fraction of eligible (rerouted-down) requests to judge. Spec target
/// is "~2% of rerouted traffic". Overridable via `TT_JUDGE_SAMPLE_RATE`.
pub const DEFAULT_JUDGE_SAMPLE_RATE: f64 = 0.02;

/// Default judge model — a cheap rubric scorer. Overridable via `TT_JUDGE_MODEL`.
/// `gpt-4o-mini` is the cheapest broadly-available OpenAI chat model and a
/// reasonable default judge; operators pick their own cheap model in config.
pub const DEFAULT_JUDGE_MODEL: &str = "gpt-4o-mini";

/// The task classes the sampled judge can score. MVP enables only
/// [`JudgeTaskClass::ChatCompletions`]; the enum exists so additional classes
/// (embeddings re-rank, messages-ingress, …) can be opted in without reshaping
/// the call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JudgeTaskClass {
    /// `POST /v1/chat/completions` — the most general class, enabled for the MVP.
    ChatCompletions,
}

impl JudgeTaskClass {
    /// Whether this task class is sampled in the current build. Only
    /// `ChatCompletions` is in scope for the MVP; everything else is opt-in
    /// later. Keeping the filter here (rather than inline at the call site) is
    /// what makes the scope explicit + extensible.
    #[must_use]
    pub fn is_sampled(self) -> bool {
        matches!(self, JudgeTaskClass::ChatCompletions)
    }
}

/// Configuration for the sampled quality judge. Built from the environment via
/// [`JudgeConfig::from_env`]; defaults keep the judge **off** until a judge
/// model + an enabled flag are configured, so wiring it changes no behavior for
/// operators who don't opt in.
#[derive(Debug, Clone)]
pub struct JudgeConfig {
    /// Master enable flag (`TT_JUDGE_ENABLED`). Off by default.
    pub enabled: bool,
    /// Fraction of eligible requests to judge, in `[0, 1]`
    /// (`TT_JUDGE_SAMPLE_RATE`, default [`DEFAULT_JUDGE_SAMPLE_RATE`]).
    pub sample_rate: f64,
    /// Cheap judge model id (`TT_JUDGE_MODEL`, default [`DEFAULT_JUDGE_MODEL`]).
    pub judge_model: String,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_rate: DEFAULT_JUDGE_SAMPLE_RATE,
            judge_model: DEFAULT_JUDGE_MODEL.to_string(),
        }
    }
}

impl JudgeConfig {
    /// Read the judge config from `TT_JUDGE_*` env vars. Malformed values fall
    /// back to the defaults; the rate is clamped to `[0, 1]`.
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = std::env::var("TT_JUDGE_ENABLED")
            .ok()
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes" || v == "on"
            })
            .unwrap_or(false);
        let sample_rate = std::env::var("TT_JUDGE_SAMPLE_RATE")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .map(|r| r.clamp(0.0, 1.0))
            .unwrap_or(DEFAULT_JUDGE_SAMPLE_RATE);
        let judge_model = std::env::var("TT_JUDGE_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_JUDGE_MODEL.to_string());
        Self {
            enabled,
            sample_rate,
            judge_model,
        }
    }
}

/// The outcome recorded for one judged request: the per-sample score plus the
/// aggregate [`RiskBand`] derived from it. For a single sample the band reflects
/// just that verdict (`Acceptable`/`Unclear` → `Low`, `Degraded` → `High`); a
/// store-backed sink aggregates many of these per route over time.
#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    /// Org the request belonged to (for per-org/route aggregation).
    pub org_id: Uuid,
    /// The matched route's id, when known — the unit a follow-up would pause.
    pub route_id: Option<Uuid>,
    /// The originally-requested (more expensive) model.
    pub requested_model: String,
    /// The served (cheaper, rerouted-to) model whose quality we judged.
    pub served_model: String,
    /// The per-sample judge score (verdict + reason + request id).
    pub score: SampleScore,
    /// Aggregate risk band for this sample.
    pub risk_band: RiskBand,
}

/// Sink that records a [`JudgeOutcome`]. Production wires a store-backed sink;
/// tests use a recording sink to assert the judge fired and produced a band.
///
/// **Record only** — implementations must not pause routes or mutate live
/// routing. Auto-pause is a deliberate follow-up.
#[async_trait]
pub trait JudgeSink: Send + Sync {
    /// Persist / aggregate one judged sample. Best-effort: errors are the sink's
    /// to log; the caller (a detached task) ignores the result.
    async fn record(&self, outcome: JudgeOutcome);
}

/// Running verdict tally for one `(requested_model → served_model)` swap.
/// Aggregation mirrors `tt_plan_core::score_quality`: `degraded_pct` is computed
/// over *classified* (non-`Unclear`) samples and fed to
/// [`RiskBand::from_degraded_pct`], so a single `Degraded` in a large acceptable
/// set is `Low`, not `High`.
#[derive(Debug, Clone, Copy, Default)]
struct VerdictTally {
    acceptable: u64,
    degraded: u64,
    unclear: u64,
}

impl VerdictTally {
    fn add(&mut self, verdict: JudgeVerdict) {
        match verdict {
            JudgeVerdict::Acceptable => self.acceptable += 1,
            JudgeVerdict::Degraded => self.degraded += 1,
            JudgeVerdict::Unclear => self.unclear += 1,
        }
    }

    /// Aggregate band over the classified samples, or `None` when nothing has
    /// been classified yet (only `Unclear` verdicts → we genuinely don't know).
    fn band(self) -> Option<RiskBand> {
        let classified = self.acceptable + self.degraded;
        if classified == 0 {
            return None;
        }
        let degraded_pct = self.degraded as f64 / classified as f64 * 100.0;
        Some(RiskBand::from_degraded_pct(degraded_pct))
    }
}

/// An in-process, store-backed [`JudgeSink`] that aggregates judged outcomes per
/// `(requested_model → served_model)` swap and exposes the aggregate
/// [`tt_preview::QualityRiskBand`] for suggestion enrichment.
///
/// This is the production join the spec asks for: the live judge records into
/// it (closing the persistence gap), and [`enrich_suggestions`] reads from it
/// through the `tt_preview` enrichment seam (closing the lookup gap) so a live
/// judge outcome flows end-to-end into a populated suggestion band — replacing
/// the hard-coded `Unknown`. It is in-process (per-instance) and bounded by the
/// distinct swap count; a follow-up can back it with Postgres without changing
/// the call sites.
///
/// **Record only** — it never pauses a route or mutates live routing.
#[derive(Debug, Default)]
pub struct InMemoryJudgeBandStore {
    /// `(requested_model, served_model) → running verdict tally`.
    tallies: std::sync::Mutex<std::collections::HashMap<(String, String), VerdictTally>>,
}

impl InMemoryJudgeBandStore {
    /// Fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Aggregate [`tt_preview::QualityRiskBand`] for one swap, or `None` when the
    /// judge has not classified that swap yet (no samples, or only `Unclear`).
    /// `None` keeps an unjudged swap honestly `Unknown` at the call site.
    #[must_use]
    pub fn band_for(
        &self,
        requested_model: &str,
        served_model: &str,
    ) -> Option<tt_preview::QualityRiskBand> {
        let key = (requested_model.to_string(), served_model.to_string());
        let tallies = self.tallies.lock().expect("judge band store poisoned");
        tallies
            .get(&key)
            .copied()
            .and_then(VerdictTally::band)
            .map(risk_band_to_preview)
    }

    /// Enrich `suggestions` (all swaps *from* `requested_model`) in place,
    /// lifting each from `Unknown` to its judged band where one exists. Joins
    /// the recorded judge outcomes to the suggestion pill through the
    /// `tt_preview` enrichment seam.
    pub fn enrich_suggestions(
        &self,
        requested_model: &str,
        suggestions: &mut [tt_preview::RouteSuggestion],
    ) {
        tt_preview::route_suggestions::enrich_with_judged_bands(suggestions, |s| {
            self.band_for(requested_model, &s.model)
        });
    }
}

#[async_trait]
impl JudgeSink for InMemoryJudgeBandStore {
    async fn record(&self, outcome: JudgeOutcome) {
        let key = (outcome.requested_model, outcome.served_model);
        let mut tallies = self.tallies.lock().expect("judge band store poisoned");
        tallies.entry(key).or_default().add(outcome.score.verdict);
    }
}

/// Map a plan-core [`RiskBand`] onto the reserved `tt_preview::QualityRiskBand`
/// hook (currently hard-coded to `Unknown` in `plan_suggest`). This is the
/// adapter that lets a live judge outcome populate the suggestion pill.
#[must_use]
pub fn risk_band_to_preview(band: RiskBand) -> tt_preview::QualityRiskBand {
    match band {
        RiskBand::Low => tt_preview::QualityRiskBand::Low,
        RiskBand::Medium => tt_preview::QualityRiskBand::Medium,
        RiskBand::High => tt_preview::QualityRiskBand::High,
    }
}

/// Aggregate risk band for a single verdict. `Degraded` is the only signal that
/// the optimization hurt quality, so it maps to `High`; `Acceptable` and the
/// non-committal `Unclear` map to `Low` (mirrors `score_quality`, which excludes
/// `Unclear` from the degraded denominator).
#[must_use]
pub fn risk_band_for_verdict(verdict: JudgeVerdict) -> RiskBand {
    match verdict {
        JudgeVerdict::Degraded => RiskBand::High,
        JudgeVerdict::Acceptable | JudgeVerdict::Unclear => RiskBand::Low,
    }
}

/// Lowercase wire string for a verdict (`acceptable` / `degraded` / `unclear`),
/// matching the `tt_plan_core::JudgeVerdict` serde representation. Used on the
/// `tokentrimmer.quality.verdict` telemetry attribute.
#[must_use]
pub fn verdict_str(verdict: JudgeVerdict) -> &'static str {
    match verdict {
        JudgeVerdict::Acceptable => "acceptable",
        JudgeVerdict::Degraded => "degraded",
        JudgeVerdict::Unclear => "unclear",
    }
}

/// Lowercase wire string for a risk band (`low` / `medium` / `high`), matching
/// the `tt_plan_core::RiskBand` serde representation. Emitted on the
/// `tokentrimmer.quality.band` telemetry attribute (the live surface; the
/// `x-tokentrimmer-quality-band` header name is reserved but not emitted because
/// the verdict is produced asynchronously after the response is built).
#[must_use]
pub fn band_str(band: RiskBand) -> &'static str {
    match band {
        RiskBand::Low => "low",
        RiskBand::Medium => "medium",
        RiskBand::High => "high",
    }
}

/// Deterministic-but-uniform sampling decision for a request, keyed on its trace
/// id. Returns `true` for a `rate` fraction of trace ids, uniformly.
///
/// Implementation: take the low 53 bits of the trace id's first 8 bytes (a v7
/// UUID is effectively uniform there once the timestamp prefix is mixed by the
/// hash below) and divide into `[0, 1)`. We hash the full 16-byte uuid with
/// FxHash-free `DefaultHasher` to avoid any structure in the timestamp/counter
/// prefix biasing the low bits. `rate <= 0` never samples; `rate >= 1` always.
#[must_use]
pub fn should_sample(trace_id: Uuid, rate: f64) -> bool {
    if rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    trace_id.hash(&mut hasher);
    let h = hasher.finish();
    // Map the 64-bit hash uniformly into [0, 1) using the top 53 bits (f64
    // mantissa width) so the fraction is exact.
    let frac = (h >> 11) as f64 / (1u64 << 53) as f64;
    frac < rate
}

/// Whether a reroute lowered the request's cost — i.e. the served model is
/// cheaper than the originally requested model for this request's realized usage.
///
/// Both pricings must be known; an unknown price on either side returns `false`
/// (we don't judge a reroute we can't prove was a downgrade). Cost is computed on
/// the **realized** `usage` so a tiny token count can't make a nominally-cheaper
/// model look more expensive.
#[must_use]
pub fn is_downgrade(
    requested_pricing: Option<&ModelPricing>,
    served_pricing: Option<&ModelPricing>,
    usage: &Usage,
) -> bool {
    let (Some(req_pr), Some(served_pr)) = (requested_pricing, served_pricing) else {
        return false;
    };
    let cost = |p: &ModelPricing| {
        // `Usage` token counts are u64; cast to f64 for the rate multiply (token
        // counts never approach 2^53, so this is lossless in practice).
        (usage.prompt_tokens as f64 * p.input_per_million
            + usage.completion_tokens as f64 * p.output_per_million)
            / 1_000_000.0
    };
    served_pr_cost_is_lower(cost(req_pr), cost(served_pr))
}

/// Strictly-lower comparison with a tiny epsilon so equal-priced models (e.g. a
/// same-family alias) aren't counted as a downgrade.
fn served_pr_cost_is_lower(requested_cost: f64, served_cost: f64) -> bool {
    served_cost + f64::EPSILON < requested_cost
}

/// Extract the assistant text from a chat completion response. Concatenates the
/// text of every choice's assistant message; tool-call-only responses yield an
/// empty string (and are skipped by the caller before reaching the judge).
fn response_text(resp: &ChatCompletionResponse) -> String {
    resp.choices
        .iter()
        .filter_map(|c| match &c.message {
            Message::Assistant { content, .. } => content.as_ref().map(content_text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                tt_shared::messages::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// A [`JudgeProvider`] backed by a real gateway [`Provider`] + a cheap judge
/// model. Sends the judge rubric as a chat completion and parses the verdict.
///
/// The judge prompt asks the cheap model to compare an original-model reference
/// answer against the served (cheaper-model) answer for the same input and emit
/// one of `ACCEPTABLE` / `DEGRADED` / `UNCLEAR` plus a one-line reason. Any
/// dispatch error surfaces as [`QualityError::Judge`]; an unparseable verdict is
/// recorded as [`JudgeVerdict::Unclear`] (never an error) so a flaky judge can't
/// poison the score.
pub struct GatewayLlmJudge {
    provider: Arc<dyn Provider>,
    judge_model: String,
    ctx: RequestContext,
}

impl GatewayLlmJudge {
    /// Build a judge that dispatches the rubric to `judge_model` on `provider`
    /// using `ctx` (org credentials etc.). The caller is responsible for picking
    /// a *cheap* model and a provider that serves it.
    pub fn new(provider: Arc<dyn Provider>, judge_model: String, ctx: RequestContext) -> Self {
        Self {
            provider,
            judge_model,
            ctx,
        }
    }

    fn build_request(&self, input: &str, original: &str, proposed: &str) -> ChatCompletionRequest {
        let system = "You are a strict quality judge for an LLM cost-optimization gateway. \
A request was served by a CHEAPER model than the one originally requested. \
Given the user input, the ORIGINAL model's reference answer, and the CHEAPER model's \
answer, decide whether the cheaper answer preserved quality. Reply with EXACTLY one \
word on the first line — ACCEPTABLE (interchangeable quality), DEGRADED (materially \
worse), or UNCLEAR (cannot tell) — then a one-line reason.";
        let user = format!(
            "INPUT:\n{input}\n\nORIGINAL ANSWER:\n{original}\n\nCHEAPER ANSWER:\n{proposed}\n\nVerdict:"
        );
        ChatCompletionRequest {
            model: self.judge_model.clone(),
            messages: vec![
                Message::System {
                    content: MessageContent::Text(system.to_string()),
                },
                Message::User {
                    content: MessageContent::Text(user),
                    name: None,
                },
            ],
            // Deterministic, short scoring: temperature 0, capped output. All
            // other fields default (serde fills the skip_serializing_if optionals
            // + flatten/extras maps) — the same idiom the routing tests use.
            temperature: Some(0.0),
            max_tokens: Some(64),
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#)
                .expect("static minimal request JSON is valid")
        }
    }
}

/// Parse a judge model's reply into a verdict + trimmed reason. Case-insensitive
/// keyword match on the reply; anything unrecognized is `Unclear`.
fn parse_verdict(reply: &str) -> (JudgeVerdict, String) {
    let upper = reply.to_ascii_uppercase();
    let verdict = if upper.contains("DEGRADED") {
        JudgeVerdict::Degraded
    } else if upper.contains("ACCEPTABLE") {
        JudgeVerdict::Acceptable
    } else {
        JudgeVerdict::Unclear
    };
    let reason = reply.trim().lines().next().unwrap_or("").trim().to_string();
    (verdict, reason)
}

#[async_trait]
impl JudgeProvider for GatewayLlmJudge {
    async fn judge(
        &self,
        input_body: &str,
        original_response: &str,
        proposed_response: &str,
    ) -> Result<(JudgeVerdict, String), QualityError> {
        let req = self.build_request(input_body, original_response, proposed_response);
        let resp = self
            .provider
            .chat_completion(req, &self.ctx)
            .await
            .map_err(|e| QualityError::Judge(e.to_string()))?;
        let reply = response_text(&resp);
        Ok(parse_verdict(&reply))
    }
}

/// Everything the detached judge task needs, captured by value so the spawn
/// borrows nothing from the request handler. Built on the response path (cheap:
/// a few clones) and moved into the [`tokio::spawn`] in [`spawn_quality_judge`].
pub struct QualityJudgeJob {
    /// Judge backend (a [`GatewayLlmJudge`] in production).
    pub judge: Arc<dyn JudgeProvider>,
    /// Where to record the outcome.
    pub sink: Arc<dyn JudgeSink>,
    /// The org the request belonged to.
    pub org_id: Uuid,
    /// The matched route id, when known.
    pub route_id: Option<Uuid>,
    /// Stable id for the judged sample (the request's trace id).
    pub request_id: Uuid,
    /// Originally requested (expensive) model.
    pub requested_model: String,
    /// Served (cheaper) model.
    pub served_model: String,
    /// The user input text the judge compares answers against.
    pub input_text: String,
    /// The served (cheaper-model) answer.
    pub served_answer: String,
    /// How to obtain the original-model reference answer the judge compares
    /// against. Resolved INSIDE the detached task so the reference dispatch never
    /// touches the user response path.
    pub reference: ReferenceSource,
}

/// Where the original-model reference answer comes from.
pub enum ReferenceSource {
    /// A ready reference string (tests supply this directly).
    Ready(String),
    /// Re-dispatch the original model on its source provider to produce a fresh
    /// reference. Runs in the detached judge task — off the user response path —
    /// so it adds zero user latency. The judge only fires for a ~2% sample of
    /// downgraded requests, bounding the extra spend.
    Dispatch {
        /// Source provider that serves the original (expensive) model.
        provider: Arc<dyn Provider>,
        /// The original request (original model, original messages).
        request: Box<ChatCompletionRequest>,
        /// Request context (org credentials) for the source provider.
        ctx: Box<RequestContext>,
    },
}

impl ReferenceSource {
    /// Resolve the reference answer. `Ready` returns the string; `Dispatch`
    /// calls the source provider and extracts the assistant text. A dispatch
    /// error surfaces as [`QualityError::Judge`] so the job records nothing
    /// rather than a misleading verdict.
    async fn resolve(self) -> Result<String, QualityError> {
        match self {
            ReferenceSource::Ready(s) => Ok(s),
            ReferenceSource::Dispatch {
                provider,
                request,
                ctx,
            } => {
                let resp = provider
                    .chat_completion(*request, &ctx)
                    .await
                    .map_err(|e| QualityError::Judge(format!("reference dispatch: {e}")))?;
                Ok(response_text(&resp))
            }
        }
    }
}

/// Run one judge job to completion: call the judge, build a [`SampleScore`] +
/// [`RiskBand`], and record the [`JudgeOutcome`]. Returns `Ok(())` even when the
/// judge declines (records `Unclear`); only a hard judge dispatch error or a
/// missing reference short-circuits without recording.
async fn run_job(job: QualityJudgeJob) -> Result<(), QualityError> {
    let reference = job.reference.resolve().await?;
    if reference.trim().is_empty() {
        // Empty reference (e.g. a tool-call-only original response) — nothing to
        // compare against, so record nothing rather than a meaningless verdict.
        return Ok(());
    }
    let (verdict, reason) = job
        .judge
        .judge(&job.input_text, &reference, &job.served_answer)
        .await?;
    let risk_band = risk_band_for_verdict(verdict);

    // Surface the per-request verdict on the telemetry path — but ONLY here,
    // where a judge actually ran and produced a verdict. An unjudged request
    // never reaches `run_job`, so the quality attributes are never fabricated
    // for one. This records onto the detached judge task's span (off the user
    // response path → zero user latency); the hosted side ingests + aggregates
    // these into "Quality preserved: …% (n=…)" via
    // `tt_plan_core::quality_preserved_summary`.
    record_quality_verdict_telemetry(
        job.request_id,
        &job.requested_model,
        &job.served_model,
        verdict,
        risk_band,
    );

    let score = SampleScore {
        request_id: job.request_id,
        verdict,
        reason,
    };
    job.sink
        .record(JudgeOutcome {
            org_id: job.org_id,
            route_id: job.route_id,
            requested_model: job.requested_model,
            served_model: job.served_model,
            score,
            risk_band,
        })
        .await;
    Ok(())
}

/// Stamp the per-request quality verdict onto the current (judge-task) span via
/// the telemetry crate, using the same per-request values recorded into the
/// sink. The score is the classified verdict's `[0, 1]` value; an `Unclear`
/// verdict has no valence, so `quality_score()` is `None` and the score
/// attribute is omitted (band/verdict still surface) rather than fabricating a
/// "preserved" score — keeping the emitted per-request scores consistent with
/// `quality_preserved_summary`, which drops `Unclear` from its denominator.
fn record_quality_verdict_telemetry(
    request_id: Uuid,
    requested_model: &str,
    served_model: &str,
    verdict: JudgeVerdict,
    risk_band: RiskBand,
) {
    tt_telemetry::gen_ai::record_quality_verdict(
        &tracing::Span::current(),
        &tt_telemetry::gen_ai::QualityVerdictAttributes {
            request_id: &request_id.to_string(),
            requested_model,
            served_model,
            score: verdict.quality_score(),
            band: band_str(risk_band),
            verdict: verdict_str(verdict),
        },
    );
}

/// Spawn the judge job on the tokio runtime and return immediately.
///
/// **This is the only public way to run the judge from the request path, and it
/// adds zero latency:** it does not await the job — it hands ownership to a
/// detached task and returns. The HTTP response is built and returned by the
/// caller before (and independently of) this task making any progress.
pub fn spawn_quality_judge(job: QualityJudgeJob) {
    use tracing::Instrument as _;
    // Instrument the detached task with its own `quality_judge` span so the
    // per-request quality verdict recorded inside `run_job` has a stable home
    // span (the cloud ingests these), and the judge's own dispatch is traceable
    // separately from the user request span it never touches.
    let request_id = job.request_id;
    let span = tracing::info_span!("quality_judge", request_id = %request_id);
    tokio::spawn(
        async move {
            if let Err(e) = run_job(job).await {
                tracing::warn!(error = %e, "quality judge sample failed");
            }
        }
        .instrument(span),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn pricing(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        }
    }

    fn usage() -> Usage {
        Usage {
            prompt_tokens: 100,
            completion_tokens: 100,
            total_tokens: 200,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
        }
    }

    #[test]
    fn downgrade_detected_when_served_cheaper() {
        // gpt-4o (5/15) → gpt-4o-mini (0.15/0.6): clearly cheaper.
        assert!(is_downgrade(
            Some(&pricing(5.0, 15.0)),
            Some(&pricing(0.15, 0.6)),
            &usage()
        ));
    }

    #[test]
    fn not_a_downgrade_when_served_more_expensive_or_equal() {
        // Equal price → not a downgrade.
        assert!(!is_downgrade(
            Some(&pricing(1.0, 2.0)),
            Some(&pricing(1.0, 2.0)),
            &usage()
        ));
        // Served pricier → not a downgrade.
        assert!(!is_downgrade(
            Some(&pricing(0.15, 0.6)),
            Some(&pricing(5.0, 15.0)),
            &usage()
        ));
    }

    #[test]
    fn unknown_pricing_is_never_a_downgrade() {
        assert!(!is_downgrade(None, Some(&pricing(0.1, 0.1)), &usage()));
        assert!(!is_downgrade(Some(&pricing(5.0, 5.0)), None, &usage()));
        assert!(!is_downgrade(None, None, &usage()));
    }

    #[test]
    fn sample_rate_zero_never_samples_one_always() {
        for _ in 0..50 {
            let id = Uuid::now_v7();
            assert!(!should_sample(id, 0.0));
            assert!(should_sample(id, 1.0));
        }
    }

    #[test]
    fn sample_is_deterministic_per_trace() {
        let id = Uuid::now_v7();
        let a = should_sample(id, 0.5);
        let b = should_sample(id, 0.5);
        assert_eq!(a, b, "same trace + rate must give the same decision");
    }

    #[test]
    fn sample_rate_is_approximately_uniform() {
        // Over many distinct trace ids, ~rate fraction should be sampled.
        let n = 20_000;
        let rate = 0.02;
        let mut kept = 0;
        for _ in 0..n {
            if should_sample(Uuid::now_v7(), rate) {
                kept += 1;
            }
        }
        let frac = kept as f64 / n as f64;
        // Generous band around 2% (binomial noise + v7 timestamp structure).
        assert!(
            (0.01..0.035).contains(&frac),
            "kept fraction {frac} should be near {rate}"
        );
    }

    #[test]
    fn verdict_parsing() {
        assert_eq!(
            parse_verdict("ACCEPTABLE\nsame substance").0,
            JudgeVerdict::Acceptable
        );
        assert_eq!(
            parse_verdict("degraded — missed the key point").0,
            JudgeVerdict::Degraded
        );
        assert_eq!(parse_verdict("hmm not sure").0, JudgeVerdict::Unclear);
        assert_eq!(parse_verdict("ACCEPTABLE\nreason").1, "ACCEPTABLE");
    }

    #[test]
    fn risk_band_mapping() {
        assert_eq!(
            risk_band_for_verdict(JudgeVerdict::Degraded),
            RiskBand::High
        );
        assert_eq!(
            risk_band_for_verdict(JudgeVerdict::Acceptable),
            RiskBand::Low
        );
        assert_eq!(risk_band_for_verdict(JudgeVerdict::Unclear), RiskBand::Low);
        assert_eq!(
            risk_band_to_preview(RiskBand::High),
            tt_preview::QualityRiskBand::High
        );
    }

    #[test]
    fn verdict_and_band_wire_strings() {
        assert_eq!(verdict_str(JudgeVerdict::Acceptable), "acceptable");
        assert_eq!(verdict_str(JudgeVerdict::Degraded), "degraded");
        assert_eq!(verdict_str(JudgeVerdict::Unclear), "unclear");
        assert_eq!(band_str(RiskBand::Low), "low");
        assert_eq!(band_str(RiskBand::Medium), "medium");
        assert_eq!(band_str(RiskBand::High), "high");
    }

    #[test]
    fn task_class_only_chat_completions_sampled() {
        assert!(JudgeTaskClass::ChatCompletions.is_sampled());
    }

    #[test]
    fn config_defaults_off_with_2pct_rate() {
        let c = JudgeConfig::default();
        assert!(!c.enabled);
        assert!((c.sample_rate - 0.02).abs() < 1e-12);
        assert_eq!(c.judge_model, DEFAULT_JUDGE_MODEL);
    }

    fn outcome(requested: &str, served: &str, verdict: JudgeVerdict) -> JudgeOutcome {
        JudgeOutcome {
            org_id: Uuid::now_v7(),
            route_id: Some(Uuid::now_v7()),
            requested_model: requested.to_string(),
            served_model: served.to_string(),
            score: SampleScore {
                request_id: Uuid::now_v7(),
                verdict,
                reason: "r".to_string(),
            },
            risk_band: risk_band_for_verdict(verdict),
        }
    }

    /// Unjudged swap → `None` (caller keeps it `Unknown`).
    #[tokio::test]
    async fn band_store_returns_none_for_unjudged_swap() {
        let store = InMemoryJudgeBandStore::new();
        assert_eq!(store.band_for("gpt-4o", "gpt-4o-mini"), None);
    }

    /// A single `Degraded` sample aggregates to `High` for that swap.
    #[tokio::test]
    async fn band_store_single_degraded_is_high() {
        let store = InMemoryJudgeBandStore::new();
        store
            .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Degraded))
            .await;
        assert_eq!(
            store.band_for("gpt-4o", "gpt-4o-mini"),
            Some(tt_preview::QualityRiskBand::High)
        );
    }

    /// Aggregation matches `score_quality`: one `Degraded` in a large acceptable
    /// set is `Low` (degraded share ≤ 5%), not `High`.
    #[tokio::test]
    async fn band_store_aggregates_degraded_pct_over_classified() {
        let store = InMemoryJudgeBandStore::new();
        for _ in 0..30 {
            store
                .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Acceptable))
                .await;
        }
        store
            .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Degraded))
            .await;
        // 1 / 31 ≈ 3.2% degraded → Low.
        assert_eq!(
            store.band_for("gpt-4o", "gpt-4o-mini"),
            Some(tt_preview::QualityRiskBand::Low)
        );
    }

    /// Only-`Unclear` samples stay `None` (genuinely unknown), and `Unclear` is
    /// excluded from the degraded denominator.
    #[tokio::test]
    async fn band_store_only_unclear_is_none() {
        let store = InMemoryJudgeBandStore::new();
        store
            .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Unclear))
            .await;
        assert_eq!(store.band_for("gpt-4o", "gpt-4o-mini"), None);
    }

    /// End-to-end join: a recorded outcome flows through the enrichment seam to
    /// populate the suggestion band, replacing the hard-coded `Unknown`. An
    /// unjudged swap in the same batch stays `Unknown`.
    #[tokio::test]
    async fn band_store_enriches_suggestions_through_seam() {
        let store = InMemoryJudgeBandStore::new();
        store
            .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Degraded))
            .await;

        let mut suggestions = vec![
            tt_preview::RouteSuggestion {
                route: "swap-to-gpt-4o-mini".into(),
                model: "gpt-4o-mini".into(),
                cost_usd: 0.001,
                savings_usd: 0.01,
                quality_risk_band: tt_preview::QualityRiskBand::Unknown,
                rationale: "x".into(),
                applicable: true,
            },
            tt_preview::RouteSuggestion {
                route: "swap-to-haiku".into(),
                model: "claude-haiku-4-5".into(),
                cost_usd: 0.001,
                savings_usd: 0.01,
                quality_risk_band: tt_preview::QualityRiskBand::Unknown,
                rationale: "x".into(),
                applicable: true,
            },
        ];

        store.enrich_suggestions("gpt-4o", &mut suggestions);

        // Judged swap → lifted to High.
        assert_eq!(
            suggestions[0].quality_risk_band,
            tt_preview::QualityRiskBand::High
        );
        // Unjudged swap → stays honestly Unknown.
        assert_eq!(
            suggestions[1].quality_risk_band,
            tt_preview::QualityRiskBand::Unknown
        );
    }

    // ── Telemetry surfacing of the per-request verdict (JUDGE-SURFACE) ──────
    //
    // The judge runs detached, AFTER the user response, so the per-request
    // verdict is surfaced on the JUDGE TASK's own span (zero user latency). The
    // hosted side ingests these `tokentrimmer.quality.*` attributes and
    // aggregates them into "Quality preserved: …% (n=…)". A request the judge
    // never scored carries no quality attributes at all (no fabricated verdict).

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::Value;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::TracerProvider;
    use std::collections::HashMap;
    use tracing_subscriber::prelude::*;

    /// Sink that swallows outcomes — the telemetry tests assert on the span, not
    /// the sink.
    struct NullSink;
    #[async_trait]
    impl JudgeSink for NullSink {
        async fn record(&self, _outcome: JudgeOutcome) {}
    }

    /// Drive `f` under a scoped OTel subscriber on a current-thread runtime and
    /// return the attributes recorded on the single `quality_judge` span as a
    /// name→Value map. `run_job` is async; a current-thread runtime keeps the
    /// future on the subscriber's thread so the span reaches the exporter.
    fn capture_judge_span_attributes<F, Fut>(f: F) -> HashMap<String, Value>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("judge-test");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::registry().with(otel_layer);

        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");
            rt.block_on(async {
                use tracing::Instrument as _;
                let span = tracing::info_span!("quality_judge");
                f().instrument(span).await;
            });
        });

        provider.force_flush();
        let spans = exporter.get_finished_spans().expect("finished spans");
        spans
            .into_iter()
            .find(|s| s.name == "quality_judge")
            .map(|s| {
                s.attributes
                    .into_iter()
                    .map(|kv| (kv.key.to_string(), kv.value))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn job_with(reference: ReferenceSource, verdict_word: &'static str) -> QualityJudgeJob {
        QualityJudgeJob {
            judge: Arc::new(tt_plan_core::MockJudge {
                verdict: match verdict_word {
                    "degraded" => JudgeVerdict::Degraded,
                    "unclear" => JudgeVerdict::Unclear,
                    _ => JudgeVerdict::Acceptable,
                },
                reason: "because".to_string(),
            }),
            sink: Arc::new(NullSink),
            org_id: Uuid::now_v7(),
            route_id: Some(Uuid::now_v7()),
            request_id: Uuid::now_v7(),
            requested_model: "gpt-4o".to_string(),
            served_model: "gpt-4o-mini".to_string(),
            input_text: "in".to_string(),
            served_answer: "served".to_string(),
            reference,
        }
    }

    /// A judged (Acceptable) request stamps `quality.score = 1.0`, `band = low`,
    /// `verdict = acceptable`, plus the request id + the swapped models.
    #[test]
    fn judged_request_records_quality_verdict_on_span() {
        let job = job_with(
            ReferenceSource::Ready("reference".to_string()),
            "acceptable",
        );
        let request_id = job.request_id.to_string();
        let attrs = capture_judge_span_attributes(|| async move {
            run_job(job).await.expect("run_job ok");
        });

        assert_eq!(
            attrs.get("tokentrimmer.quality.score"),
            Some(&Value::F64(1.0)),
            "acceptable verdict → preserved score 1.0"
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.band"),
            Some(&Value::String("low".into()))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.verdict"),
            Some(&Value::String("acceptable".into()))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.request_id"),
            Some(&Value::String(request_id.into()))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.requested_model"),
            Some(&Value::String("gpt-4o".into()))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.served_model"),
            Some(&Value::String("gpt-4o-mini".into()))
        );
    }

    /// A Degraded verdict surfaces `score = 0.0` / `band = high`.
    #[test]
    fn degraded_request_records_zero_score_high_band() {
        let job = job_with(ReferenceSource::Ready("reference".to_string()), "degraded");
        let attrs = capture_judge_span_attributes(|| async move {
            run_job(job).await.expect("run_job ok");
        });
        assert_eq!(
            attrs.get("tokentrimmer.quality.score"),
            Some(&Value::F64(0.0))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.band"),
            Some(&Value::String("high".into()))
        );
    }

    /// An Unclear verdict was judged, so it surfaces `band` + `verdict`, but it
    /// has no valence — the `score` attribute is OMITTED rather than defaulted to
    /// a "preserved" `1.0`. This keeps the emitted per-request scores consistent
    /// with `quality_preserved_summary`, which drops `Unclear` from its
    /// denominator (an emitted `1.0` would inflate an averaged headline).
    #[test]
    fn unclear_request_omits_score_but_records_band_and_verdict() {
        let job = job_with(ReferenceSource::Ready("reference".to_string()), "unclear");
        let attrs = capture_judge_span_attributes(|| async move {
            run_job(job).await.expect("run_job ok");
        });
        assert!(
            !attrs.contains_key("tokentrimmer.quality.score"),
            "unclear verdict has no valence → score attribute must be omitted"
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.verdict"),
            Some(&Value::String("unclear".into()))
        );
        // Band still surfaces (the judge ran), so the sample is visible.
        assert!(
            attrs.contains_key("tokentrimmer.quality.band"),
            "a judged unclear request still records its band"
        );
    }

    /// An UNJUDGED request (empty reference → `run_job` records nothing) carries
    /// NO quality attributes — the verdict is never fabricated.
    #[test]
    fn unjudged_request_records_no_quality_attributes() {
        // Empty reference short-circuits run_job before any judge/verdict.
        let job = job_with(ReferenceSource::Ready("   ".to_string()), "acceptable");
        let attrs = capture_judge_span_attributes(|| async move {
            run_job(job).await.expect("run_job ok");
        });
        assert!(
            !attrs.contains_key("tokentrimmer.quality.score"),
            "unjudged request must not carry a quality score"
        );
        assert!(
            !attrs.contains_key("tokentrimmer.quality.band"),
            "unjudged request must not carry a quality band"
        );
        assert!(
            !attrs.contains_key("tokentrimmer.quality.verdict"),
            "unjudged request must not carry a quality verdict"
        );
    }
}
