"""Tests for the TokenTrimmer Python SDK wrapper.

All tests mock the gateway HTTP layer with respx so no network is used. They
assert the four wrap behaviors: max_tokens default injection, the tt_* header
lifts (+ validation), the parsed .tt metadata, race-free metadata under
concurrency, and streaming pass-through.
"""

from __future__ import annotations

import concurrent.futures
import json

import httpx
import pytest
import respx

from tokentrimmer import TokenTrimmer
from tokentrimmer.client import StreamCost, TokenTrimmerMeta

GATEWAY = "http://gw.test/v1"

# A real-gateway-shaped SSE body: a normal OpenAI chunk stream, then the
# terminal `tokentrimmer.usage` event (the gateway appends this after the
# OpenAI stream — see docs/04-gateway-api-reference.md and
# crates/core/src/routes/sse.rs::usage_event), then `[DONE]`.
GATEWAY_STREAM_SSE = (
    'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,'
    '"model":"claude-haiku-4-5","choices":[{"index":0,'
    '"delta":{"role":"assistant"},"finish_reason":null}]}\n\n'
    'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,'
    '"model":"claude-haiku-4-5","choices":[{"index":0,'
    '"delta":{"content":"Hello"},"finish_reason":null}]}\n\n'
    'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,'
    '"model":"claude-haiku-4-5","choices":[{"index":0,'
    '"delta":{"content":" world"},"finish_reason":null}]}\n\n'
    'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,'
    '"model":"claude-haiku-4-5","choices":[{"index":0,"delta":{},'
    '"finish_reason":"stop"}],'
    '"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,'
    '"cached_tokens":0}}\n\n'
    "event: tokentrimmer.usage\n"
    'data: {"cost_usd":0.0001,"baseline_cost_usd":0.0004,"saved_usd":0.0003,'
    '"provider_cache_saved_usd":0.0,"input_tokens":10,"output_tokens":20,'
    '"cached_tokens":0}\n\n'
    "data: [DONE]\n\n"
)


def _gateway_stream_route() -> None:
    respx.post(f"{GATEWAY}/chat/completions").mock(
        return_value=httpx.Response(
            200,
            headers={"content-type": "text/event-stream"},
            content=GATEWAY_STREAM_SSE,
        )
    )


def _completion_route(cost: str = "0.0034", cache: str = "miss") -> respx.Route:
    """Register a /chat/completions mock returning the given TT headers."""
    return respx.post(f"{GATEWAY}/chat/completions").mock(
        return_value=httpx.Response(
            200,
            headers={
                "x-tokentrimmer-trace-id": "trace-1",
                "x-tokentrimmer-provider": "anthropic",
                "x-tokentrimmer-model-used": "claude-haiku-4-5",
                "x-tokentrimmer-cost-usd": cost,
                "x-tokentrimmer-baseline-cost-usd": "0.02",
                "x-tokentrimmer-saved-usd": "0.0166",
                "x-tokentrimmer-cache": cache,
            },
            json={
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 1,
                "model": "claude-haiku-4-5",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "hi"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            },
        )
    )


def _client() -> TokenTrimmer:
    return TokenTrimmer(api_key="tt_test_x", base_url=GATEWAY)


@respx.mock
def test_parses_tt_metadata_onto_response():
    route = _completion_route()
    resp = _client().chat.completions.create(
        model="claude-haiku-4-5", messages=[{"role": "user", "content": "hi"}]
    )
    assert route.called
    meta: TokenTrimmerMeta = resp.tt
    assert meta.trace_id == "trace-1"
    assert meta.provider == "anthropic"
    assert meta.model_used == "claude-haiku-4-5"
    assert meta.cost_usd == 0.0034
    assert meta.saved_usd == 0.0166
    assert meta.cache == "miss"


@respx.mock
def test_non_numeric_cost_header_parses_to_none():
    _completion_route(cost="not-a-number")
    resp = _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}]
    )
    assert resp.tt.cost_usd is None


@respx.mock
def test_max_tokens_default_injected_when_absent():
    route = _completion_route()
    _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}]
    )
    sent = route.calls.last.request
    body = json.loads(sent.content)
    assert body["max_tokens"] == 4096


@respx.mock
def test_explicit_max_tokens_wins():
    route = _completion_route()
    _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}], max_tokens=128
    )
    body = json.loads(route.calls.last.request.content)
    assert body["max_tokens"] == 128


@respx.mock
def test_tt_tag_cost_limit_and_cache_lift_to_headers():
    route = _completion_route()
    _client().chat.completions.create(
        model="m",
        messages=[{"role": "user", "content": "hi"}],
        tt_tag="feature=chat",
        tt_cost_limit=0.05,
        tt_cache="bypass",
    )
    req = route.calls.last.request
    assert req.headers["x-tokentrimmer-tag"] == "feature=chat"
    assert float(req.headers["x-tokentrimmer-cost-limit-usd"]) == pytest.approx(0.05)
    assert req.headers["x-tokentrimmer-cache"] == "bypass"


@respx.mock
def test_invalid_tt_cache_raises_before_send():
    route = _completion_route()
    with pytest.raises(ValueError):
        _client().chat.completions.create(
            model="m",
            messages=[{"role": "user", "content": "hi"}],
            tt_cache="hit-l1",  # a response value, NOT a valid request override
        )
    assert not route.called


@respx.mock
def test_invalid_tt_cost_limit_raises_before_send():
    route = _completion_route()
    for bad in (float("inf"), float("nan"), -1.0):
        with pytest.raises(ValueError):
            _client().chat.completions.create(
                model="m",
                messages=[{"role": "user", "content": "hi"}],
                tt_cost_limit=bad,
            )
    assert not route.called


