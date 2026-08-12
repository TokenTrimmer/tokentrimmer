"""Tests for the LangChain cost callback.

These exercise the callback against hand-built ``LLMResult`` objects (no network,
no live ``ChatOpenAI``) so the header->attribute mapping, span recording, budget
breach, and graceful no-header degradation are all covered with only
``langchain-core`` + ``opentelemetry`` (the deps CI installs via ``[test]``).
"""

from __future__ import annotations

import pytest

pytest.importorskip("langchain_core")

from langchain_core.messages import AIMessage  # noqa: E402
from langchain_core.outputs import ChatGeneration, LLMResult  # noqa: E402

from tokentrimmer import semconv  # noqa: E402
from tokentrimmer.integrations.langchain import (  # noqa: E402
    BudgetExceeded,
    TokenTrimmerCostCallback,
)

# The x-tokentrimmer-* headers a gateway response carries, as
# include_response_headers=True surfaces them (lowercased httpx keys).
TT_HEADERS = {
    "x-tokentrimmer-trace-id": "trace-1",
    "x-tokentrimmer-provider": "anthropic",
    "x-tokentrimmer-model-used": "claude-haiku-4-5",
    "x-tokentrimmer-cost-usd": "0.0034",
    "x-tokentrimmer-baseline-cost-usd": "0.0200",
    "x-tokentrimmer-saved-usd": "0.0166",
    "x-tokentrimmer-cache": "miss",
    "x-tokentrimmer-route-matched": "cheap-route",
}


def _result_with_headers_in_message(
    headers=TT_HEADERS, *, cost=None, with_usage=True
):
    """An LLMResult with headers on the message's response_metadata (the common
    langchain-openai placement)."""
    h = dict(headers)
    if cost is not None:
        h["x-tokentrimmer-cost-usd"] = str(cost)
    usage = (
        {"input_tokens": 10, "output_tokens": 20, "total_tokens": 30}
        if with_usage
        else None
    )
    msg = AIMessage(
        content="hi",
        response_metadata={"headers": h, "model_name": "claude-haiku-4-5"},
        usage_metadata=usage,
    )
    return LLMResult(generations=[[ChatGeneration(message=msg)]])


def _result_with_headers_in_llm_output(headers=TT_HEADERS):
    """An LLMResult with headers on llm_output (the alternate placement)."""
    msg = AIMessage(content="hi")
    return LLMResult(
        generations=[[ChatGeneration(message=msg)]],
        llm_output={"headers": dict(headers), "token_usage": {"prompt_tokens": 3}},
    )


# --- span recording via an in-memory OTel exporter --------------------------


def _in_memory_tracer():
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import SimpleSpanProcessor
    from opentelemetry.sdk.trace.export.in_memory_span_exporter import (
        InMemorySpanExporter,
    )

    exporter = InMemorySpanExporter()
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(exporter))
    return provider.get_tracer("test"), exporter


def test_callback_records_cost_attributes_on_current_span():
    from opentelemetry import trace as otel_trace

    tracer, exporter = _in_memory_tracer()
    cb = TokenTrimmerCostCallback()

    with tracer.start_as_current_span("chat") as span:
        assert span.is_recording()
        cb.on_llm_end(_result_with_headers_in_message(), run_id="r1")

    (finished,) = exporter.get_finished_spans()
    attrs = dict(finished.attributes or {})
    assert attrs[semconv.GEN_AI_SYSTEM] == "anthropic"
    assert attrs[semconv.GEN_AI_RESPONSE_MODEL] == "claude-haiku-4-5"
    assert attrs[semconv.TT_COST_USD] == pytest.approx(0.0034)
    assert attrs[semconv.TT_SAVED_USD] == pytest.approx(0.0166)
    assert attrs[semconv.TT_BASELINE_COST_USD] == pytest.approx(0.02)
    assert attrs[semconv.TT_CACHE] == "miss"
    assert attrs[semconv.TT_ROUTE] == "cheap-route"
    assert attrs[semconv.TT_TRACE_ID] == "trace-1"
    # Token counts fold in from the message usage_metadata.
    assert attrs[semconv.GEN_AI_USAGE_INPUT_TOKENS] == 10
    assert attrs[semconv.GEN_AI_USAGE_OUTPUT_TOKENS] == 20

    # ...and the run totals accumulate.
    assert cb.total_cost_usd == pytest.approx(0.0034)
    assert cb.total_saved_usd == pytest.approx(0.0166)
    assert cb.attributed_calls == 1

    otel_trace  # (imported to ensure the API is available in this env)


