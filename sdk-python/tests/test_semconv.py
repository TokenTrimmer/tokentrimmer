"""Tests for the OTel semantic-convention constants + CostInfo->attrs mapping.

`tokentrimmer.semconv` is dependency-free (no langchain, no opentelemetry), so
these run under the base install. They assert the constant vocabulary matches the
gateway's Rust `tt_telemetry::gen_ai` keys and that the mapping is additive
(omits fields the gateway didn't populate).
"""

from __future__ import annotations

from tokentrimmer import semconv
from tokentrimmer.client import TokenTrimmerMeta


def _meta(**overrides) -> TokenTrimmerMeta:
    base = dict(
        trace_id="trace-1",
        provider="anthropic",
        model_used="claude-haiku-4-5",
        cost_usd=0.0034,
        baseline_cost_usd=0.02,
        saved_usd=0.0166,
        cache="miss",
        route="cheap-route",
    )
    base.update(overrides)
    return TokenTrimmerMeta(**base)


def test_semconv_keys_match_gateway_vocabulary():
    # These MUST stay byte-identical to crates/telemetry/src/gen_ai.rs so a
    # client-recorded span and the gateway span carry the same attribute keys.
    assert semconv.GEN_AI_SYSTEM == "gen_ai.system"
    assert semconv.GEN_AI_PROVIDER_NAME == "gen_ai.provider.name"
    assert semconv.GEN_AI_OPERATION_NAME == "gen_ai.operation.name"
    assert semconv.GEN_AI_REQUEST_MODEL == "gen_ai.request.model"
    assert semconv.GEN_AI_RESPONSE_MODEL == "gen_ai.response.model"
    assert semconv.GEN_AI_USAGE_INPUT_TOKENS == "gen_ai.usage.input_tokens"
    assert semconv.GEN_AI_USAGE_OUTPUT_TOKENS == "gen_ai.usage.output_tokens"
    assert semconv.TT_COST_USD == "tokentrimmer.cost_usd"
    assert semconv.TT_BASELINE_COST_USD == "tokentrimmer.baseline_cost_usd"
    assert semconv.TT_SAVED_USD == "tokentrimmer.saved_usd"
    assert semconv.TT_CACHE == "tokentrimmer.cache"
    assert semconv.TT_ROUTE == "tokentrimmer.route"
    assert semconv.TT_TRACE_ID == "tokentrimmer.trace_id"


def test_gen_ai_system_maps_known_providers_and_passes_through():
    assert semconv.gen_ai_system("openai") == "openai"
    assert semconv.gen_ai_system("anthropic") == "anthropic"
    assert semconv.gen_ai_system("gemini") == "gcp.gemini"
    assert semconv.gen_ai_system("mistral") == "mistral_ai"
    assert semconv.gen_ai_system("groq") == "groq"
    # Unregistered ids pass through verbatim (aggregators, cache pseudo-provider).
    assert semconv.gen_ai_system("openrouter") == "openrouter"
    assert semconv.gen_ai_system("cache") == "cache"


def test_cost_info_to_attributes_full_mapping():
    attrs = semconv.cost_info_to_attributes(_meta())
    assert attrs[semconv.GEN_AI_SYSTEM] == "anthropic"
    assert attrs[semconv.GEN_AI_PROVIDER_NAME] == "anthropic"
    assert attrs[semconv.GEN_AI_RESPONSE_MODEL] == "claude-haiku-4-5"
    assert attrs[semconv.TT_COST_USD] == 0.0034
    assert attrs[semconv.TT_BASELINE_COST_USD] == 0.02
    assert attrs[semconv.TT_SAVED_USD] == 0.0166
    assert attrs[semconv.TT_CACHE] == "miss"
    assert attrs[semconv.TT_ROUTE] == "cheap-route"
    assert attrs[semconv.TT_TRACE_ID] == "trace-1"


def test_cost_info_to_attributes_maps_provider_to_semconv_value():
    attrs = semconv.cost_info_to_attributes(_meta(provider="gemini"))
    assert attrs[semconv.GEN_AI_SYSTEM] == "gcp.gemini"
    assert attrs[semconv.GEN_AI_PROVIDER_NAME] == "gcp.gemini"


def test_cost_info_to_attributes_is_additive_omitting_none_fields():
    """A self-hosted gateway with no cost headers yields an empty attr dict."""
    empty = TokenTrimmerMeta(
        trace_id=None,
        provider=None,
        model_used=None,
        cost_usd=None,
        baseline_cost_usd=None,
        saved_usd=None,
        cache=None,
        route=None,
    )
    assert semconv.cost_info_to_attributes(empty) == {}

    # Partial population: only present fields appear; nothing is defaulted to 0.
    partial = semconv.cost_info_to_attributes(
        _meta(cache=None, route=None, baseline_cost_usd=None)
    )
    assert semconv.TT_CACHE not in partial
    assert semconv.TT_ROUTE not in partial
    assert semconv.TT_BASELINE_COST_USD not in partial
    assert partial[semconv.TT_COST_USD] == 0.0034
