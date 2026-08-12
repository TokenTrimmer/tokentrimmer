"""LangChain callback that surfaces TokenTrimmer cost/savings into spans.

When you point a LangChain ``ChatOpenAI`` at the TokenTrimmer gateway (a plain
``base_url`` swap), the gateway's ``x-tokentrimmer-*`` cost/savings response
headers are invisible to the frameworks people already watch. This callback
recovers them on every LLM finish and:

1. records them as OpenTelemetry span attributes using the shared semconv keys
   (:mod:`tokentrimmer.semconv`), so a LangSmith / Tempo / Grafana trace carries
   the same ``gen_ai.*`` / ``tokentrimmer.*`` cost attributes the gateway stamps
   on its own span; and
2. accumulates a per-run cost / savings total and optionally checks a
   ``post_response_budget_usd`` after each completed LLM call. A breach raises
   :class:`BudgetExceeded` before the chain's next step; it cannot prevent or
   refund the call whose response was just observed.

How the callback gets the headers
----------------------------------

LangChain does not put raw HTTP response headers in a callback by default. The
clean hook is ``langchain_openai``'s ``include_response_headers=True``: with it
set, the response headers are attached to the output message's
``response_metadata["headers"]`` (and ``LLMResult.llm_output["headers"]``), both
of which this callback reads in ``on_llm_end``. Wire it up like::

    from langchain_openai import ChatOpenAI
    from tokentrimmer.integrations.langchain import TokenTrimmerCostCallback

    cb = TokenTrimmerCostCallback(post_response_budget_usd=0.50)
    llm = ChatOpenAI(
        model="claude-haiku-4-5",
        base_url="https://api.tokentrimmer.com/v1",
        api_key="tt_live_...",
        include_response_headers=True,  # <- surfaces x-tokentrimmer-* headers
        use_responses_api=False,        # <- gateway speaks Chat Completions
        callbacks=[cb],
    )
    llm.invoke("Hello")
    print(cb.total_cost_usd, cb.total_saved_usd)

:func:`make_gateway_chat_openai` builds exactly that pre-wired client for you.

If ``include_response_headers`` is not set (or a future LangChain release stops
surfacing headers), the callback degrades gracefully: no headers are found, no
attributes/totals are recorded, and nothing is raised — the chain runs normally,
just without cost attribution.

Scope: this is the LangChain (Python) + OTel-semconv integration. It also drives
**LangGraph** unchanged — a LangGraph graph runs on LangChain's callback manager,
so passing this callback via ``graph.invoke(..., config={"callbacks": [cb]})``
propagates it to every LLM call inside the graph's nodes (see the README). The
sibling **LiteLLM** logger lives in :mod:`tokentrimmer.integrations.litellm`;
both reuse :mod:`tokentrimmer.semconv`, :meth:`TokenTrimmerMeta.from_headers`, and
the shared :class:`~tokentrimmer.integrations._budget.BudgetExceeded` identically.
"""

from __future__ import annotations

import logging
from typing import Any, Dict, List, Mapping, Optional

try:
    from langchain_core.callbacks import BaseCallbackHandler
    from langchain_core.outputs import LLMResult
except ImportError as exc:  # pragma: no cover - exercised via the extras guard
    raise ImportError(
        "tokentrimmer.integrations.langchain requires LangChain. Install the "
        "optional extra: pip install 'tokentrimmer[langchain]'"
    ) from exc

from tokentrimmer import semconv

# BudgetExceeded is the shared per-run budget primitive (originally defined here
# in 0.2.0, lifted to _budget.py in 0.3.0 so the LiteLLM logger shares it). It is
# re-exported here so ``from tokentrimmer.integrations.langchain import
# BudgetExceeded`` keeps working.
from tokentrimmer.client import DEFAULT_BASE_URL, TokenTrimmerMeta
from tokentrimmer.integrations._budget import BudgetExceeded
from tokentrimmer.integrations._spans import record_on_current_span

_logger = logging.getLogger("tokentrimmer.integrations.langchain")

__all__ = [
    "BudgetExceeded",
    "TokenTrimmerCostCallback",
    "make_gateway_chat_openai",
]


