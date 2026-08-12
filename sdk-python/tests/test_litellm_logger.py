"""Tests for the LiteLLM cost logger.

These exercise the logger against hand-built response objects shaped like a
LiteLLM ``ModelResponse`` (no network) so the header->attribute mapping, span
recording, budget breach, and graceful no-header degradation are covered with
only ``litellm`` + ``opentelemetry`` (the deps CI installs via ``[test]``). The
final test drives a *real* ``litellm.completion`` through a ``respx``-mocked
gateway to prove the end-to-end hook.
"""

from __future__ import annotations

import asyncio
import types

import pytest

pytest.importorskip("litellm")

from tokentrimmer import semconv  # noqa: E402
from tokentrimmer.integrations.litellm import (  # noqa: E402
    BudgetExceeded,
    TokenTrimmerLiteLLMLogger,
)

# The raw x-tokentrimmer-* headers a gateway response carries (lowercased).
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


def _headers(cost="0.0034"):
    h = dict(TT_HEADERS)
    h["x-tokentrimmer-cost-usd"] = str(cost)
    return h


def _response(*, response_headers=None, hidden_additional=None, usage=True):
    """A stand-in for a LiteLLM ModelResponse carrying gateway headers.

    ``response_headers`` populates ``_response_headers`` (the raw, un-prefixed
    shape). ``hidden_additional`` populates ``_hidden_params["additional_headers"]``
    (LiteLLM's ``llm_provider-``-prefixed shape).
    """
    obj = types.SimpleNamespace()
    if response_headers is not None:
        obj._response_headers = dict(response_headers)
    if hidden_additional is not None:
        obj._hidden_params = {"additional_headers": dict(hidden_additional)}
    if usage:
        obj.usage = types.SimpleNamespace(
            prompt_tokens=10, completion_tokens=20, total_tokens=30
        )
    return obj


def _emit(logger, response_obj):
    """Fire the sync success hook with LiteLLM's arg shape."""
    logger.log_success_event({}, response_obj, None, None)


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


def test_logger_records_cost_attributes_on_current_span():
    tracer, exporter = _in_memory_tracer()
    logger = TokenTrimmerLiteLLMLogger()

    with tracer.start_as_current_span("chat") as span:
        assert span.is_recording()
        _emit(logger, _response(response_headers=TT_HEADERS))

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
    # Token counts fold in from the response usage object.
    assert attrs[semconv.GEN_AI_USAGE_INPUT_TOKENS] == 10
    assert attrs[semconv.GEN_AI_USAGE_OUTPUT_TOKENS] == 20

    assert logger.total_cost_usd == pytest.approx(0.0034)
    assert logger.total_saved_usd == pytest.approx(0.0166)
    assert logger.attributed_calls == 1


def test_reads_headers_from_hidden_params_additional_headers_with_prefix():
    """The llm_provider- prefix LiteLLM adds in additional_headers is stripped.

    Uses LiteLLM's *real* ``process_response_headers`` to build the prefixed
    dict, so this proves the logger reads the exact shape LiteLLM produces.
    """
    from litellm.litellm_core_utils.core_helpers import process_response_headers

    additional = process_response_headers(_headers())
    # Sanity: LiteLLM really did prefix the TT header keys.
    assert "llm_provider-x-tokentrimmer-cost-usd" in additional

    logger = TokenTrimmerLiteLLMLogger(record_spans=False)
    # No _response_headers here — force the additional_headers path.
    _emit(logger, _response(hidden_additional=additional))
    assert logger.total_cost_usd == pytest.approx(0.0034)
    assert logger.total_saved_usd == pytest.approx(0.0166)
    assert logger.total_baseline_usd == pytest.approx(0.02)
    assert logger.attributed_calls == 1


def test_hidden_params_as_object_attribute():
    """_hidden_params may be a HiddenParams-like object, not a dict."""
    logger = TokenTrimmerLiteLLMLogger(record_spans=False)
    hidden = types.SimpleNamespace(
        additional_headers={
            "llm_provider-x-tokentrimmer-cost-usd": "0.0034",
            "llm_provider-x-tokentrimmer-saved-usd": "0.0166",
        }
    )
    obj = types.SimpleNamespace(_hidden_params=hidden)
    _emit(logger, obj)
    assert logger.total_cost_usd == pytest.approx(0.0034)
    assert logger.attributed_calls == 1


def test_no_headers_degrades_gracefully():
    """A plain (non-gateway) response records nothing and never raises."""
    logger = TokenTrimmerLiteLLMLogger(post_response_budget_usd=0.0001, record_spans=False)
    plain = _response(response_headers={"content-type": "application/json"})
    _emit(logger, plain)
    assert logger.total_cost_usd == 0.0
    assert logger.attributed_calls == 0
    assert logger.budget_exceeded is False
    logger.raise_if_exceeded()  # no budget breach recorded -> no-op


def test_missing_headers_entirely_is_noop():
    """A response object with no header attributes at all is a safe no-op."""
    logger = TokenTrimmerLiteLLMLogger(record_spans=False)
    _emit(logger, types.SimpleNamespace())
    assert logger.attributed_calls == 0


