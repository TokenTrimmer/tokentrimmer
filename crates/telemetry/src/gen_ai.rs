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
