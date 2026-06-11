//! OpenTelemetry GenAI semantic-convention attributes for the gateway request
//! span.
//!
//! When a request completes the gateway knows the provider, the requested and
//! served models, the token usage, the routing/cache outcome, and the
//! TokenTrimmer cost split (the same values it stamps onto the
//! `x-tokentrimmer-*` response headers). This module records those onto the
//! active request span as OpenTelemetry attributes so a trace carries model +
//! token + cost data and downstream tooling (Grafana, Tempo, etc.) can query
//! spend, savings, and cache-hit rate directly from spans.
//!
//! ## Attribute names
//!
//! The `gen_ai.*` keys follow the OpenTelemetry [GenAI semantic conventions]:
//!
//! * `gen_ai.system` — the GenAI provider (`openai`, `anthropic`, …). The newer
//!   semconv renames this to `gen_ai.provider.name`; we emit **both** so
//!   dashboards keyed on either name resolve. (The rename is still
//!   "Development"-status at time of writing; `gen_ai.system` remains the
//!   widely-deployed key.)
//! * `gen_ai.operation.name` — the operation (`chat`, `embeddings`).
//! * `gen_ai.request.model` — the model the caller asked for.
//! * `gen_ai.response.model` — the model that actually served the request
//!   (differs from the request model after routing / cross-model failover).
//! * `gen_ai.usage.input_tokens` / `gen_ai.usage.output_tokens` — token counts.
//!
//! The `tokentrimmer.*` keys mirror the `x-tokentrimmer-*` response headers and
//! are TokenTrimmer-specific (not part of the upstream semconv):
//!
//! * `tokentrimmer.cost_usd` — what the provider actually bills.
//! * `tokentrimmer.baseline_cost_usd` — cost with no TokenTrimmer optimisation.
//! * `tokentrimmer.saved_usd` — TokenTrimmer-attributed savings.
//! * `tokentrimmer.provider_cache_saved_usd` — provider-side cache discount.
//! * `tokentrimmer.cache` — cache outcome (`hit-l1`, `hit-l2`, `miss`, …).
//! * `tokentrimmer.route` — the matched route name, when routing applied.
//!
//! The recording is a pure side-effect on the passed span; it is exercised
//! hermetically with an in-memory span exporter (see the unit tests) and from
//! the gateway integration tests under `crates/core/tests/`.
//!
//! [GenAI semantic conventions]: https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/

use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// `gen_ai.system` — the GenAI provider identifier.
pub const GEN_AI_SYSTEM: &str = "gen_ai.system";
/// `gen_ai.provider.name` — the newer semconv spelling of [`GEN_AI_SYSTEM`].
pub const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
/// `gen_ai.operation.name` — the GenAI operation being performed.
pub const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
/// `gen_ai.request.model` — the model the request was made to.
pub const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
/// `gen_ai.response.model` — the model that generated the response.
pub const GEN_AI_RESPONSE_MODEL: &str = "gen_ai.response.model";
/// `gen_ai.usage.input_tokens` — prompt tokens.
pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
/// `gen_ai.usage.output_tokens` — completion tokens.
pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";

/// `tokentrimmer.cost_usd` — what the provider actually bills (USD).
pub const TT_COST_USD: &str = "tokentrimmer.cost_usd";
/// `tokentrimmer.baseline_cost_usd` — cost without TokenTrimmer (USD).
pub const TT_BASELINE_COST_USD: &str = "tokentrimmer.baseline_cost_usd";
/// `tokentrimmer.saved_usd` — TokenTrimmer-attributed savings (USD).
pub const TT_SAVED_USD: &str = "tokentrimmer.saved_usd";
/// `tokentrimmer.provider_cache_saved_usd` — provider-side cache discount (USD).
pub const TT_PROVIDER_CACHE_SAVED_USD: &str = "tokentrimmer.provider_cache_saved_usd";
/// `tokentrimmer.cache` — cache outcome (`hit-l1`, `hit-l2`, `miss`, `none`, …).
pub const TT_CACHE: &str = "tokentrimmer.cache";
/// `tokentrimmer.route` — the matched route name (when routing applied).
pub const TT_ROUTE: &str = "tokentrimmer.route";
/// `tokentrimmer.traffic_split_pct` — the matched route's canary `traffic_pct`
/// (0-100) when a traffic split was configured. ADDITIVE: omitted entirely when
/// the route declared no split, so dashboards unaware of it are unaffected.
pub const TT_TRAFFIC_SPLIT_PCT: &str = "tokentrimmer.traffic_split_pct";
/// `tokentrimmer.shadow_model` — the shadow-mode candidate model that was ALSO
/// dispatched (and discarded) for this request. ADDITIVE: omitted when no shadow
/// fired.
pub const TT_SHADOW_MODEL: &str = "tokentrimmer.shadow_model";
/// `tokentrimmer.shadow_cost_usd` — the cost (USD) the discarded shadow dispatch
/// incurred, recorded SEPARATELY from `tokentrimmer.cost_usd` so the doubled
/// spend never folds into the served-traffic cost. ADDITIVE: omitted when no
/// shadow fired.
pub const TT_SHADOW_COST_USD: &str = "tokentrimmer.shadow_cost_usd";