class TokenTrimmerCostCallback(BaseCallbackHandler):
    """A LangChain callback that records TokenTrimmer cost/savings on spans.

    :param post_response_budget_usd: optional observed-cost budget (USD). After
        each LLM response completes, ``on_llm_end`` raises
        :class:`BudgetExceeded` if the accumulated served cost exceeds it. The
        exception can stop the next chain step, not the completed call.
        ``None`` (default) disables the check.
    :param record_spans: when ``True`` (default) the cost attributes are recorded
        onto the current OpenTelemetry span (if OpenTelemetry is installed and a
        span is recording). When OpenTelemetry is absent the callback still
        accumulates totals — span recording is a best-effort no-op.
    :param logger: optional logger for a per-call cost line (``DEBUG``). Defaults
        to the module logger.

    The handler is stateful: reuse one instance per logical run and read
    :attr:`total_cost_usd` / :attr:`total_saved_usd` after, or call
    :meth:`reset` to reuse it for another run.
    """

    #: Make a :class:`BudgetExceeded` raised in a callback propagate out of the
    #: LangChain call instead of being logged and swallowed.
    raise_error = True

    def __init__(
        self,
        *,
        post_response_budget_usd: Optional[float] = None,
        record_spans: bool = True,
        logger: Optional[logging.Logger] = None,
    ) -> None:
        super().__init__()
        self.post_response_budget_usd = post_response_budget_usd
        self.record_spans = record_spans
        self._logger = logger or _logger
        #: Accumulated served cost (USD) across this handler's LLM calls.
        self.total_cost_usd: float = 0.0
        #: Accumulated TokenTrimmer-attributed savings (USD).
        self.total_saved_usd: float = 0.0
        #: Accumulated baseline (un-optimised) cost (USD).
        self.total_baseline_usd: float = 0.0
        #: Number of LLM finishes that carried TokenTrimmer cost headers.
        self.attributed_calls: int = 0

    def reset(self) -> None:
        """Zero the accumulated totals so the handler can drive another run."""
        self.total_cost_usd = 0.0
        self.total_saved_usd = 0.0
        self.total_baseline_usd = 0.0
        self.attributed_calls = 0

    # -- LangChain callback hook ---------------------------------------------

    def on_llm_end(self, response: LLMResult, **kwargs: Any) -> None:
        """Record cost attributes + accumulate totals when the LLM call finishes.

        Reads the ``x-tokentrimmer-*`` headers surfaced by
        ``include_response_headers=True`` (see the module docstring), parses them
        through the SDK's canonical :meth:`TokenTrimmerMeta.from_headers`, maps
        them to OTel span attributes via :mod:`tokentrimmer.semconv`, and checks
        the optional post-response budget. A response with no TokenTrimmer
        headers is a no-op.
        """
        headers = _extract_tt_headers(response)
        if headers is None:
            # No cost headers on this finish (e.g. include_response_headers not
            # set, a cache-only path, or a self-hosted gateway without pricing).
            # Degrade quietly — never break the chain over missing telemetry.
            return

        meta = TokenTrimmerMeta.from_headers(headers)
        attrs = semconv.cost_info_to_attributes(meta)
        # Token counts aren't on the headers, but LangChain's result carries them
        # — fold them in under the gen_ai.usage.* keys when present.
        attrs.update(_token_attributes(response))

        if self.record_spans and attrs:
            record_on_current_span(attrs)

        if meta.cost_usd is not None:
            self.total_cost_usd += meta.cost_usd
        if meta.saved_usd is not None:
            self.total_saved_usd += meta.saved_usd
        if meta.baseline_cost_usd is not None:
            self.total_baseline_usd += meta.baseline_cost_usd
        self.attributed_calls += 1

        self._logger.debug(
            "tokentrimmer llm_end: cost=%s saved=%s route=%s cache=%s "
            "(run total cost=$%.6f saved=$%.6f)",
            meta.cost_usd,
            meta.saved_usd,
            meta.route,
            meta.cache,
            self.total_cost_usd,
            self.total_saved_usd,
        )

        if (
            self.post_response_budget_usd is not None
            and self.total_cost_usd > self.post_response_budget_usd
        ):
            raise BudgetExceeded(
                self.total_cost_usd, self.post_response_budget_usd
            )


# --- helpers ----------------------------------------------------------------