@respx.mock
def test_metadata_is_per_call_correct_under_concurrency():
    """Each concurrent caller must observe ITS OWN response's metadata.

    The mock derives a unique cost from each request's marker, so a caller that
    picked up another call's metadata would see a mismatched cost. The
    with_raw_response design reads headers off the exact per-call raw response
    (no shared stash to leak across calls/threads); this is a regression guard
    for that isolation.
    """

    def responder(request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content)
        marker = body["messages"][0]["content"]  # "req-<i>"
        i = int(marker.split("-", 1)[1])
        return httpx.Response(
            200,
            headers={"x-tokentrimmer-cost-usd": f"0.{i:04d}"},
            json={
                "id": "c",
                "object": "chat.completion",
                "created": 1,
                "model": "m",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "x"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            },
        )

    respx.post(f"{GATEWAY}/chat/completions").mock(side_effect=responder)
    client = TokenTrimmer(api_key="tt_test_x", base_url=GATEWAY)

    def call(i: int):
        r = client.chat.completions.create(
            model="m", messages=[{"role": "user", "content": f"req-{i}"}]
        )
        return i, r.tt.cost_usd

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as ex:
        results = list(ex.map(call, range(32)))

    for i, cost in results:
        assert cost == float(f"0.{i:04d}"), f"caller {i} observed {cost}"


@respx.mock
def test_streaming_strips_usage_frame_and_yields_clean_chunks():
    """The terminal tokentrimmer.usage frame must NOT reach the user as a chunk.

    Regression guard: passed straight to the OpenAI SSE parser, the usage frame
    yields a malformed ChatCompletionChunk (id=None, choices=None) that crashes
    naive `chunk.choices[0]` iteration. We strip it before the parser sees it.
    """
    _gateway_stream_route()
    stream = _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}], stream=True
    )
    chunks = list(stream)
    # Every yielded chunk is a well-formed ChatCompletionChunk with choices.
    for c in chunks:
        assert c.object == "chat.completion.chunk"
        assert c.choices is not None and len(c.choices) >= 1
    # The reassembled assistant text is the full OpenAI stream, nothing dropped.
    text = "".join(c.choices[0].delta.content or "" for c in chunks)
    assert text == "Hello world"


@respx.mock
def test_streaming_surfaces_cost_on_stream_tt():
    """The stripped usage payload is surfaced as `stream.tt` (StreamCost)."""
    _gateway_stream_route()
    stream = _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}], stream=True
    )
    # Cost isn't known until the stream is drained.
    list(stream)
    cost: StreamCost = stream.tt
    assert cost is not None
    assert cost.cost_usd == pytest.approx(0.0001)
    assert cost.baseline_cost_usd == pytest.approx(0.0004)
    assert cost.saved_usd == pytest.approx(0.0003)
    assert cost.provider_cache_saved_usd == pytest.approx(0.0)
    assert cost.input_tokens == 10
    assert cost.output_tokens == 20
    assert cost.cached_tokens == 0


@respx.mock
def test_streaming_tt_is_none_before_drain():
    """`stream.tt` is None until the terminal usage frame has been consumed."""
    _gateway_stream_route()
    stream = _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}], stream=True
    )
    assert stream.tt is None
    list(stream)
    assert stream.tt is not None


@respx.mock
def test_streaming_without_usage_frame_still_works():
    """A plain OpenAI stream (no usage frame) iterates cleanly; tt stays None."""
    sse = (
        'data: {"id":"c","object":"chat.completion.chunk","created":1,"model":"m",'
        '"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}\n\n'
        "data: [DONE]\n\n"
    )
    respx.post(f"{GATEWAY}/chat/completions").mock(
        return_value=httpx.Response(
            200, headers={"content-type": "text/event-stream"}, content=sse
        )
    )
    stream = _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}], stream=True
    )
    chunks = list(stream)
    assert chunks[0].choices[0].delta.content == "hi"
    assert stream.tt is None


# --- API-key resolution -----------------------------------------------------
# Precedence: explicit api_key arg > TOKENTRIMMER_API_KEY > OPENAI_API_KEY.


def test_explicit_api_key_wins_over_env(monkeypatch):
    monkeypatch.setenv("TOKENTRIMMER_API_KEY", "tt_from_env")
    monkeypatch.setenv("OPENAI_API_KEY", "tt_from_openai_env")
    client = TokenTrimmer(api_key="tt_explicit", base_url=GATEWAY)
    assert client.api_key == "tt_explicit"


def test_tokentrimmer_env_used_when_arg_absent(monkeypatch):
    monkeypatch.setenv("TOKENTRIMMER_API_KEY", "tt_from_env")
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)
    client = TokenTrimmer(base_url=GATEWAY)
    assert client.api_key == "tt_from_env"


def test_tokentrimmer_env_takes_precedence_over_openai_env(monkeypatch):
    monkeypatch.setenv("TOKENTRIMMER_API_KEY", "tt_from_env")
    monkeypatch.setenv("OPENAI_API_KEY", "tt_from_openai_env")
    client = TokenTrimmer(base_url=GATEWAY)
    assert client.api_key == "tt_from_env"


def test_falls_back_to_openai_env_when_no_tokentrimmer_key(monkeypatch):
    monkeypatch.delenv("TOKENTRIMMER_API_KEY", raising=False)
    monkeypatch.setenv("OPENAI_API_KEY", "tt_from_openai_env")
    client = TokenTrimmer(base_url=GATEWAY)
    assert client.api_key == "tt_from_openai_env"