/// `tokentrimmer.quality.request_id` — trace/request id of the judged request.
pub const TT_QUALITY_REQUEST_ID: &str = "tokentrimmer.quality.request_id";
/// `tokentrimmer.quality.requested_model` — originally-requested (expensive) model.
pub const TT_QUALITY_REQUESTED_MODEL: &str = "tokentrimmer.quality.requested_model";
/// `tokentrimmer.quality.served_model` — served (cheaper) model whose quality was judged.
pub const TT_QUALITY_SERVED_MODEL: &str = "tokentrimmer.quality.served_model";
/// `tokentrimmer.quality.score` — per-request quality score in `[0, 1]`
/// (`1.0` = preserved, `0.0` = degraded). **Omitted for an `unclear` verdict**
/// (no valence) so averaging the emitted scores matches
/// `quality_preserved_summary`. This is the live wire surface for the score;
/// [`HEADER_QUALITY_SCORE`] is a reserved (not-yet-emitted) header name.
pub const TT_QUALITY_SCORE: &str = "tokentrimmer.quality.score";
/// `tokentrimmer.quality.band` — `low` / `medium` / `high` risk band for the
/// judged sample. Live wire surface for the band; [`HEADER_QUALITY_BAND`] is a
/// reserved (not-yet-emitted) header name.
pub const TT_QUALITY_BAND: &str = "tokentrimmer.quality.band";
/// `tokentrimmer.quality.verdict` — raw judge verdict (`acceptable` / `degraded`
/// / `unclear`).
pub const TT_QUALITY_VERDICT: &str = "tokentrimmer.quality.verdict";
/// `tokentrimmer.quality.judge_cost_usd` — the judge tax: cost (USD) of the
/// judge call(s) that produced this verdict. Measurement spend, kept OUT of
/// the request cost attributes so savings stay invoice-reconcilable; the
/// durable `quality_verdicts` row is canonical for Phase 2 attribution
/// netting, this attribute is the ops-visible mirror. Always emitted for a
/// judged request; `0.0` means the judge model had no catalog pricing
/// (unmetered), never "free".
pub const TT_QUALITY_JUDGE_COST_USD: &str = "tokentrimmer.quality.judge_cost_usd";

/// **Reserved** wire name for a per-request quality score header in `[0, 1]`,
/// following the existing `x-tokentrimmer-*` header convention.
///
/// Nothing currently attaches this header to a response. The judge runs
/// asynchronously *after* the HTTP response is built (see
/// [`record_quality_verdict`]), so the verdict does not exist at
/// response-header time — the live surface for the score is the
/// [`TT_QUALITY_SCORE`] (`tokentrimmer.quality.*`) span attribute on the
/// detached judge task's span, which the hosted side ingests. This name is held
/// for a future *synchronous* surface (e.g. a blocking inline judge) that could
/// stamp the verdict on the response itself; until then, consumers must read the
/// span attribute, not this header.
pub const HEADER_QUALITY_SCORE: &str = "x-tokentrimmer-quality-score";
/// **Reserved** wire name for a per-request quality band header
/// (`low` / `medium` / `high`). Companion to [`HEADER_QUALITY_SCORE`]; like it,
/// nothing emits this header today (the verdict is async — see
/// [`HEADER_QUALITY_SCORE`]). The live surface for the band is the
/// [`TT_QUALITY_BAND`] span attribute.
pub const HEADER_QUALITY_BAND: &str = "x-tokentrimmer-quality-band";

