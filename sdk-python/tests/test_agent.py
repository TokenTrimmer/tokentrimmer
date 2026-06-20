"""Tests for the server-side agent-loop driver (``client.agent.run``).

These mirror the Rust ``tt-client`` agent driver tests (crates/client/src/agent.rs):
a multi-turn run that executes a client tool then resumes to a final answer with
aggregated cost; a resume-cap test; an executor-error-fed-back test; and a
terminal-on-first-response test. The gateway HTTP layer is mocked with respx so
no network is used (and the SDK's own httpx client is exercised end to end).

Endpoint shape (docs + crates/client/src/agent.rs):
- ``POST /v1/agent/runs`` with ``{ model, messages, tools?, max_turns?, stream }``.
- Resume: ``POST /v1/agent/runs/{id}/tool_outputs`` with
  ``{ tool_outputs: [{ tool_call_id, output }] }``.
- A ``Run`` body = ``{ id, status, messages, turns, usage{...,cost_usd}, ... }``.
  There are NO ``x-tokentrimmer-*`` headers on this endpoint; aggregate cost is
  ``run.usage.cost_usd``.
"""

from __future__ import annotations

import json

import httpx
import pytest
import respx

from tokentrimmer import TokenTrimmer

GATEWAY = "http://gw.test/v1"
RUNS = f"{GATEWAY}/agent/runs"
RUN_ID = "11111111-1111-1111-1111-111111111111"
RESUME = f"{RUNS}/{RUN_ID}/tool_outputs"


def _client() -> TokenTrimmer:
    return TokenTrimmer(api_key="tt_test_x", base_url=GATEWAY)


def _requires_action_run() -> dict:
    """A paused run on a client tool ``do_thing`` (id ``call_1``), unanswered."""
    return {
        "id": RUN_ID,
        "status": "requires_action",
        "turns": 1,
        "usage": {"prompt_tokens": 10, "completion_tokens": 4, "cost_usd": 0.0002},
        "messages": [
            {"role": "user", "content": "do it"},
            {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "do_thing", "arguments": '{"x":1}'},
                    }
                ],
            },
        ],
    }


def _completed_run_after_resume() -> dict:
    """Terminal run after resume: tool output appended, final text produced.

    Aggregate cost spans both turns (read from the body, not headers).
    """
    return {
        "id": RUN_ID,
        "status": "completed",
        "turns": 2,
        "usage": {"prompt_tokens": 22, "completion_tokens": 9, "cost_usd": 0.0005},
        "messages": [
            {"role": "user", "content": "do it"},
            {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "do_thing", "arguments": '{"x":1}'},
                    }
                ],
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "did the thing"},
            {"role": "assistant", "content": "All done."},
        ],
    }


@respx.mock
def test_run_executes_client_tool_then_resumes_to_completion():
    create = respx.post(RUNS).mock(
        return_value=httpx.Response(200, json=_requires_action_run())
    )
    resume = respx.post(RESUME).mock(
        return_value=httpx.Response(200, json=_completed_run_after_resume())
    )

    seen = {}

    def executor(name: str, arguments: str) -> str:
        seen["name"] = name
        seen["arguments"] = arguments
        return "did the thing"

    out = _client().agent.run(
        model="gpt-4o-mini",
        messages=[
            {"role": "system", "content": "be helpful"},
            {"role": "user", "content": "do it"},
        ],
        tools=[
            {
                "type": "function",
                "function": {"name": "do_thing", "description": "does", "parameters": {"type": "object"}},
            }
        ],
        executor=executor,
    )

    assert create.called
    assert resume.called
    # The executor saw the model's tool call.
    assert seen["name"] == "do_thing"
    assert seen["arguments"] == '{"x":1}'
    assert out.resume_rounds == 1
    assert out.run.status == "completed"
    assert out.text == "All done."
    # Aggregate cost is read from the terminal run body, not headers.
    assert out.usage.cost_usd == pytest.approx(0.0005)
    assert out.usage.prompt_tokens == 22
    # The resume body carries the tool output keyed by the pending call id.
    body = json.loads(resume.calls.last.request.content)
    assert body == {"tool_outputs": [{"tool_call_id": "call_1", "output": "did the thing"}]}


