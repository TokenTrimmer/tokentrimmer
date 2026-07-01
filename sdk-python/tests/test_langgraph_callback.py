"""LangGraph reuses the LangChain cost callback unchanged.

A LangGraph graph runs on LangChain-core's callback manager, so the #268
``TokenTrimmerCostCallback`` needs no LangGraph-specific code: passing it via
``graph.invoke(..., config={"callbacks": [cb]})`` propagates it to every LLM call
inside the graph's nodes, where ``on_llm_end`` fires with the gateway's
``x-tokentrimmer-*`` headers. These tests prove that propagation end to end with a
minimal graph + a fake chat model — no network, no ``langchain-openai`` — using
only ``langgraph`` + ``langchain-core`` (the deps CI installs via ``[test]``).
"""

from __future__ import annotations

from typing import Any, List, Optional

import pytest

pytest.importorskip("langgraph")
pytest.importorskip("langchain_core")

from langchain_core.callbacks import CallbackManagerForLLMRun  # noqa: E402
from langchain_core.language_models.chat_models import BaseChatModel  # noqa: E402
from langchain_core.messages import AIMessage, BaseMessage  # noqa: E402
from langchain_core.outputs import ChatGeneration, ChatResult  # noqa: E402
from langgraph.graph import END, START, StateGraph  # noqa: E402
from typing_extensions import TypedDict  # noqa: E402

from tokentrimmer import semconv  # noqa: E402
from tokentrimmer.integrations.langchain import TokenTrimmerCostCallback  # noqa: E402

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


class FakeGatewayChat(BaseChatModel):
    """A minimal chat model that returns an AIMessage carrying gateway headers.

    Stands in for a ``ChatOpenAI(include_response_headers=True)`` pointed at the
    gateway: it puts the ``x-tokentrimmer-*`` headers on the message's
    ``response_metadata["headers"]`` exactly where langchain-openai surfaces them,
    so the callback's ``on_llm_end`` recovers them.
    """

    @property
    def _llm_type(self) -> str:
        return "fake-gateway"

    def _generate(
        self,
        messages: List[BaseMessage],
        stop: Optional[List[str]] = None,
        run_manager: Optional[CallbackManagerForLLMRun] = None,
        **kwargs: Any,
    ) -> ChatResult:
        msg = AIMessage(
            content="hi from node",
            response_metadata={"headers": dict(TT_HEADERS)},
            usage_metadata={"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
        )
        return ChatResult(generations=[ChatGeneration(message=msg)])


class _State(TypedDict):
    messages: list


def _build_graph(llm: BaseChatModel):
    def call_model(state: _State) -> dict:
        resp = llm.invoke(state["messages"])
        return {"messages": state["messages"] + [resp]}

    g = StateGraph(_State)
    g.add_node("model", call_model)
    g.add_edge(START, "model")
    g.add_edge("model", END)
    return g.compile()


def test_callback_propagates_through_langgraph_invoke():
    app = _build_graph(FakeGatewayChat())
    cb = TokenTrimmerCostCallback(record_spans=False)

    result = app.invoke(
        {"messages": [("user", "hello")]}, config={"callbacks": [cb]}
    )

    # The graph ran the node, which called the LLM -> on_llm_end fired with the
    # gateway headers because the config callbacks propagate into the node.
    assert result["messages"][-1].content == "hi from node"
    assert cb.attributed_calls == 1
    assert cb.total_cost_usd == pytest.approx(0.0034)
    assert cb.total_saved_usd == pytest.approx(0.0166)


def test_callback_records_span_attributes_from_within_a_graph_node():
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import SimpleSpanProcessor
    from opentelemetry.sdk.trace.export.in_memory_span_exporter import (
        InMemorySpanExporter,
    )

    exporter = InMemorySpanExporter()
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(exporter))
    tracer = provider.get_tracer("test")

    app = _build_graph(FakeGatewayChat())
    cb = TokenTrimmerCostCallback()

    with tracer.start_as_current_span("graph-run"):
        app.invoke({"messages": [("user", "hi")]}, config={"callbacks": [cb]})

    (finished,) = exporter.get_finished_spans()
    attrs = dict(finished.attributes or {})
    assert attrs[semconv.TT_COST_USD] == pytest.approx(0.0034)
    assert attrs[semconv.TT_SAVED_USD] == pytest.approx(0.0166)
    assert attrs[semconv.GEN_AI_SYSTEM] == "anthropic"
    assert attrs[semconv.GEN_AI_USAGE_INPUT_TOKENS] == 10