/// Map a TokenTrimmer provider id to the OpenTelemetry `gen_ai.system` /
/// `gen_ai.provider.name` well-known value.
///
/// The semconv defines a fixed enum for common providers (`openai`,
/// `anthropic`, `gcp.gemini`, `groq`, `mistral_ai`, …). For providers without a
/// registered value (aggregators like `openrouter`/`together`, OpenAI-compat
/// shims, local runtimes, and the synthetic `cache`/`sandbox` pseudo-providers
/// used on cache hits) we pass the raw id through — the spec explicitly permits
/// custom values, and a stable string is more useful in a dashboard than
/// dropping the attribute.
#[must_use]
pub fn gen_ai_system(provider_id: &str) -> String {
    match provider_id {
        "openai" => "openai",
        "anthropic" => "anthropic",
        // The semconv well-known value for Google Gemini is `gcp.gemini`.
        "gemini" => "gcp.gemini",
        "groq" => "groq",
        "mistral" => "mistral_ai",
        // No registered semconv value — pass the TokenTrimmer id through.
        other => other,
    }
    .to_string()
}

/// Token usage + cost breakdown for one request, mirroring the values the
/// gateway stamps onto the `x-tokentrimmer-*` response headers.
///
/// All figures are pulled from the per-request values the gateway already
/// computed (token usage + `compute_cost`); nothing is recomputed here.
#[derive(Debug, Clone, Copy)]
pub struct RequestSpanCost {
    /// Prompt (input) tokens → `gen_ai.usage.input_tokens`.
    pub input_tokens: u64,
    /// Completion (output) tokens → `gen_ai.usage.output_tokens`.
    pub output_tokens: u64,
    /// What the provider actually bills → `tokentrimmer.cost_usd`.
    pub cost_usd: f64,
    /// Cost with no TokenTrimmer optimisation → `tokentrimmer.baseline_cost_usd`.
    pub baseline_cost_usd: f64,
    /// TokenTrimmer-attributed savings → `tokentrimmer.saved_usd`.
    pub saved_usd: f64,
    /// Provider-side automatic cache discount → `tokentrimmer.provider_cache_saved_usd`.
    pub provider_cache_saved_usd: f64,
}

/// One request's GenAI + cost attributes, ready to record onto a span.
///
/// `provider_id` is the TokenTrimmer provider id (mapped to `gen_ai.system` via
/// [`gen_ai_system`]); `request_model` is the model the caller asked for and
/// `response_model` is the model that served the request (they differ after
/// routing). `operation` is the GenAI operation name (`chat`, `embeddings`).
/// `cache_outcome` is the cache state string (`hit-l1`, `hit-l2`, `miss`,
/// `none`, …) and `route` is the matched route name when routing applied.
#[derive(Debug, Clone, Copy)]
pub struct RequestSpanAttributes<'a> {
    /// TokenTrimmer provider id → `gen_ai.system` / `gen_ai.provider.name`.
    pub provider_id: &'a str,
    /// Model the caller asked for → `gen_ai.request.model`.
    pub request_model: &'a str,
    /// Model that served the request → `gen_ai.response.model`.
    pub response_model: &'a str,
    /// GenAI operation → `gen_ai.operation.name`.
    pub operation: &'a str,
    /// Token usage + cost split.
    pub cost: RequestSpanCost,
    /// Cache outcome → `tokentrimmer.cache` (omitted when `None`).
    pub cache_outcome: Option<&'a str>,
    /// Matched route name → `tokentrimmer.route` (omitted when `None`).
    pub route: Option<&'a str>,
    /// Canary traffic-split percentage → `tokentrimmer.traffic_split_pct`
    /// (omitted when `None` — i.e. the route declared no split). ADDITIVE.
    pub traffic_split_pct: Option<u32>,
    /// Shadow-mode candidate model → `tokentrimmer.shadow_model` (omitted when
    /// `None`). ADDITIVE.
    pub shadow_model: Option<&'a str>,
    /// Cost (USD) of the discarded shadow dispatch → `tokentrimmer.shadow_cost_usd`
    /// (omitted when `None` — kept SEPARATE from the primary cost). ADDITIVE.
    pub shadow_cost_usd: Option<f64>,
}