@respx.mock
def test_run_returns_immediately_when_first_response_is_terminal():
    # The gateway ran the whole loop server-side (only gateway tools) and
    # returned a completed run — no client tool, no resume.
    create = respx.post(RUNS).mock(
        return_value=httpx.Response(
            200,
            json={
                "id": "abc",
                "status": "completed",
                "turns": 3,
                "usage": {"prompt_tokens": 30, "completion_tokens": 12, "cost_usd": 0.0009},
                "messages": [
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": "Answer."},
                ],
            },
        )
    )

    def executor(name: str, arguments: str) -> str:  # pragma: no cover - never called
        raise AssertionError("executor must not run when nothing pauses")

    out = _client().agent.run(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": "hi"}],
        executor=executor,
    )
    assert create.called
    assert out.resume_rounds == 0
    assert out.text == "Answer."
    assert out.usage.cost_usd == pytest.approx(0.0009)


@respx.mock
def test_run_stops_at_resume_cap_when_run_keeps_pausing():
    # The gateway never finishes: every create/resume returns the SAME
    # requires_action run. With max_resume_rounds=2 the driver makes the create
    # + exactly 2 resumes, then returns the still-paused run.
    respx.post(RUNS).mock(return_value=httpx.Response(200, json=_requires_action_run()))
    resume = respx.post(RESUME).mock(
        return_value=httpx.Response(200, json=_requires_action_run())
    )

    out = _client().agent.run(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": "loop"}],
        tools=[
            {
                "type": "function",
                "function": {"name": "do_thing", "description": "does", "parameters": {"type": "object"}},
            }
        ],
        executor=lambda name, args: "again",
        max_resume_rounds=2,
    )
    assert out.resume_rounds == 2
    assert out.run.status == "requires_action"
    assert resume.call_count == 2


@respx.mock
def test_run_feeds_executor_error_back_as_tool_output():
    respx.post(RUNS).mock(return_value=httpx.Response(200, json=_requires_action_run()))
    resume = respx.post(RESUME).mock(
        return_value=httpx.Response(200, json=_completed_run_after_resume())
    )

    def boom(name: str, arguments: str) -> str:
        raise RuntimeError("tool exploded")

    out = _client().agent.run(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": "go"}],
        tools=[
            {
                "type": "function",
                "function": {"name": "do_thing", "description": "does", "parameters": {"type": "object"}},
            }
        ],
        executor=boom,
    )
    # The loop did NOT abort: the error was submitted as the tool output.
    assert resume.called
    body = json.loads(resume.calls.last.request.content)
    output = json.loads(body["tool_outputs"][0]["output"])
    assert "exploded" in output["error"]
    assert out.run.status == "completed"


@respx.mock
def test_run_forwards_tag_and_max_turns():
    route = respx.post(RUNS).mock(
        return_value=httpx.Response(
            200,
            json={
                "id": "z",
                "status": "completed",
                "turns": 1,
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "cost_usd": 0.0},
                "messages": [{"role": "assistant", "content": "ok"}],
            },
        )
    )
    _client().agent.run(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": "hi"}],
        executor=lambda name, args: "x",
        max_turns=4,
        tt_tag="proj-x",
    )
    req = route.calls.last.request
    assert req.headers["x-tokentrimmer-tag"] == "proj-x"
    body = json.loads(req.content)
    assert body["max_turns"] == 4
    assert body["model"] == "gpt-4o-mini"
    assert body["stream"] is False


def test_run_without_model_raises_before_any_request():
    with pytest.raises(ValueError):
        _client().agent.run(
            model="",
            messages=[{"role": "user", "content": "hi"}],
            executor=lambda name, args: "x",
        )


@respx.mock
def test_run_propagates_gateway_error_on_create():
    respx.post(RUNS).mock(return_value=httpx.Response(500, text="boom"))
    with pytest.raises(httpx.HTTPStatusError):
        _client().agent.run(
            model="gpt-4o-mini",
            messages=[{"role": "user", "content": "go"}],
            executor=lambda name, args: "x",
        )


@respx.mock
def test_run_propagates_gateway_error_on_resume():
    respx.post(RUNS).mock(return_value=httpx.Response(200, json=_requires_action_run()))
    respx.post(RESUME).mock(return_value=httpx.Response(503, text="store unavailable"))
    with pytest.raises(httpx.HTTPStatusError):
        _client().agent.run(
            model="gpt-4o-mini",
            messages=[{"role": "user", "content": "go"}],
            tools=[
                {
                    "type": "function",
                    "function": {"name": "do_thing", "description": "does", "parameters": {"type": "object"}},
                }
            ],
            executor=lambda name, args: "x",
        )