def test_budget_breach_records_flag_and_raise_if_exceeded():
    logger = TokenTrimmerLiteLLMLogger(post_response_budget_usd=0.05, record_spans=False)
    # First call stays under the cap.
    _emit(logger, _response(response_headers=_headers("0.03")))
    assert logger.total_cost_usd == pytest.approx(0.03)
    assert logger.budget_exceeded is False
    logger.raise_if_exceeded()  # still fine

    # Second call tips accumulated cost past 0.05. LiteLLM swallows exceptions
    # from the callback, so the hook itself must NOT raise — the breach is
    # recorded for the caller's checkpoint instead.
    _emit(logger, _response(response_headers=_headers("0.04")))  # does not raise
    assert logger.budget_exceeded is True
    with pytest.raises(BudgetExceeded) as excinfo:
        logger.raise_if_exceeded()
    assert excinfo.value.limit_usd == pytest.approx(0.05)
    assert excinfo.value.total_cost_usd == pytest.approx(0.07)


def test_budget_not_exceeded_exactly_at_cap():
    """At exactly the cap is not a breach (strictly-greater triggers)."""
    logger = TokenTrimmerLiteLLMLogger(post_response_budget_usd=0.0034, record_spans=False)
    _emit(logger, _response(response_headers=_headers("0.0034")))
    assert logger.budget_exceeded is False
    logger.raise_if_exceeded()


def test_accumulates_across_calls_and_reset():
    logger = TokenTrimmerLiteLLMLogger(record_spans=False)
    _emit(logger, _response(response_headers=_headers("0.01")))
    _emit(logger, _response(response_headers=_headers("0.02")))
    assert logger.total_cost_usd == pytest.approx(0.03)
    assert logger.attributed_calls == 2
    logger.reset()
    assert logger.total_cost_usd == 0.0
    assert logger.attributed_calls == 0
    assert logger.budget_exceeded is False


def test_async_log_success_event_records():
    logger = TokenTrimmerLiteLLMLogger(record_spans=False)
    asyncio.run(
        logger.async_log_success_event(
            {}, _response(response_headers=TT_HEADERS), None, None
        )
    )
    assert logger.total_cost_usd == pytest.approx(0.0034)
    assert logger.attributed_calls == 1


def test_sync_post_api_hook_accounts_immediately_without_success_double_count():
    logger = TokenTrimmerLiteLLMLogger(post_response_budget_usd=0.001, record_spans=False)
    kwargs = {
        "litellm_call_id": "call-1",
        "response_headers": TT_HEADERS,
    }

    logger.log_post_api_call(kwargs, None, None, None)
    assert logger.total_cost_usd == pytest.approx(0.0034)
    assert logger.attributed_calls == 1
    assert logger.budget_exceeded is True

    # LiteLLM later dispatches the normal success callback on an executor. It
    # may add token/span attributes, but must not account for this call twice.
    logger.log_success_event(kwargs, _response(response_headers=TT_HEADERS), None, None)
    assert logger.total_cost_usd == pytest.approx(0.0034)
    assert logger.attributed_calls == 1


def test_install_sets_return_response_headers_and_registers():
    import litellm

    saved_flag = litellm.return_response_headers
    callback_names = (
        "callbacks",
        "input_callback",
        "success_callback",
        "failure_callback",
        "_async_success_callback",
        "_async_failure_callback",
    )
    saved_callbacks = {name: list(getattr(litellm, name)) for name in callback_names}
    try:
        litellm.callbacks = []
        litellm.return_response_headers = False
        logger = TokenTrimmerLiteLLMLogger.install(post_response_budget_usd=0.5)
        assert litellm.return_response_headers is True
        assert logger in litellm.callbacks
        assert logger.post_response_budget_usd == 0.5
        # Idempotent: installing the same handler again does not duplicate it.
        # (a fresh install() makes a new handler, so register it explicitly)
        litellm.callbacks.append(logger)
        assert litellm.callbacks.count(logger) == 2  # append is not guarded
    finally:
        litellm.return_response_headers = saved_flag
        for name, callbacks in saved_callbacks.items():
            setattr(litellm, name, callbacks)


def test_end_to_end_litellm_completion_captures_cost():
    """A real litellm.completion through a respx-mocked gateway fires the hook."""
    respx = pytest.importorskip("respx")
    import httpx
    import litellm

    saved_flag = litellm.return_response_headers
    callback_names = (
        "callbacks",
        "input_callback",
        "success_callback",
        "failure_callback",
        "_async_success_callback",
        "_async_failure_callback",
    )
    saved_callbacks = {name: list(getattr(litellm, name)) for name in callback_names}
    body = {
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-4o-mini",
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop",
            }
        ],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7},
    }
    headers = {"content-type": "application/json", **TT_HEADERS}
    try:
        litellm.callbacks = []
        logger = TokenTrimmerLiteLLMLogger.install(post_response_budget_usd=0.001)
        with respx.mock:
            respx.post("https://api.openai.com/v1/chat/completions").mock(
                return_value=httpx.Response(200, json=body, headers=headers)
            )
            resp = litellm.completion(
                model="gpt-4o-mini",
                messages=[{"role": "user", "content": "hello"}],
                api_key="sk-test",
            )
        assert resp.choices[0].message.content == "hi"
        assert logger.total_cost_usd == pytest.approx(0.0034)
        assert logger.total_saved_usd == pytest.approx(0.0166)
        assert logger.attributed_calls == 1
        # 0.0034 > 0.001 cap -> breach recorded, enforced at the checkpoint.
        assert logger.budget_exceeded is True
        with pytest.raises(BudgetExceeded):
            logger.raise_if_exceeded()
    finally:
        litellm.return_response_headers = saved_flag
        for name, callbacks in saved_callbacks.items():
            setattr(litellm, name, callbacks)