/// Record the GenAI semantic-convention attributes plus TokenTrimmer cost
/// attributes onto `span`.
///
/// Call this once per request, at the point the cost is known (end of request,
/// alongside header stamping). Setting attributes on a span that is not bridged
/// to an OpenTelemetry layer (e.g. a plain `fmt` subscriber in dev) is a cheap
/// no-op.
pub fn record_request_attributes(span: &Span, attrs: &RequestSpanAttributes<'_>) {
    let cost = &attrs.cost;
    span.set_attribute(GEN_AI_OPERATION_NAME, attrs.operation.to_string());

    let system = gen_ai_system(attrs.provider_id);
    span.set_attribute(GEN_AI_SYSTEM, system.clone());
    // Emit the newer semconv spelling too so dashboards keyed on either resolve.
    span.set_attribute(GEN_AI_PROVIDER_NAME, system);

    span.set_attribute(GEN_AI_REQUEST_MODEL, attrs.request_model.to_string());
    span.set_attribute(GEN_AI_RESPONSE_MODEL, attrs.response_model.to_string());

    // Token counts are non-negative and fit i64 in practice; saturate rather
    // than wrap on the (impossible) overflow so a bogus huge count never flips
    // negative in the trace.
    span.set_attribute(
        GEN_AI_USAGE_INPUT_TOKENS,
        i64::try_from(cost.input_tokens).unwrap_or(i64::MAX),
    );
    span.set_attribute(
        GEN_AI_USAGE_OUTPUT_TOKENS,
        i64::try_from(cost.output_tokens).unwrap_or(i64::MAX),
    );

    span.set_attribute(TT_COST_USD, cost.cost_usd);
    span.set_attribute(TT_BASELINE_COST_USD, cost.baseline_cost_usd);
    span.set_attribute(TT_SAVED_USD, cost.saved_usd);
    span.set_attribute(TT_PROVIDER_CACHE_SAVED_USD, cost.provider_cache_saved_usd);

    if let Some(cache) = attrs.cache_outcome {
        span.set_attribute(TT_CACHE, cache.to_string());
    }
    if let Some(route) = attrs.route {
        span.set_attribute(TT_ROUTE, route.to_string());
    }
    // Canary attributes are ADDITIVE: each is set only when present, so a span
    // for non-canary traffic carries none of them and existing dashboards keyed
    // on the original attribute set are unaffected.
    if let Some(pct) = attrs.traffic_split_pct {
        span.set_attribute(TT_TRAFFIC_SPLIT_PCT, i64::from(pct));
    }
    if let Some(shadow) = attrs.shadow_model {
        span.set_attribute(TT_SHADOW_MODEL, shadow.to_string());
    }
    if let Some(shadow_cost) = attrs.shadow_cost_usd {
        span.set_attribute(TT_SHADOW_COST_USD, shadow_cost);
    }
}

/// One judged request's quality verdict, ready to record onto a span.
///
/// This is the telemetry shape of the sampled quality judge's per-request
/// outcome. `score` is the per-request quality in `[0, 1]` (`1.0` preserved /
/// `0.0` degraded) for a *classified* verdict, or `None` for `unclear` — an
/// unclassified verdict has no valence, so the score attribute is omitted rather
/// than defaulted (see below). `band` is `low`/`medium`/`high`, and `verdict` is
/// the raw judge verdict (`acceptable`/`degraded`/`unclear`). The gateway maps
/// its `tt_plan_core` verdict into this struct, mirroring how [`RequestSpanCost`]
/// is mapped from the gateway's `CostBreakdown`.
#[derive(Debug, Clone, Copy)]
pub struct QualityVerdictAttributes<'a> {
    /// Trace/request id of the judged request → `tokentrimmer.quality.request_id`.
    pub request_id: &'a str,
    /// Originally-requested (expensive) model → `tokentrimmer.quality.requested_model`.
    pub requested_model: &'a str,
    /// Served (cheaper) model whose quality was judged → `tokentrimmer.quality.served_model`.
    pub served_model: &'a str,
    /// Per-request quality score in `[0, 1]` → `tokentrimmer.quality.score`.
    /// `None` for an `unclear` verdict: the attribute is then **omitted** so a
    /// consumer that averages the emitted per-request scores gets the same
    /// headline as `tt_plan_core::quality_preserved_summary`, which likewise
    /// drops `unclear` from the preserved denominator. Never default this to a
    /// number for `unclear` — that would count an unclassified sample as fully
    /// preserved and diverge from the canonical aggregation.
    pub score: Option<f64>,
    /// Risk band (`low`/`medium`/`high`) → `tokentrimmer.quality.band`.
    pub band: &'a str,
    /// Raw judge verdict (`acceptable`/`degraded`/`unclear`) → `tokentrimmer.quality.verdict`.
    pub verdict: &'a str,
    /// Judge tax (USD) → `tokentrimmer.quality.judge_cost_usd`. Always emitted
    /// for a judged request — `0.0` means the judge model had no catalog
    /// pricing (unmetered), never "free". See [`TT_QUALITY_JUDGE_COST_USD`].
    pub judge_cost_usd: f64,
}