def _extract_tt_headers(response: LLMResult) -> Optional[Dict[str, str]]:
    """Recover the response headers from an ``LLMResult``, if present.

    ``include_response_headers=True`` attaches them either to
    ``LLMResult.llm_output["headers"]`` or to each generation message's
    ``response_metadata["headers"]`` (placement varies by langchain-openai
    version), so we check both. Only a mapping that actually contains at least
    one ``x-tokentrimmer-*`` key is returned; anything else yields ``None`` so a
    plain (non-gateway) response is treated as "no cost data".
    """
    candidates: List[Any] = []

    llm_output = getattr(response, "llm_output", None)
    if isinstance(llm_output, Mapping):
        candidates.append(llm_output.get("headers"))

    for batch in getattr(response, "generations", None) or []:
        for gen in batch:
            message = getattr(gen, "message", None)
            meta = getattr(message, "response_metadata", None)
            if isinstance(meta, Mapping):
                candidates.append(meta.get("headers"))
            info = getattr(gen, "generation_info", None)
            if isinstance(info, Mapping):
                candidates.append(info.get("headers"))

    for headers in candidates:
        if isinstance(headers, Mapping) and any(
            str(k).lower().startswith("x-tokentrimmer-") for k in headers
        ):
            return {str(k): str(v) for k, v in headers.items()}
    return None


def _token_attributes(response: LLMResult) -> Dict[str, Any]:
    """Best-effort ``gen_ai.usage.*`` token counts from an ``LLMResult``.

    Prefers the per-message ``usage_metadata`` (the modern LangChain shape) and
    falls back to ``llm_output["token_usage"]``. Returns an empty dict when no
    counts are available.
    """
    for batch in getattr(response, "generations", None) or []:
        for gen in batch:
            usage = getattr(getattr(gen, "message", None), "usage_metadata", None)
            if isinstance(usage, Mapping):
                inp = usage.get("input_tokens")
                out = usage.get("output_tokens")
                if inp is not None or out is not None:
                    attrs: Dict[str, Any] = {}
                    if inp is not None:
                        attrs[semconv.GEN_AI_USAGE_INPUT_TOKENS] = int(inp)
                    if out is not None:
                        attrs[semconv.GEN_AI_USAGE_OUTPUT_TOKENS] = int(out)
                    return attrs

    llm_output = getattr(response, "llm_output", None)
    if isinstance(llm_output, Mapping):
        tok = llm_output.get("token_usage")
        if isinstance(tok, Mapping):
            attrs = {}
            if tok.get("prompt_tokens") is not None:
                attrs[semconv.GEN_AI_USAGE_INPUT_TOKENS] = int(tok["prompt_tokens"])
            if tok.get("completion_tokens") is not None:
                attrs[semconv.GEN_AI_USAGE_OUTPUT_TOKENS] = int(
                    tok["completion_tokens"]
                )
            if attrs:
                return attrs
    return {}


def make_gateway_chat_openai(
    *,
    model: str,
    api_key: Optional[str] = None,
    base_url: str = DEFAULT_BASE_URL,
    callbacks: Optional[List[Any]] = None,
    **kwargs: Any,
) -> Any:
    """Build a ``ChatOpenAI`` pre-wired for the TokenTrimmer gateway.

    A convenience factory that sets the two easy-to-miss flags —
    ``include_response_headers=True`` (so :class:`TokenTrimmerCostCallback` can
    see the ``x-tokentrimmer-*`` headers) and ``use_responses_api=False`` (the
    gateway speaks the OpenAI **Chat Completions** schema, not the Responses API)
    — and points ``base_url`` at the gateway. Requires ``langchain-openai``.

    Any extra keyword arguments pass straight through to ``ChatOpenAI``; explicit
    ``include_response_headers`` / ``use_responses_api`` in ``kwargs`` win.
    """
    try:
        from langchain_openai import ChatOpenAI
    except ImportError as exc:  # pragma: no cover - depends on optional dep
        raise ImportError(
            "make_gateway_chat_openai requires langchain-openai. Install it with: "
            "pip install 'tokentrimmer[langchain]' langchain-openai"
        ) from exc

    kwargs.setdefault("include_response_headers", True)
    kwargs.setdefault("use_responses_api", False)
    return ChatOpenAI(
        model=model,
        base_url=base_url,
        api_key=api_key,
        callbacks=callbacks,
        **kwargs,
    )