def test_callback_reads_headers_from_llm_output_placement():
    cb = TokenTrimmerCostCallback(record_spans=False)
    cb.on_llm_end(_result_with_headers_in_llm_output(), run_id="r1")
    assert cb.total_cost_usd == pytest.approx(0.0034)
    assert cb.total_saved_usd == pytest.approx(0.0166)
    assert cb.attributed_calls == 1


def test_callback_accumulates_across_calls():
    cb = TokenTrimmerCostCallback(record_spans=False)
    cb.on_llm_end(_result_with_headers_in_message(cost="0.01"), run_id="r1")
    cb.on_llm_end(_result_with_headers_in_message(cost="0.02"), run_id="r2")
    assert cb.total_cost_usd == pytest.approx(0.03)
    assert cb.attributed_calls == 2
    cb.reset()
    assert cb.total_cost_usd == 0.0
    assert cb.attributed_calls == 0


def test_no_headers_degrades_gracefully():
    """A plain (non-gateway) response records nothing and never raises."""
    cb = TokenTrimmerCostCallback(post_response_budget_usd=0.0001, record_spans=False)
    plain = LLMResult(
        generations=[[ChatGeneration(message=AIMessage(content="hi"))]],
        llm_output={"token_usage": {"prompt_tokens": 1, "completion_tokens": 1}},
    )
    # No x-tokentrimmer-* headers -> no accumulation, no BudgetExceeded.
    cb.on_llm_end(plain, run_id="r1")
    assert cb.total_cost_usd == 0.0
    assert cb.attributed_calls == 0


def test_budget_exceeded_raises_past_the_cap():
    cb = TokenTrimmerCostCallback(post_response_budget_usd=0.05, record_spans=False)
    # First call stays under the cap.
    cb.on_llm_end(_result_with_headers_in_message(cost="0.03"), run_id="r1")
    assert cb.total_cost_usd == pytest.approx(0.03)
    # Second call tips the accumulated cost past 0.05 -> raise.
    with pytest.raises(BudgetExceeded) as excinfo:
        cb.on_llm_end(_result_with_headers_in_message(cost="0.04"), run_id="r2")
    assert excinfo.value.limit_usd == pytest.approx(0.05)
    assert excinfo.value.total_cost_usd == pytest.approx(0.07)


def test_budget_not_exceeded_exactly_at_cap():
    """At exactly the cap is not a breach (strictly-greater triggers)."""
    cb = TokenTrimmerCostCallback(post_response_budget_usd=0.0034, record_spans=False)
    cb.on_llm_end(_result_with_headers_in_message(), run_id="r1")  # cost 0.0034
    assert cb.total_cost_usd == pytest.approx(0.0034)


def test_raise_error_is_true_so_langchain_propagates_budget_breach():
    # LangChain only re-raises a callback exception when the handler opts in via
    # raise_error; the budget stop hook relies on this being True.
    assert TokenTrimmerCostCallback.raise_error is True


def test_span_recording_is_noop_without_active_span():
    """With no active/recording span, recording is a silent no-op (still totals)."""
    cb = TokenTrimmerCostCallback(record_spans=True)
    cb.on_llm_end(_result_with_headers_in_message(), run_id="r1")
    assert cb.total_cost_usd == pytest.approx(0.0034)


def test_make_gateway_chat_openai_sets_header_and_completions_flags():
    pytest.importorskip("langchain_openai")
    from tokentrimmer.integrations.langchain import make_gateway_chat_openai

    llm = make_gateway_chat_openai(model="claude-haiku-4-5", api_key="tt_test")
    assert llm.include_response_headers is True
    assert getattr(llm, "use_responses_api", False) is False