/// Record the per-request quality verdict onto `span`.
///
/// **Call this only when a judge actually ran** for the request (the sampled
/// ~2% of rerouted-down traffic). Surfacing code must never call it for an
/// unjudged request — there is deliberately no "default" verdict, so an
/// unjudged request carries no quality attributes at all and the hosted
/// aggregation never counts a fabricated score.
///
/// The judge is detached from the user response path, so this records onto the
/// judge task's own span, not the user request span — it adds zero user
/// latency. Setting attributes on a span not bridged to an OpenTelemetry layer
/// is a cheap no-op, as with [`record_request_attributes`].
pub fn record_quality_verdict(span: &Span, attrs: &QualityVerdictAttributes<'_>) {
    span.set_attribute(TT_QUALITY_REQUEST_ID, attrs.request_id.to_string());
    span.set_attribute(
        TT_QUALITY_REQUESTED_MODEL,
        attrs.requested_model.to_string(),
    );
    span.set_attribute(TT_QUALITY_SERVED_MODEL, attrs.served_model.to_string());
    // Omit the score for an `unclear` verdict (`None`): it has no valence, so
    // emitting a number would let a consumer average it in as if preserved,
    // diverging from `quality_preserved_summary` (which drops `unclear`).
    if let Some(score) = attrs.score {
        span.set_attribute(TT_QUALITY_SCORE, score);
    }
    span.set_attribute(TT_QUALITY_BAND, attrs.band.to_string());
    span.set_attribute(TT_QUALITY_VERDICT, attrs.verdict.to_string());
    span.set_attribute(TT_QUALITY_JUDGE_COST_USD, attrs.judge_cost_usd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::Value;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::TracerProvider;
    use std::collections::HashMap;
    use tracing_subscriber::prelude::*;

    #[test]
    fn provider_id_maps_to_semconv_system_value() {
        assert_eq!(gen_ai_system("openai"), "openai");
        assert_eq!(gen_ai_system("anthropic"), "anthropic");
        assert_eq!(gen_ai_system("gemini"), "gcp.gemini");
        assert_eq!(gen_ai_system("groq"), "groq");
        assert_eq!(gen_ai_system("mistral"), "mistral_ai");
        // Unregistered providers pass through verbatim.
        assert_eq!(gen_ai_system("openrouter"), "openrouter");
        assert_eq!(gen_ai_system("cache"), "cache");
    }

    /// Drive a closure under a scoped OTel subscriber and return the attributes
    /// recorded on the single exported span as a name→Value map.
    fn capture_attributes(f: impl FnOnce(&Span)) -> HashMap<String, Value> {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("gen-ai-test");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::registry().with(otel_layer);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("test_request");
            let _enter = span.enter();
            f(&span);
        });

        provider.force_flush();
        let spans = exporter.get_finished_spans().expect("finished spans");
        let span = spans
            .into_iter()
            .find(|s| s.name == "test_request")
            .expect("test span should be exported");
        span.attributes
            .into_iter()
            .map(|kv| (kv.key.to_string(), kv.value))
            .collect()
    }

    #[test]
    fn records_gen_ai_and_cost_attributes_on_span() {
        let attrs = capture_attributes(|span| {
            record_request_attributes(
                span,
                &RequestSpanAttributes {
                    provider_id: "openai",
                    request_model: "gpt-4o",
                    response_model: "gpt-4o-mini",
                    operation: "chat",
                    cost: RequestSpanCost {
                        input_tokens: 123,
                        output_tokens: 45,
                        cost_usd: 0.001_2,
                        baseline_cost_usd: 0.003_4,
                        saved_usd: 0.002_2,
                        provider_cache_saved_usd: 0.0,
                    },
                    cache_outcome: Some("miss"),
                    route: Some("cheap-route"),
                    traffic_split_pct: Some(30),
                    shadow_model: Some("claude-haiku-4-5"),
                    shadow_cost_usd: Some(0.000_9),
                },
            );
        });

        assert_eq!(
            attrs.get(GEN_AI_SYSTEM),
            Some(&Value::String("openai".into()))
        );
        assert_eq!(
            attrs.get(GEN_AI_PROVIDER_NAME),
            Some(&Value::String("openai".into()))
        );
        assert_eq!(
            attrs.get(GEN_AI_OPERATION_NAME),
            Some(&Value::String("chat".into()))
        );
        assert_eq!(
            attrs.get(GEN_AI_REQUEST_MODEL),
            Some(&Value::String("gpt-4o".into()))
        );
        assert_eq!(
            attrs.get(GEN_AI_RESPONSE_MODEL),
            Some(&Value::String("gpt-4o-mini".into()))
        );
        assert_eq!(attrs.get(GEN_AI_USAGE_INPUT_TOKENS), Some(&Value::I64(123)));
        assert_eq!(attrs.get(GEN_AI_USAGE_OUTPUT_TOKENS), Some(&Value::I64(45)));
        assert_eq!(attrs.get(TT_COST_USD), Some(&Value::F64(0.001_2)));
        assert_eq!(attrs.get(TT_BASELINE_COST_USD), Some(&Value::F64(0.003_4)));
        assert_eq!(attrs.get(TT_SAVED_USD), Some(&Value::F64(0.002_2)));
        assert_eq!(
            attrs.get(TT_PROVIDER_CACHE_SAVED_USD),
            Some(&Value::F64(0.0))
        );
        assert_eq!(attrs.get(TT_CACHE), Some(&Value::String("miss".into())));
        assert_eq!(
            attrs.get(TT_ROUTE),
            Some(&Value::String("cheap-route".into()))
        );
        // Canary attributes are recorded when present.
        assert_eq!(attrs.get(TT_TRAFFIC_SPLIT_PCT), Some(&Value::I64(30)));
        assert_eq!(
            attrs.get(TT_SHADOW_MODEL),
            Some(&Value::String("claude-haiku-4-5".into()))
        );
        assert_eq!(attrs.get(TT_SHADOW_COST_USD), Some(&Value::F64(0.000_9)));
    }

    /// Canary attributes are ADDITIVE: when the request had no traffic split and
    /// no shadow, none of the three keys appear on the span — existing dashboards
    /// keyed on the original attribute set are unaffected.
    #[test]
    fn canary_attributes_omitted_when_absent() {
        let attrs = capture_attributes(|span| {
            record_request_attributes(
                span,
                &RequestSpanAttributes {
                    provider_id: "openai",
                    request_model: "gpt-4o",
                    response_model: "gpt-4o",
                    operation: "chat",
                    cost: RequestSpanCost {
                        input_tokens: 1,
                        output_tokens: 1,
                        cost_usd: 0.0,
                        baseline_cost_usd: 0.0,
                        saved_usd: 0.0,
                        provider_cache_saved_usd: 0.0,
                    },
                    cache_outcome: Some("miss"),
                    route: None,
                    traffic_split_pct: None,
                    shadow_model: None,
                    shadow_cost_usd: None,
                },
            );
        });
        assert!(!attrs.contains_key(TT_TRAFFIC_SPLIT_PCT));
        assert!(!attrs.contains_key(TT_SHADOW_MODEL));
        assert!(!attrs.contains_key(TT_SHADOW_COST_USD));
    }

    #[test]
    fn records_quality_verdict_attributes_on_span() {
        let attrs = capture_attributes(|span| {
            record_quality_verdict(
                span,
                &QualityVerdictAttributes {
                    request_id: "11111111-1111-1111-1111-111111111111",
                    requested_model: "gpt-4o",
                    served_model: "gpt-4o-mini",
                    score: Some(1.0),
                    band: "low",
                    verdict: "acceptable",
                    judge_cost_usd: 0.000_05,
                },
            );
        });

        assert_eq!(
            attrs.get(TT_QUALITY_REQUEST_ID),
            Some(&Value::String(
                "11111111-1111-1111-1111-111111111111".into()
            ))
        );
        assert_eq!(
            attrs.get(TT_QUALITY_REQUESTED_MODEL),
            Some(&Value::String("gpt-4o".into()))
        );
        assert_eq!(
            attrs.get(TT_QUALITY_SERVED_MODEL),
            Some(&Value::String("gpt-4o-mini".into()))
        );
        assert_eq!(attrs.get(TT_QUALITY_SCORE), Some(&Value::F64(1.0)));
        assert_eq!(
            attrs.get(TT_QUALITY_BAND),
            Some(&Value::String("low".into()))
        );
        assert_eq!(
            attrs.get(TT_QUALITY_VERDICT),
            Some(&Value::String("acceptable".into()))
        );
        assert_eq!(
            attrs.get(TT_QUALITY_JUDGE_COST_USD),
            Some(&Value::F64(0.000_05)),
            "the judge tax must land on the span"
        );
    }

    /// An `unclear` verdict carries no `score` (`None`), so the
    /// `tokentrimmer.quality.score` attribute is omitted — band/verdict still
    /// surface. This keeps averaging the emitted scores aligned with
    /// `quality_preserved_summary`, which drops `unclear` from its denominator.
    #[test]
    fn unclear_verdict_omits_score_attribute() {
        let attrs = capture_attributes(|span| {
            record_quality_verdict(
                span,
                &QualityVerdictAttributes {
                    request_id: "22222222-2222-2222-2222-222222222222",
                    requested_model: "gpt-4o",
                    served_model: "gpt-4o-mini",
                    score: None,
                    band: "low",
                    verdict: "unclear",
                    judge_cost_usd: 0.0,
                },
            );
        });

        assert!(
            !attrs.contains_key(TT_QUALITY_SCORE),
            "score attribute must be omitted for an unclear verdict"
        );
        assert_eq!(
            attrs.get(TT_QUALITY_BAND),
            Some(&Value::String("low".into()))
        );
        assert_eq!(
            attrs.get(TT_QUALITY_VERDICT),
            Some(&Value::String("unclear".into()))
        );
    }

    /// The header-name constants stay aligned with the spec'd wire format the
    /// cloud ingests.
    #[test]
    fn quality_header_names_match_convention() {
        assert_eq!(HEADER_QUALITY_SCORE, "x-tokentrimmer-quality-score");
        assert_eq!(HEADER_QUALITY_BAND, "x-tokentrimmer-quality-band");
    }

    #[test]
    fn omits_optional_attributes_when_absent() {
        let attrs = capture_attributes(|span| {
            record_request_attributes(
                span,
                &RequestSpanAttributes {
                    provider_id: "anthropic",
                    request_model: "claude-3-5-sonnet",
                    response_model: "claude-3-5-sonnet",
                    operation: "chat",
                    cost: RequestSpanCost {
                        input_tokens: 1,
                        output_tokens: 1,
                        cost_usd: 0.0,
                        baseline_cost_usd: 0.0,
                        saved_usd: 0.0,
                        provider_cache_saved_usd: 0.0,
                    },
                    cache_outcome: None,
                    route: None,
                    traffic_split_pct: None,
                    shadow_model: None,
                    shadow_cost_usd: None,
                },
            );
        });

        assert_eq!(
            attrs.get(GEN_AI_SYSTEM),
            Some(&Value::String("anthropic".into()))
        );
        assert!(
            !attrs.contains_key(TT_CACHE),
            "cache attribute must be absent when no cache outcome is supplied"
        );
        assert!(
            !attrs.contains_key(TT_ROUTE),
            "route attribute must be absent when no route matched"
        );
    }
}
